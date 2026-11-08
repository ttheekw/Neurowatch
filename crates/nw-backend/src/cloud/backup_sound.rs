//! Cloud Backup's sound: AAC as the camera sends it; any other sound the
//! recorder can decode (G.711, PCM, Opus…) converted to AAC here, on the
//! recorder, so the backup servers never run audio encoders. The ingest
//! stores AAC in each minute's MP4 beside the video.
use base64::Engine;
use gstreamer::prelude::*;
use nw_backup_protocol::messages::{Codec, TrackFormat};

use crate::streams::SharedUnit;

/// One AAC frame to back up.
pub struct SoundFrame {
    /// When the camera's sound for it arrived (µs since the Unix epoch).
    pub time_us: i64,
    pub data: Vec<u8>,
}

/// Turns one sound stream's units (one set of caps) into AAC frames.
pub enum SoundPath {
    /// The camera's AAC, raw or ADTS.
    Aac {
        adts: bool,
        format: Option<TrackFormat>,
    },
    Convert(Converter),
}

impl SoundPath {
    pub fn for_caps(caps: &gstreamer::Caps) -> Result<Self, String> {
        let structure = caps.structure(0).ok_or("sound without caps")?;
        if structure.name() == "audio/mpeg"
            && structure
                .get::<i32>("mpegversion")
                .is_ok_and(|v| v == 4 || v == 2)
        {
            let adts = structure
                .get::<&str>("stream-format")
                .is_ok_and(|f| f == "adts");
            let config = structure
                .get::<gstreamer::Buffer>("codec_data")
                .ok()
                .and_then(|buffer| buffer.map_readable().ok().map(|m| m.to_vec()));
            let format = match (
                config,
                structure.get::<i32>("rate"),
                structure.get::<i32>("channels"),
            ) {
                (Some(config), Ok(rate), Ok(channels)) => {
                    Some(aac_format(&config, rate as u32, channels as u8))
                }
                // ADTS without codec_data: read from the first frame's header.
                _ => None,
            };
            return Ok(SoundPath::Aac { adts, format });
        }
        Converter::new(caps).map(SoundPath::Convert)
    }

    /// The AAC track's format, once known.
    pub fn format(&self) -> Option<TrackFormat> {
        match self {
            SoundPath::Aac { format, .. } => format.clone(),
            SoundPath::Convert(converter) => converter.format.clone(),
        }
    }

    /// AAC frames the converter has finished since the last call (it runs on
    /// its own thread: they come out a few milliseconds after their units).
    pub fn ready(&mut self) -> Vec<SoundFrame> {
        match self {
            SoundPath::Convert(converter) => converter.pull(),
            SoundPath::Aac { .. } => Vec::new(),
        }
    }

    /// AAC frames ready after this unit (none, one or a few).
    pub fn push(&mut self, unit: &SharedUnit) -> Vec<SoundFrame> {
        let Ok(map) = unit.buffer.map_readable() else {
            return Vec::new();
        };
        match self {
            SoundPath::Aac { adts: false, .. } => vec![SoundFrame {
                time_us: unit.arrived_us,
                data: map.to_vec(),
            }],
            SoundPath::Aac { adts: true, format } => {
                let mut frames = Vec::new();
                let mut at = 0;
                while let Some((header, length, config)) = adts_header(&map[at..]) {
                    if format.is_none() {
                        *format = Some(config);
                    }
                    frames.push(SoundFrame {
                        time_us: unit.arrived_us,
                        data: map[at + header..at + length].to_vec(),
                    });
                    at += length;
                }
                frames
            }
            SoundPath::Convert(converter) => converter.push(unit),
        }
    }
}

fn aac_format(config: &[u8], rate: u32, channels: u8) -> TrackFormat {
    TrackFormat {
        codec: Codec::Aac,
        width: None,
        height: None,
        sample_rate: Some(rate),
        channels: Some(channels.max(1)),
        codec_data: Some(base64::engine::general_purpose::STANDARD.encode(config)),
    }
}

/// ADTS frame at the start of `data`: (header length, frame length, format).
fn adts_header(data: &[u8]) -> Option<(usize, usize, TrackFormat)> {
    if data.len() < 7 || data[0] != 0xff || data[1] & 0xf0 != 0xf0 {
        return None;
    }
    let header = if data[1] & 1 == 1 { 7 } else { 9 };
    let length =
        (usize::from(data[3] & 3) << 11) | (usize::from(data[4]) << 3) | usize::from(data[5] >> 5);
    if length <= header || length > data.len() {
        return None;
    }
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let object = u16::from(data[2] >> 6) + 1;
    let index = (data[2] >> 2) & 0xf;
    let channels = (u16::from(data[2] & 1) << 2) | u16::from(data[3] >> 6);
    let rate = *RATES.get(usize::from(index))?;
    let config = ((object << 11) | (u16::from(index) << 7) | (channels << 3)).to_be_bytes();
    Some((header, length, aac_format(&config, rate, channels as u8)))
}

/// Decode and encode to AAC (the clip export's encoder choice).
pub struct Converter {
    pipeline: gstreamer::Pipeline,
    input: gstreamer_app::AppSrc,
    output: gstreamer_app::AppSink,
    format: Option<TrackFormat>,
    /// The first unit's arrival and stream time: output times map back to
    /// arrival times through them.
    origin: Option<(i64, u64)>,
}

impl Converter {
    fn new(caps: &gstreamer::Caps) -> Result<Self, String> {
        let decoder = crate::pipeline::audio::audio_decoder(caps)
            .ok_or_else(|| format!("no decoder for {caps}"))?;
        let encoder = crate::export::aac_encoder_description();
        let input = gstreamer_app::AppSrc::builder()
            .caps(caps)
            .format(gstreamer::Format::Time)
            .build();
        let tail = gstreamer::parse::bin_from_description(
            &format!("audioconvert ! audioresample ! {encoder} ! aacparse ! capsfilter caps=\"audio/mpeg,mpegversion=4,stream-format=raw\""),
            true,
        )
        .map_err(|e| format!("no AAC encoder: {e}"))?;
        let output = gstreamer_app::AppSink::builder()
            .sync(false)
            .max_buffers(200)
            .build();
        let pipeline = gstreamer::Pipeline::with_name("backup_sound");
        let mut chain: Vec<gstreamer::Element> = vec![input.clone().upcast()];
        if let Some(factory) = decoder {
            chain.push(factory.create().build().map_err(|e| e.to_string())?);
        }
        chain.push(tail.upcast());
        chain.push(output.clone().upcast());
        pipeline.add_many(&chain).map_err(|e| e.to_string())?;
        gstreamer::Element::link_many(&chain).map_err(|e| e.to_string())?;
        pipeline
            .set_state(gstreamer::State::Playing)
            .map_err(|_| "the sound converter would not start".to_owned())?;
        Ok(Self {
            pipeline,
            input,
            output,
            format: None,
            origin: None,
        })
    }

    fn push(&mut self, unit: &SharedUnit) -> Vec<SoundFrame> {
        let pts = unit.buffer.pts().map(gstreamer::ClockTime::nseconds);
        if self.origin.is_none() {
            self.origin = Some((unit.arrived_us, pts.unwrap_or(0)));
        }
        let _ = self.input.push_buffer(unit.buffer.copy());
        self.pull()
    }

    fn pull(&mut self) -> Vec<SoundFrame> {
        let mut frames = Vec::new();
        while let Some(sample) = self.output.try_pull_sample(gstreamer::ClockTime::ZERO) {
            if self.format.is_none() {
                self.format = sample.caps().and_then(|caps| {
                    let s = caps.structure(0)?;
                    let config = s
                        .get::<gstreamer::Buffer>("codec_data")
                        .ok()?
                        .map_readable()
                        .ok()?
                        .to_vec();
                    Some(aac_format(
                        &config,
                        s.get::<i32>("rate").ok()? as u32,
                        s.get::<i32>("channels").ok()? as u8,
                    ))
                });
            }
            let Some(buffer) = sample.buffer() else {
                continue;
            };
            let Ok(map) = buffer.map_readable() else {
                continue;
            };
            let Some((arrived, base)) = self.origin else {
                continue;
            };
            let time_us = buffer.pts().map_or(arrived, |p| {
                arrived + (p.nseconds() as i64 - base as i64) / 1000
            });
            frames.push(SoundFrame {
                time_us,
                data: map.to_vec(),
            });
        }
        frames
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adts_headers_give_frames_and_their_format() {
        // 16 kHz mono AAC-LC, a 10-byte frame (7-byte header, no CRC).
        let mut frame = vec![0xff, 0xf1, 0x60, 0x40, 0x01, 0x5f, 0xfc];
        frame.extend_from_slice(&[1, 2, 3]);
        let (header, length, format) = adts_header(&frame).unwrap();
        assert_eq!((header, length), (7, 10));
        assert_eq!(
            (format.sample_rate, format.channels),
            (Some(16000), Some(1))
        );
        assert_eq!(
            format.codec_data.as_deref(),
            Some("FAg="),
            "AudioSpecificConfig 0x1408"
        );
        assert!(adts_header(&frame[..6]).is_none());
        assert!(adts_header(&[0u8; 10]).is_none());
    }

    #[test]
    fn g711_becomes_aac() {
        let _ = gstreamer::init();
        let encoder_installed = ["voaacenc", "fdkaacenc", "avenc_aac", "faac"]
            .iter()
            .any(|name| gstreamer::ElementFactory::find(name).is_some());
        if !encoder_installed || gstreamer::ElementFactory::find("alawdec").is_none() {
            eprintln!("no AAC encoder or G.711 decoder here: skipped");
            return;
        }
        let caps = gstreamer::Caps::builder("audio/x-alaw")
            .field("rate", 8000i32)
            .field("channels", 1i32)
            .build();
        let mut path = SoundPath::for_caps(&caps)
            .expect("an AAC encoder is installed: the converter must build");
        let mut frames = Vec::new();
        // 2 s of G.711 in 20 ms units (160 bytes each).
        for i in 0..100u64 {
            let mut buffer = gstreamer::Buffer::from_mut_slice(vec![0x7fu8; 160]);
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gstreamer::ClockTime::from_mseconds(i * 20));
            let unit = SharedUnit {
                media: crate::streams::Media::Audio,
                buffer,
                caps: caps.clone(),
                arrived_us: 1_000_000_000 + i as i64 * 20_000,
                frame: None,
            };
            frames.extend(path.push(&unit));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        frames.extend(path.ready());
        let format = path.format().expect("the encoder said its format");
        assert_eq!(format.codec, Codec::Aac);
        assert!(format.codec_data.is_some());
        assert!(frames.len() > 5, "{} frames", frames.len());
        assert!(
            frames.windows(2).all(|w| w[1].time_us >= w[0].time_us),
            "times only go forward"
        );
        assert!(frames[0].time_us >= 1_000_000_000 && frames[0].time_us < 1_000_200_000);
    }
}
