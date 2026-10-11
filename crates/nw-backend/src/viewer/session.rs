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
    ) -> Result<Self, ViewerError> {
        let pipeline_str = build_pipeline_str(camera, hwaccel, sink_factory);
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

        Ok(Self {
            id,
            camera_id: camera.id.clone(),
            pipeline,
            videosink,
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
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
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
