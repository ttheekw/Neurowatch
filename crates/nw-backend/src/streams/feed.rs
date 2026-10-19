//! Feeding a decoder from a hub: video units pushed into the `appsrc` at the
//! head of a decode pipeline (a desktop tile's, a native overlay's).
//!
//! The feed rebases the hub's timestamps to the pipeline (its first unit at
//! 0) and keeps them moving forward across a reconnect, so the decoder never
//! sees time go back. Units after a gap or a reconnect carry `DISCONT`. The
//! caller decides what to do when the stream's format changes (a desktop
//! view rebuilds its pipeline).

use super::SharedUnit;

/// Spacing assumed between the last unit before a reconnect and the first
/// after it.
const RESTART_GAP_NS: u64 = 40_000_000;

/// What happened to a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    Pushed,
    /// Before the feed's timeline (sound older than the first picture).
    Skipped,
    /// The unit has other caps than the feed's: not pushed.
    FormatChanged,
}

pub struct VideoFeed {
    input: gstreamer_app::AppSrc,
    caps: Option<gstreamer::Caps>,
    /// `(hub time of the timeline's first unit, pipeline time it maps to)`.
    origin: Option<(u64, u64)>,
    /// Latest pipeline time pushed.
    last_ns: u64,
    discont: bool,
}

impl VideoFeed {
    pub fn new(input: gstreamer_app::AppSrc) -> Self {
        Self {
            input,
            caps: None,
            origin: None,
            last_ns: 0,
            discont: false,
        }
    }

    /// Bytes waiting in the `appsrc` (the decoder is behind when this grows).
    pub fn backlog_bytes(&self) -> u64 {
        self.input.current_level_bytes()
    }

    /// The connection was rebuilt: what comes next starts a new timeline,
    /// continuing after what was pushed.
    pub fn restart(&mut self) {
        self.origin = None;
        self.discont = true;
    }

    /// Push `unit`. The first unit sets the feed's caps; a unit with other
    /// caps is not pushed ([`Pushed::FormatChanged`]) unless `follow_caps`,
    /// which hands the new caps to the pipeline and goes on.
    pub fn push(
        &mut self,
        unit: &SharedUnit,
        after_gap: bool,
        follow_caps: bool,
    ) -> Result<Pushed, gstreamer::FlowError> {
        match &self.caps {
            None => {
                self.input.set_caps(Some(&unit.caps));
                self.caps = Some(unit.caps.clone());
            }
            Some(caps) if *caps != unit.caps => {
                if !follow_caps {
                    return Ok(Pushed::FormatChanged);
                }
                self.input.set_caps(Some(&unit.caps));
                self.caps = Some(unit.caps.clone());
                self.discont = true;
            }
            Some(_) => {}
        }
        let buffer = &unit.buffer;
        let dts = buffer.dts().map(gstreamer::ClockTime::nseconds);
        let pts = buffer.pts().map(gstreamer::ClockTime::nseconds);
        let Some(time) = dts.or(pts) else {
            return Ok(Pushed::Skipped);
        };
        let (hub_origin, pipeline_origin) = *self.origin.get_or_insert_with(|| {
            let start = if self.last_ns == 0 && !self.discont {
                0
            } else {
                self.last_ns + RESTART_GAP_NS
            };
            (time, start)
        });
        if pts.unwrap_or(time) < hub_origin {
            return Ok(Pushed::Skipped);
        }
        let map = |t: u64| gstreamer::ClockTime::from_nseconds(t - hub_origin + pipeline_origin);
        // A shallow copy: new timestamps and flags, the same memory.
        let mut out = buffer.copy();
        if let Some(writable) = out.get_mut() {
            writable.set_pts(pts.filter(|p| *p >= hub_origin).map(map));
            writable.set_dts(dts.filter(|d| *d >= hub_origin).map(map));
            if after_gap || std::mem::take(&mut self.discont) {
                writable.set_flags(gstreamer::BufferFlags::DISCONT);
            }
        }
        self.last_ns = self.last_ns.max(time - hub_origin + pipeline_origin);
        self.input.push_buffer(out)?;
        Ok(Pushed::Pushed)
    }
}

#[cfg(test)]
mod tests {
    use gstreamer::prelude::*;

    use super::*;
    use crate::streams::Media;

    fn unit(ms: u64, caps: &gstreamer::Caps) -> SharedUnit {
        let mut buffer = gstreamer::Buffer::from_slice(vec![0u8; 4]);
        {
            let b = buffer.get_mut().unwrap();
            b.set_pts(gstreamer::ClockTime::from_mseconds(ms));
            b.set_dts(gstreamer::ClockTime::from_mseconds(ms));
        }
        SharedUnit {
            media: Media::Video,
            buffer,
            caps: caps.clone(),
            arrived_us: 0,
        }
    }

    #[test]
    fn timestamps_start_at_zero_and_keep_going_forward_after_a_reconnect() {
        gstreamer::init().unwrap();
        let caps = gstreamer::Caps::builder("video/x-h264").build();
        let input = gstreamer_app::AppSrc::builder()
            .format(gstreamer::Format::Time)
            .build();
        let sink = gstreamer_app::AppSink::builder().sync(false).build();
        let pipeline = gstreamer::Pipeline::new();
        pipeline
            .add_many([
                input.upcast_ref::<gstreamer::Element>(),
                sink.upcast_ref::<gstreamer::Element>(),
            ])
            .unwrap();
        input.link(&sink).unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let mut feed = VideoFeed::new(input);
        let times = |sink: &gstreamer_app::AppSink, n: usize| -> Vec<(u64, bool)> {
            (0..n)
                .map(|_| {
                    let sample = sink.pull_sample().unwrap();
                    let buffer = sample.buffer().unwrap();
                    (
                        buffer.pts().unwrap().mseconds(),
                        buffer.flags().contains(gstreamer::BufferFlags::DISCONT),
                    )
                })
                .collect()
        };
        for ms in [5_000, 5_040, 5_080] {
            assert_eq!(
                feed.push(&unit(ms, &caps), false, false),
                Ok(Pushed::Pushed)
            );
        }
        // (GStreamer marks a stream's first buffer DISCONT itself.)
        assert_eq!(times(&sink, 3), vec![(0, true), (40, false), (80, false)]);
        // The camera reconnected: its clock starts again near 0.
        feed.restart();
        feed.push(&unit(100, &caps), false, false).unwrap();
        feed.push(&unit(140, &caps), true, false).unwrap();
        assert_eq!(times(&sink, 2), vec![(120, true), (160, true)]);
        let other = gstreamer::Caps::builder("video/x-h265").build();
        assert_eq!(
            feed.push(&unit(180, &other), false, false),
            Ok(Pushed::FormatChanged)
        );
        pipeline.set_state(gstreamer::State::Null).unwrap();
    }
}
