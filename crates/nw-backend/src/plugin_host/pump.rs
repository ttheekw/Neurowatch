//! One binding's media: the camera's shared connection → compressed units
//! for the plugin. Byte rewriting only (to Annex-B); nothing is decoded.
use super::session::{Offer, Outbox};
use crate::streams::{
    annexb::AnnexB, Delivery, Phase, StreamHubs, SubscriberPolicy, Subscription, Unit,
};
use nw_analytics_protocol::{
    frame::{flags, MediaKind, MediaUnit},
    messages::{Codec, DeliveryMode, Gap, GapReason, StreamChoice, StreamFormat},
    method,
    rpc::Message,
};
use nw_core::camera::CameraConfig;
use nw_core::encoded::VideoCodec;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

#[derive(Default)]
pub(super) struct Counters {
    pub sent: AtomicU64,
    pub dropped: AtomicU64,
    /// Why units are not reaching the plugin, while that lasts.
    pub problem: Mutex<Option<String>>,
    /// The stream's format (`H.264 640×360`).
    pub format: Mutex<Option<String>>,
    /// The video wall's decoded pictures are being shared with the plugin
    /// now (it then needs no decoder of its own).
    pub shared_pictures: AtomicBool,
    /// The measured sharing rate, in tenths of a frame per second.
    pub shared_fps_tenths: std::sync::atomic::AtomicU32,
    /// Only pictures this binding actually received can be referred to.
    pub frames: Mutex<crate::streams::frames::History>,
}

impl Counters {
    fn set_problem(&self, problem: Option<String>) {
        if let Ok(mut current) = self.problem.lock() {
            *current = problem;
        }
    }
}

pub(super) struct Pump {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub counters: Arc<Counters>,
}

pub(super) struct MediaOptions {
    pub binding: u32,
    pub stream: StreamChoice,
    pub keyframes_only: bool,
    pub delivery: DeliveryMode,
    pub max_buffer_ms: u32,
    pub decoded: Option<nw_analytics_protocol::decoded::DecodedRequest>,
}

impl Pump {
    pub fn start(
        camera: &CameraConfig,
        options: MediaOptions,
        streams: &StreamHubs,
        out: Outbox,
    ) -> Result<Self, String> {
        let mut source = camera.clone();
        if options.stream == StreamChoice::Sub {
            if let Some(sub) = camera.sub_url.as_ref().filter(|url| !url.is_empty()) {
                source.url = sub.clone();
            }
        }
        if !crate::streams::enabled() {
            return Err("Plugins need shared camera connections (NW_STREAM_HUB=0 is set)".into());
        }
        if !crate::streams::shareable(&source) {
            return Err(
                "This camera's source has no compressed video to share with plugins".into(),
            );
        }
        let policy = match options.delivery {
            DeliveryMode::Latest => SubscriberPolicy::plugin(),
            DeliveryMode::Reliable => SubscriberPolicy::plugin_reliable(options.max_buffer_ms),
        };
        let subscription = streams.subscribe(&source, policy, None)?;
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let thread = {
            let stop = Arc::clone(&stop);
            let counters = Arc::clone(&counters);
            let decoded_sources = (streams.clone(), camera.clone());
            std::thread::Builder::new()
                .name(format!("plugin-pump-{}", camera.id))
                .spawn(move || {
                    run(
                        subscription,
                        options,
                        out,
                        &stop,
                        &counters,
                        decoded_sources,
                    )
                })
                .map_err(|error| format!("could not start the plugin's media: {error}"))?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
            counters,
        })
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    subscription: Subscription,
    options: MediaOptions,
    out: Outbox,
    stop: &AtomicBool,
    counters: &Counters,
    decoded_sources: (StreamHubs, CameraConfig),
) {
    let binding = options.binding;
    let keyframes_only = options.keyframes_only;
    let mut decoded = options.decoded.filter(|_| !keyframes_only).map(|request| {
        super::decoded::Forwarder::new(
            subscription.decoded(),
            request,
            decoded_sources.0,
            decoded_sources.1,
        )
    });
    let mut converter: Option<(gstreamer::Caps, AnnexB)> = None;
    // The next unit sent must be a keyframe, marked as a discontinuity.
    let mut restart = true;
    let mut gap: Option<GapReason> = None;
    let mut last_sent = None;
    let mut last_hub_drops = 0;
    while !stop.load(Ordering::Acquire) {
        if let Some(decoded) = decoded.as_mut() {
            decoded.tick(binding, &out, counters);
        }
        let delivery = match subscription.recv(Duration::from_millis(100)) {
            Some(delivery) => delivery,
            None => {
                if subscription.state().phase == Phase::Closed {
                    counters.set_problem(Some("The camera connection closed".into()));
                    return;
                }
                continue;
            }
        };
        let (unit, after_gap) = match delivery {
            Delivery::Discontinuity { .. } => {
                gap = Some(GapReason::CameraLost);
                restart = true;
                continue;
            }
            Delivery::Unit { unit, after_gap } => (unit, after_gap),
        };
        let hub_drops = subscription.dropped();
        counters
            .dropped
            .fetch_add(hub_drops.saturating_sub(last_hub_drops), Ordering::Relaxed);
        last_hub_drops = hub_drops;
        if after_gap {
            gap.get_or_insert(GapReason::Behind);
            restart = true;
        }
        let keyframe = unit.keyframe();
        if (keyframes_only || restart) && !keyframe {
            continue;
        }
        if converter
            .as_ref()
            .map_or(true, |(caps, _)| *caps != unit.caps)
        {
            match AnnexB::from_caps(&unit.caps) {
                Ok(next) => {
                    let format = StreamFormat {
                        binding,
                        codec: match next.codec {
                            VideoCodec::H264 => Codec::H264,
                            VideoCodec::H265 => Codec::H265,
                        },
                        width: next.width,
                        height: next.height,
                    };
                    if !out.message(&Message::notification(method::STREAM_FORMAT, &format)) {
                        return;
                    }
                    if let Ok(mut shown) = counters.format.lock() {
                        let codec = match format.codec {
                            Codec::H264 => "H.264",
                            Codec::H265 => "H.265",
                        };
                        *shown = Some(format!("{codec} {}×{}", format.width, format.height));
                    }
                    converter = Some((unit.caps.clone(), next));
                    counters.set_problem(None);
                    restart = true;
                }
                Err(name) => {
                    converter = None;
                    counters.set_problem(Some(format!(
                        "The camera sends {name}; plugins receive H.264 or H.265"
                    )));
                    continue;
                }
            }
        }
        let Some((_, annexb)) = converter.as_ref() else {
            continue;
        };
        let data = match unit.buffer.map_readable() {
            Ok(map) => annexb.convert(&map, keyframe),
            Err(_) => None,
        };
        let Some(data) = data else {
            counters.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let Some(frame) = unit.frame else {
            continue;
        };
        if let Some(reason) = gap.take() {
            if !out.message(&Message::notification(
                method::GAP,
                Gap {
                    binding,
                    reason,
                    from_seq: last_sent,
                    to_seq: Some(frame.seq),
                },
            )) {
                return;
            }
        }
        let mut unit_flags = 0;
        if keyframe {
            unit_flags |= flags::KEYFRAME;
        }
        if restart {
            unit_flags |= flags::DISCONTINUITY;
        }
        let media = MediaUnit {
            media: MediaKind::Video,
            flags: unit_flags,
            binding,
            seq: frame.seq,
            time_us: frame.time_us,
            pts_ns: unit.buffer.pts().map(gstreamer::ClockTime::nseconds),
            dts_ns: unit.buffer.dts().map(gstreamer::ClockTime::nseconds),
            data,
        };
        match out.media(media) {
            Offer::Queued => {
                last_sent = Some(frame.seq);
                if let Ok(mut frames) = counters.frames.lock() {
                    frames.remember(frame);
                }
                restart = false;
                counters.sent.fetch_add(1, Ordering::Relaxed);
            }
            Offer::Full => {
                counters.dropped.fetch_add(1, Ordering::Relaxed);
                gap.get_or_insert(GapReason::Behind);
                restart = true;
            }
            Offer::Closed => return,
        }
    }
}
