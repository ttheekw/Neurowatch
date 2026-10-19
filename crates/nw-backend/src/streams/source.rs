//! A hub's camera connection: receive, depayload and parse, never decode.
//!
//! The pipeline ends in one `appsink` per media type. Its callback wraps the
//! sample in a [`SharedUnit`] and hands it to the hub, which fans it out; it
//! returns at once (the appsink rule). Units are parsed into what Matroska
//! stores as it is (H.264 `avc`, H.265 `hvc1`/`hev1`, JPEG; AAC, G.711, Opus,
//! PCM), the recorder's format, so a recording is the camera's bitstream.
//!
//! One thread per hub runs [`run`]: connect, watch the bus and the video's
//! progress, and on failure tell every subscriber (a discontinuity), back
//! off (as the desktop view does) and connect again, while anyone subscribes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gstreamer::prelude::*;
use nw_core::camera::{CameraConfig, Transport};

use super::{lock, Hub, Media, Phase, SharedUnit};
use crate::pipeline::builder::{self, gst_quote, is_local_file_url, url_with_credentials_str};
use crate::pipeline::supervisor::{
    backoff_duration, backoff_slice, jittered, probe_source, NO_FRAME_TIMEOUT,
};

const VIDEO_SINK: &str = "hub_video";
const AUDIO_SINK: &str = "hub_audio";
const SOURCE: &str = "hub_src";
/// How often a running connection checks its bus, progress and subscribers.
const POLL: Duration = Duration::from_millis(100);

enum Outcome {
    Closed,
    Failed { reason: String, delivered: bool },
}

/// The pipeline for `camera`'s URL up to its appsinks, and whether a file
/// is paced to its timestamps. `None`: nothing compressed to share (V4L2).
pub(super) fn description(camera: &CameraConfig) -> Option<(String, bool)> {
    let url = &camera.url;
    let video = format!("appsink name={VIDEO_SINK}");
    let with_credentials = || {
        gst_quote(&url_with_credentials_str(
            url,
            camera.username.as_deref(),
            camera.password_b64.as_deref(),
        ))
    };
    if url.starts_with("rtsp://") || url.starts_with("rtsps://") {
        let protocols = match camera.transport {
            Transport::Tcp => "tcp",
            Transport::Udp => "udp",
        };
        // The audio branch takes the camera's audio track when it has one
        // the hub selects (`select-stream`); otherwise it stays idle.
        Some((
            format!(
                "rtspsrc name={SOURCE} location=\"{}\" latency=100 protocols={protocols} \
                 do-rtsp-keep-alive=true retry=5 \
                 {SOURCE}. ! application/x-rtp,media=video ! parsebin ! {video} \
                 {SOURCE}. ! application/x-rtp,media=audio ! parsebin ! appsink name={AUDIO_SINK}",
                with_credentials()
            ),
            false,
        ))
    } else if url.starts_with("rtmp://") {
        Some((
            format!(
                "rtmpsrc location=\"{}\" ! flvdemux ! h264parse config-interval=-1 ! {video}",
                gst_quote(url)
            ),
            false,
        ))
    } else if url.starts_with("http://") || url.starts_with("https://") {
        Some((
            format!(
                "souphttpsrc location=\"{}\" is-live=true ! multipartdemux ! jpegparse ! {video}",
                with_credentials()
            ),
            false,
        ))
    } else if is_local_file_url(url) {
        Some((
            format!(
                "filesrc location=\"{}\" ! parsebin ! {video}",
                gst_quote(url)
            ),
            true,
        ))
    } else {
        None
    }
}

/// What the segment writer (`matroskamux`) takes for `media`, so the parsers
/// output the stream in a form it stores as it is.
fn matroska_caps(media: Media) -> gstreamer::Caps {
    static VIDEO: OnceLock<gstreamer::Caps> = OnceLock::new();
    static AUDIO: OnceLock<gstreamer::Caps> = OnceLock::new();
    let (cell, template) = match media {
        Media::Video => (&VIDEO, "video_%u"),
        Media::Audio => (&AUDIO, "audio_%u"),
    };
    cell.get_or_init(|| {
        gstreamer::ElementFactory::find("matroskamux")
            .and_then(|factory| {
                factory
                    .static_pad_templates()
                    .iter()
                    .find(|t| t.name_template() == template)
                    .map(|t| t.caps())
            })
            .unwrap_or_else(gstreamer::Caps::new_any)
    })
    .clone()
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

/// What a hub sink takes and how it runs. The sound sink never holds up the
/// pipeline: a camera without sound never sends it anything, and a sink still
/// waiting for its first buffer would keep the whole connection from playing
/// (no video would arrive either).
fn configure_sink(sink: &gstreamer_app::AppSink, media: Media, paced: bool) {
    sink.set_caps(Some(&matroska_caps(media)));
    sink.set_property("sync", paced);
    if media == Media::Audio {
        sink.set_property("async", false);
    }
}

/// Feed `sink`'s samples to the hub as `media` units.
fn connect_sink(
    pipeline: &gstreamer::Pipeline,
    name: &str,
    media: Media,
    paced: bool,
    hub: &Arc<Hub>,
    video_units: &Arc<AtomicU64>,
) {
    let Some(sink) = pipeline
        .by_name(name)
        .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
    else {
        return;
    };
    configure_sink(&sink, media, paced);
    let (hub, units) = (Arc::clone(hub), Arc::clone(video_units));
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                let (Some(buffer), Some(caps)) = (sample.buffer_owned(), sample.caps_owned())
                else {
                    return Ok(gstreamer::FlowSuccess::Ok);
                };
                if media == Media::Video {
                    units.fetch_add(1, Ordering::Relaxed);
                }
                hub.publish(SharedUnit {
                    media,
                    buffer,
                    caps,
                    arrived_us: now_us(),
                });
                Ok(gstreamer::FlowSuccess::Ok)
            })
            .build(),
    );
}

/// The hub's thread: connect, stream, reconnect, until nobody subscribes.
pub(super) fn run(hub: Arc<Hub>) {
    let cam_id = hub.camera.id.clone();
    let mut failures = 0u32;
    loop {
        if hub.try_close() {
            break;
        }
        hub.set_state(Phase::Connecting, failures, None, false);
        match connect(&hub) {
            Outcome::Closed => break,
            Outcome::Failed { reason, delivered } => {
                let reason = crate::pipeline::sanitize_credentials(&reason);
                lock(&hub.core).disconnect(&reason);
                if delivered {
                    failures = 0;
                }
                let attempt = failures;
                failures = failures.saturating_add(1);
                tracing::debug!(cam_id = %cam_id, attempt = failures, reason = %reason, "Camera connection failed");
                hub.set_state(Phase::Retrying, failures, Some(reason), delivered);
                if !rest(&hub, jittered(backoff_duration(attempt), &cam_id, attempt)) {
                    break;
                }
            }
        }
    }
    tracing::debug!(cam_id = %cam_id, "Camera connection closed");
}

/// Wait out a backoff; `false` once the hub closed. Ends early when the
/// camera answered elsewhere (its desktop view got data).
fn rest(hub: &Arc<Hub>, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    let answers = hub.reachability.as_ref().map(|r| (r, r.mark()));
    while let Some(slice) = backoff_slice(until, Instant::now()) {
        if hub.try_close() {
            return false;
        }
        if answers.is_some_and(|(r, mark)| r.answered_since(mark)) {
            break;
        }
        std::thread::sleep(slice.min(POLL));
    }
    !hub.try_close()
}

fn connect(hub: &Arc<Hub>) -> Outcome {
    let failed = |reason: String| Outcome::Failed {
        reason,
        delivered: false,
    };
    if let Err(reason) = probe_source(&hub.camera.url) {
        return failed(reason);
    }
    let Some((description, paced)) = description(&hub.camera) else {
        return failed("this source has no compressed video to share".into());
    };
    let pipeline = match builder::build(&description) {
        Ok(pipeline) => pipeline,
        Err(error) => return failed(error.to_string()),
    };
    let video_units = Arc::new(AtomicU64::new(0));
    connect_sink(
        &pipeline,
        VIDEO_SINK,
        Media::Video,
        paced,
        hub,
        &video_units,
    );
    connect_sink(
        &pipeline,
        AUDIO_SINK,
        Media::Audio,
        paced,
        hub,
        &video_units,
    );
    match pipeline.by_name(SOURCE) {
        Some(rtsp) => {
            *lock(&hub.audio_label) = None;
            let hub = Arc::clone(hub);
            // Only the video and a sound track that can be kept as it is are
            // set up (no metadata or backchannel streams).
            rtsp.connect("select-stream", false, move |values| {
                let caps = values
                    .get(2)
                    .and_then(|value| value.get::<gstreamer::Caps>().ok());
                let Some(structure) = caps.as_ref().and_then(|caps| caps.structure(0)) else {
                    return Some(false.to_value());
                };
                if let Some(label) = crate::pipeline::audio::audio_from_rtp(structure) {
                    let mut known = lock(&hub.audio_label);
                    let first = !matches!(*known, Some(Some(_)));
                    if first {
                        *known = Some(Some(label.clone()));
                    }
                    return Some((first && crate::pipeline::audio::recordable(&label)).to_value());
                }
                let video = structure.get::<String>("media").ok().as_deref() == Some("video");
                Some(video.to_value())
            });
        }
        None => *lock(&hub.audio_label) = Some(None),
    }
    let outcome = stream(hub, &pipeline, &video_units);
    let _ = pipeline.set_state(gstreamer::State::Null);
    let _ = pipeline.state(Some(gstreamer::ClockTime::from_seconds(5)));
    outcome
}

fn stream(hub: &Arc<Hub>, pipeline: &gstreamer::Pipeline, video_units: &AtomicU64) -> Outcome {
    let failed = |reason: String, units: u64| Outcome::Failed {
        reason,
        delivered: units > 0,
    };
    if pipeline.set_state(gstreamer::State::Playing).is_err() {
        return failed("the camera connection would not start".into(), 0);
    }
    let Some(bus) = pipeline.bus() else {
        return failed("the camera connection has no bus".into(), 0);
    };
    let mut last_units = 0;
    let mut last_progress = Instant::now();
    loop {
        if hub.try_close() {
            return Outcome::Closed;
        }
        let units = video_units.load(Ordering::Relaxed);
        if units != last_units {
            if last_units == 0 {
                // The stream was described before its first picture.
                lock(&hub.audio_label).get_or_insert(None);
                hub.set_state(Phase::Streaming, 0, None, true);
            }
            last_units = units;
            last_progress = Instant::now();
        } else if last_progress.elapsed() >= NO_FRAME_TIMEOUT {
            return failed(
                if units == 0 {
                    "No video received from camera".into()
                } else {
                    "Video stopped arriving from camera".into()
                },
                units,
            );
        }
        let Some(message) =
            bus.timed_pop(gstreamer::ClockTime::from_mseconds(POLL.as_millis() as u64))
        else {
            continue;
        };
        match message.view() {
            gstreamer::MessageView::Error(error) => {
                return failed(
                    format!("{}: {}", error.error(), error.debug().unwrap_or_default()),
                    units,
                );
            }
            // A file source ended, or the camera closed the stream.
            gstreamer::MessageView::Eos(_) => return failed("Stream ended (EOS)".into(), units),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtsp_sources_receive_and_parse_only() {
        let mut camera = CameraConfig::new("Cam", "rtsp://example/main");
        camera.username = Some("admin".into());
        camera.password_b64 = Some("cGFzcw==".into());
        let (description, paced) = description(&camera).unwrap();
        assert!(!paced);
        assert!(description.contains("location=\"rtsp://admin:pass@example/main\""));
        assert!(description.contains(
            "hub_src. ! application/x-rtp,media=video ! parsebin ! appsink name=hub_video"
        ));
        assert!(description.contains(
            "hub_src. ! application/x-rtp,media=audio ! parsebin ! appsink name=hub_audio"
        ));
        let elements: Vec<&str> = description
            .split('!')
            .filter_map(|link| link.split_whitespace().next())
            .collect();
        assert!(
            elements
                .iter()
                .all(|e| !e.contains("dec") && !e.contains("enc") && !e.contains("convert")),
            "nothing decodes or encodes: {elements:?}"
        );
    }

    /// A camera without sound: its sound branch never gets a buffer, and
    /// video must flow all the same.
    #[test]
    fn a_camera_without_sound_still_streams_video() {
        gstreamer::init().unwrap();
        let pipeline = builder::build(&format!(
            "videotestsrc is-live=true ! video/x-raw,width=64,height=48,framerate=30/1 ! \
             jpegenc ! jpegparse ! appsink name={VIDEO_SINK} \
             appsrc name=silent ! appsink name={AUDIO_SINK}"
        ))
        .unwrap();
        let sink = |name: &str| {
            pipeline
                .by_name(name)
                .unwrap()
                .downcast::<gstreamer_app::AppSink>()
                .unwrap()
        };
        let (video, audio) = (sink(VIDEO_SINK), sink(AUDIO_SINK));
        configure_sink(&video, Media::Video, false);
        configure_sink(&audio, Media::Audio, false);
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let samples = (0..3)
            .filter(|_| {
                video
                    .try_pull_sample(gstreamer::ClockTime::from_seconds(5))
                    .is_some()
            })
            .count();
        pipeline.set_state(gstreamer::State::Null).unwrap();
        assert_eq!(samples, 3, "video arrives although no sound ever does");
    }

    #[test]
    fn files_are_paced_and_devices_are_not_shared() {
        let file = std::env::temp_dir().join(format!("nw-hub-{}.mp4", uuid::Uuid::new_v4()));
        std::fs::write(&file, b"x").unwrap();
        let paced = description(&CameraConfig::new("Cam", file.to_str().unwrap()));
        std::fs::remove_file(&file).unwrap();
        assert!(paced.unwrap().1);
        assert!(description(&CameraConfig::new("Cam", "/dev/video0")).is_none());
        let mjpeg = description(&CameraConfig::new("Cam", "http://cam/video.mjpg")).unwrap();
        assert!(mjpeg.0.contains("multipartdemux ! jpegparse ! appsink"));
    }
}
