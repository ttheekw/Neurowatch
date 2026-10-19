//! Analysis-only pipeline controls. Never change registry ranks or desktop decoders.
use crate::pipeline::{builder, hwaccel::HwAccelKind};
use gstreamer::prelude::*;
use nw_core::camera::CameraConfig;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

#[derive(Default)]
pub(super) struct Counters {
    pub received: AtomicU64,
    pub skipped: AtomicU64,
    pub decoder: Mutex<Option<String>>,
}

pub(super) fn pipeline(camera: &CameraConfig, acceleration: HwAccelKind) -> String {
    let acceleration = if camera.motion.low_cpu {
        HwAccelKind::Software
    } else {
        acceleration
    };
    let source = builder::select_pipeline(camera, acceleration);
    // Remove only the builder's output suffix. Never replace element names or
    // caps strings inside the user's quoted camera URL.
    let prefix = source
        .rsplit_once(" ! appsink name=frame_sink ")
        .map(|(prefix, _)| prefix)
        .unwrap_or(&source);
    let prefix = prefix
        .strip_suffix(" ! video/x-raw,format=RGB")
        .unwrap_or(prefix);
    format!("{prefix} ! {}", tail())
}

pub(super) fn shared_pipeline() -> String {
    format!(
        "appsrc name=analysis_source is-live=true format=time ! decodebin ! {}",
        tail()
    )
}

/// Raw desktop pictures need only rate/size/colour conversion, never a decoder.
pub(super) fn decoded_pipeline() -> String {
    format!(
        "appsrc name=analysis_source is-live=true format=time ! {}",
        // The worker sets this filter to the last analysis output's caps.
        // RGB tiles otherwise produce full-range gray, which can look like
        // a lighting change when switching from the decoder's limited luma.
        tail().replace("video/x-raw,format=GRAY8,width=320,height=180",
            "capsfilter name=analysis_format caps=\"video/x-raw,format=GRAY8,width=320,height=180,colorimetry=bt601\"")
    )
}

fn tail() -> &'static str {
    "videorate drop-only=true max-rate=5 ! videoscale ! videoconvert ! video/x-raw,format=GRAY8,width=320,height=180 ! appsink name=analysis_sink max-buffers=1 drop=true sync=false emit-signals=false"
}

/// Called before Playing, including for nested decodebin children added later.
/// Parsers mark complete access units with DELTA_UNIT. Discarding those at the
/// decoder input avoids decoding them at all; skip-frame=nonkey is not supported
/// by the libav decoders in our supported GStreamer versions.
pub(super) fn instrument(pipeline: &gstreamer::Pipeline, low_cpu: bool) -> Arc<Counters> {
    let counters = Arc::new(Counters::default());
    let dynamic = Arc::clone(&counters);
    pipeline.connect_deep_element_added(move |_, _, element| configure(element, low_cpu, &dynamic));
    for element in pipeline.iterate_recurse().into_iter().flatten() {
        configure(&element, low_cpu, &counters);
    }
    if let Some(pad) = pipeline
        .by_name("analysis_sink")
        .and_then(|e| e.static_pad("sink"))
    {
        let received = Arc::clone(&counters);
        pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer_mut() {
                let sequence = received.received.fetch_add(1, Ordering::Relaxed) + 1;
                buffer.make_mut().set_offset(sequence);
            }
            gstreamer::PadProbeReturn::Ok
        });
    }
    counters
}

fn configure(element: &gstreamer::Element, low_cpu: bool, counters: &Arc<Counters>) {
    let Some(factory) = element.factory() else {
        return;
    };
    if factory.name() == "rtspsrc" {
        element.set_property("tcp-timeout", 5_000_000u64);
        element.set_property("timeout", 5_000_000u64);
        element.set_property("teardown-timeout", 100_000_000u64);
    }
    if low_cpu && factory.name() == "decodebin" {
        element.set_property("force-sw-decoders", true);
    }
    let class = factory.metadata("klass").unwrap_or_default();
    if !class.contains("Decoder") || !class.contains("Video") {
        return;
    }
    if let Ok(mut name) = counters.decoder.lock() {
        *name = Some(factory.name().to_string());
    }
    if !low_cpu {
        return;
    }
    let Some(pad) = element.static_pad("sink") else {
        return;
    };
    let skipped = Arc::clone(counters);
    pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
        if info
            .buffer()
            .is_some_and(|b| b.flags().contains(gstreamer::BufferFlags::DELTA_UNIT))
        {
            skipped.skipped.fetch_add(1, Ordering::Relaxed);
            // Consume explicitly. Returning Drop with an owned probe buffer
            // double-unrefs it in the supported Rust/GStreamer combination.
            drop(info.take_buffer());
            gstreamer::PadProbeReturn::Handled
        } else {
            gstreamer::PadProbeReturn::Ok
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn borrowed_rgb_and_nv12_pictures_keep_the_analysis_luma_range() {
        fn picture(tail: &str) -> gstreamer::Sample {
            let pipeline = builder::build(&format!("videotestsrc num-buffers=1 pattern=ball ! video/x-raw,format=I420,width=1280,height=720,colorimetry=bt709 ! {tail} ! appsink name=picture sync=false")).unwrap();
            pipeline.set_state(gstreamer::State::Playing).unwrap();
            let sample = pipeline
                .by_name("picture")
                .unwrap()
                .downcast::<gstreamer_app::AppSink>()
                .unwrap()
                .try_pull_sample(gstreamer::ClockTime::from_seconds(3))
                .unwrap();
            pipeline.set_state(gstreamer::State::Null).unwrap();
            sample
        }
        gstreamer::init().unwrap();
        let reference =
            picture("videoscale ! videoconvert ! video/x-raw,format=GRAY8,width=320,height=180");
        let black = reference.buffer().unwrap().map_readable().unwrap()[0];
        for format in ["RGB", "NV12"] {
            let raw = picture(&format!("videoconvert ! video/x-raw,format={format}"));
            let pipeline = builder::build(&decoded_pipeline()).unwrap();
            let mut target = reference.caps().unwrap().to_owned();
            target
                .make_mut()
                .structure_mut(0)
                .unwrap()
                .set("framerate", gstreamer::Fraction::new(5, 1));
            pipeline
                .by_name("analysis_format")
                .unwrap()
                .set_property("caps", target);
            let source = pipeline
                .by_name("analysis_source")
                .unwrap()
                .downcast::<gstreamer_app::AppSrc>()
                .unwrap();
            let sink = pipeline
                .by_name("analysis_sink")
                .unwrap()
                .downcast::<gstreamer_app::AppSink>()
                .unwrap();
            pipeline.set_state(gstreamer::State::Playing).unwrap();
            source.set_caps(Some(&raw.caps().unwrap().to_owned()));
            let mut buffer = raw.buffer().unwrap().copy();
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gstreamer::ClockTime::ZERO);
            source.push_buffer(buffer).unwrap();
            source.end_of_stream().unwrap();
            let sample = sink
                .try_pull_sample(gstreamer::ClockTime::from_seconds(3))
                .unwrap();
            pipeline.set_state(gstreamer::State::Null).unwrap();
            assert!(pipeline.iterate_recurse().into_iter().flatten().all(|e| !e
                .factory()
                .is_some_and(|f| f.metadata("klass").is_some_and(|c| c.contains("Decoder")))));
            let info = gstreamer_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
            assert_eq!((info.width(), info.height()), (320, 180));
            assert_eq!(info.format(), gstreamer_video::VideoFormat::Gray8);
            assert_eq!(
                sample.buffer().unwrap().map_readable().unwrap()[0],
                black,
                "{format} must match the camera decoder's background luma"
            );
        }
    }

    #[test]
    fn analysis_options_do_not_rewrite_camera_urls_or_global_decoder_choices() {
        gstreamer::init().unwrap();
        let mut camera = CameraConfig::new("test", "rtsp://127.0.0.1/decodebin");
        camera.motion.low_cpu = true;
        let pipeline = builder::build(&pipeline(&camera, HwAccelKind::NvDec)).unwrap();
        instrument(&pipeline, true);
        let elements: Vec<_> = pipeline.iterate_recurse().into_iter().flatten().collect();
        let source = elements
            .iter()
            .find(|e| e.factory().is_some_and(|f| f.name() == "rtspsrc"))
            .unwrap();
        assert_eq!(source.property::<String>("location"), camera.url);
        assert_eq!(source.property::<u64>("tcp-timeout"), 5_000_000);
        let decoder = elements
            .iter()
            .find(|e| e.factory().is_some_and(|f| f.name() == "decodebin"))
            .unwrap();
        assert!(decoder.property::<bool>("force-sw-decoders"));
        let desktop =
            builder::build(&builder::select_pipeline(&camera, HwAccelKind::Software)).unwrap();
        let decoder = desktop
            .iterate_recurse()
            .into_iter()
            .flatten()
            .find(|e| e.factory().is_some_and(|f| f.name() == "decodebin"))
            .unwrap();
        assert!(!decoder.property::<bool>("force-sw-decoders"));
    }

    #[test]
    #[ignore = "requires x264/x265 encoders; run explicitly for motion qualification"]
    fn keyframe_filter_prevents_delta_units_from_reaching_the_decoder() {
        gstreamer::init().unwrap();
        // Real encoded pictures, not just a string assertion. No sockets/camera.
        for (encoder, parser, decoder) in [
            (
                "x264enc tune=zerolatency key-int-max=15",
                "h264parse",
                "avdec_h264",
            ),
            (
                "x265enc tune=zerolatency key-int-max=15 option-string=pools=1:frame-threads=1",
                "h265parse",
                "avdec_h265",
            ),
        ] {
            let description = format!("videotestsrc num-buffers=30 pattern=ball ! video/x-raw,width=320,height=180,framerate=15/1 ! {encoder} ! {parser} ! {decoder} ! videoconvert ! video/x-raw,format=GRAY8 ! appsink name=analysis_sink sync=false");
            for (low_cpu, expected) in [(false, 30), (true, 2)] {
                let pipeline = builder::build(&description).unwrap();
                let counters = instrument(&pipeline, low_cpu);
                let sink = pipeline
                    .by_name("analysis_sink")
                    .unwrap()
                    .downcast::<gstreamer_app::AppSink>()
                    .unwrap();
                pipeline.set_state(gstreamer::State::Playing).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut frames = 0;
                while Instant::now() < deadline {
                    if sink
                        .try_pull_sample(gstreamer::ClockTime::from_mseconds(100))
                        .is_some()
                    {
                        frames += 1;
                    } else if sink.is_eos() {
                        break;
                    }
                }
                pipeline.set_state(gstreamer::State::Null).unwrap();
                assert_eq!(frames, expected, "low_cpu={low_cpu}");
                assert_eq!(counters.skipped.load(Ordering::Relaxed), 30 - expected);
                assert_eq!(counters.decoder.lock().unwrap().as_deref(), Some(decoder));
            }
        }
    }

    #[test]
    fn a_slow_analysis_consumer_keeps_only_the_latest_picture_and_measures_queue_loss() {
        gstreamer::init().unwrap();
        let pipeline = builder::build("videotestsrc num-buffers=100 ! video/x-raw,format=GRAY8,width=320,height=180 ! appsink name=analysis_sink max-buffers=1 drop=true sync=false wait-on-eos=false").unwrap();
        let counters = instrument(&pipeline, false);
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(5),
                &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gstreamer::MessageType::Eos);
        let sink = pipeline
            .by_name("analysis_sink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        let sample = sink.try_pull_sample(gstreamer::ClockTime::ZERO).unwrap();
        assert_eq!(sample.buffer().unwrap().offset(), 100);
        assert_eq!(counters.received.load(Ordering::Relaxed), 100);
        assert!(sink.try_pull_sample(gstreamer::ClockTime::ZERO).is_none());
        assert_eq!(counters.skipped.load(Ordering::Relaxed), 0);
        pipeline.set_state(gstreamer::State::Null).unwrap();
    }
}
