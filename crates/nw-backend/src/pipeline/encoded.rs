//! Browser live view's camera input: receive, depayload and parse a camera
//! stream into H.264/H.265 access units. Nothing here decodes or encodes.
//!
//! `parsebin` picks the RTP depayloader (or demuxer) and parser for whatever
//! the camera sends, but fixes its output at `stream-format=avc`. On its video
//! pad a codec-specific parser converts to Annex-B access units and repeats
//! the parameter sets before every keyframe (`config-interval=-1`), so a
//! viewer can start at any keyframe. A stream in another codec (MJPEG over
//! RTSP, say) is reported as such; it is never transcoded.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gstreamer::prelude::*;
use nw_core::camera::{CameraConfig, Transport};
use nw_core::encoded::{EncodedAccessUnit, VideoCodec};

use super::builder::{gst_quote, is_local_file_url, url_with_credentials_str};

const PARSE: &str = "encoded_parse";
pub const ENCODED_SINK: &str = "encoded_sink";

/// What the pipeline reports: each access unit, and the stream's codec once
/// known (or the caps name of a codec that cannot be forwarded).
#[derive(Clone)]
pub struct EncodedOutputs {
    pub unit: Arc<dyn Fn(EncodedAccessUnit) + Send + Sync>,
    pub codec: Arc<dyn Fn(Result<VideoCodec, String>) + Send + Sync>,
}

/// Whether `url` names a source with compressed video to forward: RTSP, RTMP
/// or a local file. MJPEG over HTTP and V4L2 devices have none.
pub fn forwardable(url: &str) -> bool {
    url.starts_with("rtsp://")
        || url.starts_with("rtsps://")
        || url.starts_with("rtmp://")
        || is_local_file_url(url)
}

/// The pipeline up to `parsebin` for `config`'s URL, and whether it is a live
/// source (a file is paced to its timestamps instead). `None` when the source
/// has no compressed H.264/H.265 to forward.
pub fn source_description(config: &CameraConfig) -> Option<(String, bool)> {
    let url = &config.url;
    if url.starts_with("rtsp://") || url.starts_with("rtsps://") {
        let protocols = match config.transport {
            Transport::Tcp => "tcp",
            Transport::Udp => "udp",
        };
        let location = gst_quote(&url_with_credentials_str(
            url,
            config.username.as_deref(),
            config.password_b64.as_deref(),
        ));
        Some((
            format!(
                "rtspsrc location=\"{location}\" latency=100 protocols={protocols} \
                 do-rtsp-keep-alive=true retry=5 ! application/x-rtp,media=video ! \
                 parsebin name={PARSE}"
            ),
            true,
        ))
    } else if url.starts_with("rtmp://") {
        Some((
            format!(
                "rtmpsrc location=\"{}\" ! parsebin name={PARSE}",
                gst_quote(url)
            ),
            true,
        ))
    } else if is_local_file_url(url) {
        Some((
            format!(
                "filesrc location=\"{}\" ! parsebin name={PARSE}",
                gst_quote(url)
            ),
            false,
        ))
    } else {
        None
    }
}

/// Build the pipeline for `source` (ending in a `parsebin` named
/// `encoded_parse`), delivering access units to `outputs`.
pub fn build(
    source: &str,
    live: bool,
    outputs: EncodedOutputs,
) -> anyhow::Result<gstreamer::Pipeline> {
    let pipeline = super::builder::build(source)?;
    let parse = pipeline
        .by_name(PARSE)
        .ok_or_else(|| anyhow::anyhow!("encoded source has no parsebin"))?;
    // An RTP stream names its codec before `parsebin` sees a byte, and
    // `parsebin` fails without exposing a pad for some (RTP JPEG), so the
    // codec is checked on the way in.
    if let Some(sink) = parse.static_pad("sink") {
        let report = Arc::clone(&outputs.codec);
        sink.add_probe(gstreamer::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
            let Some(gstreamer::PadProbeData::Event(event)) = &info.data else {
                return gstreamer::PadProbeReturn::Ok;
            };
            let gstreamer::EventView::Caps(caps) = event.view() else {
                return gstreamer::PadProbeReturn::Ok;
            };
            let rtp = caps
                .caps()
                .structure(0)
                .filter(|structure| structure.name() == "application/x-rtp")
                .and_then(|structure| structure.get::<String>("encoding-name").ok());
            if let Some(encoding) = rtp.filter(|encoding| !matches!(encoding.as_str(), "H264" | "H265")) {
                let name = format!("RTP {encoding}");
                report(Err(name.clone()));
                if let Some(element) = pad.parent_element() {
                    let message = format!("camera video is {name}; browser live view forwards only H.264 or H.265");
                    let _ = element.post_message(
                        gstreamer::message::Error::builder(gstreamer::StreamError::CodecNotFound, &message)
                            .src(&element)
                            .build(),
                    );
                }
                return gstreamer::PadProbeReturn::Drop;
            }
            gstreamer::PadProbeReturn::Ok
        });
    }
    let bin = pipeline.clone();
    parse.connect_pad_added(move |element, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let Some(name) = caps.structure(0).map(|s| s.name().to_string()) else {
            return;
        };
        // Audio and other streams stay unlinked: browser live view is video only.
        if !name.starts_with("video/") && !name.starts_with("image/") {
            return;
        }
        let Some(codec) = VideoCodec::from_caps_name(&name) else {
            (outputs.codec)(Err(name.clone()));
            let message =
                format!("camera video is {name}; browser live view forwards only H.264 or H.265");
            let _ = element.post_message(
                gstreamer::message::Error::builder(gstreamer::StreamError::CodecNotFound, &message)
                    .src(element)
                    .build(),
            );
            return;
        };
        (outputs.codec)(Ok(codec));
        if let Err(error) = link_codec_tail(&bin, pad, codec, live, &outputs) {
            tracing::warn!(%error, "encoded live tail could not be linked");
        }
    });
    Ok(pipeline)
}

/// A pulled access-unit source: the sink appears once the stream's codec is
/// known (`None` until then); `Err` holds a codec that cannot be sent.
pub type PulledSink =
    Arc<std::sync::Mutex<Option<Result<(gstreamer_app::AppSink, VideoCodec), String>>>>;

/// Like [`build`], but for a reader that paces itself (recorded playback):
/// access units are pulled from an appsink that holds a few and then makes
/// the file wait, instead of being pushed as they are parsed.
pub fn build_pulled(source: &str) -> anyhow::Result<(gstreamer::Pipeline, PulledSink)> {
    let pipeline = super::builder::build(source)?;
    let parse = pipeline
        .by_name(PARSE)
        .ok_or_else(|| anyhow::anyhow!("encoded source has no parsebin"))?;
    let slot: PulledSink = Arc::default();
    let bin = pipeline.clone();
    let found = Arc::clone(&slot);
    parse.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let Some(name) = caps.structure(0).map(|s| s.name().to_string()) else {
            return;
        };
        if !name.starts_with("video/") && !name.starts_with("image/") {
            return;
        }
        let outcome = match VideoCodec::from_caps_name(&name) {
            Some(codec) => {
                let sink = gstreamer_app::AppSink::builder()
                    .name(ENCODED_SINK)
                    .sync(false)
                    .max_buffers(8)
                    .build();
                link_tail(&bin, pad, codec, sink.clone())
                    .map(|()| (sink, codec))
                    .map_err(|error| error.to_string())
            }
            None => Err(name),
        };
        if let Ok(mut found) = found.lock() {
            found.get_or_insert(outcome);
        }
    });
    Ok((pipeline, slot))
}

fn link_codec_tail(
    bin: &gstreamer::Pipeline,
    pad: &gstreamer::Pad,
    codec: VideoCodec,
    live: bool,
    outputs: &EncodedOutputs,
) -> anyhow::Result<()> {
    let unit = Arc::clone(&outputs.unit);
    let sink = gstreamer_app::AppSink::builder()
        .name(ENCODED_SINK)
        // A file plays at its own pace; a camera arrives at its own pace.
        .sync(!live)
        .callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink
                        .pull_sample()
                        .map_err(|_| gstreamer::FlowError::Error)?;
                    if let Some(access_unit) = access_unit(&sample, codec) {
                        unit(access_unit);
                    }
                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        )
        .build();
    link_tail(bin, pad, codec, sink)
}

/// Link `pad` (a parsed H.264/H.265 stream) through its codec parser, which
/// emits Annex-B access units with parameter sets before every keyframe, to
/// `sink`.
fn link_tail(
    bin: &gstreamer::Pipeline,
    pad: &gstreamer::Pad,
    codec: VideoCodec,
    sink: gstreamer_app::AppSink,
) -> anyhow::Result<()> {
    let (parser, caps) = match codec {
        VideoCodec::H264 => (
            "h264parse",
            "video/x-h264,stream-format=byte-stream,alignment=au",
        ),
        VideoCodec::H265 => (
            "h265parse",
            "video/x-h265,stream-format=byte-stream,alignment=au",
        ),
    };
    let parser = gstreamer::ElementFactory::make(parser)
        .property("config-interval", -1i32)
        .build()?;
    let filter = gstreamer::ElementFactory::make("capsfilter")
        .property("caps", gstreamer::Caps::from_str(caps)?)
        .build()?;
    let elements = [&parser, &filter, sink.upcast_ref()];
    bin.add_many(elements)?;
    gstreamer::Element::link_many(elements)?;
    for element in elements {
        element.sync_state_with_parent()?;
    }
    let target = parser
        .static_pad("sink")
        .ok_or_else(|| anyhow::anyhow!("parser has no sink pad"))?;
    pad.link(&target)?;
    Ok(())
}

use std::str::FromStr as _;

/// One access unit from a parser sample; `None` for an empty buffer or a
/// stream whose size the parser has not found yet.
pub fn access_unit(sample: &gstreamer::Sample, codec: VideoCodec) -> Option<EncodedAccessUnit> {
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    if map.is_empty() {
        return None;
    }
    let structure = sample.caps()?.structure(0)?;
    let dimension = |field| {
        structure
            .get::<i32>(field)
            .ok()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
    };
    Some(EncodedAccessUnit {
        codec,
        payload: map.to_vec(),
        keyframe: !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT),
        width: dimension("width")?,
        height: dimension("height")?,
        timestamp_us: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_micros() as i64),
        // Stream time, not the raw timestamp: a recording's later fragments
        // continue its timeline, so their raw timestamps start far from 0.
        pts_us: buffer
            .pts()
            .map(|pts| {
                sample
                    .segment()
                    .and_then(|segment| {
                        segment
                            .downcast_ref::<gstreamer::ClockTime>()?
                            .to_stream_time(pts)
                    })
                    .unwrap_or(pts)
            })
            .map(|time| i64::try_from(time.useconds()).unwrap_or(i64::MAX)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Collected = (
        Arc<Mutex<Vec<EncodedAccessUnit>>>,
        Arc<Mutex<Vec<Result<VideoCodec, String>>>>,
    );

    fn collecting() -> (EncodedOutputs, Collected) {
        let units = Arc::new(Mutex::new(Vec::new()));
        let codecs = Arc::new(Mutex::new(Vec::new()));
        let (u, c) = (Arc::clone(&units), Arc::clone(&codecs));
        (
            EncodedOutputs {
                unit: Arc::new(move |unit| u.lock().unwrap().push(unit)),
                codec: Arc::new(move |codec| c.lock().unwrap().push(codec)),
            },
            (units, codecs),
        )
    }

    /// Run until end of stream or an error; the error text if any.
    fn run(pipeline: &gstreamer::Pipeline) -> Option<String> {
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        let outcome = loop {
            let message = bus
                .timed_pop(gstreamer::ClockTime::from_seconds(20))
                .expect("pipeline finished in time");
            match message.view() {
                gstreamer::MessageView::Eos(_) => break None,
                gstreamer::MessageView::Error(error) => break Some(error.error().to_string()),
                _ => {}
            }
        };
        pipeline.set_state(gstreamer::State::Null).unwrap();
        outcome
    }

    /// Every element in the pipeline, except the test's own camera stand-in,
    /// that decodes or encodes video.
    fn codecs_in_runtime_graph(pipeline: &gstreamer::Pipeline) -> Vec<String> {
        pipeline
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
            .collect()
    }

    /// A camera stand-in: test pictures encoded and sent as RTP. Only these
    /// elements (named `camera_*`) encode; the pipeline under test must not.
    fn rtp_camera(encoder: &str, payloader: &str) -> String {
        format!(
            "videotestsrc name=camera_source num-buffers=45 is-live=false ! \
             video/x-raw,width=320,height=240,framerate=30/1 ! \
             {encoder} name=camera_encoder key-int-max=15 ! {payloader} name=camera_pay ! \
             application/x-rtp,media=video ! parsebin name={PARSE}"
        )
    }

    fn assert_forwarded(source: &str, codec: VideoCodec) {
        gstreamer::init().unwrap();
        let (outputs, (units, codecs)) = collecting();
        let pipeline = build(source, true, outputs).unwrap();
        assert_eq!(run(&pipeline), None);
        assert!(
            codecs_in_runtime_graph(&pipeline).is_empty(),
            "the live path must not decode or encode: {:?}",
            codecs_in_runtime_graph(&pipeline)
        );
        assert_eq!(codecs.lock().unwrap().as_slice(), &[Ok(codec)]);
        let units = units.lock().unwrap();
        assert_eq!(units.len(), 45, "every picture is forwarded");
        assert!(units[0].keyframe, "the stream starts at a keyframe");
        assert_eq!(units.iter().filter(|unit| unit.keyframe).count(), 3);
        for unit in units.iter() {
            assert_eq!((unit.codec, unit.width, unit.height), (codec, 320, 240));
            assert_eq!(&unit.payload[..4], &[0, 0, 0, 1], "Annex-B start code");
        }
        // Parameter sets travel with every keyframe, so a late viewer can start there.
        let nal_types = |unit: &EncodedAccessUnit| -> Vec<u8> {
            unit.payload
                .windows(4)
                .enumerate()
                .filter(|(_, w)| w[..3] == [0, 0, 1])
                .filter_map(|(i, _)| unit.payload.get(i + 3).copied())
                .map(|header| match codec {
                    VideoCodec::H264 => header & 0x1f,
                    VideoCodec::H265 => (header >> 1) & 0x3f,
                })
                .collect()
        };
        let parameter_set = match codec {
            VideoCodec::H264 => 7,  // SPS
            VideoCodec::H265 => 32, // VPS
        };
        for unit in units.iter().filter(|unit| unit.keyframe) {
            assert!(
                nal_types(unit).contains(&parameter_set),
                "{:?}",
                nal_types(unit)
            );
        }
    }

    #[test]
    fn h264_from_rtp_is_forwarded_without_decoding() {
        assert_forwarded(&rtp_camera("x264enc", "rtph264pay"), VideoCodec::H264);
    }

    #[test]
    fn h265_from_rtp_is_forwarded_without_decoding() {
        assert_forwarded(&rtp_camera("x265enc", "rtph265pay"), VideoCodec::H265);
    }

    #[test]
    fn another_codec_is_reported_and_never_transcoded() {
        gstreamer::init().unwrap();
        let source = format!(
            "videotestsrc name=camera_source num-buffers=10 ! video/x-raw,width=320,height=240 ! \
             jpegenc name=camera_encoder ! rtpjpegpay name=camera_pay ! \
             application/x-rtp,media=video ! parsebin name={PARSE}"
        );
        let (outputs, (units, codecs)) = collecting();
        let pipeline = build(&source, true, outputs).unwrap();
        assert!(run(&pipeline).is_some(), "an unsupported codec fails");
        assert_eq!(codecs.lock().unwrap().as_slice(), &[Err("RTP JPEG".into())]);
        assert!(units.lock().unwrap().is_empty());
        assert!(codecs_in_runtime_graph(&pipeline).is_empty());
    }

    #[test]
    fn only_sources_with_compressed_video_are_forwardable() {
        let camera = |url: &str| CameraConfig::new("Camera", url);
        let (rtsp, live) = source_description(&camera("rtsp://10.0.0.5/stream1")).unwrap();
        assert!(live);
        assert!(rtsp.starts_with("rtspsrc location=\"rtsp://10.0.0.5/stream1\""));
        assert!(rtsp.ends_with("parsebin name=encoded_parse"));
        assert!(source_description(&camera("rtmp://10.0.0.5/live")).is_some());
        assert!(source_description(&camera("http://10.0.0.5/mjpeg")).is_none());
        assert!(source_description(&camera("/dev/video0")).is_none());
        assert!(!forwardable("http://10.0.0.5/mjpeg"));
        assert!(forwardable("rtsps://10.0.0.5/stream"));
    }

    #[test]
    fn rtsp_credentials_are_embedded_for_the_source_only() {
        let mut camera = CameraConfig::new("Camera", "rtsp://10.0.0.5/stream1");
        camera.username = Some("admin".into());
        camera.password_b64 = Some("cGFzcw==".into());
        let (description, _) = source_description(&camera).unwrap();
        assert!(description.contains("rtsp://admin:pass@10.0.0.5/stream1"));
    }

    #[test]
    #[ignore = "requires an RTSP H.264 stream at NW_RTSP_TEST_URL"]
    fn a_real_rtsp_camera_is_forwarded_without_decoding() {
        gstreamer::init().unwrap();
        let url = std::env::var("NW_RTSP_TEST_URL").expect("NW_RTSP_TEST_URL");
        let (source, live) = source_description(&CameraConfig::new("Cam", &url)).unwrap();
        let (outputs, (units, codecs)) = collecting();
        let pipeline = build(&source, live, outputs).unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while units.lock().unwrap().len() < 50 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let graph = codecs_in_runtime_graph(&pipeline);
        pipeline.set_state(gstreamer::State::Null).unwrap();
        assert!(graph.is_empty(), "{graph:?}");
        assert_eq!(codecs.lock().unwrap().as_slice(), &[Ok(VideoCodec::H264)]);
        let units = units.lock().unwrap();
        assert!(units.len() >= 50, "only {} units", units.len());
        // Joining a live camera usually starts mid-GOP; the publisher waits
        // for the first keyframe (`EncodedSubmitter`).
        assert!(units.iter().any(|unit| unit.keyframe));
        assert!(units
            .iter()
            .all(|unit| unit.width > 0 && unit.payload.starts_with(&[0, 0, 0, 1])));
    }
}
