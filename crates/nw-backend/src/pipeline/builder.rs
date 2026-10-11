//! GStreamer pipeline construction helpers.

use anyhow::Context as _;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use gstreamer::prelude::*;
use nw_core::camera::{CameraConfig, Transport};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::hwaccel::HwAccelKind;

/// Inject `user[:pass]@` into an RTSP or HTTP URL from separate credential fields.
/// Skips injection if the URL already contains `@` after the scheme, or if no
/// username is configured.  `password_b64` is Base64-decoded before embedding.
pub fn url_with_credentials_str(
    url: &str,
    username: Option<&str>,
    password_b64: Option<&str>,
) -> String {
    let user = match username {
        Some(u) if !u.is_empty() => u,
        _ => return url.to_owned(),
    };
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    if rest.contains('@') {
        return url.to_owned(); // credentials already embedded
    }
    let pass = password_b64
        .and_then(|b| STANDARD.decode(b).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    let enc_u = percent_encode(user);
    if pass.is_empty() {
        format!("{scheme}://{enc_u}@{rest}")
    } else {
        let enc_p = percent_encode(&pass);
        format!("{scheme}://{enc_u}:{enc_p}@{rest}")
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build an RTSP receive + decode pipeline string.
///
/// Output: appsink named "frame_sink" emitting RGB24 frames (3 bytes/pixel).
/// On NVIDIA, `nvvidconv` performs YUV→RGB on the GPU if available.
pub fn rtsp_pipeline_str(config: &CameraConfig, hwaccel: HwAccelKind) -> String {
    let protocols = match config.transport {
        Transport::Tcp => "tcp",
        Transport::Udp => "udp",
    };
    let effective = url_with_credentials_str(
        &config.url,
        config.username.as_deref(),
        config.password_b64.as_deref(),
    );
    let location = gst_quote(&effective);
    let converter = super::hwaccel::color_converter(hwaccel);
    format!(
        "rtspsrc location=\"{url}\" latency=100 protocols={protocols} \
           do-rtsp-keep-alive=true retry=5 ! \
         application/x-rtp,media=video ! decodebin ! \
         {converter} ! video/x-raw,format=RGB ! \
         appsink name=frame_sink max-buffers=2 drop=true sync=false emit-signals=true",
        url = location,
    )
}

pub fn mjpeg_pipeline_str(config: &CameraConfig, _hwaccel: HwAccelKind) -> String {
    let effective = url_with_credentials_str(
        &config.url,
        config.username.as_deref(),
        config.password_b64.as_deref(),
    );
    let location = gst_quote(&effective);
    format!(
        "souphttpsrc location=\"{url}\" is-live=true ! \
         multipartdemux ! jpegdec ! \
         videoconvert ! video/x-raw,format=RGB ! \
         appsink name=frame_sink max-buffers=2 drop=true sync=false emit-signals=true",
        url = location,
    )
}

pub fn rtmp_pipeline_str(config: &CameraConfig, hwaccel: HwAccelKind) -> String {
    let decoder = super::hwaccel::h264_decoder(hwaccel);
    let location = gst_quote(&config.url);
    format!(
        "rtmpsrc location=\"{url}\" ! \
         flvdemux ! h264parse ! \
         {decoder} ! \
         videoconvert ! video/x-raw,format=RGB ! \
         appsink name=frame_sink max-buffers=2 drop=true sync=false emit-signals=true",
        url = location,
    )
}

pub fn v4l2_pipeline_str(config: &CameraConfig) -> String {
    let device = gst_quote(&config.url);
    format!(
        "v4l2src device=\"{device}\" ! \
         videoconvert ! video/x-raw,format=RGB ! \
         appsink name=frame_sink max-buffers=2 drop=true sync=false emit-signals=true",
        device = device,
    )
}

pub fn file_pipeline_str(config: &CameraConfig) -> String {
    let path = gst_quote(&config.url);
    format!(
        "filesrc location=\"{path}\" ! decodebin ! \
         videoconvert ! video/x-raw,format=RGB ! \
         appsink name=frame_sink max-buffers=2 drop=true sync=false emit-signals=true",
        path = path,
    )
}

pub fn gst_quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// True if `url` is a local filesystem path that `select_pipeline` would
/// route to `file_pipeline_str` — i.e. not a recognized network/device
/// scheme, and pointing at a file that exists. Relative paths qualify, since
/// `select_pipeline` also resolves paths via `Path::exists`, not by
/// requiring a leading `/`.
///
/// The EOS handler in `supervisor.rs` uses this same predicate to decide
/// whether a stream that reached end-of-stream is a loopable local file
/// (seek back to zero) rather than a live source that should error out.
pub fn is_local_file_url(url: &str) -> bool {
    !url.starts_with("rtsp://")
        && !url.starts_with("rtsps://")
        && !url.starts_with("http://")
        && !url.starts_with("https://")
        && !url.starts_with("rtmp://")
        && !url.starts_with("/dev/video")
        && Path::new(url).exists()
}

pub fn select_pipeline(config: &CameraConfig, hwaccel: HwAccelKind) -> String {
    let url = &config.url;
    if url.starts_with("rtsp://") || url.starts_with("rtsps://") {
        rtsp_pipeline_str(config, hwaccel)
    } else if url.starts_with("http://") || url.starts_with("https://") {
        mjpeg_pipeline_str(config, hwaccel)
    } else if url.starts_with("rtmp://") {
        rtmp_pipeline_str(config, hwaccel)
    } else if url.starts_with("/dev/video") {
        v4l2_pipeline_str(config)
    } else if is_local_file_url(url) {
        file_pipeline_str(config)
    } else {
        rtsp_pipeline_str(config, hwaccel)
    }
}

static RECORDING_RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn recording_location(storage_path: &str, cam_id: &str) -> String {
    let epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = RECORDING_RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}/{cam_id}_{epoch_ms}_{sequence}_%05d.mkv",
        storage_path.trim_end_matches('/')
    )
}

/// The recording's segment writer; its `audio_0` pad takes a camera's audio.
pub(crate) const RECORDING_MUX: &str = "recording_mux";
/// The RTSP source of a recording, so its audio track can be linked too.
const RECORDING_SOURCE: &str = "recording_src";

/// The writer end of a recording: `ingest_tap` (where the recorder counts
/// access units and reads codec and size), then disk I/O on its own thread.
/// The queue holds complete compressed frames, bounded in bytes as well as
/// count, and `splitmuxsink` cuts segments at keyframes. `paced` holds a
/// file source to its own timestamps; a camera arrives at its own pace.
pub(crate) fn recording_tail(location: &str, segment_minutes: u32, paced: bool) -> String {
    let ns = u64::from(segment_minutes) * 60 * 1_000_000_000;
    let location = gst_quote(location);
    let sync = if paced { " sync=true" } else { "" };
    format!(
        "identity name=ingest_tap silent=true{sync} \
         ! queue max-size-buffers=300 max-size-time=0 max-size-bytes=524288 \
         ! splitmuxsink name={RECORDING_MUX} location=\"{location}\" max-size-time={ns} \
              async-finalize=true muxer-factory=matroskamux"
    )
}

/// Whether a camera at `url` can be recorded without transcoding.
pub fn recording_supported(url: &str) -> bool {
    url.starts_with("rtsp://")
        || url.starts_with("rtsps://")
        || url.starts_with("rtmp://")
        || url.starts_with("http://")
        || url.starts_with("https://")
        || is_local_file_url(url)
}

/// Record `source` exactly as the camera sends it: RTP is depayloaded and
/// parsed, never decoded or re-encoded. H.264/H.265 over RTSP or RTMP and in
/// local files are kept as they are; MJPEG over HTTP is kept as JPEG. `None`
/// for a source with nothing compressed to keep (a V4L2 device), which
/// cannot be recorded.
///
/// `with_audio` (RTSP only): also keep the camera's audio track, as it
/// arrives, in the same segments. Only ask for it when the camera describes
/// an audio track Matroska stores as it is (`audio::recordable`): the muxer
/// waits for every track it was given, so a missing one would stall it.
pub fn encoded_recording_pipeline(
    source: &CameraConfig,
    cam_id: &str,
    storage_path: &str,
    segment_minutes: u32,
    with_audio: bool,
) -> Option<String> {
    let location = recording_location(storage_path, cam_id);
    let url = &source.url;
    let with_credentials = || {
        gst_quote(&url_with_credentials_str(
            url,
            source.username.as_deref(),
            source.password_b64.as_deref(),
        ))
    };
    let is_rtsp = url.starts_with("rtsp://") || url.starts_with("rtsps://");
    let head = if is_rtsp {
        let protocols = match source.transport {
            Transport::Tcp => "tcp",
            Transport::Udp => "udp",
        };
        format!(
            "rtspsrc name={RECORDING_SOURCE} location=\"{}\" latency=100 protocols={protocols} \
             do-rtsp-keep-alive=true retry=5 ! application/x-rtp,media=video ! parsebin",
            with_credentials()
        )
    } else if url.starts_with("rtmp://") {
        format!(
            "rtmpsrc location=\"{}\" ! flvdemux ! h264parse config-interval=-1",
            gst_quote(url)
        )
    } else if url.starts_with("http://") || url.starts_with("https://") {
        format!(
            "souphttpsrc location=\"{}\" is-live=true ! multipartdemux ! jpegparse",
            with_credentials()
        )
    } else if is_local_file_url(url) {
        format!("filesrc location=\"{}\" ! parsebin", gst_quote(url))
    } else {
        return None;
    };
    let paced = is_local_file_url(url);
    // The audio track from the same connection: depayloaded and parsed,
    // never decoded, into the segment writer next to the video.
    let audio = if with_audio && is_rtsp {
        format!(
            " {RECORDING_SOURCE}. ! application/x-rtp,media=audio ! parsebin \
             ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=1048576 \
             ! {RECORDING_MUX}.audio_0"
        )
    } else {
        String::new()
    };
    Some(format!(
        "{head} ! {}{audio}",
        recording_tail(&location, segment_minutes, paced)
    ))
}

/// Parse the pipeline string and return a `gstreamer::Pipeline`.
pub fn build(pipeline_str: &str) -> anyhow::Result<gstreamer::Pipeline> {
    gstreamer::parse::launch(pipeline_str)
        .context("GStreamer failed to parse pipeline")?
        .downcast::<gstreamer::Pipeline>()
        .map_err(|_| anyhow::anyhow!("parse::launch did not return a Pipeline element"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use gstreamer::prelude::*;
    use nw_core::camera::CameraConfig;

    use super::{
        encoded_recording_pipeline, is_local_file_url, recording_location, select_pipeline,
        HwAccelKind, RECORDING_MUX,
    };

    #[test]
    fn select_pipeline_by_scheme() {
        let mut cam = CameraConfig::new("Cam", "rtsp://example/stream");
        let rtsp = select_pipeline(&cam, HwAccelKind::Software);
        assert!(rtsp.contains("rtspsrc"));
        assert!(rtsp.contains("application/x-rtp,media=video ! decodebin"));
        assert!(!rtsp.contains("h264parse"));

        cam.url = "http://example/mjpeg".to_owned();
        assert!(select_pipeline(&cam, HwAccelKind::Software).contains("souphttpsrc location=\""));

        cam.url = "rtmp://example/live".to_owned();
        assert!(select_pipeline(&cam, HwAccelKind::Software).contains("rtmpsrc location=\""));

        cam.url = "/dev/video0".to_owned();
        assert!(select_pipeline(&cam, HwAccelKind::Software).contains("v4l2src device=\""));
    }

    #[test]
    fn select_pipeline_for_existing_file() {
        let path = std::env::temp_dir().join("nw_builder_test.mp4");
        fs::write(&path, b"test").expect("temp test file");
        let cam = CameraConfig::new("Cam", path.display().to_string());
        let selected = select_pipeline(&cam, HwAccelKind::Software);
        assert!(selected.contains("filesrc location="));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn is_local_file_url_treats_relative_existing_file_as_loopable() {
        // Regression: the EOS handler in supervisor.rs must agree with
        // select_pipeline on which URLs are local-file sources so a camera
        // configured with a relative path still loops on EOS instead of
        // erroring out. select_pipeline resolves paths via `Path::exists`,
        // which does not require a leading '/'.
        let filename = "nw_eos_relative_test_fixture.mp4";
        let abs_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(filename);
        fs::write(&abs_path, b"test").expect("temp test file");

        let cam = CameraConfig::new("Cam", filename);
        let selected = select_pipeline(&cam, HwAccelKind::Software);
        assert!(
            selected.contains("filesrc location="),
            "a relative path to an existing file must select the file pipeline"
        );
        assert!(
            is_local_file_url(filename),
            "a relative path accepted by select_pipeline's file branch must also be reported as loopable on EOS"
        );

        let _ = fs::remove_file(&abs_path);
    }

    #[test]
    fn rtsp_pipeline_quotes_special_chars() {
        let cam = CameraConfig::new("Cam", "rtsp://example/live?foo=1&bar=2");
        let selected = select_pipeline(&cam, HwAccelKind::Software);
        assert!(selected.contains("location=\"rtsp://example/live?foo=1&bar=2\""));
    }

    /// Elements that decode or encode video; none may appear in a recording.
    const CODECS: [&str; 6] = [
        "decodebin",
        "avdec",
        "x264enc",
        "jpegdec",
        "videoconvert",
        "x265enc",
    ];

    fn assert_kept_compressed(pipeline: &str) {
        for codec in CODECS {
            assert!(!pipeline.contains(codec), "{codec} in {pipeline}");
        }
        assert!(pipeline.contains("async-finalize=true"));
        assert!(pipeline.contains("muxer-factory=matroskamux"));
        assert!(pipeline.contains("identity name=ingest_tap"));
    }

    #[test]
    fn rtsp_recording_depayloads_and_parses_only() {
        let mut cam = CameraConfig::new("Cam", "rtsp://example/main");
        cam.username = Some("admin".into());
        cam.password_b64 = Some("cGFzcw==".into());
        let rec = encoded_recording_pipeline(&cam, "cam-1", "/tmp", 1, false).unwrap();
        assert!(rec
            .starts_with("rtspsrc name=recording_src location=\"rtsp://admin:pass@example/main\""));
        assert!(rec.contains("application/x-rtp,media=video ! parsebin ! identity name=ingest_tap"));
        assert!(!rec.contains("media=audio"), "no audio track asked for");
        assert_kept_compressed(&rec);
    }

    #[test]
    fn rtsp_recording_keeps_the_cameras_audio_track_when_asked() {
        let cam = CameraConfig::new("Cam", "rtsp://example/main");
        let rec = encoded_recording_pipeline(&cam, "cam-1", "/tmp", 1, true).unwrap();
        assert!(rec.contains(
            "recording_src. ! application/x-rtp,media=audio ! parsebin \
             ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=1048576 \
             ! recording_mux.audio_0"
        ));
        assert!(rec.contains("splitmuxsink name=recording_mux"));
        assert_kept_compressed(&rec);
        // Audio comes on its own track only over RTSP.
        let rtmp = encoded_recording_pipeline(
            &CameraConfig::new("Cam", "rtmp://example/live"),
            "c",
            "/tmp",
            1,
            true,
        )
        .unwrap();
        assert!(!rtmp.contains("media=audio"));
    }

    #[test]
    fn a_recording_keeps_the_cameras_audio_without_a_codec() {
        gstreamer::init().unwrap();
        let dir = std::env::temp_dir().join(format!("nw-rec-a-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        let location = recording_location(dir.to_str().unwrap(), "cam-1");
        // A camera stand-in sending H.264 video and G.711 audio over RTP
        // (only `camera_*` elements encode), then the recorder's branches.
        let description = format!(
            "videotestsrc name=camera_source num-buffers=60 is-live=true \
               ! video/x-raw,width=320,height=240,framerate=30/1 \
               ! x264enc name=camera_encoder key-int-max=15 tune=zerolatency \
               ! rtph264pay name=camera_pay ! application/x-rtp,media=video ! parsebin ! {tail} \
             audiotestsrc name=camera_audio num-buffers=100 is-live=true \
               ! audio/x-raw,rate=8000,channels=1 ! alawenc name=camera_audio_encoder \
               ! rtppcmapay name=camera_audio_pay ! application/x-rtp,media=audio ! parsebin \
               ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=1048576 \
               ! {RECORDING_MUX}.audio_0",
            tail = super::recording_tail(&location, 1, false)
        );
        let pipeline = super::build(&description).unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        loop {
            let message = bus
                .timed_pop(gstreamer::ClockTime::from_seconds(20))
                .expect("finished");
            match message.view() {
                gstreamer::MessageView::Eos(_) => break,
                gstreamer::MessageView::Error(error) => panic!("{}", error.error()),
                _ => {}
            }
        }
        let codecs: Vec<String> = pipeline
            .iterate_recurse()
            .into_iter()
            .filter_map(Result::ok)
            .filter(|element| !element.name().starts_with("camera_"))
            .filter_map(|element| {
                let factory = element.factory()?;
                let klass = factory.metadata("klass").unwrap_or_default().to_string();
                (klass.contains("Decoder") || klass.contains("Encoder"))
                    .then(|| factory.name().to_string())
            })
            .collect();
        pipeline.set_state(gstreamer::State::Null).unwrap();
        assert!(
            codecs.is_empty(),
            "nothing decoded or re-encoded: {codecs:?}"
        );

        let segment = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .next()
            .expect("a segment")
            .path();
        // Reading the segment back reaches PAUSED only if both tracks are
        // there: each demuxer pad must preroll its sink.
        let check = super::build(&format!(
            "filesrc location=\"{}\" ! matroskademux name=demux \
             demux.video_0 ! queue ! fakesink \
             demux.audio_0 ! queue ! audio/x-alaw ! fakesink",
            segment.display()
        ))
        .unwrap();
        let _ = check.set_state(gstreamer::State::Paused);
        let (result, state, _) = check.state(gstreamer::ClockTime::from_seconds(5));
        check.set_state(gstreamer::State::Null).unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert!(
            result.is_ok(),
            "segment readable with its tracks: {result:?}"
        );
        assert_eq!(
            state,
            gstreamer::State::Paused,
            "an H.264 and an A-law track"
        );
    }

    #[test]
    fn rtmp_and_mjpeg_recordings_keep_their_bitstreams() {
        let rtmp = encoded_recording_pipeline(
            &CameraConfig::new("Cam", "rtmp://example/live"),
            "c",
            "/tmp",
            1,
            false,
        )
        .unwrap();
        assert!(rtmp.contains("flvdemux ! h264parse config-interval=-1 ! identity"));
        assert_kept_compressed(&rtmp);
        let mjpeg = encoded_recording_pipeline(
            &CameraConfig::new("Cam", "http://example/mjpeg"),
            "c",
            "/tmp",
            1,
            false,
        )
        .unwrap();
        assert!(mjpeg.contains("multipartdemux ! jpegparse ! identity"));
        assert_kept_compressed(&mjpeg);
    }

    #[test]
    fn a_source_without_compressed_video_cannot_be_recorded() {
        assert!(encoded_recording_pipeline(
            &CameraConfig::new("Cam", "/dev/video0"),
            "c",
            "/tmp",
            1,
            false
        )
        .is_none());
    }

    #[test]
    fn a_recording_writes_the_cameras_own_stream_without_a_codec() {
        gstreamer::init().unwrap();
        let dir = std::env::temp_dir().join(format!("nw-rec-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        let location = recording_location(dir.to_str().unwrap(), "cam-1");
        // A camera stand-in (only `camera_*` elements encode), then the recorder.
        let description = format!(
            "videotestsrc name=camera_source num-buffers=60 ! video/x-raw,width=320,height=240 ! \
             x264enc name=camera_encoder key-int-max=15 ! rtph264pay name=camera_pay ! \
             application/x-rtp,media=video ! parsebin ! {}",
            super::recording_tail(&location, 1, false)
        );
        let pipeline = super::build(&description).unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        loop {
            let message = bus
                .timed_pop(gstreamer::ClockTime::from_seconds(20))
                .expect("finished");
            match message.view() {
                gstreamer::MessageView::Eos(_) => break,
                gstreamer::MessageView::Error(error) => panic!("{}", error.error()),
                _ => {}
            }
        }
        let codecs: Vec<String> = pipeline
            .iterate_recurse()
            .into_iter()
            .filter_map(Result::ok)
            .filter(|element| !element.name().starts_with("camera_"))
            .filter_map(|element| {
                let factory = element.factory()?;
                let klass = factory.metadata("klass").unwrap_or_default().to_string();
                (klass.contains("Decoder") || klass.contains("Encoder"))
                    .then(|| factory.name().to_string())
            })
            .collect();
        pipeline.set_state(gstreamer::State::Null).unwrap();
        assert!(codecs.is_empty(), "{codecs:?}");
        let segments: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(Result::ok).collect();
        assert_eq!(segments.len(), 1);
        assert!(segments[0].metadata().unwrap().len() > 1000);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recording_locations_are_unique_across_pipeline_restarts() {
        let first = recording_location("/tmp/recordings", "cam-1");
        let second = recording_location("/tmp/recordings", "cam-1");
        assert_ne!(first, second);
        assert!(first.starts_with("/tmp/recordings/cam-1_"));
        assert!(first.ends_with("_%05d.mkv"));
    }

    #[test]
    fn unsupported_dedicated_recording_source_log_does_not_include_source_url() {
        let source = include_str!("builder.rs");
        let forbidden = ["source_url = %", "source_cfg.url"].concat();
        assert!(!source.contains(&forbidden));
    }
}
