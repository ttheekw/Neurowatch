//! Copy existing desktop pictures into a local, read-only capability. This
//! module never creates a decoder, converter, or scaler.
use super::{pump::Counters, session::Outbox};
use crate::streams::{
    decoded::{Channel, Frame, Receiver, SWITCH_DELAY},
    StreamHubs,
};
use gstreamer_video::{prelude::*, VideoFormat, VideoFrameRef, VideoInfo};
use nw_analytics_protocol::{
    decoded::{
        DecodedAvailable, DecodedRequest, DecodedWithdrawn, LocalityChallenge, RawFormat, RawLayout,
    },
    method,
    rpc::Message,
};
use nw_analytics_sdk::ring::{PictureIdentity, RingWriter};
use nw_core::camera::CameraConfig;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) struct Forwarder {
    receivers: Vec<(Arc<Channel>, Receiver)>,
    active: Option<Arc<Channel>>,
    streams: StreamHubs,
    camera: CameraConfig,
    checked: Instant,
    request: DecodedRequest,
    ring: Option<RingWriter>,
    seen: Instant,
    pacer: Pacer,
    /// When recent pictures were shared (the last two seconds), for the rate.
    shared: std::collections::VecDeque<Instant>,
    retry: Option<Instant>,
}

/// Shares at a plugin's requested rate, by schedule rather than by the time
/// since the last picture: a picture a little early still counts, so a 10 fps
/// stream gives a plugin asking for 10 fps all 10, not every other one.
struct Pacer {
    next: Option<Instant>,
}

impl Pacer {
    fn due(&mut self, now: Instant, interval: Duration) -> bool {
        let slack = interval / 4;
        match self.next {
            Some(next) if now + slack < next => false,
            Some(next) => {
                // On schedule, or after a gap: never a burst to catch up.
                self.next = Some((next + interval).max(now + interval / 2));
                true
            }
            None => {
                self.next = Some(now + interval);
                true
            }
        }
    }

    fn reset(&mut self) {
        self.next = None;
    }
}

/// How long the measured sharing rate looks back.
const RATE_WINDOW: Duration = Duration::from_secs(2);

impl Forwarder {
    pub fn new(
        channel: Arc<Channel>,
        request: DecodedRequest,
        streams: StreamHubs,
        camera: CameraConfig,
    ) -> Self {
        Self {
            receivers: vec![(Arc::clone(&channel), channel.subscribe())],
            active: None,
            streams,
            camera,
            checked: Instant::now() - Duration::from_secs(1),
            request,
            ring: None,
            seen: Instant::now(),
            pacer: Pacer { next: None },
            shared: std::collections::VecDeque::new(),
            retry: None,
        }
    }

    fn withdraw(&mut self, binding: u32, out: &Outbox, reason: &str) {
        if self.ring.take().is_some() {
            out.message(&Message::notification(
                method::DECODED_WITHDRAWN,
                DecodedWithdrawn {
                    binding,
                    reason: reason.into(),
                },
            ));
        }
        self.pacer.reset();
        self.shared.clear();
    }

    fn picture(&mut self) -> Option<(Frame, bool, Arc<Channel>)> {
        if (self.request.min_width > 0 || self.request.min_height > 0)
            && self.checked.elapsed() >= Duration::from_millis(500)
        {
            self.checked = Instant::now();
            let channels = self.streams.decoded_candidates(&self.camera);
            self.receivers.retain(|(channel, _)| {
                channels.iter().any(|current| Arc::ptr_eq(channel, current))
            });
            for channel in channels {
                if !self
                    .receivers
                    .iter()
                    .any(|(known, _)| Arc::ptr_eq(known, &channel))
                {
                    let receiver = channel.subscribe();
                    self.receivers.push((channel, receiver));
                }
            }
        }
        let mut fallback = None;
        let mut unavailable = None;
        for (channel, receiver) in &mut self.receivers {
            let Some((frame, stable)) = receiver.take() else {
                continue;
            };
            let active = self
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, channel));
            let suitable = frame
                .sample
                .caps()
                .and_then(|caps| VideoInfo::from_caps(caps).ok())
                .and_then(|info| choose_layout(&info, &self.request))
                .is_some();
            if stable && suitable {
                if active {
                    return Some((frame, true, Arc::clone(channel)));
                }
                if fallback.is_none() {
                    fallback = Some((frame, true, Arc::clone(channel)));
                }
            } else if active {
                unavailable = Some((frame, false, Arc::clone(channel)));
            }
        }
        if self.active.is_some() && unavailable.is_none() && self.seen.elapsed() < SWITCH_DELAY {
            return None;
        }
        fallback.or(unavailable)
    }

    pub fn tick(&mut self, binding: u32, out: &Outbox, counters: &Counters) {
        self.share(binding, out, counters);
        counters
            .shared_pictures
            .store(self.ring.is_some(), std::sync::atomic::Ordering::Relaxed);
        let now = Instant::now();
        while self
            .shared
            .front()
            .is_some_and(|at| now.duration_since(*at) > RATE_WINDOW)
        {
            self.shared.pop_front();
        }
        // Tenths of a frame per second; nothing shared reads as zero.
        let tenths = self.shared.len() as f64 * 10.0 / RATE_WINDOW.as_secs_f64();
        counters
            .shared_fps_tenths
            .store(tenths.round() as u32, std::sync::atomic::Ordering::Relaxed);
    }

    fn share(&mut self, binding: u32, out: &Outbox, counters: &Counters) {
        let Some((picture, stable, source)) = self.picture() else {
            if self.seen.elapsed() >= SWITCH_DELAY {
                self.withdraw(binding, out, "desktop picture unavailable");
            }
            return;
        };
        if self
            .active
            .as_ref()
            .is_some_and(|active| !Arc::ptr_eq(active, &source))
        {
            self.withdraw(binding, out, "desktop stream changed");
        }
        self.active = Some(source);
        self.seen = Instant::now();
        if !stable {
            self.withdraw(binding, out, "desktop decoder warming up");
            return;
        }
        if self.request.max_fps == 0 || self.retry.is_some_and(|at| Instant::now() < at) {
            return;
        }
        let fps = self
            .request
            .max_fps
            .min(crate::streams::decoded::MAX_SHARED_FPS);
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        if !self.pacer.due(Instant::now(), interval) {
            return;
        }
        let Some(identity) = picture.identity else {
            return;
        };
        let Some(caps) = picture.sample.caps() else {
            return;
        };
        let Ok(info) = VideoInfo::from_caps(caps) else {
            return;
        };
        let Some(layout) = choose_layout(&info, &self.request) else {
            self.withdraw(binding, out, "desktop format unavailable");
            return;
        };
        let Some(buffer) = picture.sample.buffer() else {
            return;
        };
        let Ok(frame) = VideoFrameRef::from_buffer_ref_readable(buffer, &info) else {
            return;
        };
        let Ok(first) = frame.plane_data(0) else {
            return;
        };
        let second = if layout.format == RawFormat::Nv12 {
            let Ok(plane) = frame.plane_data(1) else {
                return;
            };
            plane
        } else {
            &[]
        };
        let Ok(stride) = usize::try_from(frame.plane_stride()[0]) else {
            return;
        };
        let second_stride = if layout.format == RawFormat::Nv12 {
            let Ok(stride) = usize::try_from(frame.plane_stride()[1]) else {
                return;
            };
            stride
        } else {
            0
        };
        if self
            .ring
            .as_ref()
            .is_some_and(|ring| ring.layout() != layout)
        {
            self.withdraw(binding, out, "desktop format changed");
        }
        let fresh = self.ring.is_none();
        if fresh {
            let directory =
                std::env::temp_dir().join(format!("neurowatch-analytics-{}", std::process::id()));
            match RingWriter::create(&directory, layout) {
                Ok(ring) => self.ring = Some(ring),
                Err(error) => {
                    tracing::debug!(%error, "Could not share desktop picture");
                    self.retry = Some(Instant::now() + Duration::from_secs(5));
                    return;
                }
            }
        }
        let Some(ring) = self.ring.as_mut() else {
            return;
        };
        // Register before publication: the plugin can observe the mapping as
        // soon as write finishes, before the compressed unit reaches its socket.
        if let Ok(mut frames) = counters.frames.lock() {
            frames.remember(identity);
        }
        if ring
            .write(
                PictureIdentity {
                    seq: identity.seq,
                    time_us: identity.time_us,
                    pts_ns: buffer.pts().map(gstreamer::ClockTime::nseconds),
                },
                [first, second],
                [stride, second_stride],
            )
            .is_err()
        {
            self.withdraw(binding, out, "desktop planes unavailable");
            return;
        }
        self.shared.push_back(Instant::now());
        if fresh {
            if let Ok(file) = ring.descriptor() {
                out.descriptor(
                    &Message::notification(
                        method::DECODED_AVAILABLE,
                        DecodedAvailable {
                            binding,
                            layout,
                            ring: LocalityChallenge {
                                ring_id: ring.id().into(),
                                bytes: ring.bytes(),
                                handle: None,
                            },
                        },
                    ),
                    file,
                );
            }
        }
    }
}

fn choose_layout(info: &VideoInfo, request: &DecodedRequest) -> Option<RawLayout> {
    if info.is_interlaced()
        || info.width() < request.min_width
        || info.height() < request.min_height
    {
        return None;
    }
    let format = match info.format() {
        VideoFormat::Rgb if request.formats.contains(&RawFormat::Rgb24) => RawFormat::Rgb24,
        VideoFormat::Nv12 if request.formats.contains(&RawFormat::Gray8) => RawFormat::Gray8,
        VideoFormat::Nv12 if request.formats.contains(&RawFormat::Nv12) => RawFormat::Nv12,
        _ => return None,
    };
    let layout = RawLayout {
        width: info.width(),
        height: info.height(),
        format,
        color: if format == RawFormat::Nv12 {
            crate::pipeline::preview_format::descriptor(info)
        } else {
            None
        },
    };
    nw_analytics_sdk::ring::plane_sizes(layout).ok()?;
    Some(layout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn another_desktop_stream_is_used_only_when_the_active_stream_stalls() {
        gstreamer::init().unwrap();
        let primary = Arc::new(Channel::default());
        let alternate = Arc::new(Channel::default());
        let camera: CameraConfig = serde_json::from_value(serde_json::json!({
            "id":"camera", "name":"Camera", "url":"rtsp://example.invalid/main",
            "transport":"tcp", "notes":""
        }))
        .unwrap();
        let request = DecodedRequest {
            formats: vec![RawFormat::Rgb24],
            max_fps: 5,
            min_width: 320,
            min_height: 180,
        };
        let mut forwarder =
            Forwarder::new(Arc::clone(&primary), request, StreamHubs::default(), camera);
        forwarder.checked = Instant::now();
        forwarder
            .receivers
            .push((Arc::clone(&alternate), alternate.subscribe()));
        let sample = |width, height| {
            let info = VideoInfo::builder(VideoFormat::Rgb, width, height)
                .build()
                .unwrap();
            gstreamer::Sample::builder()
                .caps(&info.to_caps().unwrap())
                .build()
        };
        // A stable Main picture may replace the unavailable Sub picture.
        alternate.stable_picture(&sample(640, 360));
        let (_, stable, source) = forwarder.picture().unwrap();
        assert!(stable && Arc::ptr_eq(&source, &alternate));
        forwarder.active = Some(source);
        forwarder.seen = Instant::now();
        primary.stable_picture(&sample(320, 180));
        assert!(
            forwarder.picture().is_none(),
            "a fresh active stream must not flap"
        );
        // Once it stalls, a fresh suitable primary can take over.
        forwarder.seen = Instant::now() - SWITCH_DELAY;
        primary.stable_picture(&sample(320, 180));
        forwarder.receivers[0].1 = primary.subscribe();
        assert!(Arc::ptr_eq(&forwarder.picture().unwrap().2, &primary));
        forwarder.active = None;
        primary.stable_picture(&sample(160, 90));
        forwarder.receivers[0].1 = primary.subscribe();
        assert!(
            forwarder.picture().is_none(),
            "undersized alternatives are refused"
        );
    }
    #[test]
    fn sharing_follows_the_plugins_rate_whatever_the_cameras_jitter() {
        let start = Instant::now();
        let shared = |camera_fps: u64, asked: u32, jitter_ms: i64| {
            let mut pacer = Pacer { next: None };
            let interval = Duration::from_secs_f64(1.0 / f64::from(asked));
            (0..camera_fps * 10)
                .filter(|i| {
                    // Every other picture a little early, every other a little late.
                    let ms = (i * 1000 / camera_fps) as i64
                        + if i % 2 == 0 { -jitter_ms } else { jitter_ms };
                    pacer.due(
                        start + Duration::from_millis(ms.max(0) as u64 + 10),
                        interval,
                    )
                })
                .count() as f64
                / 10.0
        };
        // A plugin asking for 10 gets all of a 10 fps camera, early pictures too.
        assert_eq!(shared(10, 10, 8), 10.0);
        // From faster cameras, about what it asked for; never more.
        for (camera, asked) in [(25, 10), (30, 10), (30, 5), (25, 5), (30, 30), (15, 30)] {
            let rate = shared(camera, asked, 3);
            let expected = f64::from(asked).min(camera as f64);
            assert!(
                rate <= expected + 0.1 && rate >= expected * 0.9,
                "{camera} fps, asked {asked}: {rate}"
            );
        }
    }

    #[test]
    fn a_pause_never_brings_a_burst_afterwards() {
        let start = Instant::now();
        let mut pacer = Pacer { next: None };
        let interval = Duration::from_millis(100);
        assert!(pacer.due(start, interval));
        // Nothing for five seconds, then 30 fps again: 10 a second, not a catch-up.
        let after = (0..30)
            .filter(|i| {
                pacer.due(
                    start + Duration::from_secs(5) + Duration::from_millis(i * 33),
                    interval,
                )
            })
            .count();
        assert!((9..=11).contains(&after), "{after}");
    }

    #[test]
    fn sharing_uses_only_native_planes_and_never_converts_rgb_to_luma() {
        gstreamer::init().unwrap();
        let info = |format| VideoInfo::builder(format, 640, 360).build().unwrap();
        let mut request = DecodedRequest {
            formats: vec![RawFormat::Gray8],
            max_fps: 5,
            min_width: 320,
            min_height: 180,
        };
        assert!(choose_layout(&info(VideoFormat::Rgb), &request).is_none());
        assert_eq!(
            choose_layout(&info(VideoFormat::Nv12), &request)
                .unwrap()
                .format,
            RawFormat::Gray8
        );
        assert!(choose_layout(&info(VideoFormat::I420), &request).is_none());
        request.formats.push(RawFormat::Rgb24);
        assert_eq!(
            choose_layout(&info(VideoFormat::Rgb), &request)
                .unwrap()
                .format,
            RawFormat::Rgb24
        );
    }
}
