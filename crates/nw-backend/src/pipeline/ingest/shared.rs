//! Recording a camera through its shared connection (`crate::streams`).
//!
//! The recorder subscribes to the camera's main stream and pushes the units
//! it receives, as they are, into a recording pipeline that starts at an
//! `appsrc` ([`builder::shared_recording_pipeline`]). Each run (one set of
//! segment files) starts at a keyframe: the hub's cached one when the
//! connection is already open, so recording starts without waiting for the
//! camera's next keyframe. A connection loss ends the run (the hub
//! reconnects by itself); the next run starts with the new stream.
//!
//! Timestamps: a run's times start at its first keyframe's decoding time,
//! and the run's epoch (in its file names) is that keyframe's arrival time,
//! so `epoch + a segment's first timestamp` is when its first picture
//! arrived, as with a dedicated connection.
//!
//! The camera's status follows the hub: its failures are the ingest's
//! (`reconnecting`, then `offline` from the third), and units reaching
//! `ingest_tap` make it `streaming` / `recording_ready` as before.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gstreamer::prelude::*;

use super::{
    finalize, tap_stream_info, warn_when_the_writer_falls_behind, Attempt, IngestSupervisor,
    AUDIO_DESCRIBE_GRACE, POLL_SLICE,
};
use crate::pipeline::builder;
use crate::pipeline::supervisor::failure_status;
use crate::streams::{Delivery, Media, Phase, SharedUnit, SubscriberPolicy, Subscription, Unit};

/// Most bytes waiting inside the recording's `appsrc` before the recorder
/// stops taking units (its subscription queue then absorbs the rest).
const INPUT_LIMIT_BYTES: u64 = 16 * 1024 * 1024;

/// How a run ended.
enum RunEnd {
    Stopped,
    /// The connection was lost; the hub reconnects.
    Discontinuity,
    /// The stream's format changed at this unit: start a new run there.
    FormatChanged(SharedUnit),
    /// The recording pipeline itself failed (e.g. the drive).
    Failed(Attempt),
}

/// A run's time base.
struct Timing {
    base_ns: u64,
}

impl Timing {
    /// The run starting at `first` (a keyframe), and its epoch in ms.
    fn starting_at(first: &SharedUnit) -> (Self, u128) {
        let buffer = &first.buffer;
        let pts = buffer.pts().map(gstreamer::ClockTime::nseconds);
        let base_ns = buffer
            .dts()
            .map(gstreamer::ClockTime::nseconds)
            .or(pts)
            .unwrap_or(0);
        // The first picture is shown `pts - base` into the run.
        let lead_ms = pts.map_or(0, |pts| pts.saturating_sub(base_ns) / 1_000_000);
        let arrived_ms = u128::try_from(first.arrived_us.max(0) / 1000).unwrap_or(0);
        (
            Self { base_ns },
            arrived_ms.saturating_sub(u128::from(lead_ms)),
        )
    }

    /// `unit`'s buffer with the run's times; `None` before the run started.
    fn rebase(&self, unit: &SharedUnit, after_gap: bool) -> Option<gstreamer::Buffer> {
        let pts = unit.buffer.pts().map(gstreamer::ClockTime::nseconds);
        if pts.is_some_and(|pts| pts < self.base_ns) {
            return None;
        }
        // A shallow copy: new timestamps, the same memory.
        let mut buffer = unit.buffer.copy();
        let writable = buffer.get_mut()?;
        if let Some(pts) = pts {
            writable.set_pts(gstreamer::ClockTime::from_nseconds(pts - self.base_ns));
        }
        if let Some(dts) = unit.buffer.dts() {
            writable.set_dts(gstreamer::ClockTime::from_nseconds(
                dts.nseconds().saturating_sub(self.base_ns),
            ));
        }
        if after_gap {
            writable.set_flags(gstreamer::BufferFlags::DISCONT);
        }
        Some(buffer)
    }
}

impl IngestSupervisor {
    /// Record through the shared connection until stopped, or until the
    /// recording pipeline itself fails (the ingest then backs off and
    /// subscribes again). Connection failures are the hub's to retry.
    pub(super) fn record_shared(&mut self) -> Attempt {
        let failed = |reason: String| Attempt::Failed {
            reason,
            delivered: false,
        };
        let Some((storage_path, segment_minutes)) = self.recording.clone() else {
            return failed("not recording".into());
        };
        if !crate::streams::shareable(&self.source) {
            return failed("this source has no compressed video to record".into());
        }
        let subscription = match self.hubs.subscribe(
            &self.source,
            SubscriberPolicy::recorder(),
            self.reachability.clone(),
        ) {
            Ok(subscription) => subscription,
            Err(reason) => return failed(reason),
        };
        let mut seen = u64::MAX;
        let mut next: Option<SharedUnit> = None;
        loop {
            let first = match next.take().filter(SharedUnit::keyframe) {
                Some(first) => first,
                None => match self.await_keyframe(&subscription, &mut seen) {
                    Some(first) => first,
                    None => return Attempt::Stopped,
                },
            };
            match self.record_run(
                &subscription,
                first,
                &storage_path,
                segment_minutes,
                &mut seen,
            ) {
                RunEnd::Stopped => return Attempt::Stopped,
                RunEnd::Discontinuity => {}
                RunEnd::FormatChanged(unit) => {
                    tracing::info!(cam_id = %self.cam_id, "Camera stream format changed; starting new recording files");
                    next = Some(unit);
                }
                RunEnd::Failed(attempt) => return attempt,
            }
        }
    }

    /// Mirror the hub's connection state into the camera's status.
    fn follow_hub(&mut self, subscription: &Subscription, seen: &mut u64) {
        let state = subscription.state();
        if state.generation == *seen {
            return;
        }
        *seen = state.generation;
        match state.phase {
            Phase::Connecting => {
                self.failures = state.failures;
                self.announce_attempt();
            }
            Phase::Retrying => {
                self.failures = state.failures;
                let status = failure_status(state.failures);
                let reason = state.reason.unwrap_or_default();
                tracing::warn!(cam_id = %self.cam_id, attempt = state.failures, reason = %reason, "Ingest attempt failed");
                self.publish_state(status);
                self.notify_state(status, Some(&reason));
            }
            Phase::Streaming => self.failures = 0,
            Phase::Closed => {}
        }
    }

    /// Wait for a video keyframe to start a run at; `None` once stopped.
    fn await_keyframe(
        &mut self,
        subscription: &Subscription,
        seen: &mut u64,
    ) -> Option<SharedUnit> {
        loop {
            if self.stopped() {
                return None;
            }
            self.follow_hub(subscription, seen);
            match subscription.recv(POLL_SLICE) {
                Some(Delivery::Unit { unit, .. }) if unit.keyframe() => return Some(unit),
                Some(_) => {}
                None if subscription.state().phase == Phase::Closed => return None,
                None => {}
            }
        }
    }

    /// Whether this run records sound: the camera describes a track kept as
    /// it is, and its first unit arrives within the grace after the picture.
    /// Units received meanwhile are returned to be recorded first.
    fn sound_for_run(
        &mut self,
        subscription: &Subscription,
        first: &SharedUnit,
    ) -> (Option<gstreamer::Caps>, Vec<(SharedUnit, bool)>) {
        let label = subscription.audio_label().flatten();
        self.audio = Some(label.clone());
        let wanted = !self.audio_failed
            && label
                .as_deref()
                .is_some_and(crate::pipeline::audio::recordable);
        let mut pending = Vec::new();
        if !wanted {
            return (None, pending);
        }
        let until = Instant::now() + AUDIO_DESCRIBE_GRACE;
        while Instant::now() < until && !self.stopped() {
            match subscription.recv(POLL_SLICE) {
                Some(Delivery::Unit { unit, after_gap }) => {
                    if unit.media == Media::Audio && unit.time_ns() >= first.time_ns() {
                        let caps = unit.caps.clone();
                        pending.push((unit, after_gap));
                        return (Some(caps), pending);
                    }
                    pending.push((unit, after_gap));
                }
                // The connection dropped meanwhile: this run's units are stale.
                Some(Delivery::Discontinuity { .. }) => return (None, Vec::new()),
                None => {}
            }
        }
        tracing::debug!(cam_id = %self.cam_id, "No sound arrived with the picture; recording video only");
        (None, pending)
    }

    fn record_run(
        &mut self,
        subscription: &Subscription,
        first: SharedUnit,
        storage_path: &str,
        segment_minutes: u32,
        seen: &mut u64,
    ) -> RunEnd {
        let failed =
            |reason: String, delivered: bool| RunEnd::Failed(Attempt::Failed { reason, delivered });
        let (audio_caps, pending) = self.sound_for_run(subscription, &first);
        let with_audio = audio_caps.is_some();
        let (timing, epoch_ms) = Timing::starting_at(&first);
        let location = builder::recording_location_at(storage_path, &self.cam_id, epoch_ms);
        let description =
            builder::shared_recording_pipeline(&location, segment_minutes, with_audio);
        let pipeline = match builder::build(&description) {
            Ok(pipeline) => pipeline,
            Err(error) => return failed(error.to_string(), false),
        };
        let input = |name: &str| {
            pipeline
                .by_name(name)
                .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
        };
        let (Some(video_in), Some(tap)) = (
            input(builder::RECORDING_VIDEO_INPUT),
            pipeline.by_name("ingest_tap"),
        ) else {
            return failed("recording pipeline has no input".into(), false);
        };
        video_in.set_caps(Some(&first.caps));
        let audio_in = input(builder::RECORDING_AUDIO_INPUT);
        if let (Some(audio_in), Some(caps)) = (&audio_in, &audio_caps) {
            audio_in.set_caps(Some(caps));
        }
        let units = self.count_at_tap(&tap);
        warn_when_the_writer_falls_behind(&pipeline, &self.cam_id);
        self.segment_opened = false;
        let feed = Feed {
            timing,
            video: (video_in, first.caps.clone()),
            audio: audio_in.zip(audio_caps),
        };
        let mut queued = vec![(first, false)];
        queued.extend(pending);
        let end = self.pump(&pipeline, &tap, &units, subscription, &feed, queued, seen);
        if with_audio && !self.segment_opened && matches!(end, RunEnd::Failed(_)) {
            self.audio_failed = true;
            tracing::warn!(cam_id = %self.cam_id, "Recording with audio did not start; recording video only");
        }
        let _ = pipeline.set_state(gstreamer::State::Null);
        let _ = pipeline.state(Some(gstreamer::ClockTime::from_seconds(5)));
        end
    }

    #[allow(clippy::too_many_arguments)]
    fn pump(
        &mut self,
        pipeline: &gstreamer::Pipeline,
        tap: &gstreamer::Element,
        units: &AtomicU64,
        subscription: &Subscription,
        feed: &Feed,
        queued: Vec<(SharedUnit, bool)>,
        seen: &mut u64,
    ) -> RunEnd {
        let delivered = || units.load(Ordering::Relaxed) > 0;
        if pipeline.set_state(gstreamer::State::Playing).is_err() {
            return RunEnd::Failed(Attempt::Failed {
                reason: "recording pipeline would not start".into(),
                delivered: false,
            });
        }
        let Some(bus) = pipeline.bus() else {
            return RunEnd::Failed(Attempt::Failed {
                reason: "recording pipeline has no bus".into(),
                delivered: false,
            });
        };
        for (unit, after_gap) in queued {
            if let Some(end) = feed.push(&unit, after_gap) {
                finalize(pipeline, &self.cam_id);
                return end;
            }
        }
        let mut segment_open = false;
        let mut info_complete = false;
        let mut last_units = 0;
        loop {
            if self.stopped() {
                finalize(pipeline, &self.cam_id);
                return RunEnd::Stopped;
            }
            self.follow_hub(subscription, seen);
            let count = units.load(Ordering::Relaxed);
            if count != last_units {
                if last_units == 0 {
                    self.failures = 0;
                    let status = if segment_open {
                        "recording_ready"
                    } else {
                        "streaming"
                    };
                    self.publish_state(status);
                    self.notify_state(status, None);
                }
                last_units = count;
            }
            if !info_complete {
                if let Some(mut info) = tap_stream_info(tap) {
                    info_complete = info.resolution.is_some();
                    info.audio = self.audio.clone().flatten();
                    self.publish_stream_info(info);
                }
            }
            while let Some(message) = bus.pop() {
                match message.view() {
                    gstreamer::MessageView::Error(error) => {
                        return RunEnd::Failed(Attempt::Failed {
                            reason: format!(
                                "{}: {}",
                                error.error(),
                                error.debug().unwrap_or_default()
                            ),
                            delivered: delivered(),
                        });
                    }
                    gstreamer::MessageView::Eos(_) => {
                        return RunEnd::Failed(Attempt::Failed {
                            reason: "Recording ended (EOS)".into(),
                            delivered: delivered(),
                        });
                    }
                    gstreamer::MessageView::Element(element)
                        if !segment_open
                            && element
                                .structure()
                                .is_some_and(|s| s.name() == "splitmuxsink-fragment-opened") =>
                    {
                        segment_open = true;
                        self.segment_opened = true;
                        if last_units > 0 {
                            self.publish_state("recording_ready");
                            self.notify_state("recording_ready", None);
                        }
                        tracing::info!(cam_id = %self.cam_id, "Recording segment opened");
                    }
                    _ => {}
                }
            }
            // The writer is behind: leave units in the subscription, whose
            // own bound decides what is lost; never block the camera.
            if feed.video.0.current_level_bytes() > INPUT_LIMIT_BYTES {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            match subscription.recv(POLL_SLICE) {
                Some(Delivery::Unit { unit, after_gap }) => {
                    if let Some(end) = feed.push(&unit, after_gap) {
                        finalize(pipeline, &self.cam_id);
                        return end;
                    }
                }
                Some(Delivery::Discontinuity { .. }) => {
                    finalize(pipeline, &self.cam_id);
                    return RunEnd::Discontinuity;
                }
                None if subscription.state().phase == Phase::Closed => {
                    finalize(pipeline, &self.cam_id);
                    return RunEnd::Stopped;
                }
                None => {}
            }
        }
    }
}

/// Where a run's units go.
struct Feed {
    timing: Timing,
    video: (gstreamer_app::AppSrc, gstreamer::Caps),
    audio: Option<(gstreamer_app::AppSrc, gstreamer::Caps)>,
}

impl Feed {
    /// Push `unit` into the recording; `Some` when the run must end here.
    fn push(&self, unit: &SharedUnit, after_gap: bool) -> Option<RunEnd> {
        let (input, caps) = match unit.media {
            Media::Video => (&self.video.0, &self.video.1),
            Media::Audio => match &self.audio {
                Some((input, caps)) => (input, caps),
                None => return None,
            },
        };
        if unit.caps != *caps {
            return Some(RunEnd::FormatChanged(unit.clone()));
        }
        let buffer = self.timing.rebase(unit, after_gap)?;
        match input.push_buffer(buffer) {
            Ok(_) => None,
            // Flushing: the pipeline is stopping.
            Err(gstreamer::FlowError::Flushing) => None,
            Err(error) => Some(RunEnd::Failed(Attempt::Failed {
                reason: format!("the recording did not take the camera's video: {error:?}"),
                delivered: true,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(dts_ms: Option<u64>, pts_ms: u64, arrived_ms: i64) -> SharedUnit {
        gstreamer::init().unwrap();
        let mut buffer = gstreamer::Buffer::from_slice(vec![0u8; 8]);
        {
            let b = buffer.get_mut().unwrap();
            b.set_pts(gstreamer::ClockTime::from_mseconds(pts_ms));
            if let Some(dts) = dts_ms {
                b.set_dts(gstreamer::ClockTime::from_mseconds(dts));
            }
        }
        SharedUnit {
            media: Media::Video,
            buffer,
            caps: gstreamer::Caps::new_empty_simple("video/x-h264"),
            arrived_us: arrived_ms * 1000,
        }
    }

    #[test]
    fn a_run_starts_at_its_first_keyframe_and_keeps_wall_time() {
        // A B-frame stream: the keyframe decodes at 5.000 s and shows at 5.080 s.
        let first = unit(Some(5_000), 5_080, 1_700_000_000_000);
        let (timing, epoch_ms) = Timing::starting_at(&first);
        let rebased = timing.rebase(&first, false).unwrap();
        assert_eq!(rebased.dts(), Some(gstreamer::ClockTime::ZERO));
        assert_eq!(rebased.pts(), Some(gstreamer::ClockTime::from_mseconds(80)));
        // epoch + the segment's first timestamp = when the picture arrived.
        assert_eq!(epoch_ms + 80, 1_700_000_000_000);
        // The shared buffer itself is untouched (other subscribers see it).
        assert_eq!(
            first.buffer.pts(),
            Some(gstreamer::ClockTime::from_mseconds(5_080))
        );
        let later = timing.rebase(&unit(Some(5_040), 5_200, 0), true).unwrap();
        assert_eq!(later.pts(), Some(gstreamer::ClockTime::from_mseconds(200)));
        assert!(later.flags().contains(gstreamer::BufferFlags::DISCONT));
        assert!(
            timing.rebase(&unit(None, 4_990, 0), false).is_none(),
            "sound from before the run's first picture is left out"
        );
    }
}
