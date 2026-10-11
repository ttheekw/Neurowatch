//! Backend startup and shutdown.
//!
//! Responsibilities:
//! - Initialize GStreamer
//! - Load configuration
//! - Wire up pipeline supervisors, IPC server, shared memory
//! - Run an optional [`Extension`]
//! - Handle SIGTERM / SIGINT for graceful shutdown

use anyhow::Context as _;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing_subscriber::EnvFilter;

use crate::app_state::AppState;
use crate::extension::{Extension, Services};
use crate::stats::gpu::{CachedGpuSource, SystemGpuProbe};
use crate::stats::ipc_metrics::IpcMetricsRecorder;
use crate::stats::system::ProcSystemCollector;
use crate::stats::{
    CameraHealthSource, GpuMetricsSource, IpcMetricsSource, PerformanceAggregator,
    SystemMetricsSource,
};
use crate::{discovery, ipc, pipeline, playback, recording, resource_limits, viewer};

/// Run the backend until it is stopped, with an optional extension.
pub fn main_with(extension: Option<Box<dyn Extension>>) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to start the async runtime")?
        .block_on(run(extension))
}

async fn run(mut extension: Option<Box<dyn Extension>>) -> anyhow::Result<()> {
    let config_path = PathBuf::from(std::env::var("NW_CONFIG").unwrap_or_else(|_| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("neurowatch")
            .join("config.json")
            .to_string_lossy()
            .into_owned()
    }));

    let cfg = crate::config::AppConfig::load_or_default(&config_path)?;
    let extension_logs = extension
        .as_ref()
        .map(|extension| extension.log_directives())
        .unwrap_or_default();
    init_tracing(&cfg.debug.gst_log, &extension_logs);
    resource_limits::configure();

    // Initialize GStreamer — must happen before any gst:: calls
    gstreamer::init().context("Failed to initialize GStreamer")?;
    tracing::info!(
        version = %gstreamer::version_string(),
        "GStreamer initialized"
    );

    // Graceful shutdown channel
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("Shutdown signal received — stopping gracefully");
        let _ = shutdown_tx_clone.send(());
    });

    tracing::info!(
        path = %config_path.display(),
        cameras = cfg.cameras.len(),
        "Config loaded"
    );

    // Probe hardware acceleration once at startup
    let hwaccel = pipeline::hwaccel::probe();
    tracing::info!(?hwaccel, "Hardware acceleration probed");

    // Build shared application state
    let mut initial_state = AppState::new(cfg.clone(), config_path.clone(), hwaccel);
    // `./neurowatch.sh --headless`: no desktop UI will read camera frames.
    let headless = std::env::var("NW_HEADLESS").is_ok_and(|value| value == "1");
    if headless {
        tracing::info!("Headless: no desktop UI; camera frames are converted only on demand");
    }
    initial_state.set_headless(headless);
    if let Some(extension) = extension.as_mut() {
        extension.prepare(&mut initial_state);
    }
    let state = Arc::new(RwLock::new(initial_state));
    // Keep the segment index current in the background, so readers of it
    // (such as coverage reports) never wait for a scan; then delete the
    // oldest recordings beyond camera quotas or the disk threshold.
    let indexer = playback::handler::PlaybackHandler::new(Arc::clone(&state));
    let retention_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut retention = recording::retention::Retention::default();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            indexer.index_segments().await;
            retention.run_once(&retention_state).await;
        }
    });
    // Spawn a pipeline supervisor for each configured camera
    {
        let mut st = state.write().await;
        for cam in &cfg.cameras {
            st.spawn_supervisor(cam);
        }
    }
    tracing::info!(count = cfg.cameras.len(), "Pipeline supervisors spawned");

    // Build the native viewer runtime (probes for VideoOverlay-capable sinks).
    let viewer_caps = if pipeline::hwaccel::live_view_block_reason().is_some() {
        viewer::capabilities::ViewerCapabilities::unavailable("gstreamer_va_update_required")
    } else {
        viewer::probe_capabilities()
    };
    tracing::info!(
        available = viewer_caps.available,
        preferred_sink = ?viewer_caps.preferred_sink,
        reason = ?viewer_caps.reason,
        "Native viewer capabilities probed"
    );
    let viewer_manager = Arc::new(viewer::ViewerManager::new(hwaccel, viewer_caps));
    // A desktop UI that stops renewing its view lease (closed or crashed)
    // must not leave decoders behind: drop lapsed leases, stop their views
    // and detach their native sessions.
    {
        let state = Arc::clone(&state);
        let viewer_manager = Arc::clone(&viewer_manager);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let lapsed = state
                    .write()
                    .await
                    .expire_view_demand(std::time::Instant::now());
                for client in lapsed {
                    let detached = viewer_manager.detach_owner(&client);
                    if detached > 0 {
                        tracing::info!(
                            native_sessions = detached,
                            "Native sessions of a lapsed desktop view detached"
                        );
                    }
                }
            }
        });
    }

    // Build stats collectors: real system/GPU probes + shared per-camera
    // health registry + shared IPC recorder.
    let ipc_recorder = IpcMetricsRecorder::new();
    let system_source: Arc<dyn SystemMetricsSource> = Arc::new(ProcSystemCollector::new());
    let gpu_source: Arc<dyn GpuMetricsSource> = Arc::new(CachedGpuSource::spawn(
        SystemGpuProbe,
        std::time::Duration::from_secs(2),
    ));
    let ipc_source: Arc<dyn IpcMetricsSource> = Arc::new(ipc_recorder.clone());
    let camera_source: Arc<dyn CameraHealthSource> = state.read().await.stream_health.clone();
    let aggregator = Arc::new(PerformanceAggregator::new(
        system_source,
        gpu_source,
        ipc_source,
        camera_source,
    ));

    // One camera-search service for every client: they share its result
    // cache and its one-search-at-a-time limit.
    let discovery = Arc::new(discovery::DiscoveryService::new());
    let exports = Arc::clone(&state.read().await.exports);
    let running_extension = extension.map(|extension| {
        extension.start(Services {
            state: Arc::clone(&state),
            viewer_manager: Arc::clone(&viewer_manager),
            stats: Arc::clone(&aggregator),
            discovery: Arc::clone(&discovery),
            exports,
            config_path: config_path.clone(),
            headless,
        })
    });

    // Start IPC server
    let mut rpc_handler = ipc::handler::RpcHandler::new(
        Arc::clone(&state),
        Arc::clone(&viewer_manager),
        Arc::clone(&aggregator),
    )
    .with_discovery(discovery)
    .with_shutdown(shutdown_tx.clone());
    if let Some(rpc) = running_extension.as_ref().and_then(|running| running.rpc()) {
        rpc_handler = rpc_handler.with_extension(rpc);
    }
    let handler = Arc::new(rpc_handler);
    let ipc_shutdown = shutdown_tx.subscribe();
    let ipc_metrics = ipc_recorder.clone();
    // Without its IPC endpoint nothing can reach or stop this backend (the UI,
    // the tray, the service manager): stop, and exit with an error so the
    // service manager sees a failed start instead of a running service.
    let ipc_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ipc_failure = Arc::clone(&ipc_failed);
    let ipc_stop = shutdown_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = ipc::server::run(handler, ipc_shutdown, ipc_metrics).await {
            tracing::error!(error = %e, "IPC server failed; stopping the backend");
            ipc_failure.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = ipc_stop.send(());
        }
    });

    // Wait for shutdown signal
    shutdown_tx.subscribe().recv().await.ok();
    shutdown_watchdog();

    if let Some(running) = &running_extension {
        running.shutdown().await;
    }

    // Stop all camera supervisors, giving each time to finalize its recording.
    let supervisors = {
        let mut st = state.write().await;
        st.stop_all();
        st.take_supervisor_tasks()
    };
    for task in supervisors {
        let _ = task.await;
    }

    tracing::info!("Neurowatch backend stopped");
    if ipc_failed.load(std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("the IPC server could not run; see the log above");
    }
    Ok(())
}

/// SIGINT (Ctrl+C) or, on Unix, SIGTERM — what the launchers send when the
/// desktop UI closes. Without the SIGTERM arm the process died on the spot:
/// recordings were not finalized and nothing heard the backend leave.
/// Longest a graceful shutdown may take. Camera supervisors run on Tokio's
/// blocking pool, and a GStreamer teardown that blocks (for example on a camera
/// that just went away) would otherwise keep the process alive forever.
const SHUTDOWN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

/// Bound the graceful shutdown that just started: a second Ctrl+C or SIGTERM
/// exits at once, and the process exits on its own after [`SHUTDOWN_DEADLINE`].
fn shutdown_watchdog() {
    tokio::spawn(async {
        wait_for_shutdown_signal().await;
        tracing::warn!("Second stop signal: exiting without waiting for cameras to stop");
        std::process::exit(130);
    });
    // A plain thread: it must keep running while the runtime itself shuts down.
    std::thread::spawn(|| {
        std::thread::sleep(SHUTDOWN_DEADLINE);
        tracing::warn!(
            seconds = SHUTDOWN_DEADLINE.as_secs(),
            "Cameras did not stop in time; exiting anyway (their last recording segment may be incomplete)"
        );
        std::process::exit(0);
    });
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "SIGTERM handler unavailable; only Ctrl+C stops gracefully")
            }
        }
    }
    tokio::signal::ctrl_c().await.ok();
}

fn init_tracing(default_level: &str, extension_directives: &[String]) {
    let gst_level = normalize_tracing_level(default_level);
    let mut fallback_filter = default_log_filter(gst_level);
    for directive in extension_directives {
        fallback_filter.push(',');
        fallback_filter.push_str(directive);
    }
    let env_filter = std::env::var("RUST_LOG")
        .ok()
        .map(|value| normalize_env_filter(&value))
        .and_then(|value| EnvFilter::try_new(value).ok());
    let filter = env_filter.unwrap_or_else(|| {
        EnvFilter::try_new(&fallback_filter)
            .unwrap_or_else(|_| EnvFilter::new("nw_backend=info,neurowatch=info,gstreamer=warn"))
    });
    let json = std::env::var("NW_LOG_JSON").is_ok();
    if json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt()
            .pretty()
            .with_env_filter(filter)
            .init();
    }
}

/// Default directives when `RUST_LOG` is unset: this crate's own target at
/// info (it logs as `nw_backend::…`), with `neurowatch` kept for compatibility.
pub fn default_log_filter(gst_level: &str) -> String {
    format!("nw_backend=info,neurowatch=info,gstreamer={gst_level}")
}

fn normalize_tracing_level(level: &str) -> &str {
    match level.trim().to_ascii_lowercase().as_str() {
        "warning" => "warn",
        "err" => "error",
        "off" => "off",
        "error" => "error",
        "warn" => "warn",
        "info" => "info",
        "debug" => "debug",
        "trace" => "trace",
        "0" => "0",
        "1" => "1",
        "2" => "2",
        "3" => "3",
        "4" => "4",
        "5" => "5",
        _ => "warn",
    }
}

fn normalize_env_filter(filter: &str) -> String {
    filter
        .split(',')
        .map(|directive| {
            let Some((target, level)) = directive.rsplit_once('=') else {
                return directive.to_owned();
            };
            format!("{target}={}", normalize_tracing_level(level))
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_log_filter_enables_this_crates_own_log_target() {
        // Events are emitted under the crate target `nw_backend::…`; a filter
        // that only names `neurowatch` silently drops every backend log line.
        let filter = default_log_filter("warn");
        assert!(
            filter.split(',').any(|d| d == "nw_backend=info"),
            "{filter}"
        );
        assert!(EnvFilter::try_new(&filter).is_ok());
    }
}
