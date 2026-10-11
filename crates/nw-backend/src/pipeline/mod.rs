//! GStreamer pipeline management.
//!
//! Each camera gets a `PipelineSupervisor` running in its own tokio task.
//! The supervisor owns the GStreamer pipeline and handles the full lifecycle:
//! Connecting → Streaming → Error → BackoffWait → Reconnecting → Stopped

use anyhow::Context as _;
use base64::Engine as _;
use gstreamer::prelude::*;

use self::hwaccel::HwAccelKind;

pub mod audio;
pub mod builder;
pub mod encoded;
pub mod hwaccel;
pub mod ingest;
mod preview_format;
mod preview_rate;
pub mod reachability;
pub mod supervisor;

pub struct ProbeResult {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// `H264`, `H265`, `MJPEG`, … as the stream is sent.
    pub codec: Option<String>,
    /// A decoded picture, only for an explicitly displayed desktop preview.
    pub thumbnail_b64: Option<String>,
}

/// Connect to `url` and read its codec, size and frame rate from the
/// parser, without decoding a picture: what every connection test does
/// except an explicitly displayed desktop preview ([`probe_url`]).
pub async fn probe_encoded(url: &str) -> anyhow::Result<ProbeResult> {
    let url = url.to_owned();
    tokio::task::spawn_blocking(move || probe_encoded_blocking(&url))
        .await
        .context("Probe task join failed")?
}

pub fn encoded_probe_pipeline_str(url: &str) -> String {
    let location = builder::gst_quote(url);
    let head = if url.starts_with("rtsp://") || url.starts_with("rtsps://") {
        format!("rtspsrc location=\"{location}\" latency=100 ! application/x-rtp,media=video ! parsebin")
    } else if url.starts_with("rtmp://") {
        format!("rtmpsrc location=\"{location}\" ! flvdemux ! h264parse")
    } else if url.starts_with("http://") || url.starts_with("https://") {
        format!("souphttpsrc location=\"{location}\" is-live=true ! multipartdemux ! jpegparse")
    } else if url.starts_with("/dev/video") {
        // Raw device frames: nothing to decode, only the negotiated format.
        format!("v4l2src device=\"{location}\"")
    } else {
        let path = url.strip_prefix("file://").unwrap_or(url);
        format!(
            "filesrc location=\"{}\" ! parsebin",
            builder::gst_quote(path)
        )
    };
    format!("{head} ! appsink name=probe_sink max-buffers=1 drop=true sync=false")
}

fn probe_encoded_blocking(url: &str) -> anyhow::Result<ProbeResult> {
    let pipeline = builder::build(&encoded_probe_pipeline_str(url))
        .map_err(|e| anyhow::anyhow!("{}", sanitize_credentials(&e.to_string())))?;
    let appsink = pipeline
        .by_name("probe_sink")
        .and_then(|el| el.downcast::<gstreamer_app::AppSink>().ok())
        .ok_or_else(|| anyhow::anyhow!("Probe appsink not found"))?;
    pipeline
        .set_state(gstreamer::State::Playing)
        .map_err(|_| anyhow::anyhow!("Failed to start the connection test"))?;
    let result = (|| {
        let sample = appsink
            .try_pull_sample(gstreamer::ClockTime::from_seconds(8))
            .ok_or_else(|| anyhow::anyhow!("Timed out waiting for video"))?;
        let caps = sample
            .caps()
            .ok_or_else(|| anyhow::anyhow!("Missing caps"))?;
        let s = caps
            .structure(0)
            .ok_or_else(|| anyhow::anyhow!("Invalid caps"))?;
        let (width, height) = supervisor::resolution_from_structure(s).unwrap_or((0, 0));
        let fps = s
            .get::<gstreamer::Fraction>("framerate")
            .ok()
            .filter(|f| f.denom() > 0 && f.numer() > 0)
            .map(|f| f.numer() as f64 / f.denom() as f64)
            .unwrap_or(0.0);
        Ok(ProbeResult {
            width,
            height,
            fps,
            codec: supervisor::codec_from_structure(s),
            thumbnail_b64: None,
        })
    })();
    let _ = pipeline.set_state(gstreamer::State::Null);
    result.map_err(|e: anyhow::Error| anyhow::anyhow!("{}", sanitize_credentials(&e.to_string())))
}

/// Replace every URL in `input` with `[scheme source]`.
///
/// The entire URL — scheme, authority (including any embedded credentials),
/// path, query, and fragment — is replaced. No host, path segment, credential,
/// or query parameter ever reaches a log record or IPC error message.
pub fn sanitize_credentials(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(rel_scheme_end) = input[cursor..].find("://") {
        let scheme_end = cursor + rel_scheme_end;
        let mut scheme_start = scheme_end;
        while scheme_start > cursor {
            let ch = input.as_bytes()[scheme_start - 1] as char;
            if ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.') {
                scheme_start -= 1;
            } else {
                break;
            }
        }

        let scheme = &input[scheme_start..scheme_end];
        let url_body_start = scheme_end + 3;
        // URL ends at any whitespace or common delimiters that cannot be part of a URL.
        let url_end = input[url_body_start..]
            .find([
                ' ', '\t', '\n', '\r', '"', '\'', '>', '<', '`', '{', '}', '|', '\\',
            ])
            .map(|rel| url_body_start + rel)
            .unwrap_or(input.len());

        out.push_str(&input[cursor..scheme_start]);
        out.push('[');
        out.push_str(scheme);
        out.push_str(" source]");
        cursor = url_end;
    }
    out.push_str(&input[cursor..]);
    out
}

pub async fn probe_url(url: &str, _hwaccel: HwAccelKind) -> anyhow::Result<ProbeResult> {
    let url_owned = url.to_owned();
    tokio::task::spawn_blocking(move || probe_url_blocking(&url_owned))
        .await
        .context("Probe task join failed")?
}

/// Build the pipeline string used to test-connect `url`, routing each source
/// through the same source element `builder::select_pipeline` would use for a
/// live pipeline (`rtmpsrc` for `rtmp://`, `v4l2src` for `/dev/video*`), so a
/// scheme `uridecodebin` cannot resolve is never sent through file URI
/// handling. RTSP/HTTP/file inputs keep the existing `uridecodebin` behavior.
fn probe_pipeline_str(url: &str) -> String {
    if url.starts_with("rtmp://") {
        return format!(
            "rtmpsrc location=\"{url}\" ! \
             flvdemux ! decodebin ! \
             videoconvert ! video/x-raw,format=RGB ! \
             appsink name=probe_sink max-buffers=1 drop=true sync=false"
        );
    }
    if url.starts_with("/dev/video") {
        return format!(
            "v4l2src device=\"{url}\" ! \
             videoconvert ! video/x-raw,format=RGB ! \
             appsink name=probe_sink max-buffers=1 drop=true sync=false"
        );
    }

    let uri = if url.starts_with("rtsp://")
        || url.starts_with("rtsps://")
        || url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("file://")
    {
        url.to_owned()
    } else {
        format!("file://{url}")
    };

    format!(
        "uridecodebin uri=\"{uri}\" ! \
         videoconvert ! video/x-raw,format=RGB ! \
         appsink name=probe_sink max-buffers=1 drop=true sync=false"
    )
}

fn probe_url_blocking(url: &str) -> anyhow::Result<ProbeResult> {
    let pipeline_str = probe_pipeline_str(url);
    let pipeline = builder::build(&pipeline_str)
        .map_err(|e| anyhow::anyhow!("{}", sanitize_credentials(&e.to_string())))?;

    let appsink = pipeline
        .by_name("probe_sink")
        .and_then(|el| el.downcast::<gstreamer_app::AppSink>().ok())
        .ok_or_else(|| anyhow::anyhow!("Probe appsink not found"))?;

    pipeline
        .set_state(gstreamer::State::Playing)
        .map_err(|_| anyhow::anyhow!("Failed to set probe pipeline to PLAYING"))?;

    let result: anyhow::Result<ProbeResult> = (|| {
        let sample = appsink
            .try_pull_sample(gstreamer::ClockTime::from_seconds(5))
            .ok_or_else(|| anyhow::anyhow!("Timed out waiting for first frame"))?;

        let caps = sample
            .caps()
            .ok_or_else(|| anyhow::anyhow!("Missing sample caps"))?;
        let s = caps
            .structure(0)
            .ok_or_else(|| anyhow::anyhow!("Invalid sample caps"))?;

        let width = s
            .get::<i32>("width")
            .map_err(|_| anyhow::anyhow!("Missing width in caps"))? as u32;
        let height = s
            .get::<i32>("height")
            .map_err(|_| anyhow::anyhow!("Missing height in caps"))? as u32;

        let fps = s
            .get::<gstreamer::Fraction>("framerate")
            .ok()
            .and_then(|f| {
                if f.denom() > 0 {
                    Some(f.numer() as f64 / f.denom() as f64)
                } else {
                    None
                }
            })
            .unwrap_or(0.0);

        let buffer = sample
            .buffer()
            .ok_or_else(|| anyhow::anyhow!("Missing sample buffer"))?;
        let map = buffer
            .map_readable()
            .map_err(|_| anyhow::anyhow!("Failed to map sample buffer"))?;
        let bytes = map.as_slice().to_vec();

        let img = image::RgbImage::from_raw(width, height, bytes)
            .ok_or_else(|| anyhow::anyhow!("Unexpected frame buffer size"))?;
        let mut jpeg = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 75);
        enc.encode_image(&img)
            .map_err(|e| anyhow::anyhow!("JPEG encode failed: {e}"))?;

        Ok(ProbeResult {
            width,
            height,
            fps,
            codec: None,
            thumbnail_b64: Some(base64::engine::general_purpose::STANDARD.encode(jpeg)),
        })
    })();

    let _ = pipeline.set_state(gstreamer::State::Null);
    result.map_err(|e| anyhow::anyhow!("{}", sanitize_credentials(&e.to_string())))
}

#[cfg(test)]
mod tests {
    use super::{
        encoded_probe_pipeline_str, probe_encoded, probe_pipeline_str, sanitize_credentials,
    };

    #[test]
    fn probe_pipeline_str_routes_rtmp_through_rtmpsrc() {
        let pipeline = probe_pipeline_str("rtmp://example.test/live");
        assert!(
            pipeline.contains("rtmpsrc location=\"rtmp://example.test/live\""),
            "rtmp:// probe must use rtmpsrc, not uridecodebin: {pipeline}"
        );
        assert!(
            !pipeline.contains("uridecodebin"),
            "unexpected uridecodebin in rtmp probe pipeline: {pipeline}"
        );
        assert!(
            !pipeline.contains("file://rtmp://"),
            "rtmp url must not be wrapped as a file:// uri: {pipeline}"
        );
    }

    #[test]
    fn probe_pipeline_str_routes_v4l2_device_through_v4l2src() {
        let pipeline = probe_pipeline_str("/dev/video0");
        assert!(
            pipeline.contains("v4l2src device=\"/dev/video0\""),
            "device path probe must use v4l2src, not uridecodebin: {pipeline}"
        );
        assert!(
            !pipeline.contains("uridecodebin"),
            "unexpected uridecodebin in v4l2 probe pipeline: {pipeline}"
        );
        assert!(
            !pipeline.contains("file:///dev/video0"),
            "device path must not be wrapped as a file:// uri: {pipeline}"
        );
    }

    #[test]
    fn probe_pipeline_str_preserves_existing_rtsp_http_file_behavior() {
        assert!(probe_pipeline_str("rtsp://example.test/stream")
            .contains("uridecodebin uri=\"rtsp://example.test/stream\""));
        assert!(probe_pipeline_str("http://example.test/mjpeg")
            .contains("uridecodebin uri=\"http://example.test/mjpeg\""));
        assert!(probe_pipeline_str("file:///tmp/video.mp4")
            .contains("uridecodebin uri=\"file:///tmp/video.mp4\""));
        assert!(probe_pipeline_str("/tmp/video.mp4")
            .contains("uridecodebin uri=\"file:///tmp/video.mp4\""));
    }

    #[test]
    fn sanitize_credentials_removes_embedded_url_credentials() {
        let raw = "failed for rtsp://alice:super-secret@example.test/live and http://bob:pw@host/x";
        let sanitized = sanitize_credentials(raw);

        assert!(
            !sanitized.contains("alice"),
            "credential leaked: {sanitized}"
        );
        assert!(
            !sanitized.contains("super-secret"),
            "credential leaked: {sanitized}"
        );
        assert!(!sanitized.contains("bob"), "credential leaked: {sanitized}");
        assert!(
            !sanitized.contains("example.test"),
            "host leaked: {sanitized}"
        );
        assert!(!sanitized.contains("host/x"), "path leaked: {sanitized}");
        assert!(
            sanitized.contains("[rtsp source]"),
            "scheme classification missing: {sanitized}"
        );
        assert!(
            sanitized.contains("[http source]"),
            "scheme classification missing: {sanitized}"
        );
    }

    #[test]
    fn sanitize_credentials_removes_complete_sensitive_source_url() {
        let sensitive = "rtsp://user:pass@camera.example/private?token=value";
        let sanitized = sanitize_credentials(&format!("parse failed for {sensitive}"));

        for forbidden in [
            sensitive,
            "user",
            "pass",
            "camera.example",
            "private",
            "token=value",
        ] {
            assert!(
                !sanitized.contains(forbidden),
                "sanitized output leaked {forbidden:?}: {sanitized}"
            );
        }
        assert!(sanitized.contains("[rtsp source]"));
    }

    #[test]
    fn connection_tests_read_the_format_without_a_decoder() {
        for url in [
            "rtsp://cam/main",
            "rtmp://cam/live",
            "http://cam/mjpeg",
            "/dev/video0",
            "/tmp/clip.mp4",
        ] {
            let pipeline = encoded_probe_pipeline_str(url);
            for codec in [
                "decodebin",
                "uridecodebin",
                "avdec",
                "jpegdec",
                "videoconvert",
            ] {
                assert!(!pipeline.contains(codec), "{codec} in {pipeline}");
            }
        }
    }

    #[tokio::test]
    async fn an_encoded_probe_reports_codec_size_and_rate_of_a_file() {
        use gstreamer::prelude::*;
        gstreamer::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("nw-probe-{}.mkv", uuid::Uuid::new_v4().simple()));
        let writer = super::builder::build(&format!(
            "videotestsrc num-buffers=30 ! video/x-raw,width=320,height=240,framerate=15/1 ! \
             x264enc ! h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ))
        .unwrap();
        writer.set_state(gstreamer::State::Playing).unwrap();
        writer
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(20),
                &[gstreamer::MessageType::Eos],
            )
            .expect("clip written");
        writer.set_state(gstreamer::State::Null).unwrap();

        let result = probe_encoded(path.to_str().unwrap()).await.unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!((result.width, result.height), (320, 240));
        assert_eq!(result.codec.as_deref(), Some("H264"));
        assert_eq!(result.fps, 15.0);
        assert!(
            result.thumbnail_b64.is_none(),
            "no picture without a decoder"
        );
    }

    #[tokio::test]
    #[ignore = "requires an RTSP H.264 stream at NW_RTSP_TEST_URL"]
    async fn an_encoded_probe_of_a_real_rtsp_camera() {
        gstreamer::init().unwrap();
        let url = std::env::var("NW_RTSP_TEST_URL").expect("NW_RTSP_TEST_URL");
        let result = probe_encoded(&url).await.unwrap();
        assert_eq!(result.codec.as_deref(), Some("H264"));
        assert!(result.width > 0 && result.height > 0);
        assert!(result.thumbnail_b64.is_none());
    }
}
