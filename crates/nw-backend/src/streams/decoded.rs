//! Latest system-memory picture from a desktop decoder. Plugins borrow it
//! instead of decoding again; a slow reader never holds up the desktop.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

/// Fastest a desktop decoder's pictures are shared: each shared picture is a
/// full-frame copy into a plugin's ring, made by the recorder. A plugin asks
/// for its own rate (`DecodedRequest::max_fps`) up to this.
pub(crate) const MAX_SHARED_FPS: u32 = 30;
/// Publishing keeps a reference only; the forwarder paces the copies. A little
/// under a 30 fps frame time, so camera jitter never drops a frame here.
const INTERVAL: Duration = Duration::from_millis(25);
pub(crate) const SWITCH_DELAY: Duration = Duration::from_secs(1);

/// The decoder a publisher's pipeline runs, as people name it
/// (`hwaccel::decoder_label`). Found once per pipeline run, off the picture
/// path, and read by whoever holds a picture from it.
#[derive(Debug, Default)]
pub(crate) struct DecoderLabel(std::sync::atomic::AtomicU8);

/// 0 is unknown; label `n` is `LABELS[n - 1]`.
const LABELS: [&str; 7] = [
    "VA-API",
    "NVDEC",
    "Direct3D 11",
    "Direct3D 12",
    "Quick Sync",
    "V4L2",
    "CPU",
];

impl DecoderLabel {
    pub fn set(&self, label: &str) {
        if let Some(index) = LABELS.iter().position(|known| *known == label) {
            self.0.store(index as u8 + 1, Ordering::Relaxed);
        }
    }

    pub fn get(&self) -> Option<&'static str> {
        let index = usize::from(self.0.load(Ordering::Relaxed)).checked_sub(1)?;
        LABELS.get(index).copied()
    }
}

/// Who decoded the pictures a plugin got, and on what (`BindingView::shared_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SharedSource {
    /// `always_on` (the recorder's always-on decoder) or `video_wall` (a
    /// desktop view's decoder).
    pub by: &'static str,
    /// `VA-API`, `NVDEC`, `Direct3D 11`, `Direct3D 12`, `Quick Sync`, `V4L2`
    /// or `CPU`.
    pub decoder: &'static str,
}

#[derive(Clone)]
pub(crate) struct Frame {
    decoder: Option<Arc<DecoderLabel>>,
    pub sample: gstreamer::Sample,
    pub identity: Option<super::frames::Identity>,
    published: Instant,
    since: Instant,
    sequence: u64,
    /// Who published it: an always-on decoder (1) wins over a desktop view (0).
    priority: u8,
}

impl Frame {
    /// Who decoded this picture and on what; `None` until the decoder of its
    /// pipeline run is known.
    pub fn source(&self) -> Option<SharedSource> {
        Some(SharedSource {
            by: if self.priority >= 1 {
                "always_on"
            } else {
                "video_wall"
            },
            decoder: self.decoder.as_ref()?.get()?,
        })
    }
}

#[derive(Default)]
pub(crate) struct Channel {
    readers: AtomicUsize,
    latest: Mutex<Option<Frame>>,
}

impl Channel {
    #[cfg(test)]
    pub(crate) fn stable_picture(&self, sample: &gstreamer::Sample) {
        let now = Instant::now();
        *self.latest.lock().unwrap() = Some(Frame {
            decoder: None,
            sample: sample.clone(),
            identity: None,
            published: now,
            since: now - SWITCH_DELAY,
            sequence: 1,
            priority: 0,
        });
    }
    pub fn subscribe(self: &Arc<Self>) -> Receiver {
        self.readers.fetch_add(1, Ordering::Relaxed);
        Receiver {
            channel: Arc::clone(self),
            sequence: 0,
        }
    }

    #[cfg(test)]
    fn publish(&self, sample: &gstreamer::Sample, _decoder: &Arc<str>, now: Instant) {
        self.publish_identified(sample, now, None, 0, None);
    }

    /// A stream may have two decoders: a desktop view's and an always-on one.
    /// The always-on one's pictures win while it publishes, so plugins keep
    /// one source when a desktop opens or closes the camera.
    fn publish_identified(
        &self,
        sample: &gstreamer::Sample,
        now: Instant,
        identity: Option<super::frames::Identity>,
        priority: u8,
        decoder: Option<&Arc<DecoderLabel>>,
    ) {
        // The common case does not even read the clock, lock or clone a sample.
        // The caller supplies the time only after checking demand.
        if self.readers.load(Ordering::Relaxed) == 0 {
            return;
        }
        let Ok(mut latest) = self.latest.try_lock() else {
            return;
        };
        if latest.as_ref().is_some_and(|f| {
            let age = now.duration_since(f.published);
            (f.priority > priority && age < SWITCH_DELAY)
                || (f.priority == priority && age < INTERVAL)
        }) {
            return;
        }
        // Another publisher, or a gap: warming up again.
        let since = latest
            .as_ref()
            .filter(|f| f.priority == priority && now.duration_since(f.published) < SWITCH_DELAY)
            .map_or(now, |f| f.since);
        let sequence = latest.as_ref().map_or(1, |f| f.sequence + 1);
        *latest = Some(Frame {
            decoder: decoder.cloned(),
            sample: sample.clone(),
            identity,
            published: now,
            since,
            sequence,
            priority,
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
    /// Retaining a GPU pool buffer could starve the desktop decoder. Check
    /// memory features only when caps change; usual frames compare pointers.
    caps: Mutex<Option<(gstreamer::Caps, bool)>>,
    frames: Option<Arc<Mutex<super::frames::DecoderFrames>>>,
    priority: u8,
    decoder: Option<Arc<DecoderLabel>>,
}

impl Publisher {
    pub fn new(channel: Arc<Channel>, _pipeline: &gstreamer::Pipeline) -> Self {
        Self {
            channel,
            caps: Mutex::new(None),
            frames: None,
            priority: 0,
            decoder: None,
        }
    }

    /// Name the decoder of the pipeline these pictures come from, once it is
    /// known (the caller sets the label, off the picture path).
    pub fn with_decoder(mut self, decoder: Arc<DecoderLabel>) -> Self {
        self.decoder = Some(decoder);
        self
    }

    pub fn with_frames(
        channel: Arc<Channel>,
        pipeline: &gstreamer::Pipeline,
        frames: Arc<Mutex<super::frames::DecoderFrames>>,
    ) -> Self {
        let mut publisher = Self::new(channel, pipeline);
        publisher.frames = Some(frames);
        publisher
    }

    /// An always-on decoder's: its pictures win over a desktop view's.
    pub fn always_on(mut self) -> Self {
        self.priority = 1;
        self
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
        let identity = sample
            .buffer()
            .and_then(|buffer| buffer.pts())
            .and_then(|pts| self.frames.as_ref()?.try_lock().ok()?.get(pts.nseconds()));
        self.channel.publish_identified(
            sample,
            Instant::now(),
            identity,
            self.priority,
            self.decoder.as_ref(),
        );
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
    fn a_picture_says_who_decoded_it_and_on_what() {
        let channel = Arc::new(Channel::default());
        let mut receiver = channel.subscribe();
        let sample = picture();
        let label = Arc::new(DecoderLabel::default());
        let now = Instant::now();
        channel.publish_identified(&sample, now, None, 0, Some(&label));
        let frame = receiver.take().unwrap().0;
        assert_eq!(frame.source(), None, "decoder not known yet");
        // Found after the pictures started flowing: pictures already held see it.
        label.set("VA-API");
        assert_eq!(
            frame.source(),
            Some(SharedSource {
                by: "video_wall",
                decoder: "VA-API"
            })
        );
        channel.publish_identified(&sample, now + SWITCH_DELAY, None, 1, Some(&label));
        label.set("CPU");
        let frame = receiver.take().unwrap().0;
        assert_eq!(
            serde_json::to_string(&frame.source()).unwrap(),
            r#"{"by":"always_on","decoder":"CPU"}"#
        );
        label.set("unheard of");
        assert_eq!(frame.source().unwrap().decoder, "CPU");
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
    fn an_always_on_decoder_keeps_the_channel_while_a_desktop_comes_and_goes() {
        let channel = Arc::new(Channel::default());
        let mut receiver = channel.subscribe();
        let sample = picture();
        let start = Instant::now() - Duration::from_secs(5);
        let at = |ms: u64| start + Duration::from_millis(ms);
        channel.publish_identified(&sample, at(0), None, 0, None);
        // The always-on decoder takes over at once, warming up again.
        channel.publish_identified(&sample, at(30), None, 1, None);
        // A desktop view publishing meanwhile is ignored.
        for ms in [40, 70, 100, 900] {
            channel.publish_identified(&sample, at(ms), None, 0, None);
            assert_eq!(
                channel.latest.lock().unwrap().as_ref().unwrap().priority,
                1,
                "{ms}"
            );
        }
        for ms in (60..1100).step_by(30) {
            channel.publish_identified(&sample, at(ms), None, 1, None);
        }
        let latest = channel.latest.lock().unwrap().clone().unwrap();
        assert_eq!(latest.since, at(30), "the switch restarted the warm-up");
        drop(receiver.take());
        // Once it stops for SWITCH_DELAY, the desktop's pictures are used again.
        channel.publish_identified(&sample, at(1100) + SWITCH_DELAY, None, 0, None);
        assert_eq!(channel.latest.lock().unwrap().as_ref().unwrap().priority, 0);
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
        // One a millisecond is kept once per INTERVAL (25 ms): 40 a second.
        let frame = receiver.take().unwrap().0;
        let kept = 1000 / INTERVAL.as_millis() as u64;
        assert_eq!(frame.sequence, kept);
        assert_eq!(
            frame.published,
            now + Duration::from_millis((kept - 1) * INTERVAL.as_millis() as u64)
        );
        assert!(receiver.take().is_none());
        // Enough for the fastest a plugin may ask for.
        assert!(kept >= u64::from(MAX_SHARED_FPS));
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
        // Stable once it has published for SWITCH_DELAY without a gap.
        let steps = (SWITCH_DELAY.as_millis() / INTERVAL.as_millis()) as u32;
        for i in 1..steps {
            channel.publish(&sample, &decoder, start + INTERVAL * i);
        }
        assert!(!receiver.take().unwrap().1, "still warming up");
        channel.publish(&sample, &decoder, start + INTERVAL * steps);
        assert!(receiver.take().unwrap().1);
    }
}
