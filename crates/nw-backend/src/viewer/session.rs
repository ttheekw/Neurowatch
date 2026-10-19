//! A single native viewer session — one GStreamer pipeline bound to one
//! UI-provided `VideoOverlay` window handle.

use gstreamer::prelude::*;
use gstreamer_video::prelude::*;
use nw_core::camera::{CameraConfig, Transport};

use super::manager::WindowHandle;
use super::{PipelineErrorClass, ViewerError};
use crate::pipeline::builder::url_with_credentials_str;
use crate::pipeline::hwaccel::{color_converter, h264_decoder, HwAccelKind};
use crate::pipeline::sanitize_credentials;

pub type ViewerSessionId = String;

/// A live viewer session.  Dropping the value transitions the pipeline to
/// `Null` and releases all GStreamer resources.
pub struct ViewerSession {
    #[allow(dead_code)]
    id: ViewerSessionId,
    camera_id: String,
    pipeline: gstreamer::Pipeline,
    videosink: gstreamer::Element,
    /// Feeds the pipeline from the camera's shared connection, if it uses one.
    pump: Option<HubPump>,
}

/// Moves units from a subscription into the session's `appsrc` on its own
/// thread, until stopped or the connection closes.
struct HubPump {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Most bytes waiting in the session's `appsrc` before units are left in its
/// subscription (whose bound then skips to the next keyframe).
const BACKLOG_BYTES: u64 = 8 * 1024 * 1024;

impl HubPump {
    fn start(
        subscription: crate::streams::Subscription,
        input: gstreamer_app::AppSrc,
        camera_id: &str,
    ) -> Result<Self, std::io::Error> {
        use crate::streams::{Delivery, Phase};
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = std::sync::Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name(format!("viewer-{camera_id}"))
            .spawn(move || {
                let mut feed = crate::streams::feed::VideoFeed::new(input);
                while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                    if feed.backlog_bytes() > BACKLOG_BYTES {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    }
                    match subscription.recv(std::time::Duration::from_millis(100)) {
                        // An overlay has no supervisor to rebuild it: the
                        // decoder follows a format change.
                        Some(Delivery::Unit { unit, after_gap }) => {
                            if let Err(
                                gstreamer::FlowError::Error | gstreamer::FlowError::NotNegotiated,
                            ) = feed.push(&unit, after_gap, true)
                            {
                                break;
                            }
                        }
                        Some(Delivery::Discontinuity { .. }) => feed.restart(),
                        None if subscription.state().phase == Phase::Closed => break,
                        None => {}
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for HubPump {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl ViewerSession {
    /// Build and start a viewer pipeline that decodes `camera` and renders
    /// to `handle` using `sink_factory` (e.g. `"glimagesink"`).
    pub fn launch(
        id: ViewerSessionId,
        camera: &CameraConfig,
        hwaccel: HwAccelKind,
        sink_factory: &str,
        handle: WindowHandle,
        hubs: Option<&std::sync::Arc<crate::streams::StreamHubs>>,
    ) -> Result<Self, ViewerError> {
        let build_error = || ViewerError::Pipeline {
            camera_id: camera.id.clone(),
            class: PipelineErrorClass::Build,
        };
        let subscription = match hubs.filter(|_| {
            crate::streams::enabled()
                && crate::pipeline::builder::shared_view_supported(&camera.url)
        }) {
            Some(hubs) => Some(
                hubs.subscribe(
                    camera,
                    crate::streams::SubscriberPolicy::desktop_view(),
                    None,
                )
                .map_err(|_| build_error())?,
            ),
            None => None,
        };
        let pipeline_str = if subscription.is_some() {
            shared_pipeline_str(hwaccel, sink_factory)
        } else {
            build_pipeline_str(camera, hwaccel, sink_factory)
        };
        // Sanitize any parse-time error before it enters the log or an
        // IPC response — GStreamer includes the pipeline text (with the
        // camera URL) in its parse errors.
        let pipeline = gstreamer::parse::launch(&pipeline_str)
            .map_err(|e| {
                let sanitized = sanitize_credentials(&e.to_string());
                tracing::warn!(
                    camera_id = %camera.id,
                    session_id = %id,
                    error = %sanitized,
                    "viewer pipeline build failed",
                );
                ViewerError::Pipeline {
                    camera_id: camera.id.clone(),
                    class: PipelineErrorClass::Build,
                }
            })?
            .downcast::<gstreamer::Pipeline>()
            .map_err(|_| ViewerError::Pipeline {
                camera_id: camera.id.clone(),
                class: PipelineErrorClass::Build,
            })?;

        let videosink = pipeline
            .by_name("viewer_sink")
            .ok_or_else(|| ViewerError::Pipeline {
                camera_id: camera.id.clone(),
                class: PipelineErrorClass::Build,
            })?;

        attach_window_handle(&videosink, handle).map_err(|_| ViewerError::Pipeline {
            camera_id: camera.id.clone(),
            class: PipelineErrorClass::OverlayAttach,
        })?;

        pipeline
            .set_state(gstreamer::State::Playing)
            .map_err(|_| ViewerError::Pipeline {
                camera_id: camera.id.clone(),
                class: PipelineErrorClass::StateChange,
            })?;

        let pump = match subscription {
            Some(subscription) => {
                let input = pipeline
                    .by_name(SHARED_INPUT)
                    .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
                    .ok_or_else(build_error)?;
                Some(HubPump::start(subscription, input, &camera.id).map_err(|_| build_error())?)
            }
            None => None,
        };

        Ok(Self {
            id,
            camera_id: camera.id.clone(),
            pipeline,
            videosink,
            pump,
        })
    }

    #[allow(dead_code)]
    pub fn camera_id(&self) -> &str {
        &self.camera_id
    }

    /// Rebind the running pipeline to a new UI window handle.
    pub fn set_window(&self, handle: WindowHandle) -> Result<(), ViewerError> {
        attach_window_handle(&self.videosink, handle).map_err(|_| ViewerError::Pipeline {
            camera_id: self.camera_id.clone(),
            class: PipelineErrorClass::OverlayAttach,
        })
    }
}

impl Drop for ViewerSession {
    fn drop(&mut self) {
        // Stop feeding first; the subscription ends with the pump.
        self.pump = None;
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

const SHARED_INPUT: &str = "viewer_src";

/// A native viewer session decoding units from a shared camera connection.
pub fn shared_pipeline_str(hwaccel: HwAccelKind, sink_factory: &str) -> String {
    let converter = color_converter(hwaccel);
    format!(
        "appsrc name={SHARED_INPUT} format=time is-live=true do-timestamp=false block=false max-bytes=0 ! \
         decodebin ! {converter} ! {sink_factory} name=viewer_sink sync=false"
    )
}

fn attach_window_handle(sink: &gstreamer::Element, handle: WindowHandle) -> Result<(), ()> {
    // The capability probe excludes Wayland: a wl_surface pointer from the
    // UI process is invalid in this process, even though it fits in uintptr_t.
    let overlay = sink
        .dynamic_cast_ref::<gstreamer_video::VideoOverlay>()
        .ok_or(())?;
    // Safety: the UI is responsible for keeping the referenced window alive
    // for the lifetime of the session and passing a handle for the same
    // window system as the sink. Invalid handles can crash native libraries.
    unsafe {
        overlay.set_window_handle(handle.raw());
    }
    Ok(())
}

/// Compose the GStreamer pipeline string for a native viewer session.
///
/// Public for unit-test coverage; callers should use `ViewerSession::launch`.
pub fn build_pipeline_str(
    camera: &CameraConfig,
    hwaccel: HwAccelKind,
    sink_factory: &str,
) -> String {
    let sink_tail = format!("{sink_factory} name=viewer_sink sync=false");
    let url = &camera.url;
    if url.starts_with("rtsp://") || url.starts_with("rtsps://") {
        let protocols = match camera.transport {
            Transport::Tcp => "tcp",
            Transport::Udp => "udp",
        };
        let effective = url_with_credentials_str(
            &camera.url,
            camera.username.as_deref(),
            camera.password_b64.as_deref(),
        );
        let location = gst_quote(&effective);
        let converter = color_converter(hwaccel);
        format!(
            "rtspsrc location=\"{location}\" latency=100 protocols={protocols} \
               do-rtsp-keep-alive=true retry=5 ! \
             application/x-rtp,media=video ! decodebin ! {converter} ! {sink_tail}",
        )
    } else if url.starts_with("http://") || url.starts_with("https://") {
        let effective = url_with_credentials_str(
            &camera.url,
            camera.username.as_deref(),
            camera.password_b64.as_deref(),
        );
        let location = gst_quote(&effective);
        format!(
            "souphttpsrc location=\"{location}\" is-live=true ! \
             multipartdemux ! jpegdec ! videoconvert ! {sink_tail}",
        )
    } else if url.starts_with("rtmp://") {
        let location = gst_quote(&camera.url);
        let decoder = h264_decoder(hwaccel);
        format!(
            "rtmpsrc location=\"{location}\" ! flvdemux ! h264parse ! {decoder} ! \
             videoconvert ! {sink_tail}",
        )
    } else if url.starts_with("/dev/video") {
        let device = gst_quote(&camera.url);
        format!("v4l2src device=\"{device}\" ! videoconvert ! {sink_tail}",)
    } else if std::path::Path::new(url).exists() {
        let path = gst_quote(&camera.url);
        format!("filesrc location=\"{path}\" ! decodebin ! videoconvert ! {sink_tail}",)
    } else {
        // Default: assume RTSP without credentials.
        let location = gst_quote(&camera.url);
        let converter = color_converter(hwaccel);
        format!(
            "rtspsrc location=\"{location}\" latency=100 ! \
             application/x-rtp,media=video ! decodebin ! {converter} ! {sink_tail}",
        )
    }
}

fn gst_quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::build_pipeline_str;
    use crate::pipeline::hwaccel::HwAccelKind;
    use nw_core::camera::CameraConfig;

    /// A native session's decoder is fed from the camera's shared connection
    /// (an `appsink` stands in for the overlay sink, which needs a window).
    #[test]
    fn a_native_session_decodes_the_shared_connection() {
        use gstreamer::prelude::*;
        gstreamer::init().unwrap();
        let path = std::env::temp_dir().join(format!(
            "nw-hub-overlay-{}.mkv",
            uuid::Uuid::new_v4().simple()
        ));
        let make = crate::pipeline::builder::build(&format!(
            "videotestsrc num-buffers=75 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 \
             ! x264enc key-int-max=25 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ))
        .unwrap();
        make.set_state(gstreamer::State::Playing).unwrap();
        make.bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(30),
                &[gstreamer::MessageType::Eos],
            )
            .unwrap();
        make.set_state(gstreamer::State::Null).unwrap();

        let hubs = crate::streams::StreamHubs::default();
        let camera = CameraConfig::new("Cam", path.to_str().unwrap());
        let subscription = hubs
            .subscribe(
                &camera,
                crate::streams::SubscriberPolicy::desktop_view(),
                None,
            )
            .unwrap();
        let pipeline = crate::pipeline::builder::build(&super::shared_pipeline_str(
            HwAccelKind::Software,
            "appsink",
        ))
        .unwrap();
        let sink = pipeline
            .by_name("viewer_sink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        let input = pipeline
            .by_name(super::SHARED_INPUT)
            .unwrap()
            .downcast::<gstreamer_app::AppSrc>()
            .unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let pump = super::HubPump::start(subscription, input, &camera.id).unwrap();
        let mut decoded = 0;
        while decoded < 25 {
            let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::from_seconds(5)) else {
                break;
            };
            let caps = sample
                .caps()
                .unwrap()
                .structure(0)
                .unwrap()
                .name()
                .to_string();
            assert_eq!(caps, "video/x-raw", "decoded pictures reach the sink");
            decoded += 1;
        }
        drop(pump);
        pipeline.set_state(gstreamer::State::Null).unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!(decoded, 25);
        assert_eq!(
            hubs.summary().len(),
            1,
            "one connection while the session ran"
        );
    }

    #[test]
    fn pipeline_str_ends_in_named_viewer_sink() {
        let cam = CameraConfig::new("Cam", "rtsp://example/live");
        let s = build_pipeline_str(&cam, HwAccelKind::Software, "glimagesink");
        assert!(
            s.contains("glimagesink name=viewer_sink"),
            "missing named sink: {s}"
        );
    }

    #[test]
    fn pipeline_str_uses_configured_credentials() {
        let mut cam = CameraConfig::new("Cam", "rtsp://camera.example/live");
        cam.username = Some("alice".to_owned());
        // base64("secret") = "c2VjcmV0"
        cam.password_b64 = Some("c2VjcmV0".to_owned());
        let s = build_pipeline_str(&cam, HwAccelKind::Software, "glimagesink");
        assert!(
            s.contains("alice:secret@camera.example"),
            "credentials must be injected into the pipeline: {s}"
        );
    }
}
