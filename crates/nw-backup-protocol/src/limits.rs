//! A plan's limits on a camera's video, and measuring a stream against them.
//!
//! Compressed video cannot be thinned to fewer pictures without re-encoding,
//! so a stream either fits its plan or is refused with the reason (the
//! recorder then suggests the camera's sub stream). The ingest enforces this
//! (it sees every byte); the recorder runs the same check first so it can
//! say why before sending anything.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Measured frame rates may exceed the limit by this factor (camera clocks
/// and arrival jitter).
pub const FPS_TOLERANCE: f64 = 1.05;
/// Measured bitrate may exceed the limit by this factor over
/// [`BITRATE_WINDOW_US`] (variable-bitrate cameras peak on busy scenes).
pub const BITRATE_TOLERANCE: f64 = 1.25;
/// Frame rate is measured over this much of the stream.
pub const FPS_WINDOW_US: i64 = 5_000_000;
/// Bitrate is measured over this much of the stream.
pub const BITRATE_WINDOW_US: i64 = 60_000_000;
/// Measurements need at least this much of the stream.
pub const MIN_FPS_SPAN_US: i64 = 2_000_000;
pub const MIN_BITRATE_SPAN_US: i64 = 10_000_000;
/// Most units a meter keeps (60 s at 120 fps fits).
const MAX_SAMPLES: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// The larger of width and height (a portrait camera counts the same).
    pub max_long_side: u32,
    /// The smaller of width and height.
    pub max_short_side: u32,
    pub max_fps: f64,
    /// Video and sound together, bits per second.
    pub max_bitrate_bps: u64,
}

impl Limits {
    /// About 1080p at 10 fps (a plan's description, not a promise of quality).
    pub const P1080_10: Limits = Limits {
        max_long_side: 1920,
        max_short_side: 1080,
        max_fps: 10.0,
        max_bitrate_bps: 2_000_000,
    };

    pub fn check_size(&self, width: u32, height: u32) -> Result<(), Breach> {
        let (long, short) = (width.max(height), width.min(height));
        if long > self.max_long_side || short > self.max_short_side {
            return Err(Breach::Resolution { width, height });
        }
        Ok(())
    }

    pub fn check_fps(&self, fps: f64) -> Result<(), Breach> {
        if fps > self.max_fps * FPS_TOLERANCE {
            return Err(Breach::FrameRate { fps });
        }
        Ok(())
    }

    pub fn check_bitrate(&self, bps: u64) -> Result<(), Breach> {
        if bps as f64 > self.max_bitrate_bps as f64 * BITRATE_TOLERANCE {
            return Err(Breach::Bitrate { bps });
        }
        Ok(())
    }

    /// Plain words for a breach, naming this plan's limit.
    pub fn explain(&self, breach: &Breach) -> String {
        match *breach {
            Breach::Resolution { width, height } => format!(
                "This stream is {width}×{height}; the plan allows up to {}×{}.",
                self.max_long_side, self.max_short_side
            ),
            Breach::FrameRate { fps } => format!(
                "This stream sends {:.1} pictures a second; the plan allows {}.",
                fps,
                trim(self.max_fps)
            ),
            Breach::Bitrate { bps } => format!(
                "This stream sends {:.1} Mbit/s; the plan allows {:.1}.",
                bps as f64 / 1e6,
                self.max_bitrate_bps as f64 / 1e6
            ),
        }
    }
}

fn trim(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// How a stream exceeds its plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Breach {
    Resolution { width: u32, height: u32 },
    FrameRate { fps: f64 },
    Bitrate { bps: u64 },
}

/// Frame rate and bitrate of one run of a track, from its units' times.
///
/// Feed units of one run in order (a live run and a backlog each need their
/// own meter: their times interleave). A discontinuity starts again.
#[derive(Debug, Clone, Default)]
pub struct Meter {
    /// (time_us, bytes, counts as a picture).
    samples: VecDeque<(i64, u64, bool)>,
    bytes: u64,
}

impl Meter {
    pub fn new() -> Self {
        Self::default()
    }

    /// One unit: `picture` for video units (sound adds bytes only).
    pub fn add(&mut self, time_us: i64, bytes: usize, picture: bool) {
        if self
            .samples
            .back()
            .is_some_and(|&(last, _, _)| time_us < last)
        {
            // Time went backwards: a new run as far as rates go.
            self.reset();
        }
        self.samples.push_back((time_us, bytes as u64, picture));
        self.bytes += bytes as u64;
        while self.samples.len() > MAX_SAMPLES
            || self
                .samples
                .front()
                .is_some_and(|&(first, _, _)| time_us - first > BITRATE_WINDOW_US)
        {
            if let Some((_, bytes, _)) = self.samples.pop_front() {
                self.bytes -= bytes;
            }
        }
    }

    pub fn reset(&mut self) {
        self.samples.clear();
        self.bytes = 0;
    }

    /// Pictures a second over the last [`FPS_WINDOW_US`], once
    /// [`MIN_FPS_SPAN_US`] of pictures was seen.
    pub fn fps(&self) -> Option<f64> {
        let last = self.samples.iter().rev().find(|s| s.2)?.0;
        let mut first = last;
        let mut count = 0u32;
        for &(time, _, picture) in self.samples.iter().rev() {
            if last - time > FPS_WINDOW_US {
                break;
            }
            if picture {
                first = time;
                count += 1;
            }
        }
        let span = last - first;
        (span >= MIN_FPS_SPAN_US && count > 1).then(|| f64::from(count - 1) * 1e6 / span as f64)
    }

    /// Bits a second over the last [`BITRATE_WINDOW_US`], once
    /// [`MIN_BITRATE_SPAN_US`] was seen. The newest unit's bytes are left out:
    /// they arrived at the end of the span.
    pub fn bitrate_bps(&self) -> Option<u64> {
        let (first, _, _) = *self.samples.front()?;
        let (last, newest, _) = *self.samples.back()?;
        let span = last - first;
        (span >= MIN_BITRATE_SPAN_US)
            .then(|| ((self.bytes - newest) as f64 * 8e6 / span as f64) as u64)
    }

    /// The first limit the measurements break so far.
    pub fn check(&self, limits: &Limits) -> Result<(), Breach> {
        if let Some(fps) = self.fps() {
            limits.check_fps(fps)?;
        }
        if let Some(bps) = self.bitrate_bps() {
            limits.check_bitrate(bps)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `seconds` of video at `fps`, `bytes` a picture, from `start_us`.
    fn feed(meter: &mut Meter, start_us: i64, fps: f64, seconds: f64, bytes: usize) {
        let pictures = (fps * seconds) as i64;
        for i in 0..pictures {
            meter.add(start_us + (i as f64 * 1e6 / fps) as i64, bytes, true);
        }
    }

    #[test]
    fn sizes_are_checked_either_way_round() {
        let limits = Limits::P1080_10;
        assert!(limits.check_size(1920, 1080).is_ok());
        assert!(limits.check_size(1080, 1920).is_ok(), "portrait");
        assert!(limits.check_size(1280, 720).is_ok());
        assert_eq!(
            limits.check_size(2560, 1440),
            Err(Breach::Resolution {
                width: 2560,
                height: 1440
            })
        );
        assert!(limits.check_size(1920, 1200).is_err(), "taller than 1080p");
        assert_eq!(
            limits.explain(&Breach::Resolution {
                width: 2560,
                height: 1440
            }),
            "This stream is 2560×1440; the plan allows up to 1920×1080."
        );
    }

    #[test]
    fn frame_rate_is_measured_and_checked() {
        let mut meter = Meter::new();
        feed(&mut meter, 0, 10.0, 1.0, 1000);
        assert_eq!(meter.fps(), None, "too short to tell");
        feed(&mut meter, 1_000_000, 10.0, 9.0, 1000);
        let fps = meter.fps().unwrap();
        assert!((fps - 10.0).abs() < 0.01, "{fps}");
        assert!(meter.check(&Limits::P1080_10).is_ok());

        let mut fast = Meter::new();
        feed(&mut fast, 0, 25.0, 6.0, 1000);
        let Err(Breach::FrameRate { fps }) = fast.check(&Limits::P1080_10) else {
            panic!("25 fps passed a 10 fps plan");
        };
        assert!((fps - 25.0).abs() < 0.1);
        assert_eq!(
            Limits::P1080_10.explain(&Breach::FrameRate { fps: 25.0 }),
            "This stream sends 25.0 pictures a second; the plan allows 10."
        );

        // Within the tolerance: 10.4 fps on a 10 fps plan.
        let mut close = Meter::new();
        feed(&mut close, 0, 10.4, 6.0, 1000);
        assert!(close.check(&Limits::P1080_10).is_ok());
    }

    #[test]
    fn bitrate_is_measured_over_the_window() {
        // 10 fps × 25 kB = 2 Mbit/s.
        let mut meter = Meter::new();
        feed(&mut meter, 0, 10.0, 5.0, 25_000);
        assert_eq!(meter.bitrate_bps(), None);
        feed(&mut meter, 5_000_000, 10.0, 115.0, 25_000);
        let bps = meter.bitrate_bps().unwrap();
        assert!((1_990_000..=2_010_000).contains(&bps), "{bps}");
        assert!(meter.samples.len() <= 602, "only the window is kept");
        assert!(
            meter.check(&Limits::P1080_10).is_ok(),
            "2 Mbit/s on a 2 Mbit/s plan"
        );

        let mut heavy = Meter::new();
        feed(&mut heavy, 0, 10.0, 20.0, 40_000);
        assert!(matches!(
            heavy.check(&Limits::P1080_10),
            Err(Breach::Bitrate { .. })
        ));
        // Sound adds bytes, not pictures.
        let mut sound = Meter::new();
        for i in 0..1000 {
            sound.add(i * 20_000, 160, false);
        }
        assert_eq!(sound.fps(), None);
        assert_eq!(sound.bitrate_bps(), Some(64_000));
    }

    #[test]
    fn time_going_backwards_starts_again() {
        let mut meter = Meter::new();
        feed(&mut meter, 100_000_000, 25.0, 6.0, 1000);
        assert!(meter.fps().is_some());
        meter.add(0, 1000, true);
        assert_eq!(meter.fps(), None);
        assert_eq!(meter.samples.len(), 1);
    }

    #[test]
    fn limits_travel_as_json() {
        let json = serde_json::to_value(Limits::P1080_10).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"max_long_side": 1920, "max_short_side": 1080,
                               "max_fps": 10.0, "max_bitrate_bps": 2000000})
        );
    }
}
