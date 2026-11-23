//! When a hardware decoder is given up for the CPU.
//!
//! A view's (or an always-on analysis) decodebin plugs a hardware decoder when
//! it can. One that fails is not retried on the same hardware forever: this
//! decides, per stream and from what the decoder did, when to decode on the CPU
//! for the rest of that decoder's life (`hwaccel::software_only`).
//!
//! Only the decoder's own faults count (an error raised by the hardware
//! decoder element, or the decoder stalling while the camera keeps sending):
//! camera disconnects, hub reconnects, end of stream and source errors never do.
//! The policy is plain data, so it is tested without GStreamer.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Decoder-caused failures that move a stream that has shown pictures to the
/// CPU: one is a glitch, this many within [`WINDOW`] is a decoder to give up.
pub(crate) const FAILURES_BEFORE_CPU: usize = 2;
/// How long a failure counts against the decoder.
pub(crate) const WINDOW: Duration = Duration::from_secs(600);
/// No picture this long while units keep arriving from a streaming camera:
/// the decoder stalled. Between `BORROW_TIMEOUT` (8 s) and the supervisor's
/// `NO_FRAME_TIMEOUT` (20 s, which restarts the run).
pub(crate) const STALL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Not the decoder's doing, or it is a CPU decoder already.
    Ignore,
    /// Counted; the run restarts on the same decoder, as for any failure.
    Retry,
    /// Decode on the CPU from now on.
    Software,
}

#[derive(Debug, Default)]
pub(crate) struct DecoderHealth {
    /// The current run showed a picture (set by [`Self::run_started`] to
    /// false, by [`Self::picture`] to true).
    pictures_seen: bool,
    failures: VecDeque<Instant>,
    software: bool,
}

impl DecoderHealth {
    /// Decode on the CPU (the decision is final for this decoder's life).
    pub fn software(&self) -> bool {
        self.software
    }

    /// A new pipeline run begins: it has shown nothing yet. Failures counted
    /// before stay counted.
    pub fn run_started(&mut self) {
        self.pictures_seen = false;
    }

    /// The decoder delivered a picture in this run.
    pub fn picture(&mut self) {
        self.pictures_seen = true;
    }

    pub fn pictures_seen(&self) -> bool {
        self.pictures_seen
    }

    /// A run failed. `decoder_caused` is whether the decoder was at fault (see
    /// the module docs). Before the run's first picture one such failure
    /// moves to the CPU (a stream the hardware cannot take at all); after
    /// pictures it takes [`FAILURES_BEFORE_CPU`] within [`WINDOW`].
    pub fn failed(&mut self, now: Instant, decoder_caused: bool) -> Verdict {
        if self.software || !decoder_caused {
            return Verdict::Ignore;
        }
        if !self.pictures_seen {
            self.software = true;
            return Verdict::Software;
        }
        while self
            .failures
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > WINDOW)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        if self.failures.len() >= FAILURES_BEFORE_CPU {
            self.software = true;
            Verdict::Software
        } else {
            Verdict::Retry
        }
    }
}

/// Whether a bus message's source is `decoder` or inside it.
pub(crate) fn raised_by(msg: &gstreamer::Message, decoder: &gstreamer::Element) -> bool {
    use gstreamer::prelude::*;
    let decoder = decoder.upcast_ref::<gstreamer::Object>();
    let mut source = msg.src().cloned();
    while let Some(object) = source {
        if &object == decoder {
            return true;
        }
        source = object.parent();
    }
    false
}

/// Why a decoder gets blamed, for the log.
pub(crate) fn describe(stall: bool) -> &'static str {
    if stall {
        "stopped delivering pictures while the camera kept sending"
    } else {
        "reported an error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer::prelude::*;

    fn at(secs: u64, base: Instant) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn one_failure_before_the_first_picture_moves_to_the_cpu() {
        let now = Instant::now();
        let mut health = DecoderHealth::default();
        assert_eq!(health.failed(now, true), Verdict::Software);
        assert!(health.software());
    }

    #[test]
    fn failures_that_are_not_the_decoders_never_count() {
        let now = Instant::now();
        let mut health = DecoderHealth::default();
        for step in 0..5 {
            assert_eq!(health.failed(at(step, now), false), Verdict::Ignore);
        }
        health.picture();
        for step in 5..10 {
            assert_eq!(health.failed(at(step, now), false), Verdict::Ignore);
        }
        assert!(!health.software());
        assert_eq!(
            health.failed(at(10, now), true),
            Verdict::Retry,
            "none were counted"
        );
    }

    #[test]
    fn after_pictures_one_glitch_retries_and_two_within_ten_minutes_move_to_the_cpu() {
        let now = Instant::now();
        let mut health = DecoderHealth::default();
        health.picture();
        assert_eq!(health.failed(now, true), Verdict::Retry);
        assert!(!health.software());
        health.run_started();
        health.picture();
        assert_eq!(health.failed(at(599, now), true), Verdict::Software);
        assert!(health.software());
        assert_eq!(health.failed(at(600, now), true), Verdict::Ignore, "final");
    }

    #[test]
    fn failures_ten_minutes_apart_do_not_add_up() {
        let now = Instant::now();
        let mut health = DecoderHealth::default();
        health.picture();
        for round in 0..6 {
            assert_eq!(health.failed(at(round * 601, now), true), Verdict::Retry);
        }
        assert!(!health.software());
    }

    #[test]
    fn a_new_run_that_fails_before_a_picture_moves_to_the_cpu_at_once() {
        let now = Instant::now();
        let mut health = DecoderHealth::default();
        health.picture();
        assert_eq!(health.failed(now, true), Verdict::Retry);
        // The restart never gets a picture out (a stream the decoder lost).
        health.run_started();
        assert_eq!(health.failed(at(1, now), true), Verdict::Software);
    }

    #[test]
    fn an_error_is_attributed_to_the_decoder_by_its_source_chain() {
        gstreamer::init().unwrap();
        let bin = gstreamer::Bin::with_name("view_decode");
        let decoder = gstreamer::ElementFactory::make("identity")
            .name("decoder")
            .build()
            .unwrap();
        let other = gstreamer::ElementFactory::make("identity")
            .name("parser")
            .build()
            .unwrap();
        bin.add_many([&decoder, &other]).unwrap();
        let error = |source: &gstreamer::Element| {
            gstreamer::message::Error::builder(gstreamer::CoreError::Failed, "x")
                .src(source)
                .build()
        };
        assert!(raised_by(&error(&decoder), &decoder));
        assert!(!raised_by(&error(&other), &decoder));
        assert!(
            !raised_by(&error(bin.upcast_ref()), &decoder),
            "the bin is not the decoder"
        );
        // Something inside the decoder counts as the decoder.
        let inner = gstreamer::Pad::builder(gstreamer::PadDirection::Src)
            .name("inner")
            .build();
        decoder.add_pad(&inner).unwrap();
        let from_pad = gstreamer::message::Error::builder(gstreamer::CoreError::Failed, "x")
            .src(&inner)
            .build();
        assert!(raised_by(&from_pad, &decoder));
    }

    /// With the real decodebin of a view: the hardware decoder it plugs is
    /// found, its errors are attributed to it and the rest of the pipeline's
    /// are not, and a decoder given up on is replaced by a CPU one.
    #[test]
    fn a_real_hardware_decoder_is_found_blamed_and_replaced() {
        gstreamer::init().unwrap();
        for factory in ["x264enc", "h264parse", "avdec_h264", "vah264dec"] {
            if gstreamer::ElementFactory::find(factory).is_none() {
                eprintln!("skipping: GStreamer element {factory} is not installed");
                return;
            }
        }
        let run = |software: bool| {
            let pipeline = gstreamer::parse::launch(
                "videotestsrc num-buffers=30 ! videoconvert ! video/x-raw,format=I420,width=320,height=240 ! x264enc \
                 tune=zerolatency ! h264parse ! decodebin name=view_decode ! fakesink name=sink",
            )
            .unwrap()
            .downcast::<gstreamer::Pipeline>()
            .unwrap();
            let decodebin = pipeline.by_name("view_decode").unwrap();
            if software {
                crate::pipeline::hwaccel::software_only(&decodebin);
            }
            pipeline.set_state(gstreamer::State::Playing).unwrap();
            pipeline.bus().unwrap().timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(10),
                &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
            );
            let bin = decodebin.downcast::<gstreamer::Bin>().unwrap();
            let found = (
                crate::pipeline::hwaccel::hardware_decoder_element_in(&bin),
                crate::pipeline::hwaccel::decoder_label_in(&bin),
            );
            (pipeline, bin, found)
        };
        let (pipeline, bin, (decoder, label)) = run(false);
        let Some(decoder) = decoder else {
            eprintln!("skipping: decodebin did not plug a hardware decoder here");
            pipeline.set_state(gstreamer::State::Null).unwrap();
            return;
        };
        let from = |source: &gstreamer::Element| {
            gstreamer::message::Error::builder(gstreamer::CoreError::Failed, "x")
                .src(source)
                .build()
        };
        assert_eq!(label, Some("VA-API"));
        assert!(raised_by(&from(&decoder), &decoder));
        assert!(!raised_by(
            &from(&pipeline.by_name("sink").unwrap()),
            &decoder
        ));
        assert!(!raised_by(&from(bin.upcast_ref()), &decoder));
        // Two decoder errors within the window: the CPU decoder is next.
        let mut health = DecoderHealth::default();
        health.picture();
        let now = Instant::now();
        assert_eq!(health.failed(now, true), Verdict::Retry);
        assert_eq!(health.failed(now, true), Verdict::Software);
        pipeline.set_state(gstreamer::State::Null).unwrap();
        let (pipeline, _bin, (decoder, label)) = run(health.software());
        pipeline.set_state(gstreamer::State::Null).unwrap();
        assert!(decoder.is_none(), "no hardware decoder after the verdict");
        assert_eq!(label, Some("CPU"));
    }
}
