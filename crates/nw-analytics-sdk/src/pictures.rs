//! Source switching shared by analytics daemons. Compressed media continues to
//! populate one bounded GOP while a local desktop picture source is active.
use crate::{
    cache::GopCache,
    ring::{Picture, RingReader},
};
use nw_analytics_protocol::frame::MediaUnit;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const STALE: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Pictures {
    cache: GopCache,
    ring: Option<RingReader>,
    seen: Option<Instant>,
    fallback: bool,
}

pub enum Next {
    Picture(Picture),
    /// The daemon must rebuild its decoder and replay these units once.
    Fallback(Vec<Arc<MediaUnit>>),
    Waiting,
}

impl Pictures {
    pub fn offer(&mut self, ring: RingReader) {
        self.ring = Some(ring);
        self.fallback = false;
        self.seen = Some(Instant::now());
    }
    pub fn withdraw(&mut self) {
        if self.ring.take().is_some() {
            self.fallback = true;
        }
        self.seen = None;
    }
    /// Keep input by reference; `true` means the daemon should decode it now.
    pub fn unit(&mut self, unit: Arc<MediaUnit>) -> bool {
        self.cache.push(unit);
        self.ring.is_none() && !self.fallback
    }
    pub fn gap(&mut self) {
        self.cache.clear();
    }
    pub fn active(&self) -> bool {
        self.ring.is_some()
    }
    pub fn poll(&mut self) -> Next {
        self.next_at(Instant::now())
    }
    fn next_at(&mut self, now: Instant) -> Next {
        if let Some(picture) = self.ring.as_mut().and_then(RingReader::latest) {
            self.seen = Some(now);
            return Next::Picture(picture);
        }
        if self
            .seen
            .is_some_and(|seen| now.saturating_duration_since(seen) >= STALE)
        {
            self.withdraw();
        }
        if self.fallback {
            self.fallback = false;
            return Next::Fallback(self.cache.replay());
        }
        Next::Waiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::{PictureIdentity, RawFormat, RawLayout, RingWriter};
    use nw_analytics_protocol::frame::{flags, MediaKind};
    #[test]
    fn a_replacement_offer_cancels_pending_compressed_fallback() {
        let dir = std::env::temp_dir().join(format!("nw-picture-reoffer-{}", std::process::id()));
        let writer = RingWriter::create(
            &dir,
            RawLayout {
                width: 1,
                height: 1,
                format: RawFormat::Gray8,
                color: None,
            },
        )
        .unwrap();
        let reader =
            || RingReader::open(writer.descriptor().unwrap(), writer.id(), writer.bytes()).unwrap();
        let mut pictures = Pictures::default();
        pictures.offer(reader());
        pictures.withdraw();
        pictures.offer(reader());
        assert!(matches!(pictures.poll(), Next::Waiting));
        assert!(pictures.active());
        let _ = std::fs::remove_dir(dir);
    }
    #[test]
    fn desktop_loss_replays_the_cached_gop_once_without_waiting_for_another_keyframe() {
        let dir = std::env::temp_dir().join(format!("nw-picture-switch-{}", std::process::id()));
        let mut writer = RingWriter::create(
            &dir,
            RawLayout {
                width: 1,
                height: 1,
                format: RawFormat::Gray8,
                color: None,
            },
        )
        .unwrap();
        writer
            .write(
                PictureIdentity {
                    seq: 2,
                    time_us: 20,
                    pts_ns: None,
                },
                [&[12], &[]],
                [1, 0],
            )
            .unwrap();
        let mut pictures = Pictures::default();
        pictures.offer(
            RingReader::open(writer.descriptor().unwrap(), writer.id(), writer.bytes()).unwrap(),
        );
        let unit = Arc::new(MediaUnit {
            binding: 1,
            media: MediaKind::Video,
            flags: flags::KEYFRAME,
            seq: 1,
            time_us: 10,
            pts_ns: None,
            dts_ns: None,
            data: vec![1],
        });
        assert!(!pictures.unit(Arc::clone(&unit)));
        let now = Instant::now();
        assert!(matches!(pictures.next_at(now), Next::Picture(_)));
        match pictures.next_at(now + STALE) {
            Next::Fallback(units) => assert!(Arc::ptr_eq(&units[0], &unit)),
            _ => panic!("stalled rings must fall back"),
        }
        assert!(!pictures.active());
        assert!(matches!(pictures.poll(), Next::Waiting));
        assert!(pictures.unit(unit));
        drop(writer);
        let _ = std::fs::remove_dir(dir);
    }
}
