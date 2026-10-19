//! Fixed-size, deterministic motion analysis. No media, clock, IPC or database I/O.
use nw_core::camera::{CameraMotionConfig, MotionZone, ZoneKind};

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 180;
const W: usize = WIDTH / 2;
const H: usize = HEIGHT / 2;
const BLOCK: usize = 10;
pub const COLUMNS: usize = W / BLOCK;
pub const ROWS: usize = H / BLOCK;
const BLOCKS: usize = COLUMNS * ROWS;

#[derive(Debug, Clone, Default)]
pub struct Sample {
    pub active: bool,
    pub score: f32,
    pub bbox: Option<[f32; 4]>,
    pub zones: Vec<String>,
    pub blocks: Vec<u16>,
}

pub struct Detector {
    background: Vec<u16>,
    mask: Vec<bool>,
    zones: Vec<(String, Vec<bool>)>,
    threshold: u8,
    min_area: f32,
    duration_ms: i64,
    cooldown_ms: i64,
    previous_ms: Option<i64>,
    candidate_ms: Option<i64>,
    last_motion_ms: i64,
    active: bool,
}

impl Detector {
    pub fn new(settings: &CameraMotionConfig) -> Self {
        let zones: Vec<_> = settings
            .zones
            .iter()
            .filter(|zone| zone.is_valid())
            .collect();
        let includes = zones.iter().any(|zone| zone.kind == ZoneKind::Include);
        let raster: Vec<_> = zones
            .iter()
            .map(|zone| {
                let pixels: Vec<_> = (0..W * H)
                    .map(|i| {
                        contains(
                            zone,
                            (i % W) as f32 / W as f32 + 0.5 / W as f32,
                            (i / W) as f32 / H as f32 + 0.5 / H as f32,
                        )
                    })
                    .collect();
                (*zone, pixels)
            })
            .collect();
        let mask = (0..W * H)
            .map(|i| {
                (!includes
                    || raster
                        .iter()
                        .any(|(zone, pixels)| zone.kind == ZoneKind::Include && pixels[i]))
                    && !raster
                        .iter()
                        .any(|(zone, pixels)| zone.kind == ZoneKind::Exclude && pixels[i])
            })
            .collect();
        let sensitivity = settings.sensitivity.clamp(1, 10);
        Self {
            background: Vec::new(),
            mask,
            zones: raster
                .into_iter()
                .filter(|(zone, _)| zone.kind == ZoneKind::Include)
                .map(|(zone, pixels)| (zone.id.clone(), pixels))
                .collect(),
            threshold: 28 - sensitivity * 2,
            min_area: 0.10 - f32::from(sensitivity) * 0.008,
            duration_ms: seconds_ms(settings.min_duration_sec),
            cooldown_ms: seconds_ms(settings.cooldown_sec),
            previous_ms: None,
            candidate_ms: None,
            last_motion_ms: 0,
            active: false,
        }
    }

    pub fn observe(&mut self, frame: &[u8], now_ms: i64) -> Result<Sample, &'static str> {
        if frame.len() != WIDTH * HEIGHT {
            return Err("expected 320 by 180 GRAY8 frame");
        }
        if self.previous_ms.is_some_and(|previous| now_ms < previous) {
            return Err("frame clock moved backwards");
        }
        self.previous_ms = Some(now_ms);
        // Box smoothing/downscale suppresses isolated sensor noise.
        let pixels: Vec<u8> = (0..W * H)
            .map(|i| {
                let at = (i / W * 2) * WIDTH + i % W * 2;
                ((u32::from(frame[at])
                    + u32::from(frame[at + 1])
                    + u32::from(frame[at + WIDTH])
                    + u32::from(frame[at + WIDTH + 1]))
                    / 4) as u8
            })
            .collect();
        if self.background.is_empty() {
            self.background = pixels.iter().map(|v| u16::from(*v) * 256).collect();
            return Ok(Sample::default());
        }
        let changed: Vec<bool> = pixels
            .iter()
            .zip(&self.background)
            .map(|(pixel, background)| pixel.abs_diff((background / 256) as u8) > self.threshold)
            .collect();
        // Detect lighting changes over the whole image, not just a tiny include
        // zone: otherwise a person filling that zone would always be suppressed.
        if changed.iter().filter(|v| **v).count() * 10 > pixels.len() * 6 {
            self.background = pixels.iter().map(|v| u16::from(*v) * 256).collect();
            self.candidate_ms = None;
            self.active = false;
            return Ok(Sample::default());
        }
        let mut area = [0u32; BLOCKS];
        let mut movement = [0u32; BLOCKS];
        for i in 0..W * H {
            if self.mask[i] {
                let block = (i / W / BLOCK) * COLUMNS + (i % W / BLOCK);
                area[block] += 1;
                movement[block] += u32::from(changed[i]);
            }
            let target = i32::from(pixels[i]) * 256;
            let current = i32::from(self.background[i]);
            self.background[i] =
                (current + (target - current) / if changed[i] { 256 } else { 32 }) as u16;
        }
        let watched = area.iter().filter(|a| **a > 0).count();
        let blocks: Vec<usize> = (0..BLOCKS)
            .filter(|i| area[*i] > 0 && movement[*i] >= 2 && movement[*i] * 5 >= area[*i])
            .collect();
        let mut active_blocks = [false; BLOCKS];
        for block in &blocks {
            active_blocks[*block] = true;
        }
        let score = if watched == 0 {
            0.0
        } else {
            blocks.len() as f32 / watched as f32
        };
        if score >= self.min_area && !blocks.is_empty() {
            let candidate = *self.candidate_ms.get_or_insert(now_ms);
            self.last_motion_ms = now_ms;
            if now_ms - candidate >= self.duration_ms {
                self.active = true;
            }
        } else {
            self.candidate_ms = None;
            if self.active && now_ms - self.last_motion_ms >= self.cooldown_ms {
                self.active = false;
            }
        }
        let bbox = if blocks.is_empty() {
            None
        } else {
            let x1 = blocks.iter().map(|i| i % COLUMNS).min().unwrap_or(0);
            let x2 = blocks.iter().map(|i| i % COLUMNS + 1).max().unwrap_or(0);
            let y1 = blocks.iter().map(|i| i / COLUMNS).min().unwrap_or(0);
            let y2 = blocks.iter().map(|i| i / COLUMNS + 1).max().unwrap_or(0);
            Some([
                x1 as f32 * BLOCK as f32 / W as f32,
                y1 as f32 * BLOCK as f32 / H as f32,
                (x2 - x1) as f32 * BLOCK as f32 / W as f32,
                (y2 - y1) as f32 * BLOCK as f32 / H as f32,
            ])
        };
        let zones = self
            .zones
            .iter()
            .filter(|(_, zone)| {
                (0..W * H).any(|i| {
                    zone[i]
                        && self.mask[i]
                        && changed[i]
                        && active_blocks[(i / W / BLOCK) * COLUMNS + (i % W / BLOCK)]
                })
            })
            .map(|(id, _)| id.clone())
            .collect();
        Ok(Sample {
            active: self.active,
            score,
            bbox,
            zones,
            blocks: blocks.into_iter().map(|i| i as u16).collect(),
        })
    }
}

fn seconds_ms(seconds: f32) -> i64 {
    if seconds.is_finite() {
        (seconds.clamp(0.0, 3600.0) * 1000.0).round() as i64
    } else {
        0
    }
}

fn contains(zone: &MotionZone, x: f32, y: f32) -> bool {
    let mut inside = false;
    let mut previous = zone.points.len() - 1;
    for (i, [ax, ay]) in zone.points.iter().copied().enumerate() {
        let [bx, by] = zone.points[previous];
        if (ay > y) != (by > y) && x < (bx - ax) * (y - ay) / (by - ay) + ax {
            inside = !inside;
        }
        previous = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_core::camera::{CameraMotionConfig, MotionZone, ZoneKind};

    fn config() -> CameraMotionConfig {
        CameraMotionConfig {
            min_duration_sec: 0.4,
            cooldown_sec: 1.0,
            ..Default::default()
        }
    }
    fn frame(moving: bool) -> Vec<u8> {
        let mut frame = vec![30; WIDTH * HEIGHT];
        if moving {
            for y in 40..100 {
                for x in 30..110 {
                    frame[y * WIDTH + x] = 180;
                }
            }
        }
        frame
    }
    fn zone(kind: ZoneKind, x: f32) -> MotionZone {
        MotionZone {
            id: "entrance".into(),
            name: "Entrance".into(),
            kind,
            points: vec![[0., 0.], [x, 0.], [x, 1.], [0., 1.]],
        }
    }
    #[test]
    fn still_scene_never_starts_and_lighting_change_reinitializes() {
        let mut detector = Detector::new(&config());
        for at in [0, 200, 400, 600] {
            assert!(!detector.observe(&frame(false), at).unwrap().active);
        }
        assert!(
            !detector
                .observe(&vec![180; WIDTH * HEIGHT], 800)
                .unwrap()
                .active
        );
        assert!(
            !detector
                .observe(&vec![180; WIDTH * HEIGHT], 1000)
                .unwrap()
                .active
        );
    }
    #[test]
    fn minimum_duration_and_cooldown_use_elapsed_time_not_frame_count() {
        let mut detector = Detector::new(&config());
        detector.observe(&frame(false), 0).unwrap();
        assert!(!detector.observe(&frame(true), 200).unwrap().active);
        assert!(!detector.observe(&frame(true), 400).unwrap().active);
        let started = detector.observe(&frame(true), 600).unwrap();
        assert!(started.active);
        assert!(started.score > 0.02);
        assert!(started.bbox.is_some());
        assert!(detector.observe(&frame(false), 800).unwrap().active);
        assert!(detector.observe(&frame(false), 1400).unwrap().active);
        assert!(!detector.observe(&frame(false), 1600).unwrap().active);
    }
    #[test]
    fn exclusion_masks_and_include_zone_names_are_respected() {
        let mut settings = config();
        settings.zones = vec![zone(ZoneKind::Exclude, 0.5)];
        let mut detector = Detector::new(&settings);
        detector.observe(&frame(false), 0).unwrap();
        for at in [200, 600, 1000] {
            assert!(!detector.observe(&frame(true), at).unwrap().active);
        }
        settings.zones = vec![zone(ZoneKind::Include, 0.5)];
        let mut detector = Detector::new(&settings);
        detector.observe(&frame(false), 0).unwrap();
        detector.observe(&frame(true), 200).unwrap();
        let sample = detector.observe(&frame(true), 600).unwrap();
        assert!(sample.active);
        assert_eq!(sample.zones, vec!["entrance"]);
    }
    #[test]
    fn malformed_frames_and_backwards_times_are_rejected_without_state_changes() {
        let mut detector = Detector::new(&config());
        assert!(detector.observe(&[0; 3], 0).is_err());
        detector.observe(&frame(false), 100).unwrap();
        assert!(detector.observe(&frame(true), 99).is_err());
        assert!(!detector.observe(&frame(false), 200).unwrap().active);
    }
}
