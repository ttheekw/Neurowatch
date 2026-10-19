//! Latest system-memory picture from a desktop decoder. Motion borrows it
//! instead of decoding again; a slow reader never holds up the desktop.
use gstreamer::prelude::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_millis(200);
pub(crate) const SWITCH_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(crate) struct Frame {
    pub sample: gstreamer::Sample,
    pub decoder: Arc<str>,
    published: Instant,
    since: Instant,
    sequence: u64,
}

#[derive(Default)]
pub(crate) struct Channel {
    readers: AtomicUsize,
    latest: Mutex<Option<Frame>>,
}

impl Channel {
    pub fn subscribe(self: &Arc<Self>) -> Receiver {
        self.readers.fetch_add(1, Ordering::Relaxed);
        Receiver {
            channel: Arc::clone(self),
            sequence: 0,
        }
    }

    fn publish(&self, sample: &gstreamer::Sample, decoder: &Arc<str>, now: Instant) {
        // The common case does not even read the clock, lock or clone a sample.
        // The caller supplies the time only after checking demand.
        if self.readers.load(Ordering::Relaxed) == 0 {
            return;
        }
        let Ok(mut latest) = self.latest.try_lock() else {
            return;
        };
        if latest
            .as_ref()
            .is_some_and(|f| now.duration_since(f.published) < INTERVAL)
        {
            return;
        }
        let since = latest
            .as_ref()
            .filter(|f| now.duration_since(f.published) < SWITCH_DELAY)
            .map_or(now, |f| f.since);
        let sequence = latest.as_ref().map_or(1, |f| f.sequence + 1);
        *latest = Some(Frame {
            sample: sample.clone(),
            decoder: Arc::clone(decoder),
            published: now,
            since,
            sequence,
        });
    }
}

pub(crate) struct Receiver {
    channel: Arc<Channel>,
    sequence: u64,
}

impl Receiver {
    /// Only fresh frames, at most once per reader. Waiting for a stable
    /// publisher avoids switching for a brief next-page warm-up.
    pub fn take(&mut self) -> Option<(Frame, bool)> {
        let latest = self.channel.latest.lock().ok()?;
        let frame = latest.as_ref()?;
        if frame.sequence == self.sequence || frame.published.elapsed() >= SWITCH_DELAY {
            return None;
        }
        self.sequence = frame.sequence;
        Some((
            frame.clone(),
            frame.published.duration_since(frame.since) >= SWITCH_DELAY,
        ))
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        if self.channel.readers.fetch_sub(1, Ordering::Relaxed) == 1 {
            if let Ok(mut latest) = self.channel.latest.lock() {
                *latest = None;
            }
        }
    }
}

pub(crate) struct Publisher {
    channel: Arc<Channel>,
    decoder: Arc<Mutex<Arc<str>>>,
    /// Retaining a GPU pool buffer could starve the desktop decoder. Check
    /// memory features only when caps change; usual frames compare pointers.
    caps: Mutex<Option<(gstreamer::Caps, bool)>>,
}

impl Publisher {
    pub fn new(channel: Arc<Channel>, pipeline: &gstreamer::Pipeline) -> Self {
        let decoder = Arc::new(Mutex::new(Arc::from("desktop")));
        let name = Arc::clone(&decoder);
        pipeline.connect_deep_element_added(move |_, _, element| {
            if let Some(factory) = element.factory() {
                let class = factory.metadata("klass").unwrap_or_default();
                if class.contains("Decoder") && class.contains("Video") {
                    if let Ok(mut name) = name.lock() {
                        *name = Arc::from(factory.name().as_str());
                    }
                }
            }
        });
        Self {
            channel,
            decoder,
            caps: Mutex::new(None),
        }
    }

    /// Called beside existing SHM/tap delivery. No waiting, media copies or
    /// allocations: only reference increments at the analysis rate.
    pub fn publish(&self, sample: &gstreamer::Sample) {
        if self.channel.readers.load(Ordering::Relaxed) == 0 {
            return;
        }
        let Some(caps) = sample.caps() else {
            return;
        };
        let Ok(mut cached) = self.caps.try_lock() else {
            return;
        };
        let system_memory = match cached.as_ref() {
            Some((known, allowed)) if known.as_ptr() == caps.as_ptr() => *allowed,
            _ => {
                let allowed = caps.is_fixed()
                    && caps.iter_with_features().all(|(_, features)| {
                        !features.is_any()
                            && features.iter().all(|feature| {
                                !feature.starts_with("memory:")
                                    || feature == gstreamer::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY
                            })
                    });
                *cached = Some((caps.to_owned(), allowed));
                allowed
            }
        };
        drop(cached);
        if !system_memory {
            return;
        }
        if let Ok(decoder) = self.decoder.try_lock() {
            self.channel.publish(sample, &decoder, Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture() -> gstreamer::Sample {
        gstreamer::init().unwrap();
        let buffer = gstreamer::Buffer::from_slice(vec![1u8; 16]);
        let caps = gstreamer::Caps::builder("video/x-raw").build();
        gstreamer::Sample::builder()
            .buffer(&buffer)
            .caps(&caps)
            .build()
    }

    #[test]
    fn decoded_publisher_accepts_only_system_memory_caps() {
        gstreamer::init().unwrap();
        let channel = Arc::new(Channel::default());
        let mut receiver = channel.subscribe();
        let publisher = Publisher::new(Arc::clone(&channel), &gstreamer::Pipeline::new());
        let buffer = gstreamer::Buffer::from_slice(vec![1u8; 16]);
        let picture_with_caps = |caps: &str| {
            let caps: gstreamer::Caps = caps.parse().unwrap();
            gstreamer::Sample::builder()
                .buffer(&buffer)
                .caps(&caps)
                .build()
        };
        for features in ["DMABuf", "VAMemory", "CUDAMemory", "GLMemory"] {
            let sample = picture_with_caps(&format!("video/x-raw(memory:{features})"));
            for _ in 0..2 {
                publisher.publish(&sample);
                assert!(
                    receiver.take().is_none(),
                    "must not retain {features} samples"
                );
                assert!(sample.is_writable());
            }
        }
        let sample = picture_with_caps("video/x-raw(ANY)");
        publisher.publish(&sample);
        assert!(receiver.take().is_none(), "unspecified memory is not safe");

        // The same publisher must reconsider changed caps in either direction.
        for caps in ["video/x-raw", "video/x-raw(memory:SystemMemory)"] {
            let sample = picture_with_caps(caps);
            publisher.publish(&sample);
            let taken = receiver.take().unwrap().0.sample;
            assert_eq!(taken.as_ptr(), sample.as_ptr());
            drop(taken);
            *channel.latest.lock().unwrap() = None;
            receiver.sequence = 0;
            let sample = picture_with_caps("video/x-raw(memory:DMABuf)");
            publisher.publish(&sample);
            assert!(receiver.take().is_none());
        }
    }

    #[test]
    fn unwanted_decoded_frames_do_not_lock_or_retain_samples() {
        gstreamer::init().unwrap();
        let channel = Arc::new(Channel::default());
        let pipeline = gstreamer::Pipeline::new();
        let publisher = Publisher::new(Arc::clone(&channel), &pipeline);
        let sample = picture();
        let locked = channel.latest.lock().unwrap();
        publisher.publish(&sample);
        assert!(sample.is_writable(), "no sample reference retained");
        assert!(locked.is_none());
        drop(locked);
        let receiver = channel.subscribe();
        publisher.publish(&sample);
        assert!(!sample.is_writable());
        drop(receiver);
        assert!(sample.is_writable(), "last reader releases the raw picture");
    }

    #[test]
    fn decoded_frames_are_rate_limited_before_queuing() {
        let channel = Arc::new(Channel::default());
        let mut receiver = channel.subscribe();
        let sample = picture();
        let decoder = Arc::from("test");
        let now = Instant::now();
        for ms in 0..1000 {
            channel.publish(&sample, &decoder, now + Duration::from_millis(ms));
        }
        let frame = receiver.take().unwrap().0;
        assert_eq!(frame.sequence, 5);
        assert_eq!(frame.published, now + Duration::from_millis(800));
        assert!(receiver.take().is_none());
    }

    #[test]
    fn a_slow_decoded_taker_never_blocks_the_desktop() {
        let channel = Arc::new(Channel::default());
        let _receiver = channel.subscribe();
        let sample = picture();
        let decoder = Arc::from("test");
        // Even a reader paused while holding the slot cannot stall appsink.
        let locked = channel.latest.lock().unwrap();
        channel.publish(&sample, &decoder, Instant::now());
        assert!(locked.is_none());
        drop(locked);
        let start = Instant::now();
        for i in 0..10_000 {
            channel.publish(&sample, &decoder, start + INTERVAL * i);
        }
        let latest = channel.latest.lock().unwrap();
        assert_eq!(latest.as_ref().unwrap().sequence, 10_000);
        assert_eq!(latest.as_ref().unwrap().published, start + INTERVAL * 9_999);
    }

    #[test]
    fn decoded_samples_and_buffers_are_shared_by_reference() {
        let channel = Arc::new(Channel::default());
        let mut first = channel.subscribe();
        let mut second = channel.subscribe();
        let sample = picture();
        channel.publish(&sample, &Arc::from("test"), Instant::now());
        for receiver in [&mut first, &mut second] {
            let taken = receiver.take().unwrap().0.sample;
            assert_eq!(sample.as_ptr(), taken.as_ptr());
            assert_eq!(
                sample.buffer().unwrap().as_ptr(),
                taken.buffer().unwrap().as_ptr()
            );
            assert_eq!(
                sample.buffer().unwrap().peek_memory(0).as_ptr(),
                taken.buffer().unwrap().peek_memory(0).as_ptr()
            );
        }
    }

    #[test]
    fn decoded_sharing_waits_for_a_stable_publisher_after_a_gap() {
        let channel = Arc::new(Channel::default());
        let mut receiver = channel.subscribe();
        let sample = picture();
        let decoder = Arc::from("test");
        let start = Instant::now() - Duration::from_secs(2);
        for i in 0..=5 {
            channel.publish(&sample, &decoder, start + INTERVAL * i);
        }
        // Stale frames are never taken, even from a previously stable view.
        assert!(receiver.take().is_none());
        channel.publish(&sample, &decoder, Instant::now());
        assert!(!receiver.take().unwrap().1, "the gap resets the warm-up");
        let start = Instant::now();
        for i in 1..=5 {
            channel.publish(&sample, &decoder, start + INTERVAL * i);
        }
        assert!(receiver.take().unwrap().1);
    }
}
