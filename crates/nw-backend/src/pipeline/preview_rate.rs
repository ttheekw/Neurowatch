//! Pace the RGB preview branch without restarting or slowing camera decoding.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gstreamer::prelude::*;

use crate::shm::ring_buffer::ShmRingBufferWriter;
use crate::stats::HealthCounters;

#[derive(Default)]
struct PreviewPacer {
    rate: u32,
    previous_pts: Option<u64>,
    next_due: Option<u64>,
}

impl PreviewPacer {
    fn allow(&mut self, rate: u32, pts: u64) -> bool {
        if rate != self.rate || self.previous_pts.is_some_and(|previous| pts < previous) {
            self.next_due = None;
        }
        self.rate = rate;
        self.previous_pts = Some(pts);
        if rate == 0 {
            self.next_due = None;
            return true;
        }
        // Timestamp rounding/jitter at the source frame rate must not halve FPS.
        if self
            .next_due
            .is_some_and(|due| pts.saturating_add(1_000_000) < due)
        {
            return false;
        }
        let interval = 1_000_000_000_000 / u64::from(rate);
        self.next_due = Some(
            self.next_due
                .unwrap_or(pts)
                .saturating_add(interval)
                .max(pts),
        );
        true
    }
}

/// Find the raw converter's input, staying within the preview's linear branch.
/// A publication tee using RGB needs every converted frame, so that case gates
/// only the appsink input and avoids SHM copies without throttling publication.
fn input_pad(appsink: &gstreamer_app::AppSink, convert_shared: bool) -> Option<gstreamer::Pad> {
    let sink = appsink.static_pad("sink")?;
    if convert_shared {
        return Some(sink);
    }
    let mut upstream = sink.peer().and_then(|pad| pad.parent_element());
    while let Some(element) = upstream {
        let name = element.factory().map(|factory| factory.name().to_string());
        match name.as_deref() {
            Some("videoconvert" | "nvvidconv") => return element.static_pad("sink"),
            Some("capsfilter" | "identity") => {
                upstream = element
                    .static_pad("sink")
                    .and_then(|pad| pad.peer())
                    .and_then(|pad| pad.parent_element());
            }
            _ => break,
        }
    }
    Some(sink)
}

/// Whether anything wants this camera's converted frames right now.
pub(crate) type FrameDemand = Arc<dyn Fn() -> bool + Send + Sync>;

/// The raw converter's input, looking past a publication tee and its queues:
/// headless mode gates before the converter even when publication shares it.
fn converter_input(appsink: &gstreamer_app::AppSink) -> Option<gstreamer::Pad> {
    let mut upstream = appsink
        .static_pad("sink")?
        .peer()
        .and_then(|pad| pad.parent_element());
    while let Some(element) = upstream {
        let name = element.factory().map(|factory| factory.name().to_string());
        match name.as_deref() {
            Some("videoconvert" | "nvvidconv") => return element.static_pad("sink"),
            Some("capsfilter" | "identity" | "queue" | "tee") => {
                upstream = element
                    .static_pad("sink")
                    .and_then(|pad| pad.peer())
                    .and_then(|pad| pad.parent_element());
            }
            _ => return None,
        }
    }
    None
}

/// Headless gate: with no desktop UI, a decoded frame is converted only when
/// `demand` says a consumer (a cloud thumbnail request or a browser watching
/// this camera) wants it. Every decoded frame is still counted, so camera
/// health and no-frame recovery work as with the UI. Returns false if the
/// pipeline has no converter to gate.
pub(super) fn install_demand_gate(
    appsink: &gstreamer_app::AppSink,
    frame_counter: Arc<AtomicU64>,
    health: Option<Arc<HealthCounters>>,
    demand: FrameDemand,
) -> bool {
    let Some(pad) = converter_input(appsink) else {
        return false;
    };
    pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
        if info.buffer().is_none() {
            return gstreamer::PadProbeReturn::Ok;
        }
        frame_counter.fetch_add(1, Ordering::Relaxed);
        if let Some(health) = &health {
            health.record_decoded();
        }
        if demand() {
            gstreamer::PadProbeReturn::Ok
        } else {
            gstreamer::PadProbeReturn::Handled
        }
    })
    .is_some()
}

pub(super) fn install(
    appsink: &gstreamer_app::AppSink,
    writer: Arc<Mutex<ShmRingBufferWriter>>,
    frame_counter: Arc<AtomicU64>,
    health: Option<Arc<HealthCounters>>,
    convert_shared: bool,
) -> bool {
    let Some(pad) = input_pad(appsink, convert_shared) else {
        return false;
    };
    let pacer = Mutex::new(PreviewPacer::default());
    let origin = Instant::now();
    pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
        let Some(buffer) = info.buffer() else {
            return gstreamer::PadProbeReturn::Ok;
        };
        let rate = writer
            .lock()
            .map(|mut writer| writer.preview_rate_millifps())
            .unwrap_or(0);
        let pts = buffer
            .pts()
            .map(|pts| pts.nseconds())
            .unwrap_or_else(|| origin.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        let allowed = pacer
            .lock()
            .map(|mut pacer| pacer.allow(rate, pts))
            .unwrap_or(true);
        // Count decoded input, including intentionally skipped previews, so
        // health and no-frame recovery remain independent of the wall's FPS.
        frame_counter.fetch_add(1, Ordering::Relaxed);
        if let Some(health) = &health {
            health.record_decoded();
        }
        if allowed {
            gstreamer::PadProbeReturn::Ok
        } else {
            // The Rust probe owns this buffer. Handled consumes it without
            // asking older GStreamer versions to unref the cleared pointer.
            gstreamer::PadProbeReturn::Handled
        }
    })
    .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_gate_skips_conversion_until_a_consumer_wants_frames() {
        use gstreamer_app::AppSinkCallbacks;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;
        gstreamer::init().unwrap();
        for shared in [false, true] {
            let tail = if shared {
                "tee name=publication publication. ! queue ! appsink name=frame_sink sync=false \
                 publication. ! queue ! appsink name=published sync=false"
            } else {
                "appsink name=frame_sink sync=false"
            };
            let pipeline = gstreamer::parse::launch(&format!(
                "appsrc name=source format=time ! videoconvert name=converter ! \
                 video/x-raw,format=RGB ! {tail}"
            ))
            .unwrap()
            .downcast::<gstreamer::Pipeline>()
            .unwrap();
            let sink = pipeline
                .by_name("frame_sink")
                .unwrap()
                .downcast::<gstreamer_app::AppSink>()
                .unwrap();
            let decoded = Arc::new(AtomicU64::new(0));
            let wanted = Arc::new(AtomicBool::new(false));
            let demand = Arc::clone(&wanted);
            assert!(install_demand_gate(
                &sink,
                decoded.clone(),
                None,
                Arc::new(move || demand.load(Ordering::Relaxed)),
            ));
            let counter = |name: &str| {
                let count = Arc::new(AtomicU64::new(0));
                let seen = count.clone();
                if let Some(element) = pipeline.by_name(name) {
                    element
                        .downcast::<gstreamer_app::AppSink>()
                        .unwrap()
                        .set_callbacks(
                            AppSinkCallbacks::builder()
                                .new_sample(move |sink| {
                                    sink.pull_sample()
                                        .map_err(|_| gstreamer::FlowError::Error)?;
                                    seen.fetch_add(1, Ordering::Relaxed);
                                    Ok(gstreamer::FlowSuccess::Ok)
                                })
                                .build(),
                        );
                }
                count
            };
            let delivered = counter("frame_sink");
            let published = counter("published");
            let converted = Arc::new(AtomicU64::new(0));
            let conversion_count = converted.clone();
            pipeline
                .by_name("converter")
                .unwrap()
                .static_pad("src")
                .unwrap()
                .add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
                    conversion_count.fetch_add(1, Ordering::Relaxed);
                    gstreamer::PadProbeReturn::Ok
                });
            let source = pipeline
                .by_name("source")
                .unwrap()
                .downcast::<gstreamer_app::AppSrc>()
                .unwrap();
            let info =
                gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::I420, 8, 4)
                    .fps(gstreamer::Fraction::new(15, 1))
                    .build()
                    .unwrap();
            source.set_caps(Some(&info.to_caps().unwrap()));
            pipeline.set_state(gstreamer::State::Playing).unwrap();
            // Nobody wants frames, then a thumbnail or a browser does.
            for (phase, want) in [false, true].into_iter().enumerate() {
                wanted.store(want, Ordering::Relaxed);
                for i in phase * 30..(phase + 1) * 30 {
                    let mut buffer = gstreamer::Buffer::with_size(info.size()).unwrap();
                    buffer
                        .get_mut()
                        .unwrap()
                        .set_pts(gstreamer::ClockTime::from_nseconds(
                            i as u64 * 1_000_000_000 / 15,
                        ));
                    source.push_buffer(buffer).unwrap();
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                while decoded.load(Ordering::Relaxed) < ((phase + 1) * 30) as u64
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            source.end_of_stream().unwrap();
            let message = pipeline.bus().unwrap().timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(5),
                &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
            );
            pipeline.set_state(gstreamer::State::Null).unwrap();
            assert!(message.is_some_and(|m| m.type_() == gstreamer::MessageType::Eos));
            // Every decoded frame is counted for camera health...
            assert_eq!(decoded.load(Ordering::Relaxed), 60);
            // ...but only demanded frames are converted, even with a shared
            // publication branch after the converter.
            assert_eq!(converted.load(Ordering::Relaxed), 30);
            assert_eq!(delivered.load(Ordering::Relaxed), 30);
            assert_eq!(
                published.load(Ordering::Relaxed),
                if shared { 30 } else { 0 }
            );
        }
    }

    #[test]
    fn gate_limits_conversion_without_restarting_or_throttling_publication() {
        use gstreamer_app::AppSinkCallbacks;
        use std::time::Duration;
        gstreamer::init().unwrap();
        for shared in [false, true] {
            let key = format!("test-preview-rate-{}", uuid::Uuid::new_v4());
            let writer = Arc::new(Mutex::new(ShmRingBufferWriter::create(&key).unwrap()));
            assert_eq!(writer.lock().unwrap().preview_rate_millifps(), 0);
            let tail = if shared {
                "tee name=publication publication. ! queue ! appsink name=frame_sink sync=false \
                 publication. ! queue ! appsink name=published sync=false"
            } else {
                "appsink name=frame_sink sync=false"
            };
            let pipeline = gstreamer::parse::launch(&format!(
                "appsrc name=source format=time ! videoconvert name=converter ! \
                 video/x-raw,format=RGB ! {tail}"
            ))
            .unwrap()
            .downcast::<gstreamer::Pipeline>()
            .unwrap();
            let sink = pipeline
                .by_name("frame_sink")
                .unwrap()
                .downcast::<gstreamer_app::AppSink>()
                .unwrap();
            let decoded = Arc::new(AtomicU64::new(0));
            assert!(install(
                &sink,
                writer.clone(),
                decoded.clone(),
                None,
                shared
            ));
            let delivered = Arc::new(AtomicU64::new(0));
            let delivery_count = delivered.clone();
            sink.set_callbacks(
                AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        sink.pull_sample()
                            .map_err(|_| gstreamer::FlowError::Error)?;
                        delivery_count.fetch_add(1, Ordering::Relaxed);
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );
            let published = Arc::new(AtomicU64::new(0));
            if shared {
                let count = published.clone();
                pipeline
                    .by_name("published")
                    .unwrap()
                    .downcast::<gstreamer_app::AppSink>()
                    .unwrap()
                    .set_callbacks(
                        AppSinkCallbacks::builder()
                            .new_sample(move |sink| {
                                sink.pull_sample()
                                    .map_err(|_| gstreamer::FlowError::Error)?;
                                count.fetch_add(1, Ordering::Relaxed);
                                Ok(gstreamer::FlowSuccess::Ok)
                            })
                            .build(),
                    );
            }
            let converted = Arc::new(AtomicU64::new(0));
            let conversion_count = converted.clone();
            pipeline
                .by_name("converter")
                .unwrap()
                .static_pad("src")
                .unwrap()
                .add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
                    conversion_count.fetch_add(1, Ordering::Relaxed);
                    gstreamer::PadProbeReturn::Ok
                });
            let source = pipeline
                .by_name("source")
                .unwrap()
                .downcast::<gstreamer_app::AppSrc>()
                .unwrap();
            let info =
                gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::I420, 8, 4)
                    .fps(gstreamer::Fraction::new(15, 1))
                    .build()
                    .unwrap();
            source.set_caps(Some(&info.to_caps().unwrap()));
            pipeline.set_state(gstreamer::State::Playing).unwrap();
            for (phase, rate) in [2_000, 15_000, 2_000].into_iter().enumerate() {
                writer.lock().unwrap().request_preview_rate(rate);
                for i in phase * 30..(phase + 1) * 30 {
                    let mut buffer = gstreamer::Buffer::with_size(info.size()).unwrap();
                    buffer
                        .get_mut()
                        .unwrap()
                        .set_pts(gstreamer::ClockTime::from_nseconds(
                            i as u64 * 1_000_000_000 / 15,
                        ));
                    source.push_buffer(buffer).unwrap();
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                while decoded.load(Ordering::Relaxed) < ((phase + 1) * 30) as u64
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert_eq!(decoded.load(Ordering::Relaxed), ((phase + 1) * 30) as u64);
            }
            source.end_of_stream().unwrap();
            let message = pipeline.bus().unwrap().timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(5),
                &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
            );
            pipeline.set_state(gstreamer::State::Null).unwrap();
            assert!(message.is_some_and(|m| m.type_() == gstreamer::MessageType::Eos));
            assert_eq!(delivered.load(Ordering::Relaxed), 38);
            assert_eq!(
                converted.load(Ordering::Relaxed),
                if shared { 90 } else { 38 }
            );
            assert_eq!(
                published.load(Ordering::Relaxed),
                if shared { 90 } else { 0 }
            );
        }
    }

    #[test]
    fn paces_hidden_frames_and_adapts_without_waiting_for_old_deadline() {
        let mut pacer = PreviewPacer::default();
        let accepted = (0..30)
            .filter(|i| pacer.allow(2_000, i * 1_000_000_000 / 15))
            .count();
        assert_eq!(accepted, 4);
        assert!(pacer.allow(15_000, 2_000_000_000));
        assert!(pacer.allow(2_000, 2_010_000_000));
        assert!(!pacer.allow(2_000, 2_100_000_000));
        assert!(pacer.allow(0, 2_110_000_000));
        assert!(pacer.allow(0, 2_120_000_000));
    }

    #[test]
    fn source_rate_jitter_does_not_halve_preview_rate_and_loop_resets() {
        let mut pacer = PreviewPacer::default();
        for i in 0..60 {
            let pts = i * 1_000_000_000 / 15 + if i % 2 == 0 { 100_000 } else { 0 };
            assert!(pacer.allow(15_000, pts));
        }
        assert!(pacer.allow(15_000, 0));
        assert!(!pacer.allow(15_000, 1_000_000));
    }
}
