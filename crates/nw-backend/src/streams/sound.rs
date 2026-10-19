//! Sound from a hub, decoded for listening: the camera's compressed audio
//! units (AAC, G.711, Opus, PCM) pushed into `appsrc ! <decoder> !
//! audioconvert ! audioresample` and on to an [`AudioOutput`]. Only sound is
//! decoded here, never video.

use gstreamer::prelude::*;

use super::SharedUnit;
use crate::pipeline::audio::{audio_decoder, pcm_samples, AudioOutput};

/// One stream's sound (one set of caps) on its way to an output.
pub struct SoundDecoder {
    pipeline: gstreamer::Pipeline,
    input: gstreamer_app::AppSrc,
    caps: gstreamer::Caps,
    /// The first unit's time: the decoder's timeline starts there.
    base_ns: Option<u64>,
    /// Played as it arrives (speakers): stale units are skipped and times
    /// follow the output's clock ([`SoundDecoder::live`]).
    live: bool,
    /// Live playback: the pipeline time the first unit plays at.
    origin_ns: Option<u64>,
}

impl SoundDecoder {
    /// A decoder for units with `caps`, playing to `output`.
    pub fn new(caps: &gstreamer::Caps, output: &AudioOutput) -> Result<Self, String> {
        let decoder = audio_decoder(caps).ok_or_else(|| format!("no decoder for {caps}"))?;
        let make = |factory: &str| {
            gstreamer::ElementFactory::make(factory)
                .build()
                .map_err(|_| format!("{factory} is missing"))
        };
        let input = gstreamer_app::AppSrc::builder()
            .caps(caps)
            .format(gstreamer::Format::Time)
            .is_live(true)
            .build();
        let mut elements: Vec<gstreamer::Element> = vec![input.clone().upcast()];
        if let Some(factory) = decoder {
            elements.push(factory.create().build().map_err(|e| e.to_string())?);
        }
        elements.push(make("audioconvert")?);
        elements.push(make("audioresample")?);
        match output {
            AudioOutput::Speakers => elements.push(make("autoaudiosink")?),
            AudioOutput::Samples { rate, sink } => {
                let format = gstreamer::Caps::builder("audio/x-raw")
                    .field("format", "S16LE")
                    .field("layout", "interleaved")
                    .field("rate", i32::try_from(*rate).unwrap_or(48_000))
                    .field("channels", 1i32)
                    .build();
                let sink = std::sync::Arc::clone(sink);
                let appsink = gstreamer_app::AppSink::builder()
                    .caps(&format)
                    .sync(false)
                    .max_buffers(50)
                    .drop(true)
                    .callbacks(
                        gstreamer_app::AppSinkCallbacks::builder()
                            .new_sample(move |appsink| {
                                let sample = appsink
                                    .pull_sample()
                                    .map_err(|_| gstreamer::FlowError::Error)?;
                                if let Some(samples) = pcm_samples(&sample) {
                                    sink(samples);
                                }
                                Ok(gstreamer::FlowSuccess::Ok)
                            })
                            .build(),
                    )
                    .build();
                elements.push(appsink.upcast());
            }
        }
        let pipeline = gstreamer::Pipeline::with_name("hub_sound");
        pipeline.add_many(&elements).map_err(|e| e.to_string())?;
        gstreamer::Element::link_many(&elements).map_err(|e| e.to_string())?;
        pipeline
            .set_state(gstreamer::State::Playing)
            .map_err(|_| "the sound decoder would not start".to_owned())?;
        Ok(Self {
            pipeline,
            input,
            caps: caps.clone(),
            base_ns: None,
            live: false,
            origin_ns: None,
        })
    }

    /// Desktop sound starts at the live edge and is paced by the output's
    /// clock. Sample consumers keep their existing unpaced timeline.
    pub fn live(caps: &gstreamer::Caps, output: &AudioOutput) -> Result<Self, String> {
        let mut decoder = Self::new(caps, output)?;
        decoder.live = true;
        Ok(decoder)
    }

    /// The caps this decoder takes (a unit with others needs a new one).
    pub fn caps(&self) -> &gstreamer::Caps {
        &self.caps
    }

    /// Decode one unit. Never blocks for long: the decoder keeps up with
    /// sound easily, and the appsink drops what its consumer does not take.
    pub fn push(&mut self, unit: &SharedUnit) -> Result<(), String> {
        if self.live && !at_live_edge(unit.arrived_us, now_us()) {
            return Ok(());
        }
        if self.live && self.input.current_level_bytes() > 1024 * 1024 {
            return Err("the sound output fell behind".into());
        }
        let pts = unit.buffer.pts().map(gstreamer::ClockTime::nseconds);
        let base = *self.base_ns.get_or_insert(pts.unwrap_or(0));
        if pts.is_some_and(|pts| pts < base) {
            return Ok(());
        }
        let mut buffer = unit.buffer.copy();
        let origin = if self.live {
            *self.origin_ns.get_or_insert_with(|| {
                self.pipeline
                    .current_running_time()
                    .map(gstreamer::ClockTime::nseconds)
                    .unwrap_or(0)
                    .saturating_add(100_000_000)
            })
        } else {
            0
        };
        if let Some(writable) = buffer.get_mut() {
            writable.set_pts(
                pts.map(|pts| gstreamer::ClockTime::from_nseconds(sound_time(pts, base, origin))),
            );
            writable.set_dts(gstreamer::ClockTime::NONE);
        }
        match self.input.push_buffer(buffer) {
            Ok(_) | Err(gstreamer::FlowError::Flushing) => {}
            Err(error) => return Err(format!("the sound decoder stopped: {error:?}")),
        }
        if let Some(bus) = self.pipeline.bus() {
            while let Some(message) = bus.pop() {
                if let gstreamer::MessageView::Error(error) = message.view() {
                    return Err(error.error().to_string());
                }
            }
        }
        Ok(())
    }
}

fn sound_time(pts: u64, base: u64, origin: u64) -> u64 {
    pts.saturating_sub(base).saturating_add(origin)
}

pub(crate) fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_micros().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Cached sound is useful for recording, but listening must join the present.
pub(crate) fn at_live_edge(arrived_us: i64, now_us: i64) -> bool {
    now_us.saturating_sub(arrived_us) <= 300_000
}

impl Drop for SoundDecoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::streams::Media;

    #[test]
    fn live_listening_rejects_cached_sound_and_preserves_clock_spacing() {
        let now = 10_000_000;
        assert!(!at_live_edge(now - 2_000_000, now));
        assert!(!at_live_edge(now - 300_001, now));
        assert!(at_live_edge(now - 300_000, now));
        assert!(at_live_edge(now, now));
        // A delayed first unit must be scheduled at the current pipeline
        // time plus a little headroom, rather than at an already late zero.
        let origin = 2_100_000_000;
        let base = 30_000_000_000;
        let times: Vec<_> = (0..5)
            .map(|i| sound_time(base + i * 20_000_000, base, origin))
            .collect();
        assert_eq!(times[0], origin);
        assert!(times.windows(2).all(|w| w[1] - w[0] == 20_000_000));
        assert_eq!(
            times[4] - times[0],
            80_000_000,
            "cached chunks retain their playback duration"
        );
    }

    #[test]
    fn live_sound_skips_old_units_before_starting_its_timeline() {
        gstreamer::init().unwrap();
        let caps = gstreamer::Caps::builder("audio/x-alaw")
            .field("rate", 8000i32)
            .field("channels", 1i32)
            .build();
        let received = Arc::new(Mutex::new(0usize));
        let counter = Arc::clone(&received);
        let output = AudioOutput::Samples {
            rate: 48_000,
            sink: Arc::new(move |samples| *counter.lock().unwrap() += samples.len()),
        };
        let mut decoder = SoundDecoder::live(&caps, &output).unwrap();
        let unit = |ms, arrived_us| {
            let mut buffer = gstreamer::Buffer::from_slice(vec![0xd5u8; 160]);
            buffer
                .make_mut()
                .set_pts(gstreamer::ClockTime::from_mseconds(ms));
            SharedUnit {
                media: Media::Audio,
                buffer,
                caps: caps.clone(),
                arrived_us,
            }
        };
        // A joining hub's old GOP does not establish the decoder's clock.
        for ms in (5_000..7_000).step_by(20) {
            decoder.push(&unit(ms, now_us() - 2_000_000)).unwrap();
        }
        assert!(decoder.base_ns.is_none());
        assert!(decoder.origin_ns.is_none());
        assert_eq!(*received.lock().unwrap(), 0);
        std::thread::sleep(Duration::from_millis(100));
        let running = decoder.pipeline.current_running_time().unwrap().nseconds();
        let times = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&times);
        decoder.input.static_pad("src").unwrap().add_probe(
            gstreamer::PadProbeType::BUFFER,
            move |_, info| {
                captured
                    .lock()
                    .unwrap()
                    .push(info.buffer().unwrap().pts().unwrap().nseconds());
                gstreamer::PadProbeReturn::Ok
            },
        );
        for i in 0..10 {
            decoder.push(&unit(7_000 + i * 20, now_us())).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while *received.lock().unwrap() < 8_000 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!((8_000..=9_600).contains(&*received.lock().unwrap()));
        let times = times.lock().unwrap();
        assert_eq!(times.len(), 10);
        assert!(times[0] >= running + 100_000_000);
        assert!(times.windows(2).all(|w| w[1] - w[0] == 20_000_000));
    }

    #[test]
    fn g711_units_become_48_khz_mono_samples() {
        gstreamer::init().unwrap();
        let caps = gstreamer::Caps::builder("audio/x-alaw")
            .field("rate", 8000i32)
            .field("channels", 1i32)
            .build();
        let received = Arc::new(Mutex::new(0usize));
        let counter = Arc::clone(&received);
        let output = AudioOutput::Samples {
            rate: 48_000,
            sink: Arc::new(move |samples| *counter.lock().unwrap() += samples.len()),
        };
        let mut decoder = SoundDecoder::new(&caps, &output).unwrap();
        // Ten 20 ms G.711 frames (160 bytes each), starting mid-stream.
        for index in 0..10u64 {
            let mut buffer = gstreamer::Buffer::from_slice(vec![0xd5u8; 160]);
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gstreamer::ClockTime::from_mseconds(5_000 + index * 20));
            decoder
                .push(&SharedUnit {
                    media: Media::Audio,
                    buffer,
                    caps: caps.clone(),
                    arrived_us: 0,
                })
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while *received.lock().unwrap() < 8_000 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        // 200 ms at 48 kHz is 9600 samples; the resampler keeps a few back.
        let got = *received.lock().unwrap();
        assert!((8_000..=9_600).contains(&got), "{got} samples");
    }
}
