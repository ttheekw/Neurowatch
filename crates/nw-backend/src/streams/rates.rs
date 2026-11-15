//! Compressed payload rates, shared by every diagnostics reader.
use std::time::{Duration, Instant};
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateSample {
    pub video_bps: u64,
    pub audio_bps: u64,
    pub fps: f64,
}
pub struct Rates {
    start: Instant,
    window: Instant,
    last: Option<Instant>,
    video: u64,
    audio: u64,
    frames: u64,
    previous: Option<RateSample>,
}
impl Rates {
    pub fn new(now: Instant) -> Self {
        Self {
            start: now,
            window: now,
            last: None,
            video: 0,
            audio: 0,
            frames: 0,
            previous: None,
        }
    }
    fn sample(&self, seconds: f64) -> RateSample {
        RateSample {
            video_bps: (self.video as f64 * 8.0 / seconds) as u64,
            audio_bps: (self.audio as f64 * 8.0 / seconds) as u64,
            fps: self.frames as f64 / seconds,
        }
    }
    pub fn record(&mut self, video: bool, bytes: usize, now: Instant) {
        let elapsed = now.saturating_duration_since(self.window);
        if elapsed >= Duration::from_secs(1) {
            self.previous = Some(self.sample(elapsed.as_secs_f64()));
            self.window = now;
            self.video = 0;
            self.audio = 0;
            self.frames = 0;
        }
        self.last = Some(now);
        if video {
            self.video = self.video.saturating_add(bytes as u64);
            self.frames = self.frames.saturating_add(1);
        } else {
            self.audio = self.audio.saturating_add(bytes as u64);
        }
    }
    pub fn snapshot(&self, now: Instant) -> Option<RateSample> {
        if now.saturating_duration_since(self.start) < Duration::from_secs(1) {
            return None;
        }
        if self.last.map_or(true, |last| {
            now.saturating_duration_since(last) > Duration::from_secs(1)
        }) {
            return Some(RateSample {
                video_bps: 0,
                audio_bps: 0,
                fps: 0.0,
            });
        }
        let elapsed = now.saturating_duration_since(self.window);
        if elapsed >= Duration::from_secs(1) {
            Some(self.sample(elapsed.as_secs_f64()))
        } else {
            self.previous
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn rates_need_a_measured_window_and_readers_do_not_reset_them() {
        let start = Instant::now();
        let mut meter = Rates::new(start);
        assert_eq!(meter.snapshot(start), None);
        meter.record(true, 1000, start);
        meter.record(false, 250, start + Duration::from_millis(500));
        let measured = meter.snapshot(start + Duration::from_secs(1)).unwrap();
        assert_eq!(measured.video_bps, 8000);
        assert_eq!(measured.audio_bps, 2000);
        assert_eq!(measured.fps, 1.0);
        assert_eq!(
            meter.snapshot(start + Duration::from_secs(1)),
            Some(measured)
        );
        let idle = meter.snapshot(start + Duration::from_secs(3)).unwrap();
        assert_eq!(idle.video_bps, 0);
        assert_eq!(idle.audio_bps, 0);
    }
    #[test]
    fn only_the_recent_window_counts_after_a_pause() {
        let start = Instant::now();
        let mut meter = Rates::new(start);
        meter.record(true, 1000, start);
        meter.record(true, 2000, start + Duration::from_secs(5));
        meter.record(true, 2000, start + Duration::from_millis(5500));
        let measured = meter.snapshot(start + Duration::from_secs(6)).unwrap();
        assert_eq!(measured.video_bps, 32000);
        assert_eq!(measured.fps, 2.0);
    }
}
