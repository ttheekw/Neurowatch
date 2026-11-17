//! Compressed payload rates, shared by every diagnostics reader.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// One-second windows the average bitrate covers (the stream's last minute).
const AVERAGE_WINDOWS: usize = 60;
/// Seconds measured before an average is given: several GOPs, so one
/// keyframe's second does not decide it.
const AVERAGE_MIN_SECONDS: f64 = 10.0;
/// A window longer than this spans a pause in the stream, not its rate.
const AVERAGE_MAX_WINDOW: Duration = Duration::from_secs(2);
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
    /// Completed windows' (video bytes, seconds), for [`Rates::average_video_bps`].
    history: VecDeque<(u64, f64)>,
    history_bytes: u64,
    history_seconds: f64,
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
            history: VecDeque::with_capacity(AVERAGE_WINDOWS),
            history_bytes: 0,
            history_seconds: 0.0,
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
            if elapsed <= AVERAGE_MAX_WINDOW {
                self.remember(self.video, elapsed.as_secs_f64());
            }
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
    fn remember(&mut self, bytes: u64, seconds: f64) {
        if self.history.len() == AVERAGE_WINDOWS {
            if let Some((old_bytes, old_seconds)) = self.history.pop_front() {
                self.history_bytes = self.history_bytes.saturating_sub(old_bytes);
                self.history_seconds -= old_seconds;
            }
        }
        self.history.push_back((bytes, seconds));
        self.history_bytes = self.history_bytes.saturating_add(bytes);
        self.history_seconds += seconds;
    }

    /// Compressed video bits a second over the stream's last minute of
    /// streaming (pauses left out), once at least 10 s were measured: what
    /// the stream sends over time, for sizing storage (Cloud Backup prices).
    pub fn average_video_bps(&self) -> Option<u64> {
        (self.history_seconds >= AVERAGE_MIN_SECONDS)
            .then(|| (self.history_bytes as f64 * 8.0 / self.history_seconds) as u64)
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
    fn the_average_covers_the_last_minute_and_evens_out_keyframes() {
        let start = Instant::now();
        let mut meter = Rates::new(start);
        let mut at = start;
        // A GOP of 2 s: a 100 kB keyframe second, then a 20 kB second.
        for second in 0..9u64 {
            at = start + Duration::from_secs(second);
            meter.record(true, if second % 2 == 0 { 100_000 } else { 20_000 }, at);
        }
        assert_eq!(meter.average_video_bps(), None, "under 10 s measured");
        for second in 9..200u64 {
            at = start + Duration::from_secs(second);
            meter.record(true, if second % 2 == 0 { 100_000 } else { 20_000 }, at);
        }
        assert_eq!(
            meter.average_video_bps(),
            Some(480_000),
            "60 kB a second on average"
        );
        // A pause is not counted as a slow stream.
        meter.record(true, 100_000, at + Duration::from_secs(30));
        assert_eq!(meter.average_video_bps(), Some(480_000));
        // A stream that changes rate: the old minute leaves the average.
        let later = at + Duration::from_secs(40);
        for second in 0..61u64 {
            meter.record(true, 10_000, later + Duration::from_secs(second));
        }
        assert_eq!(meter.average_video_bps(), Some(80_000));
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
