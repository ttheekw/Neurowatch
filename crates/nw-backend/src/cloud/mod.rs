//! NeuroWatch Cloud: linking with an enrollment code, signed check-ins and
//! the WebSocket control plane, browser live view and playback over LiveKit,
//! portal clip export, thumbnails, diagnostics and (headless) portal-managed
//! settings. Local live view, recording and playback never depend on it.
//!
//! The runtime calls it at three points: [`Cloud::prepare`] before any camera
//! starts (it installs the [`crate::taps`] on the state), [`Cloud::start`] once
//! the local services run and before the IPC server accepts connections, and
//! [`RunningCloud::shutdown`] when the backend stops, before the cameras stop.

pub mod backup;
pub mod backup_sound;
pub mod cloud_handler;
pub mod encoded_replay;
pub mod link;
pub mod live_publish;
pub mod media_publish;
pub mod playback_publish;
pub mod previews;
pub mod remote_config;
pub mod services;
pub mod snapshots;
pub mod thumbnails;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::RwLock;

use crate::app_state::{self, AppState};
use crate::discovery::DiscoveryService;
use crate::stats::PerformanceAggregator;
use crate::viewer::ViewerManager;
use crate::{ipc, playback, storage_drives};

use link::{CloudNode, CloudOptions, CloudScheduler, EndpointPolicy, SchedulerHandle};

/// The backend's running services, handed to [`Cloud::start`].
pub struct Services {
    pub state: Arc<RwLock<AppState>>,
    pub viewer_manager: Arc<ViewerManager>,
    pub stats: Arc<PerformanceAggregator>,
    /// The one camera-search service (its cache and one-search-at-a-time limit).
    pub discovery: Arc<DiscoveryService>,
    /// Clip export jobs, shared by every client.
    pub exports: Arc<crate::export::ExportService>,
    /// Analytics plugins' drawings, sent to browsers watching a camera live.
    pub overlays: Arc<crate::plugin_host::OverlayStore>,
    /// Cloud Backup's status, shared with health events and `system.status`
    /// (`AppState::backup`).
    pub backup: Arc<backup::BackupStatus>,
    pub config_path: PathBuf,
    /// No desktop UI (`--headless`).
    pub headless: bool,
}

/// NeuroWatch Cloud before it starts: the frame and keyframe taps it feeds
/// thumbnails and the inventory from.
pub struct Cloud {
    /// Which codec each camera stream turned out to be, for the cloud inventory.
    stream_codecs: Arc<live_publish::StreamCodecs>,
    /// Decoded frames for thumbnails of cameras a desktop view decodes.
    preview_hub: Arc<previews::PreviewFrameHub>,
    /// Each recording ingest's latest keyframe, for thumbnails.
    keyframes: Arc<thumbnails::KeyframeStore>,
}

impl Cloud {
    pub fn new() -> Self {
        Self {
            stream_codecs: Arc::new(live_publish::StreamCodecs::default()),
            preview_hub: Arc::new(previews::PreviewFrameHub::default()),
            keyframes: Arc::new(thumbnails::KeyframeStore::default()),
        }
    }
}

impl Default for Cloud {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::taps::FrameTap for previews::PreviewFrameHub {
    fn wants(&self, camera_id: &str) -> bool {
        previews::PreviewFrameHub::wants(self, camera_id)
    }

    fn submit_sample(&self, camera_id: &str, sample: &gstreamer::Sample) {
        previews::PreviewFrameHub::submit_sample(self, camera_id, sample)
    }
}

impl crate::taps::KeyframeSink for thumbnails::KeyframeStore {
    fn record(&self, camera_id: &str, caps: gstreamer::Caps, buffer: gstreamer::Buffer) {
        thumbnails::KeyframeStore::record(self, camera_id, caps, buffer)
    }

    fn forget(&self, camera_id: &str) {
        thumbnails::KeyframeStore::forget(self, camera_id)
    }
}

impl Cloud {
    /// Before any camera starts: feed thumbnails from desktop views'
    /// decoded frames and recordings' keyframes.
    pub fn prepare(&self, state: &mut AppState) {
        state.set_frame_tap(Arc::clone(&self.preview_hub) as _);
        state.set_keyframe_sink(Arc::clone(&self.keyframes) as _);
    }

    /// Once the local services run, before the IPC server starts.
    pub fn start(self, services: Services) -> RunningCloud {
        start(self, services)
    }
}

/// The running cloud: what is stopped when the backend stops.
pub struct RunningCloud {
    scheduler: Mutex<Option<SchedulerHandle>>,
    /// Cloud Backup's loop and its stop signal.
    backup: Mutex<
        Option<(
            tokio::sync::watch::Sender<bool>,
            tokio::task::JoinHandle<()>,
        )>,
    >,
    departing_node: Option<CloudNode>,
    publisher: Option<Arc<media_publish::PublisherSupervisor>>,
    rpc: Option<Arc<cloud_handler::CloudHandler>>,
}

impl RunningCloud {
    /// The `cloud.*` IPC methods; None when the cloud node could not start.
    pub fn rpc(&self) -> Option<Arc<cloud_handler::CloudHandler>> {
        self.rpc.clone()
    }

    /// The backend is stopping: stop checking in, tell the cloud this
    /// recorder is going offline, end browser sessions.
    pub async fn shutdown(&self) {
        // First, while the cameras still run: the backup says goodbye, so the
        // portal does not take a clean stop for a stolen recorder.
        let backup = self.backup.lock().ok().and_then(|mut backup| backup.take());
        if let Some((stop, task)) = backup {
            let _ = stop.send(true);
            if tokio::time::timeout(std::time::Duration::from_secs(8), task)
                .await
                .is_err()
            {
                tracing::warn!("Cloud Backup did not stop in time");
            }
        }
        let scheduler = self
            .scheduler
            .lock()
            .ok()
            .and_then(|mut handle| handle.take());
        if let Some(handle) = scheduler {
            handle.shutdown().await;
        }
        // After the scheduler has stopped, so no later check-in can undo it.
        if let Some(node) = &self.departing_node {
            if node.status().phase == link::LinkPhase::Linked {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    node.announce_departure(),
                )
                .await
                {
                    Ok(Ok(())) => {
                        tracing::info!("Told NeuroWatch Cloud this recorder is going offline")
                    }
                    Ok(Err(error)) => tracing::warn!(%error, "Cloud departure notice failed"),
                    Err(_) => tracing::warn!("Cloud departure notice timed out"),
                }
            }
            node.close_websocket().await;
        }
        if let Some(publisher) = &self.publisher {
            publisher.cancel_all().await;
        }
    }
}

fn start(cloud: Cloud, services: Services) -> RunningCloud {
    let Cloud {
        stream_codecs,
        preview_hub,
        keyframes,
    } = cloud;
    let Services {
        state,
        viewer_manager,
        stats: aggregator,
        discovery,
        exports: export_service,
        overlays,
        backup: backup_status,
        config_path,
        headless,
    } = services;

    let cloud_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("cloud-node");
    let cloud_options = CloudOptions::from_build_and_env();
    match cloud_options.endpoint.as_deref() {
        Some(endpoint) => {
            tracing::info!(cloud_endpoint = %endpoint, "Cloud link endpoint configured")
        }
        None => tracing::info!(
            "Cloud link endpoint not configured; cloud linking is unavailable and local \
             capture, recording and playback are unaffected"
        ),
    }
    if cloud_options.policy.allow_loopback_http {
        tracing::warn!(
            env = EndpointPolicy::DEV_ENV,
            "Cloud development mode: plain-HTTP 127.0.0.1 cloud endpoints are accepted"
        );
    }
    let cloud_policy = cloud_options.policy;
    let cloud_node = match CloudNode::open_with_options(cloud_dir, cloud_options) {
        Ok(node) => Some(node),
        Err(error) => {
            tracing::error!(error = %error, "Cloud node disabled: protected local state unavailable");
            None
        }
    };
    let Some(node) = cloud_node else {
        return RunningCloud {
            scheduler: Mutex::new(None),
            backup: Mutex::new(None),
            departing_node: None,
            publisher: None,
            rpc: None,
        };
    };

    // Browser live view and playback, published to the cloud's SFU when a
    // linked portal asks. The node needs no media settings: each directive
    // names the SFU and carries a short-lived token the cloud minted for it.
    // Browser playback plays local recordings through the same engine as the
    // desktop playback view, publishing instead of rendering to SHM. Browser
    // live view forwards each camera's own compressed stream, without decoding.
    let media_policy = if cloud_policy.allow_loopback_http {
        nw_cloud_protocol::media_session::EndpointPolicy::DEVELOPMENT_LOOPBACK
    } else {
        nw_cloud_protocol::media_session::EndpointPolicy::PUBLIC
    };
    let publisher = {
        let opener = playback_publish::RecordedPlaybackOpener::new(
            playback::handler::PlaybackHandler::new(Arc::clone(&state)),
        );
        let live_sources = live_publish::CameraLiveSourceOpener::new(
            Arc::clone(&state),
            Arc::clone(&stream_codecs),
        );
        Arc::new(
            media_publish::PublisherSupervisor::new(media_policy)
                .with_clock(node.clock())
                .with_playback(Arc::new(opener))
                .with_live_sources(Arc::new(live_sources))
                .with_overlays(overlays)
                .with_talk(Arc::new(CameraSpeaker {
                    state: Arc::clone(&state),
                })),
        )
    };

    // Only the allowlisted inventory (id, name, status, recording, which
    // streams a browser can be sent, and each stream's size and rates) leaves
    // the node.
    let inventory_state = Arc::clone(&state);
    let inventory_codecs = Arc::clone(&stream_codecs);
    let inventory: link::InventorySource = Arc::new(move || {
        let state = Arc::clone(&inventory_state);
        let codecs = Arc::clone(&inventory_codecs);
        Box::pin(async move {
            let st = state.read().await;
            link::camera_inventory_with_live(
                &st.config.cameras,
                &st.supervisor_states(),
                |camera| {
                    Some(
                        codecs
                            .live_streams_described(camera, st.camera_main_codec(&camera.id))
                            .into_iter()
                            .map(|(representation, codec)| {
                                (representation.code(), codec.map(|c| c.code()))
                            })
                            .collect(),
                    )
                },
                |camera| {
                    Some(
                        st.streams
                            .video_formats(camera)
                            .into_iter()
                            .map(|(stream, format)| (stream, link::StreamVideo::from(format)))
                            .collect(),
                    )
                },
            )
        })
    });
    // Only *when* each camera has recordings leaves the node, for the
    // portal's playback timeline: never paths, files or contents.
    let coverage_handler = Arc::new(playback::handler::PlaybackHandler::new(Arc::clone(&state)));
    let coverage: link::CoverageSource = Arc::new(move || {
        let handler = Arc::clone(&coverage_handler);
        Box::pin(async move {
            const WINDOW_MS: i64 = 31 * 24 * 60 * 60 * 1000;
            let since = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
                - WINDOW_MS;
            link::camera_coverage(
                handler
                    .coverage_report(since, link::MAX_COVERAGE_RANGES)
                    .await,
            )
        })
    });
    let mut scheduler = CloudScheduler::new(
        node.shared(),
        std::time::Duration::from_secs(60),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
    .with_inventory(inventory)
    .with_coverage(coverage);
    let preview_state = Arc::clone(&state);
    // Thumbnails never start a decoder that keeps running: a camera a
    // desktop view is already decoding gives its current frame; any other
    // its recording's latest keyframe, its own JPEG snapshot (ONVIF), or
    // one keyframe from its stream (see `thumbnails`).
    let snapshots = Arc::new(thumbnails::Thumbnails::new(Arc::clone(&keyframes)));
    let preview_source: link::PreviewSource = Arc::new(move |camera_ids| {
        let hub = Arc::clone(&preview_hub);
        let state = Arc::clone(&preview_state);
        let snapshots = Arc::clone(&snapshots);
        Box::pin(async move {
            let (states, decoding, cameras, streams) = {
                let st = state.read().await;
                (
                    st.supervisor_states(),
                    st.viewed_cameras(),
                    st.config.cameras.clone(),
                    (*st.streams).clone(),
                )
            };
            let mut jobs = tokio::task::JoinSet::new();
            for camera_id in previews::streaming_only(camera_ids, &states) {
                let hub = Arc::clone(&hub);
                let snapshots = Arc::clone(&snapshots);
                let camera = cameras
                    .iter()
                    .find(|camera| camera.id == camera_id)
                    .cloned();
                let decoded = decoding.contains(&camera_id);
                let streams = streams.clone();
                jobs.spawn(async move {
                    let jpeg_b64 = if decoded {
                        hub.capture(&camera_id, std::time::Duration::from_secs(4))
                            .await
                    } else {
                        match camera {
                            Some(camera) => snapshots.fetch(&camera, &streams).await,
                            None => None,
                        }
                    };
                    jpeg_b64.map(|jpeg_b64| link::ThumbnailImage {
                        camera_id,
                        jpeg_b64,
                    })
                });
            }
            let mut images = Vec::new();
            while let Some(result) = jobs.join_next().await {
                if let Ok(Some(image)) = result {
                    images.push(image);
                }
            }
            images
        })
    });
    scheduler = scheduler.with_preview(preview_source);
    // Live diagnostics for a watching portal page: the same figures as the
    // desktop Performance page, plus disk space. Desktop and headless alike.
    let diagnostics_state = Arc::clone(&state);
    let diagnostics_aggregator = Arc::clone(&aggregator);
    let diagnostics: link::DiagnosticsSource = Arc::new(move || {
        let state = Arc::clone(&diagnostics_state);
        let aggregator = Arc::clone(&diagnostics_aggregator);
        Box::pin(async move {
            let (hwaccel, reported_ids, storage_path, stream_connections) = {
                let st = state.read().await;
                let mut streams = st.streams.diagnostics(&st.config.cameras);
                // Plugin names, paths and decoders per stream, as the desktop shows them.
                crate::streams::diagnostics::with_plugins(
                    &mut streams,
                    &st.plugins
                        .stream_consumers(&st.config.plugins, &st.config.cameras),
                );
                (
                    st.hwaccel,
                    link::reported_camera_ids(&st.config.cameras),
                    app_state::expand_user_path(&st.config.recording.storage_path),
                    streams,
                )
            };
            let (system, drives) = tokio::task::spawn_blocking(move || {
                (
                    link::SystemInfo::collect(),
                    storage_drives::list(true, &storage_path).unwrap_or_default(),
                )
            })
            .await
            .unwrap_or_default();
            let mut snapshot = link::diagnostics_snapshot(
                aggregator.snapshot(),
                ipc::handler::acceleration_json(hwaccel),
                &reported_ids,
                system,
                &drives,
            );
            let streams = stream_connections
                .into_iter()
                .filter_map(|mut row| {
                    row.camera_id = reported_ids.get(&row.camera_id)?.clone();
                    Some(row)
                })
                .collect();
            crate::streams::diagnostics::attach(
                &mut snapshot,
                streams,
                link::MAX_DIAGNOSTICS_BYTES,
            );
            snapshot
        })
    });
    scheduler = scheduler.with_diagnostics(diagnostics);
    // Clips the web portal asks for: cut here from the recordings (the
    // video copied), uploaded to the presigned URL the portal gives.
    let export_lookup = Arc::new(playback::handler::PlaybackHandler::new(Arc::clone(&state)));
    let segments: link::SegmentLookup = Arc::new(move |camera_id, start_ms, end_ms| {
        let handler = Arc::clone(&export_lookup);
        Box::pin(async move { handler.segments_between(&camera_id, start_ms, end_ms).await })
    });
    scheduler = scheduler.with_exports(link::ExportSource {
        service: Arc::clone(&export_service),
        segments,
    });
    {
        let publisher = Arc::clone(&publisher);
        let linked = node.shared();
        let sink: link::DirectiveSink = Arc::new(move |batch| {
            let publisher = Arc::clone(&publisher);
            // Directives arrive over this node's authenticated channel, so
            // their grants must name the node it is linked as right now.
            let linked_node_id = linked.status().node_id;
            Box::pin(async move {
                let active_session_ids = batch.active_session_ids;
                for directive in batch.directives {
                    if let Err(error) = publisher.start(directive, linked_node_id.as_deref()) {
                        tracing::warn!(error = %error, "Publish directive refused");
                    }
                }
                publisher.reconcile(&active_session_ids).await;
            })
        });
        scheduler = scheduler.with_directives(sink);
    }
    {
        // The portal's settings, events and PTZ, desktop or headless alike.
        let remote = Arc::new(remote_config::RemoteConfig::new(
            ipc::handler::RpcHandler::new(
                Arc::clone(&state),
                Arc::clone(&viewer_manager),
                Arc::clone(&aggregator),
            )
            .with_discovery(Arc::clone(&discovery)),
            Arc::clone(&state),
        ));
        let handler: link::RemoteConfigHandler = Arc::new(move |method, params, sealed| {
            let remote = Arc::clone(&remote);
            Box::pin(async move { remote.execute_from_portal(&method, &params, sealed).await })
        });
        scheduler = scheduler.with_remote_config(handler);
        tracing::info!(headless, "Portal settings and event channel enabled");
    }
    let scheduler_handle = scheduler.spawn();

    // Cloud Backup: the cameras the portal says have a plan, copied live.
    let (backup_stop, backup_stopped) = tokio::sync::watch::channel(false);
    // Hosted plugin services follow the portal, and stop with the backup.
    tokio::spawn(services::run_loop(
        node.shared(),
        Arc::clone(&state),
        backup_stopped.clone(),
    ));
    let backup_task = tokio::spawn(backup::run_loop(
        node.shared(),
        backup::BackupSource {
            state: Arc::clone(&state),
            outage_dir: backup::default_outage_dir(),
            status: Arc::clone(&backup_status),
        },
        backup_stopped,
    ));

    let departing_node = node.shared();
    let cloud_handler = cloud_handler::CloudHandler::new(node)
        .with_publisher(Arc::clone(&publisher))
        .with_backup_status(backup_status);
    RunningCloud {
        scheduler: Mutex::new(Some(scheduler_handle)),
        backup: Mutex::new(Some((backup_stop, backup_task))),
        departing_node: Some(departing_node),
        publisher: Some(publisher),
        rpc: Some(Arc::new(cloud_handler)),
    }
}

/// Live viewers talking through a camera's speaker: the recorder's talk
/// service with the viewer's decoded voice as its sound.
struct CameraSpeaker {
    state: Arc<tokio::sync::RwLock<crate::app_state::AppState>>,
}

/// A talk client id for a viewer: `web-` and their LiveKit identity, bounded.
fn speaker_client(speaker: &str) -> String {
    let id: String = speaker
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(56)
        .collect();
    format!("web-{id}")
}

impl media_publish::TalkSink for CameraSpeaker {
    fn start(
        &self,
        camera_id: String,
        speaker: String,
    ) -> media_publish::BoxFuture<'static, Result<std::sync::mpsc::Sender<Vec<i16>>, String>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let (camera, talk) = {
                let st = state.read().await;
                let camera = st
                    .config
                    .cameras
                    .iter()
                    .find(|c| c.id == camera_id)
                    .cloned()
                    .ok_or("camera not found")?;
                (camera, Arc::clone(&st.talk))
            };
            let (samples, receiver) = std::sync::mpsc::channel();
            talk.start(
                &camera,
                &speaker_client(&speaker),
                crate::talk::TalkSource::Samples(receiver),
            )
            .await?;
            Ok(samples)
        })
    }

    fn renew(&self, speaker: &str) {
        let (state, client) = (Arc::clone(&self.state), speaker_client(speaker));
        tokio::spawn(async move {
            let _ = state.read().await.talk.renew(&client);
        });
    }

    fn stop(&self, speaker: &str) {
        let (state, client) = (Arc::clone(&self.state), speaker_client(speaker));
        tokio::spawn(async move { state.read().await.talk.stop(&client) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::test_support::{StaticCameras, StaticGpu, StaticIpc, StaticSystem};
    use nw_core::stats::{GpuStatus, IpcMetrics, ProcessMetrics};

    /// The runtime's three calls with nothing linked: the taps are installed,
    /// the cloud starts (its node in the config's folder, its loops idle: an
    /// unlinked node makes no request), answers `cloud.status`, and stops.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unlinked_cloud_starts_answers_and_stops() {
        let dir = std::env::temp_dir().join(format!("nw-cloud-start-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        let mut app = AppState::with_database(
            crate::config::AppConfig::default(),
            config_path.clone(),
            crate::pipeline::hwaccel::HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        );
        let cloud = Cloud::new();
        cloud.prepare(&mut app);
        let (exports, overlays, backup) = (
            Arc::clone(&app.exports),
            Arc::clone(&app.plugins.overlays),
            Arc::clone(&app.backup),
        );
        let state = Arc::new(RwLock::new(app));
        let stats = Arc::new(PerformanceAggregator::new(
            Arc::new(StaticSystem(ProcessMetrics {
                cpu_percent: 0.0,
                rss_bytes: 0,
                memory_percent: 0.0,
                system_cpu_percent: None,
                system_memory_percent: None,
                network: None,
                siblings: vec![],
            })),
            Arc::new(StaticGpu(GpuStatus::unavailable("test"))),
            Arc::new(StaticIpc(IpcMetrics::default())),
            Arc::new(StaticCameras(std::sync::Mutex::new(vec![]))),
        ));
        let running = cloud.start(Services {
            state: Arc::clone(&state),
            viewer_manager: Arc::new(ViewerManager::new(
                crate::pipeline::hwaccel::HwAccelKind::Software,
                crate::viewer::capabilities::ViewerCapabilities::unavailable("test"),
            )),
            stats,
            discovery: Arc::new(DiscoveryService::new()),
            exports,
            overlays,
            backup,
            config_path,
            headless: true,
        });
        let rpc = running.rpc().expect("the cloud node opened");
        let status = rpc
            .dispatch("cloud.status", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(status["phase"], "unlinked", "{status}");
        assert!(
            dir.join("cloud-node").is_dir(),
            "the node lives beside the config"
        );
        tokio::time::timeout(std::time::Duration::from_secs(20), running.shutdown())
            .await
            .expect("shutdown finishes");
        let _ = std::fs::remove_dir_all(dir);
    }
}
