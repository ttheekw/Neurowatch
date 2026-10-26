//! Backend startup and shutdown.
//!
//! Responsibilities:
//! - Initialize GStreamer
//! - Load configuration
//! - Wire up pipeline supervisors, IPC server, shared memory
//! - Run NeuroWatch Cloud ([`crate::cloud`])
//! - Handle SIGTERM / SIGINT for graceful shutdown
//!
//! `nw-backend --supervise` runs [`crate::supervise`] instead, which runs
//! the backend as its child and starts it again after a crash.

use anyhow::Context as _;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::app_state::AppState;
use crate::cloud::{Cloud, Services};
use crate::stats::gpu::{CachedGpuSource, SystemGpuProbe};
use crate::stats::ipc_metrics::IpcMetricsRecorder;
use crate::stats::system::ProcSystemCollector;
use crate::stats::{
    CameraHealthSource, GpuMetricsSource, IpcMetricsSource, PerformanceAggregator,
    SystemMetricsSource,
};
use crate::{discovery, events, ipc, pipeline, playback, recording, resource_limits, viewer};

/// Run the backend until it is stopped.
pub fn main() -> anyhow::Result<()> {
    if crate::supervise::requested() {
        std::process::exit(crate::supervise::main());
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to start the async runtime")?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let config_path = PathBuf::from(std::env::var("NW_CONFIG").unwrap_or_else(|_| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("neurowatch")
            .join("config.json")
            .to_string_lossy()
            .into_owned()
    }));

    let cfg = crate::config::AppConfig::load_or_default(&config_path)?;
    crate::logging::init(&cfg.debug);
    if let Some((reason, restarts)) = crate::supervise::previous_exit() {
        tracing::warn!(
            reason = %reason,
            restarts,
            "Started again: the previous backend stopped unexpectedly"
        );
    }
    resource_limits::configure();

    // Initialize GStreamer — must happen before any gst:: calls
    gstreamer::init().context("Failed to initialize GStreamer")?;
    tracing::info!(
        version = %gstreamer::version_string(),
        "GStreamer initialized"
    );
    // GStreamer's own log joins the debug log, when that is on.
    crate::logging::apply(&cfg.debug);

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
    let hwaccel = pipeline::hwaccel::probe(cfg.video.decoder);
    tracing::info!(?hwaccel, "Hardware acceleration probed");
    if pipeline::hwaccel::waiting_for_gpu_access() {
        // Started before sign-in: GStreamer registered no hardware decoders,
        // and cannot for the life of this process. Once the graphics card can
        // be opened, stop cleanly and start again in place (`restart_in_place`).
        let restart = shutdown_tx.clone();
        tokio::spawn(async move {
            pipeline::hwaccel::gpu_accessible().await;
            tracing::info!(
                "The graphics card can be opened now; restarting the backend to decode video on it"
            );
            RESTART_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = restart.send(());
        });
    }

    // Build shared application state
    let mut initial_state = AppState::new(cfg.clone(), config_path.clone(), hwaccel);
    // `./neurowatch.sh --headless`: no desktop UI will read camera frames.
    let headless = std::env::var("NW_HEADLESS").is_ok_and(|value| value == "1");
    if headless {
        tracing::info!("Headless: no desktop UI; camera frames are converted only on demand");
    }
    initial_state.set_headless(headless);
    let cloud = Cloud::new();
    cloud.prepare(&mut initial_state);
    let state = Arc::new(RwLock::new(initial_state));
    crate::debug_monitor::spawn(Arc::clone(&state));
    {
        let policy_state = Arc::clone(&state);
        let mut policy_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(1),
            );
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = policy_shutdown.recv() => break,
                    _ = tick.tick() => policy_state.write().await.reconcile_recording_policies(),
                }
            }
        });
    }
    // Keep the segment index current in the background, so readers of it
    // (such as coverage reports) never wait for a scan; then delete the
    // oldest recordings beyond camera quotas or the disk threshold.
    let indexer = playback::handler::PlaybackHandler::new(Arc::clone(&state));
    let retention_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut retention = recording::retention::Retention::default();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut passes: u32 = 0;
        loop {
            tick.tick().await;
            indexer.index_segments().await;
            retention.run_once(&retention_state).await;
            // Old events: every 10 minutes is plenty.
            if passes % 20 == 0 {
                events::prune_old(&retention_state).await;
            }
            passes = passes.wrapping_add(1);
        }
    });
    // Recording locations that go missing (an unmounted drive or share)
    // stop recording there, and resume it when they are back.
    tokio::spawn(crate::storage_guard::watch(
        Arc::clone(&state),
        shutdown_tx.subscribe(),
    ));
    // Older recordings move to the archive tier, when one is configured.
    tokio::spawn(crate::recording::archive::run(
        Arc::clone(&state),
        shutdown_tx.subscribe(),
    ));
    // Events: system health (camera offline, recording failed, disk low)
    // and the cameras' own events over ONVIF.
    {
        let (hub, onvif) = {
            let st = state.read().await;
            (Arc::clone(&st.events), Arc::clone(&st.onvif_events))
        };
        tokio::spawn(events::health::run(
            Arc::clone(&state),
            Arc::clone(&hub),
            shutdown_tx.subscribe(),
        ));
        tokio::spawn(onvif.run(Arc::clone(&state), hub, shutdown_tx.subscribe()));
        let (hub, builtin) = {
            let st = state.read().await;
            (Arc::clone(&st.events), Arc::clone(&st.builtin_motion))
        };
        tokio::spawn(builtin.run(Arc::clone(&state), hub, shutdown_tx.subscribe()));
        tokio::spawn(crate::snapshot::run(
            Arc::clone(&state),
            shutdown_tx.subscribe(),
        ));
        let (hub, alerts) = {
            let st = state.read().await;
            (Arc::clone(&st.events), Arc::clone(&st.alerts))
        };
        tokio::spawn(alerts.run(Arc::clone(&state), hub, shutdown_tx.subscribe()));
    }
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
    let viewer_manager = Arc::new(
        viewer::ViewerManager::new(hwaccel, viewer_caps)
            .with_stream_hubs(Arc::clone(&state.read().await.streams)),
    );
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
    let running_cloud = cloud.start(Services {
        state: Arc::clone(&state),
        viewer_manager: Arc::clone(&viewer_manager),
        stats: Arc::clone(&aggregator),
        discovery: Arc::clone(&discovery),
        exports,
        config_path: config_path.clone(),
        headless,
    });

    // Start IPC server
    let mut rpc_handler = ipc::handler::RpcHandler::new(
        Arc::clone(&state),
        Arc::clone(&viewer_manager),
        Arc::clone(&aggregator),
    )
    .with_discovery(discovery)
    .with_shutdown(shutdown_tx.clone());
    if let Some(cloud) = running_cloud.rpc() {
        rpc_handler = rpc_handler.with_cloud(cloud);
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

    running_cloud.shutdown().await;

    // Stop all camera supervisors, giving each time to finalize its recording.
    let supervisors = {
        let mut st = state.write().await;
        st.stop_all();
        st.take_supervisor_tasks()
    };
    for task in supervisors {
        let _ = task.await;
    }
    // Recordings are finished: close any camera connection still open.
    state.read().await.streams.shutdown();
    state.read().await.events.flush();

    tracing::info!("Neurowatch backend stopped");
    if ipc_failed.load(std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("the IPC server could not run; see the log above");
    }
    if RESTART_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
        restart_in_place();
    }
    Ok(())
}

/// Set when the backend stops only to start again (`restart_in_place`).
static RESTART_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Make the next graceful stop start the backend again (`restart_in_place`),
/// e.g. to apply a changed video decoder choice. The caller then sends the
/// shutdown signal.
pub fn request_restart() {
    RESTART_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Exit status asking a service manager to start the backend again, when it
/// cannot replace itself (systemd `Restart=on-failure`, the dev supervisor,
/// [`crate::supervise`]).
const RESTART_EXIT_CODE: i32 = crate::supervise::RESTART_EXIT_CODE;

/// Replace this process with a fresh backend: same program, arguments,
/// environment and PID, so the service manager (systemd, the dev supervisor,
/// a detached start) sees no exit. Called after a graceful stop: recordings
/// are finished and the IPC socket closed. Should that fail, exit non-zero so
/// a service manager starts it again. Windows cannot replace a process: a
/// detached copy (same program, arguments, environment and log handles)
/// starts, and this one exits. Under [`crate::supervise`] it exits with
/// [`RESTART_EXIT_CODE`] and the supervisor starts it again.
fn restart_in_place() -> ! {
    if crate::supervise::supervised() {
        tracing::info!("Restarting: the supervisor starts the backend again");
        std::process::exit(RESTART_EXIT_CODE);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        match std::env::current_exe() {
            Ok(program) => {
                let error = std::process::Command::new(program)
                    .args(std::env::args_os().skip(1))
                    .exec();
                tracing::error!(%error, "Could not restart the backend in place; exiting so it is started again");
            }
            Err(error) => {
                tracing::error!(%error, "Could not find the backend program to restart; exiting so it is started again")
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
        match std::env::current_exe() {
            Ok(program) => {
                let spawn = |flags: u32| {
                    std::process::Command::new(&program)
                        .args(std::env::args_os().skip(1))
                        .creation_flags(flags)
                        .spawn()
                };
                // A job that forbids breakaway: still detached.
                match spawn(flags | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(flags)) {
                    Ok(child) => {
                        tracing::info!(pid = child.id(), "Started the backend again");
                        std::process::exit(0);
                    }
                    Err(error) => {
                        tracing::error!(%error, "Could not restart the backend; exiting so it is started again")
                    }
                }
            }
            Err(error) => {
                tracing::error!(%error, "Could not find the backend program to restart; exiting so it is started again")
            }
        }
    }
    std::process::exit(RESTART_EXIT_CODE);
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
        if RESTART_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            restart_in_place();
        }
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
