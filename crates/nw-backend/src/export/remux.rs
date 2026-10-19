//! Clip export's worker: recording files in, one MP4 out.
//!
//! The video is copied, never decoded or re-encoded. Each recording file is
//! demuxed and parsed into access units the way browser playback reads it
//! ([`encoded::build_pulled`]); the units are retimed onto the clip's own
//! timeline and handed to `mp4mux`. What that means:
//!
//! - **Start**: decoding can only begin at a keyframe, so a clip starts at the
//!   keyframe at or before the chosen start (at most one keyframe interval
//!   early). [`RemuxSummary::first_ms`] says where.
//! - **End**: the last picture is the last one before the chosen end.
//! - **Files**: the range's segment files follow one another on one
//!   timeline. A gap between them longer than [`MAX_KEPT_GAP_US`] (camera
//!   offline, recording off) is left out rather than shown as a frozen
//!   picture; overlaps of the files' estimated start times are closed up.
//! - **Format change**: MP4 holds one video format, so a file in another
//!   codec or picture size than the first ends the clip there
//!   ([`RemuxSummary::truncated`]).
//! - **Sound** follows the first file's track ([`SoundPlan`]). A track MP4
//!   holds as it is (AAC, Opus) is copied, never decoded; its holes are
//!   marked with GAP events. Other camera sound (G.711, PCM: MP4 players do
//!   not read it) is decoded and encoded to AAC at its own sample rate, when
//!   an AAC encoder is installed; its holes are filled with silence. Either
//!   way the sound stays in step with the pictures, and the muxer, which
//!   interleaves the two tracks, never waits for sound that is not coming.
//! - **Unsupported**: recordings that are not H.264/H.265 (MJPEG) cannot be
//!   copied into MP4.
//!
//! Nothing here is paced: files are read as fast as the disk and the muxer
//! allow, with at most [`MAX_QUEUED_BYTES`] of video waiting for the muxer.

#[path = "timelapse.rs"]
mod timelapse;
pub use timelapse::remux_timelapse;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer::prelude::*;
use nw_core::encoded::VideoCodec;

use super::ExportError;
use crate::pipeline::{audio, builder, encoded};
use crate::playback::session::PlaybackSegment;

/// Longest wait for a file to show its tracks.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const PULL_TIMEOUT: gstreamer::ClockTime = gstreamer::ClockTime::from_mseconds(200);
const SLICE: Duration = Duration::from_millis(10);
/// Longest gap between files kept as it is; longer ones are left out.
pub const MAX_KEPT_GAP_US: i64 = 2_000_000;
/// Picture interval assumed until two pictures give the real one.
const DEFAULT_FRAME_US: i64 = 40_000;
/// How far the sound of a file without sound may trail the pictures before
/// it is filled: the muxer interleaves the tracks, so a track with nothing
/// queued holds the other one up.
const MAX_SOUND_LAG_US: i64 = 1_000_000;
/// Longest wait at a file's end for the rest of its sound.
const SOUND_TAIL_WAIT: Duration = Duration::from_millis(500);
/// A jump forward in the sound larger than this is filled with silence.
const SOUND_GAP_US: i64 = 40_000;
/// Video queued for the muxer above which reading waits for it.
const MAX_QUEUED_BYTES: u64 = 16 * 1024 * 1024;
/// A muxer that takes nothing for this long has stopped.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest wait for the MP4 to be finished (its index written).
const FINISH_TIMEOUT: Duration = Duration::from_secs(300);
/// AAC encoders, cheapest first (camera sound needs no more): `voaacenc`
/// takes about a fifteenth of the CPU of FFmpeg's default "two loop" coder.
const AAC_ENCODERS: [&str; 4] = ["voaacenc", "fdkaacenc", "avenc_aac", "faac"];
/// Sample rates AAC encodes; decoded sound at another rate becomes 48 kHz.
const AAC_RATES: [u32; 9] = [
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000,
];
const DEMUX: &str = "export_demux";
/// Pictures read ahead to work out decoding times (B-frames reorder far
/// fewer than this).
const REORDER_WINDOW: usize = 16;

/// What an export wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemuxSummary {
    /// Recording time (ms since the epoch) of the first picture: the
    /// keyframe at or before the chosen start.
    pub first_ms: i64,
    /// Recording time of the last picture.
    pub last_ms: i64,
    /// The clip's length.
    pub duration_ms: i64,
    pub frames: u64,
    pub audio: bool,
    /// The sound was copied as recorded (not decoded and encoded).
    pub sound_copied: bool,
    /// Recording gaps left out.
    pub gaps: u32,
    /// Files that could not be read and were skipped.
    pub skipped_files: u32,
    /// Ended early, where the recording changed format.
    pub truncated: bool,
}

/// Export `segments` (oldest first) between `start_ms` and `end_ms` into the
/// MP4 file `output`. `progress` hears the fraction done (0–1); `cancel`
/// stops the export with [`ExportError::Cancelled`]. On error `output` may
/// hold a partial file, which the caller removes.
pub fn remux(
    segments: &[PlaybackSegment],
    start_ms: i64,
    end_ms: i64,
    output: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<RemuxSummary, ExportError> {
    gstreamer::init().map_err(|error| ExportError::Output(error.to_string()))?;
    let range = Range {
        start_us: start_ms.saturating_mul(1000),
        end_us: end_ms.saturating_mul(1000),
    };
    let total_us = segments
        .iter()
        .map(|s| range.overlap_us(s))
        .sum::<i64>()
        .max(1);
    let scratch = sibling(output, ".moov");
    let result = (|| {
        let mut writer: Option<Writer> = None;
        let mut done_us = 0_i64;
        let mut skipped = 0_u32;
        let mut truncated = false;
        for segment in segments {
            if cancel.load(Ordering::Relaxed) {
                return Err(ExportError::Cancelled);
            }
            let overlap = range.overlap_us(segment);
            if overlap <= 0 {
                continue;
            }
            let mut report = |within_us: i64| {
                progress((done_us + within_us.clamp(0, overlap)) as f64 / total_us as f64);
            };
            match read_file(
                segment,
                &range,
                &mut writer,
                output,
                &scratch,
                cancel,
                &mut report,
            ) {
                Ok(FileEnd::Ended) => {}
                Ok(FileEnd::ReachedEnd) => break,
                Ok(FileEnd::FormatChanged) => {
                    tracing::info!(path = %segment.path, "Recording format changes here; the clip ends at this file");
                    truncated = true;
                    break;
                }
                Err(ExportError::Unreadable) => {
                    tracing::warn!(path = %segment.path, "Recording file could not be read; skipped");
                    skipped += 1;
                }
                Err(ExportError::Unsupported(codec)) if writer.is_some() => {
                    tracing::info!(path = %segment.path, %codec, "Recording codec changes here; the clip ends at this file");
                    truncated = true;
                    break;
                }
                Err(error) => return Err(error),
            }
            done_us += overlap;
        }
        let Some(writer) = writer else {
            return Err(if skipped > 0 {
                ExportError::Unreadable
            } else {
                ExportError::NoRecordings
            });
        };
        let mut summary = writer.finish(cancel)?;
        summary.skipped_files = skipped;
        summary.truncated = truncated;
        Ok(summary)
    })();
    let _ = std::fs::remove_file(&scratch);
    result
}

#[derive(Clone, Copy)]
struct Range {
    start_us: i64,
    end_us: i64,
}

impl Range {
    fn overlap_us(&self, segment: &PlaybackSegment) -> i64 {
        let start = segment.start_ts.saturating_mul(1000).max(self.start_us);
        let end = segment.end_ts.saturating_mul(1000).min(self.end_us);
        (end - start).max(0)
    }
}

/// `path` with `suffix` added to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

enum FileEnd {
    /// The file was read to its end.
    Ended,
    /// The clip's end was reached.
    ReachedEnd,
    /// The file's video is not like the clip's so far.
    FormatChanged,
}

/// Stops a recording file's pipeline when dropped.
struct Source(gstreamer::Pipeline);

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.0.set_state(gstreamer::State::Null);
    }
}

/// One compressed picture, placed in recording time.
struct Unit {
    buffer: gstreamer::Buffer,
    keyframe: bool,
    /// Recording time (µs since the epoch) of its presentation and decoding.
    wall_us: i64,
    dts_wall_us: Option<i64>,
}

impl Unit {
    /// `base_us`: the recording time of the file's start.
    fn from_sample(sample: &gstreamer::Sample, base_us: i64) -> Option<Self> {
        let buffer = sample.buffer_owned()?;
        if buffer.size() == 0 {
            return None;
        }
        let pts = buffer.pts()?;
        // Stream time (the offset into the file): a recording's later
        // fragments continue its timeline, so raw timestamps start far from 0.
        let stream = sample
            .segment()
            .and_then(|segment| {
                segment
                    .downcast_ref::<gstreamer::ClockTime>()?
                    .to_stream_time(pts)
            })
            .unwrap_or(pts);
        let to_stream = stream.nseconds() as i64 - pts.nseconds() as i64;
        let pts_us = stream.useconds() as i64;
        let dts_us = buffer
            .dts()
            .map(|dts| (dts.nseconds() as i64 + to_stream) / 1000);
        Some(Self {
            keyframe: !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT),
            buffer,
            wall_us: base_us + pts_us,
            dts_wall_us: dts_us.map(|dts| base_us + dts),
        })
    }
}

/// Codec name and picture size from a parser's caps.
fn video_format(caps: &gstreamer::CapsRef) -> Option<(String, i32, i32)> {
    let s = caps.structure(0)?;
    Some((
        s.name().to_string(),
        s.get::<i32>("width").unwrap_or(0),
        s.get::<i32>("height").unwrap_or(0),
    ))
}

/// How the clip's sound is written; the first file's track decides.
#[derive(Clone, Debug)]
enum SoundPlan {
    /// No file read yet.
    Undecided,
    /// Copied as recorded: a file's track joins only if it matches these caps.
    Copy(gstreamer::Caps),
    /// Decoded to mono samples at this rate and encoded to AAC.
    Encode(u32),
    /// No sound.
    Off,
}

/// A recording file's sound, as this export reads it.
#[derive(Clone)]
struct FileSound {
    sink: gstreamer_app::AppSink,
    kind: SoundKind,
}

#[derive(Clone, Debug)]
enum SoundKind {
    /// Compressed, as recorded, in these caps.
    Coded(gstreamer::Caps),
    /// Decoded to mono S16 samples at this rate.
    Pcm(u32),
}

type FileSoundSlot = Arc<Mutex<Option<FileSound>>>;

/// Whether `mp4mux` stores `caps` as they are.
fn mp4_takes(caps: &gstreamer::Caps) -> bool {
    gstreamer::ElementFactory::find("mp4mux").is_some_and(|mux| mux.can_sink_any_caps(caps))
}

/// Open a recording file: its pipeline, the slot of its video's access-unit
/// sink, and the slot of its sound (read as `plan` needs it).
fn open_file(
    path: &str,
    plan: &SoundPlan,
) -> anyhow::Result<(gstreamer::Pipeline, encoded::PulledSink, FileSoundSlot)> {
    let description = format!(
        "filesrc location=\"{}\" ! matroskademux name={DEMUX} parsebin name=encoded_parse",
        builder::gst_quote(path)
    );
    let (pipeline, slot) = encoded::build_pulled(&description)?;
    let sound = FileSoundSlot::default();
    let (Some(demux), Some(parse)) = (pipeline.by_name(DEMUX), pipeline.by_name("encoded_parse"))
    else {
        anyhow::bail!("recording pipeline incomplete");
    };
    let bin = pipeline.clone();
    let found = Arc::clone(&sound);
    let plan = plan.clone();
    demux.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let is_sound = caps
            .structure(0)
            .is_some_and(|s| s.name().starts_with("audio/"));
        if is_sound {
            match link_sound(&bin, pad, &caps, &plan) {
                Ok(Some(sound)) => {
                    if let Ok(mut found) = found.lock() {
                        found.get_or_insert(sound);
                    }
                }
                Ok(None) => tracing::debug!(%caps, "Recording sound left out of the clip"),
                Err(error) => tracing::debug!(%error, "Recording sound could not be linked"),
            }
            return;
        }
        if let Some(target) = parse
            .static_pad("sink")
            .filter(|target| !target.is_linked())
        {
            let _ = pad.link(&target);
        }
    });
    Ok((pipeline, slot, sound))
}

/// Link a file's sound `pad` to an appsink: as recorded when the clip copies
/// it (or, for the first file, when MP4 holds it), else decoded to mono
/// samples for AAC. `None`: the clip leaves this track out.
fn link_sound(
    bin: &gstreamer::Pipeline,
    pad: &gstreamer::Pad,
    caps: &gstreamer::Caps,
    plan: &SoundPlan,
) -> anyhow::Result<Option<FileSound>> {
    use std::str::FromStr as _;
    let copy = match plan {
        SoundPlan::Undecided => mp4_takes(caps),
        SoundPlan::Copy(clip) if clip.can_intersect(caps) => true,
        SoundPlan::Copy(_) | SoundPlan::Off => return Ok(None),
        SoundPlan::Encode(_) => false,
    };
    // The sound gets its own thread, so it never holds up the demuxer and
    // with it the pictures.
    let queue = gstreamer::ElementFactory::make("queue")
        .property("max-size-buffers", 0u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 5_000_000_000u64)
        .build()?;
    let sink = gstreamer_app::AppSink::builder()
        .sync(false)
        .async_(false)
        .build();
    let mut chain = vec![queue.clone()];
    let kind = if copy {
        SoundKind::Coded(caps.clone())
    } else {
        let Some(decoder) = audio::audio_decoder(caps) else {
            return Ok(None);
        };
        let rate = match plan {
            SoundPlan::Encode(rate) => *rate,
            _ if aac_encoder().is_none() => return Ok(None),
            _ => caps
                .structure(0)
                .and_then(|s| s.get::<i32>("rate").ok())
                .and_then(|rate| u32::try_from(rate).ok())
                .filter(|rate| AAC_RATES.contains(rate))
                .unwrap_or(48_000),
        };
        chain.extend(decoder.map(|f| f.create().build()).transpose()?);
        chain.push(gstreamer::ElementFactory::make("audioconvert").build()?);
        chain.push(gstreamer::ElementFactory::make("audioresample").build()?);
        chain.push(
            gstreamer::ElementFactory::make("capsfilter")
                .property(
                    "caps",
                    gstreamer::Caps::from_str(&format!(
                        "audio/x-raw,format=S16LE,layout=interleaved,rate={rate},channels=1"
                    ))?,
                )
                .build()?,
        );
        SoundKind::Pcm(rate)
    };
    chain.push(sink.clone().upcast());
    bin.add_many(&chain)?;
    gstreamer::Element::link_many(&chain)?;
    for element in &chain {
        element.sync_state_with_parent()?;
    }
    let target = queue
        .static_pad("sink")
        .ok_or_else(|| anyhow::anyhow!("queue has no sink pad"))?;
    pad.link(&target)?;
    Ok(Some(FileSound { sink, kind }))
}

#[allow(clippy::too_many_arguments)]
fn read_file(
    segment: &PlaybackSegment,
    range: &Range,
    writer: &mut Option<Writer>,
    output: &Path,
    scratch: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(i64),
) -> Result<FileEnd, ExportError> {
    let plan = writer
        .as_ref()
        .map_or(SoundPlan::Undecided, Writer::sound_plan);
    let (pipeline, slot, sound_slot) =
        open_file(&segment.path, &plan).map_err(|_| ExportError::Unreadable)?;
    let source = Source(pipeline);
    let pipeline = &source.0;
    let bus = pipeline.bus().ok_or(ExportError::Unreadable)?;
    if pipeline.set_state(gstreamer::State::Paused).is_err() {
        return Err(ExportError::Unreadable);
    }
    let deadline = Instant::now() + OPEN_TIMEOUT;
    let (sink, codec) = loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ExportError::Cancelled);
        }
        match slot.lock().ok().and_then(|found| found.clone()) {
            Some(Ok(found)) => break found,
            Some(Err(codec)) => return Err(ExportError::Unsupported(codec)),
            None if Instant::now() >= deadline => return Err(ExportError::Unreadable),
            None => std::thread::sleep(SLICE),
        }
    };
    let _ = pipeline.state(gstreamer::ClockTime::from_seconds(5));
    let offset_ms = range.start_us / 1000 - segment.start_ts;
    if offset_ms > 0 {
        // Into the file at the keyframe before the start (the lead-in below
        // also copes with a file that cannot seek: it reads from the top).
        let _ = pipeline.seek_simple(
            gstreamer::SeekFlags::FLUSH
                | gstreamer::SeekFlags::KEY_UNIT
                | gstreamer::SeekFlags::SNAP_BEFORE,
            gstreamer::ClockTime::from_mseconds(offset_ms as u64),
        );
        let _ = pipeline.state(gstreamer::ClockTime::from_seconds(5));
    }
    if pipeline.set_state(gstreamer::State::Playing).is_err() {
        return Err(ExportError::Unreadable);
    }

    let base_us = segment.start_ts.saturating_mul(1000);
    let file_start_us = base_us.max(range.start_us);
    let mut sound = SoundQueue::default();
    // Pictures before the clip's start, from the latest keyframe: the clip
    // starts with them, since it can only start at a keyframe.
    let mut lead_in: Vec<Unit> = Vec::new();
    let mut first_caps: Option<gstreamer::Caps> = None;
    let mut need_keyframe = true;
    let mut placed = false;
    let mut wrote = false;
    let mut last_wall_us: Option<i64> = None;
    let mut end = FileEnd::Ended;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ExportError::Cancelled);
        }
        if let Some(message) = bus.pop_filtered(&[gstreamer::MessageType::Error]) {
            if let gstreamer::MessageView::Error(error) = message.view() {
                tracing::debug!(path = %segment.path, error = %error.error(), "Recording file read error");
            }
            // A file broken partway (the one still being written) ends there.
            if wrote {
                break;
            }
            return Err(ExportError::Unreadable);
        }
        let Some(sample) = sink.try_pull_sample(PULL_TIMEOUT) else {
            if sink.is_eos() {
                break;
            }
            continue;
        };
        sound.collect(&sound_slot, base_us);
        let Some(unit) = Unit::from_sample(&sample, base_us) else {
            continue;
        };
        if unit.wall_us >= range.end_us {
            end = FileEnd::ReachedEnd;
            break;
        }
        progress(unit.wall_us - file_start_us);
        if need_keyframe {
            if !unit.keyframe {
                continue;
            }
            need_keyframe = false;
        }
        // Linked before any data flows, so a file whose sound is still on its
        // way is not taken for one without.
        let file_has_sound = sound_slot.lock().map(|s| s.is_some()).unwrap_or(false);
        if writer.is_none() {
            // Waiting for the clip to begin: the pictures before its start
            // from the latest keyframe (it can only start at a keyframe) and,
            // in a recording without decoding times (B-frames), the first
            // `REORDER_WINDOW` pictures, which tell how far they reorder.
            if unit.wall_us < range.start_us && unit.keyframe {
                lead_in.clear();
            }
            lead_in.push(unit);
            if let Some(first) = lead_in.first() {
                sound.drop_before(first.wall_us);
            }
            first_caps = sample.caps_owned().or(first_caps);
            let in_clip = lead_in
                .iter()
                .filter(|u| u.wall_us >= range.start_us)
                .count();
            let reordered = lead_in.iter().any(|u| u.dts_wall_us.is_none());
            if in_clip == 0 || (reordered && in_clip < REORDER_WINDOW) {
                continue;
            }
            open_writer(
                writer,
                &mut lead_in,
                first_caps.as_ref(),
                codec,
                &sound_slot,
                (output, scratch),
                &mut sound,
                cancel,
                &mut last_wall_us,
            )?;
            placed = true;
            wrote = true;
            continue;
        }
        let Some(w) = writer.as_mut() else {
            return Err(ExportError::Unreadable);
        };
        if sample
            .caps()
            .and_then(video_format)
            .is_some_and(|format| format != w.format)
        {
            if wrote {
                // Changed within the file: end the clip here.
                end = FileEnd::FormatChanged;
                break;
            }
            return Ok(FileEnd::FormatChanged);
        }
        if !placed {
            w.begin_file(&unit);
            placed = true;
        }
        write_units(
            w,
            [unit],
            &mut sound,
            file_has_sound,
            cancel,
            &mut last_wall_us,
        )?;
        wrote = true;
    }

    // The clip began in this file but it ended before enough pictures came.
    if writer.is_none() && lead_in.iter().any(|u| u.wall_us >= range.start_us) {
        open_writer(
            writer,
            &mut lead_in,
            first_caps.as_ref(),
            codec,
            &sound_slot,
            (output, scratch),
            &mut sound,
            cancel,
            &mut last_wall_us,
        )?;
    }
    if let (Some(w), Some(last_wall_us)) = (writer.as_mut(), last_wall_us) {
        // The sound up to the end of the file's last picture, once it is
        // through its own branch.
        let until = (last_wall_us + w.frame_us).min(range.end_us);
        sound.collect_through(&sound_slot, base_us, until, cancel);
        for unit in sound.take_until(until) {
            w.write_sound(unit)?;
        }
    }
    Ok(end)
}

/// Open the clip's writer and write the pictures that waited for it.
#[allow(clippy::too_many_arguments)]
fn open_writer(
    writer: &mut Option<Writer>,
    waiting: &mut Vec<Unit>,
    caps: Option<&gstreamer::Caps>,
    codec: VideoCodec,
    sound_slot: &FileSoundSlot,
    (output, scratch): (&Path, &Path),
    sound: &mut SoundQueue,
    cancel: &AtomicBool,
    last_wall_us: &mut Option<i64>,
) -> Result<(), ExportError> {
    let caps = caps.ok_or(ExportError::Unreadable)?;
    let kind = sound_slot
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|s| s.kind.clone()));
    let file_has_sound = kind.is_some();
    let mut w = Writer::new(caps, codec, kind, output, scratch)?;
    w.begin_clip(waiting);
    let units = std::mem::take(waiting);
    let result = write_units(&mut w, units, sound, file_has_sound, cancel, last_wall_us);
    *writer = Some(w);
    result
}

/// Write pictures, each followed by the sound up to its time.
fn write_units(
    w: &mut Writer,
    units: impl IntoIterator<Item = Unit>,
    sound: &mut SoundQueue,
    file_has_sound: bool,
    cancel: &AtomicBool,
    last_wall_us: &mut Option<i64>,
) -> Result<(), ExportError> {
    for unit in units {
        let wall_us = unit.wall_us;
        w.write_video(unit, cancel)?;
        *last_wall_us = Some(last_wall_us.map_or(wall_us, |last| last.max(wall_us)));
        for piece in sound.take_until(wall_us) {
            w.write_sound(piece)?;
        }
        w.keep_sound_up(file_has_sound)?;
    }
    Ok(())
}

/// How far a recording's pictures reorder (B-frames): the least delay that
/// keeps each picture's decoding time — the presentation times in order,
/// one per picture — at or before its own presentation time.
fn reorder_delay_us(units: &[Unit]) -> i64 {
    let mut sorted: Vec<i64> = units.iter().map(|u| u.wall_us).collect();
    sorted.sort_unstable();
    units
        .iter()
        .zip(&sorted)
        .map(|(unit, in_order)| in_order - unit.wall_us)
        .max()
        .unwrap_or(0)
        .max(0)
}

/// A piece of a file's sound, in recording time (µs since the epoch).
struct SoundUnit {
    wall_us: i64,
    duration_us: i64,
    data: SoundData,
}

enum SoundData {
    /// Mono samples (an encoded clip).
    Pcm(Vec<i16>),
    /// As recorded (a copied clip).
    Coded(gstreamer::Buffer),
}

/// A file's sound waiting for the pictures of its time.
#[derive(Default)]
struct SoundQueue {
    pending: VecDeque<SoundUnit>,
}

impl SoundQueue {
    /// Take what the file's sound branch has produced so far; `true` if
    /// there was any.
    fn collect(&mut self, slot: &FileSoundSlot, base_us: i64) -> bool {
        let Some(sound) = slot.lock().ok().and_then(|sound| sound.clone()) else {
            return false;
        };
        let mut any = false;
        while let Some(sample) = sound.sink.try_pull_sample(gstreamer::ClockTime::ZERO) {
            let Some(at) = audio::stream_time_us(&sample) else {
                continue;
            };
            let unit = match &sound.kind {
                SoundKind::Pcm(rate) => audio::pcm_samples(&sample).map(|samples| SoundUnit {
                    wall_us: base_us + at,
                    duration_us: samples_to_us(samples.len() as i64, *rate),
                    data: SoundData::Pcm(samples),
                }),
                SoundKind::Coded(_) => sample.buffer_owned().map(|buffer| SoundUnit {
                    wall_us: base_us + at,
                    duration_us: buffer.duration().map_or(0, |d| d.useconds() as i64),
                    data: SoundData::Coded(buffer),
                }),
            };
            if let Some(unit) = unit {
                any = true;
                self.pending.push_back(unit);
            }
        }
        any
    }

    /// Collect until the sound reaches `wall_us` or its track ends (bounded
    /// wait: a track that stopped early does not hold the export up).
    fn collect_through(
        &mut self,
        slot: &FileSoundSlot,
        base_us: i64,
        wall_us: i64,
        cancel: &AtomicBool,
    ) {
        let Some(sink) = slot
            .lock()
            .ok()
            .and_then(|s| s.as_ref().map(|s| s.sink.clone()))
        else {
            return;
        };
        let deadline = Instant::now() + SOUND_TAIL_WAIT;
        loop {
            self.collect(slot, base_us);
            let reached = self
                .pending
                .back()
                .is_some_and(|unit| unit.wall_us + unit.duration_us >= wall_us);
            if reached || sink.is_eos() || cancel.load(Ordering::Relaxed) {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Forget sound that ends before `wall_us`.
    fn drop_before(&mut self, wall_us: i64) {
        while self
            .pending
            .front()
            .is_some_and(|unit| unit.wall_us + unit.duration_us <= wall_us)
        {
            self.pending.pop_front();
        }
    }

    /// The sound starting at or before `wall_us`.
    fn take_until(&mut self, wall_us: i64) -> Vec<SoundUnit> {
        let mut taken = Vec::new();
        while self
            .pending
            .front()
            .is_some_and(|unit| unit.wall_us <= wall_us)
        {
            if let Some(unit) = self.pending.pop_front() {
                taken.push(unit);
            }
        }
        taken
    }
}

fn samples_to_us(samples: i64, rate: u32) -> i64 {
    samples * 1_000_000 / i64::from(rate.max(1))
}

fn us_to_samples(us: i64, rate: u32) -> i64 {
    us * i64::from(rate) / 1_000_000
}

fn clock(us: i64) -> gstreamer::ClockTime {
    gstreamer::ClockTime::from_useconds(us.max(0) as u64)
}

/// The AAC encoder element for a pipeline description: FFmpeg's in its fast
/// mode (its default coder costs about twice the CPU for no gain here).
fn aac_encoder_description() -> String {
    let encoder = aac_encoder().unwrap_or("avenc_aac");
    let fast = encoder == "avenc_aac"
        && gstreamer::ElementFactory::make(encoder)
            .build()
            .is_ok_and(|element| element.find_property("aac-coder").is_some());
    if fast {
        format!("{encoder} aac-coder=fast")
    } else {
        encoder.to_owned()
    }
}

/// The installed AAC encoder of choice, if any.
fn aac_encoder() -> Option<&'static str> {
    AAC_ENCODERS
        .into_iter()
        .find(|name| gstreamer::ElementFactory::find(name).is_some())
}

/// The clip's sound track.
struct SoundTrack {
    src: gstreamer_app::AppSrc,
    kind: TrackKind,
    /// Clip time where the sound written so far ends.
    end_us: i64,
    /// Encoded track: samples written (where the next one goes).
    samples: i64,
}

#[derive(Clone)]
enum TrackKind {
    /// Copied as recorded, in these caps.
    Copy(gstreamer::Caps),
    /// Mono samples at this rate, encoded to AAC.
    Encode(u32),
}

/// The MP4 being written, and where the clip's timeline stands.
struct Writer {
    pipeline: gstreamer::Pipeline,
    bus: gstreamer::Bus,
    video: gstreamer_app::AppSrc,
    sound: Option<SoundTrack>,
    /// Codec name and picture size of the clip's video.
    format: (String, i32, i32),
    /// Recording time minus clip time, for the current file.
    shift_us: i64,
    /// Clip time where the pictures so far end (last picture + interval).
    end_us: i64,
    frame_us: i64,
    /// The current file's latest presentation times (recording time).
    recent_wall_us: VecDeque<i64>,
    first_wall_us: Option<i64>,
    last_wall_us: i64,
    frames: u64,
    gaps: u32,
    /// Recordings keep presentation times only; with B-frames `mp4mux`
    /// needs decoding times too. Pictures wait here (in decoding order) with
    /// their clip time until their decoding time is known.
    reorder: VecDeque<(gstreamer::Buffer, i64)>,
    reorder_pts: BinaryHeap<Reverse<i64>>,
    /// How far decoding times run before presentation times.
    dts_delay_us: i64,
    last_dts_us: Option<i64>,
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

impl Writer {
    /// `sound`: the first file's sound, which decides the clip's.
    fn new(
        caps: &gstreamer::Caps,
        codec: VideoCodec,
        sound: Option<SoundKind>,
        output: &Path,
        scratch: &Path,
    ) -> Result<Self, ExportError> {
        let failed = |error: &dyn std::fmt::Display| ExportError::Output(error.to_string());
        let parser = match codec {
            VideoCodec::H264 => "h264parse",
            VideoCodec::H265 => "h265parse",
        };
        let track = match sound {
            Some(SoundKind::Coded(caps)) => Some(TrackKind::Copy(caps)),
            Some(SoundKind::Pcm(rate)) => match aac_encoder() {
                Some(_) => Some(TrackKind::Encode(rate)),
                None => {
                    tracing::warn!("No AAC encoder installed; the clip is exported without sound");
                    None
                }
            },
            None => None,
        };
        // faststart: the index goes at the front, so the file plays while it
        // downloads (web portal); it is built in a scratch file beside it.
        let mut description = format!(
            "appsrc name=export_video format=time ! {parser} ! \
             mp4mux name=export_mux faststart=true ! filesink name=export_file"
        );
        match &track {
            Some(TrackKind::Copy(_)) => {
                description.push_str(" appsrc name=export_sound format=time ! export_mux.");
            }
            Some(TrackKind::Encode(_)) => {
                let encoder = aac_encoder_description();
                description.push_str(&format!(
                    " appsrc name=export_sound format=time ! audioconvert ! {encoder} ! export_mux."
                ));
            }
            None => {}
        }
        let pipeline = builder::build(&description).map_err(|e| failed(&e))?;
        let named = |name: &str| {
            pipeline
                .by_name(name)
                .ok_or_else(|| ExportError::Output(format!("{name} missing")))
        };
        named("export_file")?.set_property("location", output.to_string_lossy().as_ref());
        named("export_mux")?.set_property("faststart-file", scratch.to_string_lossy().as_ref());
        let appsrc = |name: &str, caps: &gstreamer::Caps| -> Result<_, ExportError> {
            let src = named(name)?
                .downcast::<gstreamer_app::AppSrc>()
                .map_err(|_| ExportError::Output(format!("{name} is not an appsrc")))?;
            src.set_caps(Some(caps));
            src.set_is_live(false);
            // Never block this thread on a full queue (the muxer may be
            // waiting for the other track); `throttle` bounds it instead.
            src.set_block(false);
            src.set_max_bytes(0);
            Ok(src)
        };
        let video = appsrc("export_video", caps)?;
        let sound = match track {
            Some(kind) => {
                let caps = match &kind {
                    TrackKind::Copy(caps) => caps.clone(),
                    TrackKind::Encode(rate) => gstreamer::Caps::builder("audio/x-raw")
                        .field("format", "S16LE")
                        .field("layout", "interleaved")
                        .field("rate", *rate as i32)
                        .field("channels", 1i32)
                        .build(),
                };
                Some(SoundTrack {
                    src: appsrc("export_sound", &caps)?,
                    kind,
                    end_us: 0,
                    samples: 0,
                })
            }
            None => None,
        };
        let bus = pipeline
            .bus()
            .ok_or_else(|| ExportError::Output("no bus".into()))?;
        pipeline
            .set_state(gstreamer::State::Playing)
            .map_err(|e| failed(&e))?;
        Ok(Self {
            pipeline,
            bus,
            video,
            sound,
            format: video_format(caps).unwrap_or_default(),
            shift_us: 0,
            end_us: 0,
            frame_us: DEFAULT_FRAME_US,
            recent_wall_us: VecDeque::new(),
            first_wall_us: None,
            last_wall_us: 0,
            frames: 0,
            gaps: 0,
            reorder: VecDeque::new(),
            reorder_pts: BinaryHeap::new(),
            dts_delay_us: 0,
            last_dts_us: None,
        })
    }

    /// How later files' sound is read.
    fn sound_plan(&self) -> SoundPlan {
        match self.sound.as_ref().map(|track| &track.kind) {
            Some(TrackKind::Copy(caps)) => SoundPlan::Copy(caps.clone()),
            Some(TrackKind::Encode(rate)) => SoundPlan::Encode(*rate),
            None => SoundPlan::Off,
        }
    }

    /// Place the clip's first pictures (decoding order) on its timeline.
    /// Clip time 0 is the first picture; decoding times before it
    /// (B-frames) move everything later by the difference, since clip times
    /// cannot be negative.
    fn begin_clip(&mut self, units: &[Unit]) {
        let Some(first) = units.first() else {
            return;
        };
        let lead = match first.dts_wall_us {
            Some(dts) => (first.wall_us - dts).max(0),
            None => {
                self.dts_delay_us = reorder_delay_us(units);
                self.dts_delay_us
            }
        };
        let start = units
            .iter()
            .map(|u| u.wall_us)
            .min()
            .unwrap_or(first.wall_us);
        self.shift_us = start - lead;
        self.recent_wall_us.clear();
    }

    /// Place a later file on the clip's timeline, from its first picture.
    fn begin_file(&mut self, first: &Unit) {
        self.recent_wall_us.clear();
        let next = self.end_us;
        let at = first.wall_us - self.shift_us;
        if at > next + MAX_KEPT_GAP_US {
            self.gaps += 1;
            self.shift_us = first.wall_us - next;
        } else if at < next {
            // Files' start times are estimates: an overlap means they follow
            // one another.
            self.shift_us = first.wall_us - next;
        }
    }

    fn write_video(&mut self, unit: Unit, cancel: &AtomicBool) -> Result<(), ExportError> {
        // The picture interval: the average spacing of the file's recent
        // presentation times (neighbours in decoding order jump about when
        // B-frames reorder them).
        self.recent_wall_us.push_back(unit.wall_us);
        if self.recent_wall_us.len() > REORDER_WINDOW {
            self.recent_wall_us.pop_front();
        }
        if self.recent_wall_us.len() >= 2 {
            let (low, high) = self
                .recent_wall_us
                .iter()
                .fold((i64::MAX, i64::MIN), |(lo, hi), &t| (lo.min(t), hi.max(t)));
            let interval = (high - low) / (self.recent_wall_us.len() as i64 - 1);
            if interval > 0 && interval < 1_000_000 {
                self.frame_us = interval;
            }
        }
        let pts_us = unit.wall_us - self.shift_us;
        let mut buffer = unit.buffer;
        buffer.make_mut().set_pts(clock(pts_us));
        match unit.dts_wall_us {
            Some(dts) => {
                buffer.make_mut().set_dts(clock(dts - self.shift_us));
                self.push_video(buffer)?;
            }
            None => {
                self.reorder.push_back((buffer, pts_us));
                self.reorder_pts.push(Reverse(pts_us));
                while self.reorder.len() > REORDER_WINDOW {
                    self.release_video()?;
                }
            }
        }
        self.frames += 1;
        self.first_wall_us.get_or_insert(unit.wall_us);
        self.last_wall_us = self.last_wall_us.max(unit.wall_us);
        self.end_us = self.end_us.max(pts_us + self.frame_us);
        self.throttle(cancel)
    }

    /// The oldest waiting picture, with the earliest waiting presentation
    /// time (less the delay) as its decoding time: decoding times rise one
    /// picture at a time and never pass presentation.
    fn release_video(&mut self) -> Result<(), ExportError> {
        let (Some((mut buffer, _)), Some(Reverse(earliest))) =
            (self.reorder.pop_front(), self.reorder_pts.pop())
        else {
            return Ok(());
        };
        let mut dts = earliest - self.dts_delay_us;
        if let Some(last) = self.last_dts_us {
            dts = dts.max(last + 1);
        }
        self.last_dts_us = Some(dts);
        buffer.make_mut().set_dts(clock(dts));
        self.push_video(buffer)
    }

    fn push_video(&mut self, buffer: gstreamer::Buffer) -> Result<(), ExportError> {
        self.video
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|flow| self.failure(&format!("{flow:?}")))
    }

    /// A piece of sound. Kept continuous: what overlaps sound already
    /// written is cut (dropped when copied), a jump is filled.
    fn write_sound(&mut self, unit: SoundUnit) -> Result<(), ExportError> {
        if self.frames == 0 {
            return Ok(());
        }
        let shift_us = self.shift_us;
        let Some(track) = self.sound.as_mut() else {
            return Ok(());
        };
        let at_us = unit.wall_us - shift_us;
        match (&track.kind, unit.data) {
            (TrackKind::Encode(rate), SoundData::Pcm(samples)) => {
                let rate = *rate;
                let at = us_to_samples(at_us, rate);
                let len = samples.len() as i64;
                if at + len <= track.samples {
                    return Ok(());
                }
                if at < track.samples {
                    let skip = (track.samples - at) as usize;
                    return self.push_samples(&samples[skip..]);
                }
                if at - track.samples > us_to_samples(SOUND_GAP_US, rate) {
                    self.fill_sound_until(samples_to_us(at, rate))?;
                }
                self.push_samples(&samples)
            }
            (TrackKind::Copy(_), SoundData::Coded(mut buffer)) => {
                let duration = unit.duration_us.max(1);
                // Mostly covered already: left out (a coded frame cannot be cut).
                if at_us + duration / 2 < track.end_us {
                    return Ok(());
                }
                if at_us > track.end_us + SOUND_GAP_US {
                    self.fill_sound_until(at_us)?;
                }
                let Some(track) = self.sound.as_mut() else {
                    return Ok(());
                };
                let pts = at_us.max(track.end_us);
                {
                    let buffer = buffer.make_mut();
                    buffer.set_pts(clock(pts));
                    buffer.set_dts(clock(pts));
                    buffer.set_duration(clock(duration));
                }
                track.end_us = pts + duration;
                if let Err(flow) = track.src.push_buffer(buffer) {
                    return Err(self.failure(&format!("{flow:?}")));
                }
                Ok(())
            }
            // A file whose sound does not fit the clip's: its time is filled.
            _ => Ok(()),
        }
    }

    /// For a file without sound, fill the clip's sound up to the pictures
    /// whenever it trails them by more than a second.
    fn keep_sound_up(&mut self, file_has_sound: bool) -> Result<(), ExportError> {
        // A file with sound: its own sound may be behind on its way (the
        // file is read far faster than real time), so it is waited for; only
        // a muxer stalled for it (`throttle`) or the file's end fills it.
        if file_has_sound {
            return Ok(());
        }
        let Some(track) = &self.sound else {
            return Ok(());
        };
        // Filled in pieces of about a second, not a picture at a time.
        if self.end_us - track.end_us > MAX_SOUND_LAG_US {
            self.fill_sound_until(self.end_us)?;
        }
        Ok(())
    }

    /// Bring the sound up to clip time `until_us`: silence for an encoded
    /// track, a GAP event (no data) for a copied one.
    fn fill_sound_until(&mut self, until_us: i64) -> Result<(), ExportError> {
        let Some(track) = self.sound.as_mut() else {
            return Ok(());
        };
        match track.kind {
            TrackKind::Encode(rate) => {
                let until = us_to_samples(until_us, rate);
                let chunk = i64::from(rate / 10).max(1);
                while self.sound.as_ref().is_some_and(|t| t.samples < until) {
                    let have = self.sound.as_ref().map_or(until, |t| t.samples);
                    self.push_samples(&vec![0; (until - have).min(chunk) as usize])?;
                }
                Ok(())
            }
            TrackKind::Copy(_) => {
                if until_us > track.end_us {
                    let gap = gstreamer::event::Gap::builder(clock(track.end_us))
                        .duration(clock(until_us - track.end_us))
                        .build();
                    track.src.send_event(gap);
                    track.end_us = until_us;
                }
                Ok(())
            }
        }
    }

    fn push_samples(&mut self, samples: &[i16]) -> Result<(), ExportError> {
        let Some(track) = self.sound.as_mut() else {
            return Ok(());
        };
        let TrackKind::Encode(rate) = track.kind else {
            return Ok(());
        };
        if samples.is_empty() {
            return Ok(());
        }
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut buffer = gstreamer::Buffer::from_mut_slice(bytes);
        let start = samples_to_us(track.samples, rate);
        let end = samples_to_us(track.samples + samples.len() as i64, rate);
        {
            let buffer = buffer.make_mut();
            buffer.set_pts(clock(start));
            buffer.set_duration(clock(end - start));
        }
        track.samples += samples.len() as i64;
        track.end_us = end;
        if let Err(flow) = track.src.push_buffer(buffer) {
            return Err(self.failure(&format!("{flow:?}")));
        }
        Ok(())
    }

    /// Wait while too much video waits for the muxer.
    fn throttle(&mut self, cancel: &AtomicBool) -> Result<(), ExportError> {
        let started = Instant::now();
        while self.video.current_level_bytes() > MAX_QUEUED_BYTES {
            if cancel.load(Ordering::Relaxed) {
                return Err(ExportError::Cancelled);
            }
            self.check_bus()?;
            if started.elapsed() > Duration::from_secs(2) {
                // The muxer may be waiting for sound that is not coming.
                self.fill_sound_until(self.end_us)?;
            }
            if started.elapsed() > STALL_TIMEOUT {
                return Err(ExportError::Output("the MP4 writer stopped".into()));
            }
            std::thread::sleep(SLICE);
        }
        self.check_bus()
    }

    fn check_bus(&self) -> Result<(), ExportError> {
        match self.bus.pop_filtered(&[gstreamer::MessageType::Error]) {
            Some(message) => Err(error_of(&message)
                .unwrap_or_else(|| ExportError::Output("the MP4 writer failed".into()))),
            None => Ok(()),
        }
    }

    /// The muxer's own error, if it reported one, else `flow`.
    fn failure(&self, flow: &str) -> ExportError {
        self.check_bus()
            .err()
            .unwrap_or_else(|| ExportError::Output(format!("the MP4 writer refused data ({flow})")))
    }

    /// Close both tracks and wait for the MP4 to be complete.
    fn finish(mut self, cancel: &AtomicBool) -> Result<RemuxSummary, ExportError> {
        while !self.reorder.is_empty() {
            self.release_video()?;
        }
        if matches!(
            self.sound.as_ref().map(|t| &t.kind),
            Some(TrackKind::Encode(_))
        ) {
            self.fill_sound_until(self.end_us)?;
        }
        let _ = self.video.end_of_stream();
        if let Some(sound) = &self.sound {
            let _ = sound.src.end_of_stream();
        }
        let deadline = Instant::now() + FINISH_TIMEOUT;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(ExportError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(ExportError::Output("finishing the MP4 timed out".into()));
            }
            let Some(message) = self.bus.timed_pop_filtered(
                gstreamer::ClockTime::from_mseconds(100),
                &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
            ) else {
                continue;
            };
            if let Some(error) = error_of(&message) {
                return Err(error);
            }
            break;
        }
        let _ = self.pipeline.set_state(gstreamer::State::Null);
        let first_wall_us = self.first_wall_us.unwrap_or(0);
        Ok(RemuxSummary {
            first_ms: first_wall_us / 1000,
            last_ms: self.last_wall_us / 1000,
            duration_ms: self.end_us / 1000,
            frames: self.frames,
            audio: self.sound.is_some(),
            sound_copied: matches!(
                self.sound.as_ref().map(|t| &t.kind),
                Some(TrackKind::Copy(_))
            ),
            gaps: self.gaps,
            skipped_files: 0,
            truncated: false,
        })
    }
}

fn error_of(message: &gstreamer::Message) -> Option<ExportError> {
    match message.view() {
        gstreamer::MessageView::Error(error) => {
            Some(ExportError::Output(error.error().to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A recording like the recorder writes: H.264 in Matroska, `seconds`
    /// long at 25 fps with a keyframe a second, with a sine tone in G.711
    /// when `sound`.
    pub(crate) fn recording(seconds: u32, sound: bool) -> String {
        write_recording(
            seconds,
            sound.then_some("audio/x-raw,rate=8000,channels=1 ! alawenc"),
        )
    }

    /// The same with AAC sound, as many cameras send it.
    fn recording_aac(seconds: u32) -> String {
        write_recording(
            seconds,
            Some("audio/x-raw,rate=8000,channels=1 ! avenc_aac ! aacparse"),
        )
    }

    fn write_recording(seconds: u32, sound: Option<&str>) -> String {
        write_recording_with(seconds, sound, 0)
    }

    fn write_recording_with(seconds: u32, sound: Option<&str>, bframes: u32) -> String {
        gstreamer::init().unwrap();
        let path = std::env::temp_dir().join(format!(
            "nw-export-src-{}.mkv",
            uuid::Uuid::new_v4().simple()
        ));
        let mut description = format!(
            "videotestsrc num-buffers={} ! video/x-raw,format=I420,width=160,height=120,framerate=25/1 ! \
             x264enc key-int-max=25 bframes={bframes} ! h264parse ! matroskamux name=mux ! filesink location=\"{}\"",
            seconds * 25,
            path.display()
        );
        if let Some(sound) = sound {
            description.push_str(&format!(
                " audiotestsrc num-buffers={} samplesperbuffer=800 ! {sound} ! mux.",
                seconds * 10
            ));
        }
        let pipeline = builder::build(&description).unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(30),
                &[gstreamer::MessageType::Eos],
            )
            .expect("recording written");
        pipeline.set_state(gstreamer::State::Null).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn segment(path: &str, start_ts: i64, seconds: i64) -> PlaybackSegment {
        PlaybackSegment {
            path: path.to_owned(),
            start_ts,
            end_ts: start_ts + seconds * 1000,
        }
    }

    fn output() -> PathBuf {
        std::env::temp_dir().join(format!("nw-export-{}.mp4", uuid::Uuid::new_v4().simple()))
    }

    /// The MP4's tracks (caps names) and duration.
    fn probe(path: &Path) -> (Vec<String>, gstreamer::ClockTime) {
        let pipeline = builder::build(&format!(
            "filesrc location=\"{}\" ! qtdemux name=demux",
            builder::gst_quote(&path.to_string_lossy())
        ))
        .unwrap();
        // Nothing is linked: the demuxer adds its pads and stops (not
        // linked), which is all this needs.
        let demux = pipeline.by_name("demux").unwrap();
        let all_pads = std::sync::Arc::new(AtomicBool::new(false));
        let found = std::sync::Arc::clone(&all_pads);
        demux.connect_no_more_pads(move |_| found.store(true, Ordering::Relaxed));
        let _ = pipeline.set_state(gstreamer::State::Paused);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !all_pads.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(SLICE);
        }
        let pads = demux.src_pads();
        let tracks = pads
            .iter()
            .filter_map(|pad| pad.current_caps())
            .filter_map(|caps| caps.structure(0).map(|s| s.name().to_string()))
            .collect();
        let duration = pads
            .first()
            .and_then(|pad| pad.query_duration::<gstreamer::ClockTime>())
            .unwrap_or_default();
        pipeline.set_state(gstreamer::State::Null).unwrap();
        (tracks, duration)
    }

    fn export(
        segments: &[PlaybackSegment],
        start_ms: i64,
        end_ms: i64,
    ) -> (Result<RemuxSummary, ExportError>, PathBuf, Vec<f64>) {
        let out = output();
        let mut seen = Vec::new();
        let result = remux(
            segments,
            start_ms,
            end_ms,
            &out,
            &AtomicBool::new(false),
            &mut |p| seen.push(p),
        );
        (result, out, seen)
    }

    #[test]
    fn a_range_across_two_files_becomes_one_mp4() {
        let (first, second) = (recording(2, false), recording(2, false));
        let segments = [segment(&first, 10_000, 2), segment(&second, 12_000, 2)];
        let (result, out, progress) = export(&segments, 10_000, 14_000);
        let summary = result.unwrap();
        assert_eq!(summary.frames, 100, "every picture of both files");
        assert_eq!(summary.first_ms, 10_000);
        assert_eq!(summary.gaps, 0);
        assert!(!summary.audio);
        assert!(
            progress.windows(2).all(|w| w[0] <= w[1]),
            "progress only grows"
        );
        let (tracks, duration) = probe(&out);
        assert_eq!(tracks, vec!["video/x-h264".to_string()]);
        let ms = duration.mseconds();
        assert!((3_900..=4_100).contains(&ms), "4 s clip, got {ms} ms");
        for path in [first, second] {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_file(out).unwrap();
    }

    #[test]
    fn a_trim_starts_at_the_keyframe_before_and_ends_before_the_end() {
        let file = recording(4, false);
        // 1.5 s in: the keyframe before is at 1.0 s; up to 3.0 s.
        let (result, out, _) = export(&[segment(&file, 100_000, 4)], 101_500, 103_000);
        let summary = result.unwrap();
        assert_eq!(summary.first_ms, 101_000);
        assert_eq!(summary.frames, 50, "1.0 s – 3.0 s at 25 fps");
        assert!(summary.last_ms < 103_000);
        let (_, duration) = probe(&out);
        assert!((1_900..=2_100).contains(&duration.mseconds()));
        std::fs::remove_file(file).unwrap();
        std::fs::remove_file(out).unwrap();
    }

    #[test]
    fn a_long_gap_between_files_is_left_out() {
        let (first, second) = (recording(1, false), recording(1, false));
        // An hour between the two recordings.
        let segments = [segment(&first, 0, 1), segment(&second, 3_600_000, 1)];
        let (result, out, _) = export(&segments, 0, 3_601_000);
        let summary = result.unwrap();
        assert_eq!(summary.gaps, 1);
        assert_eq!(summary.frames, 50);
        assert!((1_900..=2_100).contains(&summary.duration_ms));
        for path in [first, second] {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_file(out).unwrap();
    }

    #[test]
    fn camera_sound_is_exported_as_aac() {
        gstreamer::init().unwrap();
        if aac_encoder().is_none() {
            eprintln!("no AAC encoder installed; skipped");
            return;
        }
        let file = recording(2, true);
        let (result, out, _) = export(&[segment(&file, 0, 2)], 0, 2_000);
        let summary = result.unwrap();
        assert!(summary.audio);
        let (tracks, _) = probe(&out);
        assert!(tracks.contains(&"video/x-h264".to_string()), "{tracks:?}");
        assert!(tracks.contains(&"audio/mpeg".to_string()), "{tracks:?}");
        std::fs::remove_file(file).unwrap();
        std::fs::remove_file(out).unwrap();
    }

    #[test]
    fn camera_sound_is_encoded_at_its_own_rate() {
        gstreamer::init().unwrap();
        if aac_encoder().is_none() {
            eprintln!("no AAC encoder installed; skipped");
            return;
        }
        let file = recording(2, true);
        let (result, out, _) = export(&[segment(&file, 0, 2)], 0, 2_000);
        let summary = result.unwrap();
        assert!(summary.audio && !summary.sound_copied, "G.711 is encoded");
        let rates = sound_rates(&out);
        assert_eq!(
            rates,
            vec![8_000],
            "kept at the camera's rate, not resampled"
        );
        std::fs::remove_file(file).unwrap();
        std::fs::remove_file(out).unwrap();
    }

    #[test]
    fn aac_sound_is_copied_without_decoding() {
        gstreamer::init().unwrap();
        if gstreamer::ElementFactory::find("avenc_aac").is_none() {
            eprintln!("no AAC encoder to write the test recording; skipped");
            return;
        }
        let (first, second) = (recording_aac(2), recording_aac(2));
        let segments = [segment(&first, 0, 2), segment(&second, 2_000, 2)];
        let (result, out, _) = export(&segments, 0, 4_000);
        let summary = result.unwrap();
        assert!(summary.audio && summary.sound_copied);
        let (tracks, duration) = probe(&out);
        assert!(tracks.contains(&"audio/mpeg".to_string()), "{tracks:?}");
        assert!((3_900..=4_100).contains(&duration.mseconds()));
        for path in [first, second] {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_file(out).unwrap();
    }

    #[test]
    fn a_file_without_sound_does_not_stall_a_copied_track() {
        gstreamer::init().unwrap();
        if gstreamer::ElementFactory::find("avenc_aac").is_none() {
            eprintln!("no AAC encoder to write the test recording; skipped");
            return;
        }
        // A long silent file: well over the video the muxer queue holds.
        let (first, second, third) = (recording_aac(2), recording(20, false), recording_aac(2));
        let segments = [
            segment(&first, 0, 2),
            segment(&second, 2_000, 20),
            segment(&third, 22_000, 2),
        ];
        let started = Instant::now();
        let (result, out, _) = export(&segments, 0, 24_000);
        let summary = result.unwrap();
        assert!(started.elapsed() < Duration::from_secs(20), "no stall");
        assert!(summary.sound_copied);
        assert_eq!(summary.frames, 600);
        let (tracks, duration) = probe(&out);
        assert!(tracks.contains(&"audio/mpeg".to_string()), "{tracks:?}");
        assert!((23_900..=24_100).contains(&duration.mseconds()));
        for path in [first, second, third] {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_file(out).unwrap();
    }

    /// The sample rates of the MP4's sound tracks.
    fn sound_rates(path: &Path) -> Vec<i32> {
        let pipeline = builder::build(&format!(
            "filesrc location=\"{}\" ! qtdemux name=demux",
            builder::gst_quote(&path.to_string_lossy())
        ))
        .unwrap();
        let demux = pipeline.by_name("demux").unwrap();
        let all_pads = std::sync::Arc::new(AtomicBool::new(false));
        let found = std::sync::Arc::clone(&all_pads);
        demux.connect_no_more_pads(move |_| found.store(true, Ordering::Relaxed));
        let _ = pipeline.set_state(gstreamer::State::Paused);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !all_pads.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(SLICE);
        }
        let rates = demux
            .src_pads()
            .iter()
            .filter_map(|pad| pad.current_caps())
            .filter_map(|caps| {
                let s = caps.structure(0)?;
                s.name()
                    .starts_with("audio/")
                    .then(|| s.get::<i32>("rate").ok())?
            })
            .collect();
        pipeline.set_state(gstreamer::State::Null).unwrap();
        rates
    }

    #[test]
    fn b_frame_recordings_get_decoding_times_and_keep_their_length() {
        // Matroska keeps presentation times only; without decoding times
        // mp4mux garbles reordered pictures (a 4 s clip came out 8.8 s).
        let (first, second) = (
            write_recording_with(2, None, 2),
            write_recording_with(2, None, 2),
        );
        let segments = [segment(&first, 0, 2), segment(&second, 2_000, 2)];
        let (result, out, _) = export(&segments, 0, 4_000);
        let summary = result.unwrap();
        assert_eq!(summary.frames, 100);
        let (_, duration) = probe(&out);
        assert!(
            (3_900..=4_200).contains(&duration.mseconds()),
            "4 s clip, got {duration}"
        );
        for path in [first, second] {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_file(out).unwrap();
    }

    #[test]
    fn reorder_delay_is_how_far_pictures_run_ahead() {
        let unit = |wall_us: i64| Unit {
            buffer: gstreamer::Buffer::new(),
            keyframe: false,
            wall_us,
            dts_wall_us: None,
        };
        gstreamer::init().unwrap();
        // I0 P3 B1 B2 (decoding order), 40 ms apart.
        let reordered = [0, 120_000, 40_000, 80_000].map(unit);
        assert_eq!(reorder_delay_us(&reordered), 40_000);
        let in_order = [0, 40_000, 80_000].map(unit);
        assert_eq!(reorder_delay_us(&in_order), 0);
    }

    #[test]
    fn nothing_in_range_or_unreadable_files_are_errors() {
        let file = recording(1, false);
        let (result, out, _) = export(&[segment(&file, 0, 1)], 5_000, 6_000);
        assert!(matches!(result, Err(ExportError::NoRecordings)));
        let _ = std::fs::remove_file(&out);

        let missing = segment("/nonexistent/nw-export.mkv", 0, 1);
        let (result, out, _) = export(&[missing], 0, 1_000);
        assert!(matches!(result, Err(ExportError::Unreadable)));
        let _ = std::fs::remove_file(&out);
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn a_cancelled_export_stops() {
        let file = recording(2, false);
        let out = output();
        let result = remux(
            &[segment(&file, 0, 2)],
            0,
            2_000,
            &out,
            &AtomicBool::new(true),
            &mut |_| {},
        );
        assert!(matches!(result, Err(ExportError::Cancelled)));
        let _ = std::fs::remove_file(&out);
        std::fs::remove_file(file).unwrap();
    }
}
