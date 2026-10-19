//! Browser playback of recordings, still compressed.
//!
//! A recording is demuxed and parsed into its own H.264/H.265 access units
//! ([`crate::pipeline::encoded::build_pulled`]) and forwarded, paced by their
//! timestamps, into the same pre-encoded LiveKit track as live view; the
//! browser decodes. Nothing here decodes or encodes.
//!
//! - **Seek**: the file is entered at the keyframe before the asked moment;
//!   pictures before the moment are sent quickly ([`CATCH_UP_SPEED`]) so the
//!   browser arrives there, and paced playback starts at the moment itself.
//! - **Speed**: units are paced at `speed × real time`; a change re-anchors
//!   the clock at the current unit. **Pause** stops pulling, which holds the
//!   file where it is; a replay opened (or sought) while paused still sends
//!   the picture at its moment, so the viewer shows it.
//! - **Segments**: one recording run's consecutive, finished files play as
//!   one stream (`splitmuxsrc`, grouped as desktop playback groups them:
//!   [`session::play_units`]), so the picture, its pacing and the sound run
//!   on across file boundaries; other files play one after another, each
//!   from a keyframe. An unreadable one is skipped.
//! - A recording in a codec browsers are not sent (MJPEG) ends the
//!   publication as unsupported.
//! - **Audio**: a recording's audio track is decoded to mono samples and
//!   handed over in step with the pictures (each picture releases the sound
//!   up to its own time), at 1× only: sound is left out while catching up to
//!   a seek and at other speeds. Only the audio is decoded; a track no
//!   installed decoder reads is left out.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer::prelude::*;

use crate::cloud::media_publish::{EncodedSubmitter, PublishError, AUDIO_RATE};
use crate::pipeline::{audio, builder, encoded};
use crate::playback::session::{self, PlayUnit, PlaybackSegment};

/// Pace of the pictures between a seek's keyframe and the asked moment.
pub const CATCH_UP_SPEED: f64 = 16.0;
/// Capture-time spacing of catch-up pictures (the bridge's resent GOP uses
/// the same): far closer than any real frame interval.
const CATCH_UP_STAMP_US: i64 = 1_000;
/// Longest single wait, so stop, pause and speed changes act promptly.
const SLICE: Duration = Duration::from_millis(20);
/// Longest wait for a file to start delivering.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const DEMUX: &str = "replay_demux";
/// The speed at which sound is played.
const AUDIO_SPEED_PERCENT: u32 = 100;

#[derive(Default)]
struct Controls {
    stop: AtomicBool,
    paused: AtomicBool,
    speed_percent: AtomicU32,
    /// Recording time (ms since the epoch) of the unit last sent, or `i64::MIN`.
    position_ms: AtomicI64,
}

/// A running replay. Dropping it does not stop it; call [`EncodedReplay::stop`].
pub struct EncodedReplay {
    controls: Arc<Controls>,
}

impl EncodedReplay {
    /// Play `segments` from `offset_ms` into the first one, into `units`.
    pub fn spawn(
        segments: Vec<PlaybackSegment>,
        offset_ms: i64,
        speed_percent: u16,
        playing: bool,
        units: Arc<EncodedSubmitter>,
    ) -> Self {
        let controls = Arc::new(Controls {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(!playing),
            speed_percent: AtomicU32::new(u32::from(speed_percent.max(1))),
            position_ms: AtomicI64::new(i64::MIN),
        });
        let running = Arc::clone(&controls);
        let _ = std::thread::Builder::new()
            .name("nw-replay".into())
            .spawn(move || run(&segments, offset_ms, &running, &units));
        Self { controls }
    }

    pub fn set_speed_percent(&self, percent: u16) {
        self.controls
            .speed_percent
            .store(u32::from(percent.max(1)), Ordering::Relaxed);
    }

    pub fn set_playing(&self, playing: bool) {
        self.controls.paused.store(!playing, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.controls.stop.store(true, Ordering::Relaxed);
    }

    /// Recording time (ms since the epoch) of the picture last sent.
    pub fn position_ms(&self) -> Option<i64> {
        let ms = self.controls.position_ms.load(Ordering::Relaxed);
        (ms != i64::MIN).then_some(ms)
    }
}

enum Outcome {
    Ended,
    Stopped,
    Failed,
    Unsupported,
}

fn run(
    segments: &[PlaybackSegment],
    offset_ms: i64,
    controls: &Controls,
    units: &EncodedSubmitter,
) {
    let units_to_play = session::play_units(segments);
    for (index, unit) in units_to_play.iter().enumerate() {
        let offset = if index == 0 { offset_ms.max(0) } else { 0 };
        match replay_segment(unit, offset, controls, units) {
            Outcome::Ended | Outcome::Failed if index + 1 < units_to_play.len() => continue,
            Outcome::Unsupported => {
                tracing::warn!("Recording is not H.264 or H.265; browser playback is unavailable");
                units.fail(PublishError::UnsupportedSource);
                return;
            }
            _ => return,
        }
    }
}

fn replay_segment(
    segment: &PlayUnit,
    offset_ms: i64,
    controls: &Controls,
    units: &EncodedSubmitter,
) -> Outcome {
    let Ok((pipeline, slot, sound)) = open_recording(&segment.paths) else {
        return Outcome::Failed;
    };
    let outcome = play(
        &pipeline, &slot, &sound, segment, offset_ms, controls, units,
    );
    let _ = pipeline.set_state(gstreamer::State::Null);
    outcome
}

/// A recording opened for reading, still compressed: its pipeline, the slot
/// that receives the video's access-unit sink (and codec) once the demuxer
/// finds the track, and the sink of its decoded sound. Several files of one
/// run are read as one stream by `splitmuxsrc`, whose video and audio pads
/// are routed as a single file's demuxer pads are.
fn open_recording(
    paths: &[String],
) -> anyhow::Result<(gstreamer::Pipeline, encoded::PulledSink, SoundSink)> {
    let description = match paths {
        [path] => format!(
            "filesrc location=\"{}\" ! matroskademux name={DEMUX} parsebin name=encoded_parse",
            builder::gst_quote(path)
        ),
        [] => anyhow::bail!("no recording to open"),
        _ => format!("splitmuxsrc name={DEMUX} parsebin name=encoded_parse"),
    };
    let (pipeline, slot) = encoded::build_pulled(&description)?;
    if paths.len() > 1 {
        let source = pipeline
            .by_name(DEMUX)
            .ok_or_else(|| anyhow::anyhow!("run source missing"))?;
        session::give_run_files(&source, paths);
    }
    let sound = route_tracks(&pipeline);
    Ok((pipeline, slot, sound))
}

/// The recording's decoded audio (mono, [`AUDIO_RATE`] Hz), once its track is
/// found (and decodable).
type SoundSink = Arc<Mutex<Option<gstreamer_app::AppSink>>>;

/// Send the recording's video to the parser and its audio, when an installed
/// decoder reads it, to a sink of mono samples; anything else stays unlinked.
fn route_tracks(pipeline: &gstreamer::Pipeline) -> SoundSink {
    let sound = SoundSink::default();
    let (Some(demux), Some(parse)) = (pipeline.by_name(DEMUX), pipeline.by_name("encoded_parse"))
    else {
        return sound;
    };
    let bin = pipeline.clone();
    let found = Arc::clone(&sound);
    demux.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let name = caps
            .structure(0)
            .map(|s| s.name().to_string())
            .unwrap_or_default();
        if name.starts_with("audio/") {
            let Some(decoder) = audio_decoder(&caps) else {
                tracing::debug!(caps = %name, "No decoder for the recording's audio; replaying video only");
                return;
            };
            match link_sound(&bin, pad, decoder) {
                Ok(sink) => {
                    if let Ok(mut found) = found.lock() {
                        found.get_or_insert(sink);
                    }
                }
                Err(error) => tracing::debug!(%error, "Recording audio could not be linked"),
            }
            return;
        }
        if let Some(target) = parse.static_pad("sink").filter(|target| !target.is_linked()) {
            let _ = pad.link(&target);
        }
    });
    sound
}

/// The installed decoder of the highest rank for `caps`: `Some(None)` for raw
/// PCM, which needs none, and `None` when no decoder reads it. Picked here
/// rather than by `decodebin`, which would hold the pipeline's preroll until
/// the first sound arrives, after the first picture the replay waits for.
pub(crate) fn audio_decoder(caps: &gstreamer::Caps) -> Option<Option<gstreamer::ElementFactory>> {
    if caps.structure(0).is_some_and(|s| s.name() == "audio/x-raw") {
        return Some(None);
    }
    // Listed by decreasing rank.
    gstreamer::ElementFactory::factories_with_type(
        gstreamer::ElementFactoryType::DECODER | gstreamer::ElementFactoryType::MEDIA_AUDIO,
        gstreamer::Rank::MARGINAL,
    )
    .into_iter()
    .find(|factory| factory.can_sink_any_caps(caps))
    .map(Some)
}

/// `pad` → queue → decoder → mono [`AUDIO_RATE`] Hz → an appsink. The queue
/// gives the sound its own thread, so a sink waiting for its first buffer
/// (prerolling) or for its sound to be taken never holds up the demuxer and
/// with it the pictures, which pace the file.
fn link_sound(
    bin: &gstreamer::Pipeline,
    pad: &gstreamer::Pad,
    decoder: Option<gstreamer::ElementFactory>,
) -> anyhow::Result<gstreamer_app::AppSink> {
    use std::str::FromStr as _;
    let queue = gstreamer::ElementFactory::make("queue")
        .property("max-size-buffers", 0u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 5_000_000_000u64)
        .build()?;
    let decoder = decoder
        .map(|factory| factory.create().build())
        .transpose()?;
    let convert = gstreamer::ElementFactory::make("audioconvert").build()?;
    let resample = gstreamer::ElementFactory::make("audioresample").build()?;
    let filter = gstreamer::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gstreamer::Caps::from_str(&format!(
                "audio/x-raw,format=S16LE,layout=interleaved,rate={AUDIO_RATE},channels=1"
            ))?,
        )
        .build()?;
    let sink = gstreamer_app::AppSink::builder()
        .sync(false)
        .async_(false)
        .build();
    let mut chain = vec![queue.clone()];
    chain.extend(decoder);
    chain.extend([convert, resample, filter, sink.clone().upcast()]);
    bin.add_many(&chain)?;
    gstreamer::Element::link_many(&chain)?;
    for element in &chain {
        element.sync_state_with_parent()?;
    }
    let target = queue
        .static_pad("sink")
        .ok_or_else(|| anyhow::anyhow!("queue has no sink pad"))?;
    pad.link(&target)?;
    Ok(sink)
}

/// Decoded sound waiting for the picture of its time.
#[derive(Default)]
struct Sound {
    /// (stream time in µs, mono samples)
    pending: VecDeque<(i64, Vec<i16>)>,
}

impl Sound {
    /// Take what the decoder has produced so far.
    fn collect(&mut self, sink: &SoundSink) {
        let Some(sink) = sink.lock().ok().and_then(|sink| sink.clone()) else {
            return;
        };
        while let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::ZERO) {
            let Some(at) = stream_time_us(&sample) else {
                continue;
            };
            if let Some(samples) = audio::pcm_samples(&sample) {
                self.pending.push_back((at, samples));
            }
        }
    }

    /// Hand over the sound up to `until_us`, or drop it when it is not heard.
    fn release(&mut self, until_us: i64, heard: bool, units: &EncodedSubmitter) {
        while self.pending.front().is_some_and(|(at, _)| *at <= until_us) {
            if let Some((_, samples)) = self.pending.pop_front() {
                if heard {
                    units.submit_audio(samples);
                }
            }
        }
    }
}

/// A sample's place in the file (stream time), in µs.
pub(crate) fn stream_time_us(sample: &gstreamer::Sample) -> Option<i64> {
    let pts = sample.buffer()?.pts()?;
    let time = sample
        .segment()
        .and_then(|segment| {
            segment
                .downcast_ref::<gstreamer::ClockTime>()?
                .to_stream_time(pts)
        })
        .unwrap_or(pts);
    i64::try_from(time.useconds()).ok()
}

fn play(
    pipeline: &gstreamer::Pipeline,
    slot: &encoded::PulledSink,
    sound_sink: &SoundSink,
    segment: &PlayUnit,
    offset_ms: i64,
    controls: &Controls,
    units: &EncodedSubmitter,
) -> Outcome {
    if pipeline.set_state(gstreamer::State::Paused).is_err() {
        return Outcome::Failed;
    }
    let deadline = Instant::now() + OPEN_TIMEOUT;
    let (sink, codec) = loop {
        if controls.stop.load(Ordering::Relaxed) {
            return Outcome::Stopped;
        }
        match slot.lock().ok().and_then(|found| found.clone()) {
            Some(Ok(found)) => break found,
            Some(Err(_)) => return Outcome::Unsupported,
            None if Instant::now() >= deadline => return Outcome::Failed,
            None => std::thread::sleep(SLICE),
        }
    };
    let _ = pipeline.state(gstreamer::ClockTime::from_seconds(5));
    if offset_ms > 0 {
        // Into the file at the keyframe before the moment: playback can only
        // start at a keyframe.
        let _ = pipeline.seek_simple(
            gstreamer::SeekFlags::FLUSH
                | gstreamer::SeekFlags::KEY_UNIT
                | gstreamer::SeekFlags::SNAP_BEFORE,
            gstreamer::ClockTime::from_mseconds(offset_ms as u64),
        );
        let _ = pipeline.state(gstreamer::ClockTime::from_seconds(5));
    }
    // A new file (or a seek) starts the viewer again at its keyframe.
    units.resync();
    if pipeline.set_state(gstreamer::State::Playing).is_err() {
        return Outcome::Failed;
    }

    let moment_us = offset_ms * 1000;
    // (wall time, stream time, speed) the pacing clock is anchored at.
    let mut anchor: Option<(Instant, i64, f64)> = None;
    // Whether the picture at the moment asked for has been sent.
    let mut shown = false;
    let mut sound = Sound::default();
    let mut last_stamp_us: Option<i64> = None;
    loop {
        if controls.stop.load(Ordering::Relaxed) {
            return Outcome::Stopped;
        }
        if shown && controls.paused.load(Ordering::Relaxed) {
            anchor = None;
            std::thread::sleep(SLICE);
            continue;
        }
        let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(200)) else {
            if sink.is_eos() {
                return Outcome::Ended;
            }
            continue;
        };
        let Some(mut unit) = encoded::access_unit(&sample, codec) else {
            continue;
        };
        let pts = unit.pts_us.unwrap_or(0);
        let catching_up = pts < moment_us;
        let speed = if catching_up {
            CATCH_UP_SPEED
        } else {
            f64::from(controls.speed_percent.load(Ordering::Relaxed)) / 100.0
        };
        let (start, base, anchored_speed) = match anchor {
            Some(anchor) if anchor.2 == speed && pts >= anchor.1 => anchor,
            _ => (Instant::now(), pts, speed),
        };
        anchor = Some((start, base, anchored_speed));
        let due = start + Duration::from_micros(((pts - base) as f64 / speed).max(0.0) as u64);
        // Wait for the unit's moment, still answering stop, pause and speed.
        while Instant::now() < due {
            if controls.stop.load(Ordering::Relaxed) {
                return Outcome::Stopped;
            }
            if shown && controls.paused.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(SLICE.min(due.saturating_duration_since(Instant::now())));
        }
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_micros() as i64);
        // Pictures before the moment asked for are a catch-up, stamped 1 ms
        // apart (as the live bridge stamps a resent GOP), so the browser can
        // tell them from pictures at their own pace and keep them out of view.
        // The first picture goes 1 ms after the publication's previous one
        // (a seek's old video), even when nothing catches up: the browser
        // tells the seek's pictures from the old video still arriving by it.
        let previous = match last_stamp_us {
            Some(last) => catching_up.then_some(last),
            None => units.last_timestamp_us(),
        };
        unit.timestamp_us = previous.map_or(now_us, |last| last + CATCH_UP_STAMP_US);
        last_stamp_us = Some(unit.timestamp_us);
        units.submit(unit);
        sound.collect(sound_sink);
        let heard =
            !catching_up && controls.speed_percent.load(Ordering::Relaxed) == AUDIO_SPEED_PERCENT;
        sound.release(pts, heard, units);
        controls
            .position_ms
            .store(segment.start_ts + pts / 1000, Ordering::Relaxed);
        if !catching_up && !shown {
            shown = true;
            // The picture asked for is out: a paused replay holds here.
            anchor = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::media_publish::encoded_channel;

    /// Write a recording like the recorder does: an H.264 Matroska file, here
    /// from a test encoder, `seconds` long at 25 fps with a keyframe a second.
    fn recording(seconds: u32) -> String {
        gstreamer::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("nw-replay-{}.mkv", uuid::Uuid::new_v4().simple()));
        let pipeline = builder::build(&format!(
            "videotestsrc num-buffers={} ! video/x-raw,format=I420,width=160,height=120,framerate=25/1 ! \
             x264enc key-int-max=25 bframes=0 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            seconds * 25,
            path.display()
        ))
        .unwrap();
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

    fn segment(path: &str, start_ts: i64) -> PlaybackSegment {
        PlaybackSegment {
            path: path.to_owned(),
            start_ts,
            end_ts: start_ts + 2_000,
        }
    }

    /// Units received within `within`, with when each arrived.
    async fn receive(
        feed: &crate::cloud::media_publish::EncodedFeed,
        within: Duration,
    ) -> Vec<(Instant, nw_core::encoded::EncodedAccessUnit)> {
        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + within;
        while let Ok(Some(unit)) = tokio::time::timeout_at(deadline, feed.next()).await {
            got.push((Instant::now(), unit));
        }
        got
    }

    #[tokio::test]
    async fn a_recording_replays_compressed_at_its_own_pace_across_segments() {
        let (first, second) = (recording(2), recording(2));
        let (submitter, feed) = encoded_channel();
        let replay = EncodedReplay::spawn(
            vec![segment(&first, 1_000_000), segment(&second, 1_002_000)],
            0,
            100,
            true,
            Arc::new(submitter),
        );
        let got = receive(&feed, Duration::from_millis(4_500)).await;
        replay.stop();
        assert_eq!(got.len(), 100, "every picture of both files");
        assert!(
            got[0].1.keyframe && got[50].1.keyframe,
            "each file starts at a keyframe"
        );
        assert_eq!(got.iter().filter(|(_, unit)| unit.keyframe).count(), 4);
        let span = got[49].0 - got[0].0;
        assert!(
            span > Duration::from_millis(1_800) && span < Duration::from_millis(2_300),
            "real time: {span:?}"
        );
        assert_eq!(replay.position_ms(), Some(1_003_960));
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
    }

    /// A recording run as the recorder writes it (`splitmuxsink`, the
    /// recorder's file names), ~1.5 s files, indexed as the backend does.
    fn recorded_run(seconds: u32) -> (std::path::PathBuf, Vec<PlaybackSegment>) {
        gstreamer::init().unwrap();
        let dir =
            std::env::temp_dir().join(format!("nw-replay-run-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let cam = "cam-run";
        let run: i64 = 1_789_659_000_000;
        let pipeline = builder::build(&format!(
            "videotestsrc num-buffers={} ! video/x-raw,format=I420,width=160,height=120,framerate=25/1 ! \
             x264enc key-int-max=10 bframes=0 ! h264parse ! splitmuxsink location=\"{}\" \
             max-size-time=1500000000 muxer-factory=matroskamux",
            seconds * 25,
            dir.join(format!("{cam}_{run}_1_%05d.mkv")).display()
        ))
        .unwrap();
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
        let old = std::time::SystemTime::now() - Duration::from_secs(600);
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            std::fs::File::options()
                .write(true)
                .open(entry.path())
                .and_then(|f| f.set_modified(old))
                .unwrap();
        }
        let db = Arc::new(Mutex::new(crate::db::connection::open_in_memory().unwrap()));
        crate::recording::segment_tracker::scan_and_record(&db, &cam.to_owned(), &dir, 1_500);
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT path, start_ts, end_ts FROM segments ORDER BY start_ts")
            .unwrap();
        let segments = stmt
            .query_map([], |r| {
                Ok(PlaybackSegment {
                    path: r.get(0)?,
                    start_ts: r.get(1)?,
                    end_ts: r.get(2)?,
                })
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(segments.len() >= 3, "{segments:?}");
        (dir, segments)
    }

    #[tokio::test]
    async fn a_recording_runs_files_replay_as_one_stream_without_a_hitch() {
        let (dir, segments) = recorded_run(4);
        assert_eq!(
            session::play_units(&segments).len(),
            1,
            "one run, one stream"
        );
        let first_start = segments[0].start_ts;
        let (submitter, feed) = encoded_channel();
        let replay = EncodedReplay::spawn(segments.clone(), 0, 100, true, Arc::new(submitter));
        let got = receive(&feed, Duration::from_millis(6_000)).await;
        replay.stop();
        assert_eq!(got.len(), 100, "every picture of every file");
        // One timeline across the files (a file played on its own would
        // start again at 0): pictures 40 ms apart from first to last.
        let pts: Vec<i64> = got
            .iter()
            .map(|(_, unit)| unit.pts_us.unwrap_or(-1))
            .collect();
        assert!(
            pts.windows(2).all(|w| w[1] - w[0] == 40_000),
            "continuous: {pts:?}"
        );
        // Paced in real time straight through the file changes.
        let worst = got.windows(2).map(|w| w[1].0 - w[0].0).max().unwrap();
        assert!(worst < Duration::from_millis(120), "a {worst:?} stall");
        let span = got[99].0 - got[0].0;
        assert!(
            span > Duration::from_millis(3_700) && span < Duration::from_millis(4_300),
            "real time: {span:?}"
        );
        assert_eq!(replay.position_ms(), Some(first_start + 3_960));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_seek_starts_at_the_keyframe_before_and_catches_up_to_the_moment() {
        let file = recording(3);
        let (submitter, feed) = encoded_channel();
        // 1.6 s in: the keyframe before is at 1.0 s, 15 pictures earlier.
        let replay = EncodedReplay::spawn(
            vec![segment(&file, 5_000)],
            1_600,
            100,
            true,
            Arc::new(submitter),
        );
        let got = receive(&feed, Duration::from_millis(600)).await;
        replay.stop();
        assert!(got[0].1.keyframe);
        assert_eq!(got[0].1.pts_us, Some(1_000_000));
        // The 15 pictures before the moment arrive at 16x: well under the 600 ms they span.
        let catch_up = got[14].0 - got[0].0;
        assert!(catch_up < Duration::from_millis(150), "{catch_up:?}");
        std::fs::remove_file(file).unwrap();
    }

    #[tokio::test]
    async fn a_seeks_first_picture_follows_the_previous_replays_last() {
        // The portal tells a seek's pictures from the old video still arriving
        // by this: the seek's first is stamped 1 ms after the old video's
        // last, also when the seek lands on a keyframe and nothing catches up.
        let file = recording(3);
        let (submitter, feed) = encoded_channel();
        let submitter = Arc::new(submitter);
        let before = EncodedReplay::spawn(
            vec![segment(&file, 5_000)],
            0,
            100,
            true,
            Arc::clone(&submitter),
        );
        let mut old = receive(&feed, Duration::from_millis(400)).await;
        before.stop();
        // Whatever it still hands over before it stops is the old video too.
        old.extend(receive(&feed, Duration::from_millis(500)).await);
        let old_last = old.iter().map(|(_, unit)| unit.timestamp_us).max().unwrap();
        // 1.0 s in: exactly a keyframe.
        let after = EncodedReplay::spawn(
            vec![segment(&file, 5_000)],
            1_000,
            100,
            true,
            Arc::clone(&submitter),
        );
        let got = receive(&feed, Duration::from_millis(300)).await;
        after.stop();
        assert!(got[0].1.keyframe);
        assert_eq!(got[0].1.pts_us, Some(1_000_000));
        assert_eq!(got[0].1.timestamp_us, old_last + CATCH_UP_STAMP_US);
        // The pictures after it are at their own pace again.
        assert!(got[1].1.timestamp_us - got[0].1.timestamp_us > 200_000);
        std::fs::remove_file(file).unwrap();
    }

    #[tokio::test]
    async fn speed_and_pause_apply_in_place() {
        let file = recording(3);
        let (submitter, feed) = encoded_channel();
        let replay =
            EncodedReplay::spawn(vec![segment(&file, 0)], 0, 400, true, Arc::new(submitter));
        let fast = receive(&feed, Duration::from_millis(500)).await;
        assert!(
            fast.len() >= 40,
            "4x: about 50 pictures in 0.5 s, got {}",
            fast.len()
        );
        replay.set_playing(false);
        let _ = receive(&feed, Duration::from_millis(100)).await;
        let paused = receive(&feed, Duration::from_millis(400)).await;
        assert!(paused.is_empty(), "paused: nothing sent");
        replay.set_speed_percent(100);
        replay.set_playing(true);
        let normal = receive(&feed, Duration::from_millis(400)).await;
        assert!(
            (7..=13).contains(&normal.len()),
            "1x: about 10 in 0.4 s, got {}",
            normal.len()
        );
        replay.stop();
        std::fs::remove_file(file).unwrap();
    }

    /// A recording with G.711 A-law sound, as the recorder writes one.
    fn recording_with_sound(seconds: u32) -> String {
        gstreamer::init().unwrap();
        let path = std::env::temp_dir().join(format!(
            "nw-replay-av-{}.mkv",
            uuid::Uuid::new_v4().simple()
        ));
        let pipeline = builder::build(&format!(
            "matroskamux name=mux ! filesink location=\"{}\" \
             videotestsrc num-buffers={} ! video/x-raw,format=I420,width=160,height=120,framerate=25/1 ! \
             x264enc key-int-max=25 bframes=0 ! h264parse ! mux. \
             audiotestsrc num-buffers={} samplesperbuffer=800 ! audio/x-raw,rate=8000,channels=1 ! \
             alawenc ! mux.",
            path.display(),
            seconds * 25,
            seconds * 10,
        ))
        .unwrap();
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

    /// Samples received on the audio side within `within`.
    async fn sound(audio: &mut tokio::sync::mpsc::Receiver<Vec<i16>>, within: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + within;
        let mut samples = 0;
        while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, audio.recv()).await {
            samples += chunk.len();
        }
        samples
    }

    #[tokio::test]
    async fn a_recordings_sound_is_replayed_with_its_pictures_at_normal_speed() {
        let file = recording_with_sound(3);
        let (submitter, feed) = encoded_channel();
        let mut audio = feed.take_audio().unwrap();
        let replay =
            EncodedReplay::spawn(vec![segment(&file, 0)], 0, 100, true, Arc::new(submitter));
        let video = tokio::spawn(async move { receive(&feed, Duration::from_millis(1_000)).await });
        let samples = sound(&mut audio, Duration::from_millis(1_000)).await;
        let pictures = video.await.unwrap();
        replay.stop();
        std::fs::remove_file(file).unwrap();
        assert!(
            pictures.len() >= 20,
            "the pictures still play: {}",
            pictures.len()
        );
        // About a second of 48 kHz sound, handed over as the pictures go.
        let rate = AUDIO_RATE as usize;
        assert!(
            (rate * 7 / 10..=rate * 13 / 10).contains(&samples),
            "{samples} samples in about 1 s"
        );
    }

    #[tokio::test]
    async fn sound_is_left_out_at_other_speeds() {
        let file = recording_with_sound(3);
        let (submitter, feed) = encoded_channel();
        let mut audio = feed.take_audio().unwrap();
        let replay =
            EncodedReplay::spawn(vec![segment(&file, 0)], 0, 400, true, Arc::new(submitter));
        let video = tokio::spawn(async move { receive(&feed, Duration::from_millis(500)).await });
        let samples = sound(&mut audio, Duration::from_millis(500)).await;
        let pictures = video.await.unwrap();
        replay.stop();
        std::fs::remove_file(file).unwrap();
        assert!(pictures.len() >= 40, "4x pictures: {}", pictures.len());
        assert_eq!(samples, 0, "no sound at 4x");
    }

    #[tokio::test]
    async fn a_recording_in_another_codec_ends_as_unsupported() {
        gstreamer::init().unwrap();
        let path = std::env::temp_dir().join(format!(
            "nw-replay-jpeg-{}.mkv",
            uuid::Uuid::new_v4().simple()
        ));
        let writer = builder::build(&format!(
            "videotestsrc num-buffers=10 ! jpegenc ! jpegparse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ))
        .unwrap();
        writer.set_state(gstreamer::State::Playing).unwrap();
        writer
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(20),
                &[gstreamer::MessageType::Eos],
            )
            .unwrap();
        writer.set_state(gstreamer::State::Null).unwrap();
        let (submitter, feed) = encoded_channel();
        let _replay = EncodedReplay::spawn(
            vec![segment(path.to_str().unwrap(), 0)],
            0,
            100,
            true,
            Arc::new(submitter),
        );
        assert!(tokio::time::timeout(Duration::from_secs(5), feed.next())
            .await
            .unwrap()
            .is_none());
        assert_eq!(feed.failure(), Some(PublishError::UnsupportedSource));
        std::fs::remove_file(path).unwrap();
    }

    /// A recording file whose timestamps start at `offset_s`, as a run's later
    /// fragments do (splitmuxsink continues the run's timeline).
    fn fragment(offset_s: u64, seconds: u32) -> String {
        gstreamer::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("nw-fragment-{}.mkv", uuid::Uuid::new_v4().simple()));
        let pipeline = builder::build(&format!(
            "videotestsrc num-buffers={} timestamp-offset={} ! video/x-raw,format=I420,width=160,height=120,framerate=25/1 ! \
             x264enc key-int-max=25 bframes=0 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            seconds * 25,
            offset_s * 1_000_000_000,
            path.display()
        ))
        .unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(30),
                &[gstreamer::MessageType::Eos],
            )
            .expect("fragment written");
        pipeline.set_state(gstreamer::State::Null).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn a_later_fragment_plays_at_the_recording_time_asked_for() {
        // 3 s of video whose timestamps start at 60 s.
        let file = fragment(60, 3);
        let (submitter, feed) = encoded_channel();
        let replay = EncodedReplay::spawn(
            vec![PlaybackSegment {
                path: file.clone(),
                start_ts: 1_000_000,
                end_ts: 1_003_000,
            }],
            1_000,
            100,
            true,
            Arc::new(submitter),
        );
        let got = receive(&feed, Duration::from_millis(400)).await;
        let at = replay.position_ms().expect("a position");
        replay.stop();
        std::fs::remove_file(file).unwrap();
        assert!(!got.is_empty());
        assert_eq!(
            got[0].1.pts_us,
            Some(1_000_000),
            "file time, not the run's timeline"
        );
        // Within a few pictures of the second asked for, not a minute later.
        assert!((1_001_000..=1_001_500).contains(&at), "position {at}");
    }

    #[tokio::test]
    async fn a_replay_opened_paused_shows_its_picture_and_waits_for_play() {
        let file = recording(3);
        let (submitter, feed) = encoded_channel();
        let replay = EncodedReplay::spawn(
            vec![PlaybackSegment {
                path: file.clone(),
                start_ts: 1_000_000,
                end_ts: 1_003_000,
            }],
            0,
            100,
            false,
            Arc::new(submitter),
        );
        let first = receive(&feed, Duration::from_millis(500)).await;
        assert_eq!(first.len(), 1, "the picture at the moment, then nothing");
        assert!(first[0].1.keyframe);
        let held = replay.position_ms().unwrap();
        assert!(receive(&feed, Duration::from_millis(300)).await.is_empty());
        replay.set_playing(true);
        let playing = receive(&feed, Duration::from_millis(600)).await;
        replay.stop();
        std::fs::remove_file(file).unwrap();
        assert!(
            playing.len() >= 10,
            "after Play it advances: {}",
            playing.len()
        );
        assert!(replay.position_ms().unwrap() >= held + 300);
    }
}
