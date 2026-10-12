//! Playback session — one per UI request, runs a GStreamer file pipeline.
//!
//! Each session opens a recorded segment file, writes decoded frames into a
//! POSIX SHM ring buffer (key = `pb_{session_id}`), and exposes watch-channel
//! handles for seek / speed / stop control.
//!
//! A segment with the camera's audio plays it on this computer's audio
//! output while the session's audio is on ([`PlaybackSession::set_audio`];
//! the desktop turns it on for one camera at a time). Between 0.5× and 2×
//! the pitch is kept (`scaletempo`); at other speeds it is muted. Without a
//! working audio output, playback goes on silently.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use gstreamer::prelude::*;
use gstreamer_video::VideoFrameExt;
use tokio::sync::watch;

use crate::shm::ring_buffer::ShmRingBufferWriter;

/// Where a desktop playback session's decoded frames go. Browser playback
/// does not decode: see `playback::encoded_replay`.
pub enum PlaybackOutput {
    /// RGB frames into the desktop UI's SHM ring `neurowatch_{key}`.
    Shm { key: String },
}

/// The opened form of a [`PlaybackOutput`], shared by every segment of a playlist.
enum SegmentSink {
    Shm(Arc<StdMutex<ShmRingBufferWriter>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackSegment {
    pub path: String,
    pub start_ts: i64,
    pub end_ts: i64,
}

/// What one pipeline plays: a single segment file, or several consecutive
/// segments of one recording run played as one stream (`splitmuxsrc`), so
/// the picture and sound run on across segment boundaries instead of the
/// player being rebuilt at every one (a visible hitch and a sound glitch).
/// Browser playback (the cloud extension) groups its playlist the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayUnit {
    /// The files, in order; more than one only for one run's consecutive,
    /// finished segments, whose stream time then runs across them all.
    pub paths: Vec<String>,
    /// Recording time (ms since the epoch) where the first file starts.
    pub start_ts: i64,
    pub end_ts: i64,
}

/// Segments of one run whose indexed times meet within this are played as
/// one stream (`segment_tracker` indexes a run's segments back to back).
const JOIN_TOLERANCE_MS: i64 = 100;
/// At most this many segments per stream: opening the stream measures each
/// one, and a seek further away reopens the session anyway.
const MAX_JOINED: usize = 30;
/// A file modified this recently may still be being written: played alone.
const OPEN_FILE_MS: u128 = 10_000;

/// `{camera}_{run_epoch_ms}_{sequence}` of a recording segment's file name
/// (`{camera}_{run_epoch_ms}_{sequence}_{fragment}.mkv`), with its folder;
/// None for other names.
fn run_of(path: &str) -> Option<(std::path::PathBuf, String)> {
    let path = std::path::Path::new(path);
    let stem = path.file_name()?.to_str()?.strip_suffix(".mkv")?;
    let mut parts = stem.rsplitn(4, '_');
    let fragment = parts.next()?;
    let sequence = parts.next()?;
    let epoch = parts.next()?;
    let camera = parts.next()?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !(digits(fragment) && digits(sequence) && digits(epoch)) || camera.is_empty() {
        return None;
    }
    Some((
        path.parent()?.to_path_buf(),
        format!("{camera}_{epoch}_{sequence}"),
    ))
}

fn recently_modified(path: &str) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map_or(true, |age| age.as_millis() < OPEN_FILE_MS)
}

/// The playlist as pipelines: each run's consecutive, finished segments
/// together (up to `MAX_JOINED`), everything else on its own.
pub fn play_units(segments: &[PlaybackSegment]) -> Vec<PlayUnit> {
    let mut units: Vec<PlayUnit> = Vec::new();
    let mut last_run: Option<(std::path::PathBuf, String)> = None;
    for segment in segments {
        let run = run_of(&segment.path).filter(|_| !recently_modified(&segment.path));
        let joins = match (units.last(), &run, &last_run) {
            (Some(unit), Some(run), Some(previous)) => {
                run == previous
                    && unit.paths.len() < MAX_JOINED
                    && (segment.start_ts - unit.end_ts).abs() <= JOIN_TOLERANCE_MS
            }
            _ => false,
        };
        if joins {
            if let Some(unit) = units.last_mut() {
                unit.paths.push(segment.path.clone());
                unit.end_ts = segment.end_ts;
            }
        } else {
            units.push(PlayUnit {
                paths: vec![segment.path.clone()],
                start_ts: segment.start_ts,
                end_ts: segment.end_ts,
            });
        }
        last_run = run;
    }
    units
}

/// Live handle returned to the IPC handler for controlling the pipeline.
pub struct PlaybackSession {
    pub session_id: String,
    pub cam_id: String,
    /// SHM key used after `neurowatch_` in the ring name.  E.g. `pb_<uuid>`.
    pub shm_key: String,
    current_segment: Arc<StdMutex<(i64, i64)>>,
    stop_tx: watch::Sender<bool>,
    /// `Some(offset_ms)` triggers a seek; `None` = no pending seek.
    seek_tx: watch::Sender<Option<i64>>,
    speed_tx: watch::Sender<f64>,
    playing_tx: watch::Sender<bool>,
    speed_state: StdMutex<f64>,
    playing_state: AtomicBool,
    audio: Arc<AudioState>,
}

/// The session's audio: whether it is wanted, whether the recording has it,
/// and the speed (audio is heard only between [`AUDIBLE_RATES`]).
#[derive(Debug, Default)]
pub struct AudioState {
    enabled: AtomicBool,
    present: AtomicBool,
    /// The audio branch exists and is unmuted right now (what is heard).
    audible: AtomicBool,
    /// `f64` bits of the playback rate.
    rate_bits: AtomicU64,
}

/// Rates at which audio plays (pitch kept); muted outside.
const AUDIBLE_RATES: std::ops::RangeInclusive<f64> = 0.5..=2.0;
/// The audio branch's volume element, for muting.
const AUDIO_VOLUME: &str = "playback_volume";

impl AudioState {
    fn new(rate: f64) -> Self {
        let state = Self::default();
        state
            .rate_bits
            .store(rate.to_bits(), AtomicOrdering::Relaxed);
        state
    }

    fn rate(&self) -> f64 {
        f64::from_bits(self.rate_bits.load(AtomicOrdering::Relaxed))
    }

    /// Whether the audio branch should be silent right now.
    fn muted(&self) -> bool {
        !self.enabled.load(AtomicOrdering::Relaxed) || !AUDIBLE_RATES.contains(&self.rate())
    }
}

impl PlaybackSession {
    /// Spawn the file pipeline in a blocking thread and return the handle,
    /// writing to `output` (desktop SHM ring or browser publication) at the
    /// given initial speed and play state.
    ///
    /// `initial_seek_ms` — offset in ms from the file start to begin
    /// playback at, computed as `ts_ms - segment.start_ts` by the handler.
    pub fn spawn_with(
        session_id: String,
        cam_id: String,
        segments: Vec<PlaybackSegment>,
        initial_seek_ms: i64,
        output: PlaybackOutput,
        speed: f64,
        playing: bool,
    ) -> Self {
        let shm_key = format!("pb_{session_id}");
        let initial_range = segments
            .first()
            .map(|segment| (segment.start_ts, segment.end_ts))
            .unwrap_or((0, 0));
        let current_segment = Arc::new(StdMutex::new(initial_range));

        let (stop_tx, stop_rx) = watch::channel(false);
        let (seek_tx, seek_rx) = watch::channel::<Option<i64>>(None);
        let speed = speed.max(0.01);
        let (speed_tx, speed_rx) = watch::channel(speed);
        let (playing_tx, playing_rx) = watch::channel(playing);
        let audio = Arc::new(AudioState::new(speed));
        let audio_worker = Arc::clone(&audio);

        let current_segment_worker = Arc::clone(&current_segment);
        tokio::task::spawn_blocking(move || {
            run_pipeline(
                output,
                segments,
                initial_seek_ms,
                current_segment_worker,
                PipelineControls {
                    stop_rx,
                    seek_rx,
                    speed_rx,
                    playing_rx,
                    audio: audio_worker,
                },
            );
        });

        Self {
            session_id,
            cam_id,
            shm_key,
            current_segment,
            stop_tx,
            seek_tx,
            speed_tx,
            playing_tx,
            speed_state: StdMutex::new(speed),
            playing_state: AtomicBool::new(playing),
            audio,
        }
    }

    /// Hear (or mute) this session's recorded audio.
    pub fn set_audio(&self, enabled: bool) {
        self.audio.enabled.store(enabled, AtomicOrdering::Relaxed);
        // Wake the pipeline thread's control check; it applies the mute.
        // (`send_modify` takes the lock once: `send(*borrow())` would hold
        // the read lock while asking for the write lock, and deadlock.)
        self.speed_tx.send_modify(|_| {});
    }

    /// Whether the session's audio is on (it may still be silent: no audio
    /// in the recording, or a speed outside 0.5×–2×).
    pub fn audio_enabled(&self) -> bool {
        self.audio.enabled.load(AtomicOrdering::Relaxed)
    }

    /// Whether the session's audio reaches the output right now (its branch
    /// exists and is unmuted).
    pub fn audible(&self) -> bool {
        self.audio.audible.load(AtomicOrdering::Relaxed)
    }

    /// Whether the recording being played has the camera's audio.
    pub fn has_audio(&self) -> bool {
        self.audio.present.load(AtomicOrdering::Relaxed)
    }

    /// Request a seek to `offset_ms` from the file start.
    pub fn seek(&self, offset_ms: i64) {
        let _ = self.seek_tx.send(Some(offset_ms.max(0)));
    }

    /// Change playback rate (1.0 = real-time, 2.0 = double speed, etc.).
    pub fn set_speed(&self, rate: f64) {
        let normalized = rate.max(0.01);
        if let Ok(mut r) = self.speed_state.lock() {
            *r = normalized;
        }
        let _ = self.speed_tx.send(normalized);
    }

    /// Toggle playback state. `false` pauses decode clock progression.
    pub fn set_playing(&self, playing: bool) {
        self.playing_state.store(playing, AtomicOrdering::Relaxed);
        let _ = self.playing_tx.send(playing);
    }

    pub fn is_playing(&self) -> bool {
        self.playing_state.load(AtomicOrdering::Relaxed)
    }

    pub fn current_speed(&self) -> f64 {
        self.speed_state.lock().map(|r| *r).unwrap_or(1.0_f64)
    }

    pub fn current_segment_range(&self) -> (i64, i64) {
        self.current_segment
            .lock()
            .map(|range| *range)
            .unwrap_or((0, 0))
    }

    /// Signal the pipeline thread to stop and clean up.
    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }
}

// ---------------------------------------------------------------------------
// Pipeline thread
// ---------------------------------------------------------------------------

/// Receiver side of the session's control channels, owned by the pipeline thread.
struct PipelineControls {
    stop_rx: watch::Receiver<bool>,
    seek_rx: watch::Receiver<Option<i64>>,
    speed_rx: watch::Receiver<f64>,
    playing_rx: watch::Receiver<bool>,
    audio: Arc<AudioState>,
}

fn run_pipeline(
    output: PlaybackOutput,
    segments: Vec<PlaybackSegment>,
    initial_seek_ms: i64,
    current_segment: Arc<StdMutex<(i64, i64)>>,
    controls: PipelineControls,
) {
    let PipelineControls {
        stop_rx,
        mut seek_rx,
        mut speed_rx,
        mut playing_rx,
        audio,
    } = controls;

    let PlaybackOutput::Shm { key: shm_key } = &output;
    let shm_key = shm_key.clone();
    if segments.is_empty() {
        tracing::error!(shm_key = %shm_key, "Playback session has no segments");
        return;
    }
    tracing::info!(shm_key = %shm_key, segment_count = segments.len(), "Playback session starting");

    // Keep one SHM inode for the whole playlist. Recreating it at every file
    // boundary makes the UI reader wait for its inode-replacement poll and
    // produces a visible freeze between otherwise contiguous segments.
    let writer = match output {
        PlaybackOutput::Shm { key } => match ShmRingBufferWriter::create(&key) {
            Ok(writer) => SegmentSink::Shm(Arc::new(StdMutex::new(writer))),
            Err(error) => {
                tracing::error!(shm_key = %shm_key, %error, "SHM create failed for playback session");
                return;
            }
        },
    };

    let units = play_units(&segments);
    for (index, unit) in units.iter().enumerate() {
        if *stop_rx.borrow() {
            break;
        }
        if let Ok(mut range) = current_segment.lock() {
            *range = (unit.start_ts, unit.end_ts);
        }
        let seek_ms = if index == 0 { initial_seek_ms } else { 0 };
        let has_next = index + 1 < units.len();
        match run_segment(
            &shm_key,
            unit,
            seek_ms,
            has_next,
            &writer,
            &stop_rx,
            &mut seek_rx,
            &mut speed_rx,
            &mut playing_rx,
            &audio,
        ) {
            SegmentExit::Eos if has_next => continue,
            // An unreadable file (e.g. cut short when recording stopped
            // abruptly) is skipped rather than ending the whole playback.
            SegmentExit::Error if has_next => {
                tracing::warn!(shm_key = %shm_key, paths = ?unit.paths, "Skipping unreadable recording");
                continue;
            }
            SegmentExit::Eos | SegmentExit::Stop | SegmentExit::Error => break,
        }
    }

    tracing::info!(shm_key = %shm_key, "Playback session stopped");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentExit {
    Eos,
    Stop,
    Error,
}

#[allow(clippy::too_many_arguments)]
fn run_segment(
    shm_key: &str,
    unit: &PlayUnit,
    initial_seek_ms: i64,
    advance_on_eos: bool,
    writer: &SegmentSink,
    stop_rx: &watch::Receiver<bool>,
    seek_rx: &mut watch::Receiver<Option<i64>>,
    speed_rx: &mut watch::Receiver<f64>,
    playing_rx: &mut watch::Receiver<bool>,
    audio: &Arc<AudioState>,
) -> SegmentExit {
    tracing::debug!(
        shm_key,
        paths = ?unit.paths,
        start_ts = unit.start_ts,
        end_ts = unit.end_ts,
        "Opening playback segment"
    );

    let pipeline_str = match unit.paths.as_slice() {
        [single] => build_pipeline_str(single),
        _ => build_run_pipeline_str(),
    };
    let pipeline = match gstreamer::parse::launch(&pipeline_str)
        .ok()
        .and_then(|e| e.downcast::<gstreamer::Pipeline>().ok())
    {
        Some(p) => p,
        None => {
            tracing::error!(shm_key = %shm_key, "Failed to build playback pipeline");
            return SegmentExit::Error;
        }
    };

    attach_audio(&pipeline, shm_key, audio);
    if unit.paths.len() > 1 && !wire_run_source(&pipeline, shm_key, &unit.paths, audio) {
        let _ = pipeline.set_state(gstreamer::State::Null);
        return SegmentExit::Error;
    }

    // The frame held when first paused is the file's start; with a seek to
    // come it is not shown, so the view never flashes the wrong moment.
    let show_preroll = Arc::new(AtomicBool::new(initial_seek_ms <= 0));
    if !wire_appsink(&pipeline, shm_key, writer, &show_preroll) {
        let _ = pipeline.set_state(gstreamer::State::Null);
        return SegmentExit::Error;
    }

    // Preroll paused and seek before playing, so the first frame shown is
    // the one asked for rather than the start of the file.
    if pipeline.set_state(gstreamer::State::Paused).is_err() {
        tracing::error!(shm_key = %shm_key, "Failed to preroll playback pipeline");
        let _ = pipeline.set_state(gstreamer::State::Null);
        return SegmentExit::Error;
    }
    let _ = pipeline.state(gstreamer::ClockTime::from_seconds(5));
    if initial_seek_ms > 0 {
        show_preroll.store(true, AtomicOrdering::Relaxed);
        apply_seek(&pipeline, initial_seek_ms);
        let _ = pipeline.state(gstreamer::ClockTime::from_seconds(5));
    }

    // A paused session stays paused: its first frame is already on screen
    // (the appsinks deliver the preroll frame).
    let playing = *playing_rx.borrow_and_update();
    if playing && pipeline.set_state(gstreamer::State::Playing).is_err() {
        tracing::error!(shm_key = %shm_key, "Failed to set pipeline to PLAYING");
        let _ = pipeline.set_state(gstreamer::State::Null);
        return SegmentExit::Error;
    }

    let speed = *speed_rx.borrow_and_update();
    audio
        .rate_bits
        .store(speed.to_bits(), AtomicOrdering::Relaxed);
    if (speed - 1.0).abs() > f64::EPSILON {
        apply_rate_change(&pipeline, speed);
    }
    apply_audio_mute(&pipeline, audio);

    let bus = match pipeline.bus() {
        Some(b) => b,
        None => {
            tracing::error!(shm_key = %shm_key, "Cannot get pipeline bus");
            let _ = pipeline.set_state(gstreamer::State::Null);
            return SegmentExit::Error;
        }
    };

    let exit = loop {
        if *stop_rx.borrow() {
            break SegmentExit::Stop;
        }

        if speed_rx.has_changed().unwrap_or(false) {
            let rate = *speed_rx.borrow_and_update();
            let previous = audio.rate();
            audio
                .rate_bits
                .store(rate.to_bits(), AtomicOrdering::Relaxed);
            // Also woken by `set_audio`: only a new rate needs a seek.
            if (rate - previous).abs() > f64::EPSILON {
                apply_rate_change(&pipeline, rate);
            }
            apply_audio_mute(&pipeline, audio);
        }

        if playing_rx.has_changed().unwrap_or(false) {
            let playing = *playing_rx.borrow_and_update();
            let target = if playing {
                gstreamer::State::Playing
            } else {
                gstreamer::State::Paused
            };
            if pipeline.set_state(target).is_err() {
                tracing::warn!(playing, "Failed to change playback state");
            }
        }

        if seek_rx.has_changed().unwrap_or(false) {
            if let Some(offset_ms) = *seek_rx.borrow_and_update() {
                apply_seek(&pipeline, offset_ms);
                let target = if *playing_rx.borrow() {
                    gstreamer::State::Playing
                } else {
                    gstreamer::State::Paused
                };
                let _ = pipeline.set_state(target);
            }
        }

        // Short bus timeout so control channels stay responsive.
        if let Some(msg) = bus.timed_pop(gstreamer::ClockTime::from_mseconds(100)) {
            use gstreamer::MessageView;
            match msg.view() {
                MessageView::Error(e) => {
                    tracing::warn!(
                        shm_key = %shm_key,
                        error = %e.error(),
                        "Playback pipeline error"
                    );
                    break SegmentExit::Error;
                }
                MessageView::Eos(_) => {
                    if advance_on_eos {
                        tracing::debug!(shm_key, "Playback segment ended; advancing");
                        break SegmentExit::Eos;
                    }
                    tracing::debug!(shm_key, "Playback reached final segment");
                    if pipeline.set_state(gstreamer::State::Paused).is_err() {
                        tracing::warn!(shm_key = %shm_key, "Failed to pause pipeline at EOS");
                    }
                }
                _ => {}
            }
        }
    };

    let _ = pipeline.set_state(gstreamer::State::Null);
    // Wait for NULL so in-flight appsink callbacks complete before SHM drops.
    let _ = pipeline.state(Some(gstreamer::ClockTime::from_seconds(5)));
    exit
}

fn build_pipeline_str(path: &str) -> String {
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
    // Desktop playback decodes for its view; `sync=true` keeps the requested rate.
    format!(
        "filesrc location=\"{escaped}\" ! decodebin name={PLAYBACK_DECODE} ! videoconvert \
         ! video/x-raw,format=RGB \
         ! appsink name=frame_sink sync=true max-buffers=2 drop=true emit-signals=true"
    )
}

/// Several segments of one recording run as one stream: `splitmuxsrc`
/// reads the files the run's timeline spans (`wire_run_source` gives it the
/// list and links its video and audio streams).
fn build_run_pipeline_str() -> String {
    format!(
        "splitmuxsrc name={RUN_SOURCE} decodebin name={PLAYBACK_DECODE} ! videoconvert \
         ! video/x-raw,format=RGB \
         ! appsink name=frame_sink sync=true max-buffers=2 drop=true emit-signals=true"
    )
}

/// The run's source in `build_run_pipeline_str`.
const RUN_SOURCE: &str = "playback_src";
/// The decoder of a run's audio stream.
const PLAYBACK_AUDIO_DECODE: &str = "playback_audio_decode";

/// Give `splitmuxsrc` the files of a run, in order (its `format-location`).
pub fn give_run_files(source: &gstreamer::Element, paths: &[String]) {
    let files: Vec<String> = paths.to_vec();
    source.connect("format-location", false, move |_| Some(files.to_value()));
}

/// Give the run's source its files, in order, and link its streams: video
/// to the playback decoder, audio to a decoder of its own whose output gets
/// the audio branch (see `attach_audio_to`).
fn wire_run_source(
    pipeline: &gstreamer::Pipeline,
    shm_key: &str,
    paths: &[String],
    audio: &Arc<AudioState>,
) -> bool {
    let (Some(source), Some(decode)) = (
        pipeline.by_name(RUN_SOURCE),
        pipeline.by_name(PLAYBACK_DECODE),
    ) else {
        tracing::error!(shm_key, "Playback run pipeline is incomplete");
        return false;
    };
    give_run_files(&source, paths);
    let weak = pipeline.downgrade();
    let (audio, key) = (Arc::clone(audio), shm_key.to_owned());
    source.connect_pad_added(move |_source, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        let name = pad.name();
        let target = if name.starts_with("video") {
            decode.static_pad("sink").filter(|sink| !sink.is_linked())
        } else if name.starts_with("audio") && !audio_output_available() {
            // Nowhere to play it: drained, so the run's source never stops
            // on an unlinked stream.
            audio.present.store(true, AtomicOrdering::Relaxed);
            let Ok(drain) = gstreamer::ElementFactory::make("fakesink")
                .property("sync", false)
                .build()
            else {
                return;
            };
            if pipeline.add(&drain).is_err() {
                return;
            }
            let _ = drain.sync_state_with_parent();
            drain.static_pad("sink")
        } else if name.starts_with("audio") {
            let Ok(audio_decode) = gstreamer::ElementFactory::make("decodebin")
                .name(PLAYBACK_AUDIO_DECODE)
                .build()
            else {
                return;
            };
            if pipeline.add(&audio_decode).is_err() {
                return;
            }
            attach_audio_to(&pipeline, &audio_decode, &key, &audio);
            let _ = audio_decode.sync_state_with_parent();
            audio_decode.static_pad("sink")
        } else {
            None
        };
        if let Some(sink) = target {
            if pad.link(&sink).is_err() {
                tracing::warn!(shm_key = %key, pad = %name, "Could not link the run's stream");
            }
        }
    });
    true
}

/// The segment's decoder; its audio pad (if the recording has audio) gets
/// the audio branch.
const PLAYBACK_DECODE: &str = "playback_decode";

/// The audio branch of a desktop playback pipeline: keeps the pitch at
/// changed speeds, mutes by volume, plays on the computer's audio output.
fn audio_branch_description() -> String {
    format!(
        "queue max-size-time=1000000000 ! audioconvert ! audioresample ! scaletempo \
         ! audioconvert ! volume name={AUDIO_VOLUME} ! autoaudiosink"
    )
}

/// Whether this computer has an audio output to play to (checked once).
fn audio_output_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let Ok(sink) = gstreamer::ElementFactory::make("autoaudiosink").build() else {
            tracing::info!("No audio output element; recordings play without sound");
            return false;
        };
        let ready = sink.set_state(gstreamer::State::Ready).is_ok();
        let _ = sink.set_state(gstreamer::State::Null);
        if !ready {
            tracing::info!("No audio output available; recordings play without sound");
        }
        ready
    })
}

/// When the segment's audio track appears, play it through the audio
/// branch (muted unless the session's audio is on). A recording without
/// audio, or a computer without an audio output, plays video only.
fn attach_audio(pipeline: &gstreamer::Pipeline, shm_key: &str, audio: &Arc<AudioState>) {
    if let Some(decode) = pipeline.by_name(PLAYBACK_DECODE) {
        attach_audio_to(pipeline, &decode, shm_key, audio);
    }
}

/// Play the audio pad `decode` adds through the audio branch.
fn attach_audio_to(
    pipeline: &gstreamer::Pipeline,
    decode: &gstreamer::Element,
    shm_key: &str,
    audio: &Arc<AudioState>,
) {
    let weak = pipeline.downgrade();
    let (audio, shm_key) = (Arc::clone(audio), shm_key.to_owned());
    decode.connect_pad_added(move |_decode, pad| {
        let is_audio = pad
            .current_caps()
            .unwrap_or_else(|| pad.query_caps(None))
            .structure(0)
            .is_some_and(|s| s.name().starts_with("audio/"));
        if !is_audio || pad.is_linked() {
            return;
        }
        audio.present.store(true, AtomicOrdering::Relaxed);
        if !audio_output_available() {
            return;
        }
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        let branch = match gstreamer::parse::bin_from_description(&audio_branch_description(), true)
        {
            Ok(branch) => branch,
            Err(error) => {
                tracing::warn!(shm_key = %shm_key, %error, "Could not build the playback audio branch");
                return;
            }
        };
        if let Some(volume) = branch.by_name(AUDIO_VOLUME) {
            let muted = audio.muted();
            volume.set_property("mute", muted);
            audio.audible.store(!muted, AtomicOrdering::Relaxed);
        }
        if pipeline.add(&branch).is_err() {
            return;
        }
        let linked = branch
            .static_pad("sink")
            .is_some_and(|sink| pad.link(&sink).is_ok());
        if !linked || branch.sync_state_with_parent().is_err() {
            tracing::warn!(shm_key = %shm_key, "Could not play the recording's audio");
            let _ = branch.set_state(gstreamer::State::Null);
            let _ = pipeline.remove(&branch);
            return;
        }
        tracing::debug!(shm_key = %shm_key, "Playback audio attached");
    });
}

/// Mute or unmute the audio branch for the session's audio state and speed.
fn apply_audio_mute(pipeline: &gstreamer::Pipeline, audio: &AudioState) {
    if let Some(volume) = pipeline.by_name(AUDIO_VOLUME) {
        let muted = audio.muted();
        volume.set_property("mute", muted);
        audio.audible.store(!muted, AtomicOrdering::Relaxed);
    }
}

/// Wire `frame_sink` to the playlist's persistent SHM ring buffer.
fn wire_appsink(
    pipeline: &gstreamer::Pipeline,
    shm_key: &str,
    sink: &SegmentSink,
    show_preroll: &Arc<AtomicBool>,
) -> bool {
    let SegmentSink::Shm(writer) = sink;
    let Some(appsink) = pipeline
        .by_name("frame_sink")
        .and_then(|el| el.downcast::<gstreamer_app::AppSink>().ok())
    else {
        tracing::error!(shm_key, "Playback pipeline has no frame_sink appsink");
        return false;
    };

    let writer_cb = Arc::clone(writer);
    let counter = Arc::new(AtomicU64::new(0));
    let counter_cb = Arc::clone(&counter);
    let key_cb = shm_key.to_owned();

    let write = Arc::new(
        move |sample: &gstreamer::Sample| -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            let caps = sample.caps().ok_or(gstreamer::FlowError::Error)?;
            let info = gstreamer_video::VideoInfo::from_caps(caps)
                .map_err(|_| gstreamer::FlowError::Error)?;
            if info.format() != gstreamer_video::VideoFormat::Rgb {
                return Err(gstreamer::FlowError::Error);
            }
            let width = info.width();
            let height = info.height();
            let buffer = sample.buffer().ok_or(gstreamer::FlowError::Error)?;
            let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                .map_err(|_| gstreamer::FlowError::Error)?;
            let stride = usize::try_from(frame.plane_stride()[0])
                .map_err(|_| gstreamer::FlowError::Error)?;
            let pixels = frame
                .plane_data(0)
                .map_err(|_| gstreamer::FlowError::Error)?;

            if let Ok(mut w) = writer_cb.lock() {
                w.write_frame(width, height, stride, pixels);
            }

            let n = counter_cb.fetch_add(1, AtomicOrdering::Relaxed) + 1;
            if n % 300 == 0 {
                tracing::debug!(
                    shm_key = %key_cb,
                    frame = n,
                    width,
                    height,
                    "Playback frame checkpoint"
                );
            }
            Ok(gstreamer::FlowSuccess::Ok)
        },
    );
    let preroll = Arc::clone(&write);
    let show = Arc::clone(show_preroll);
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            // The frame a paused pipeline holds (on open, and after a seek
            // while paused) is shown too, so a paused view is never black.
            .new_preroll(move |sink| {
                let sample = sink
                    .pull_preroll()
                    .map_err(|_| gstreamer::FlowError::Error)?;
                if !show.load(AtomicOrdering::Relaxed) {
                    return Ok(gstreamer::FlowSuccess::Ok);
                }
                preroll(&sample)
            })
            .new_sample(move |sink| {
                let sample = sink
                    .pull_sample()
                    .map_err(|_| gstreamer::FlowError::Error)?;
                write(&sample)
            })
            .build(),
    );

    tracing::debug!(shm_key = %shm_key, "Playback appsink wired to SHM");
    true
}

fn apply_seek(pipeline: &gstreamer::Pipeline, offset_ms: i64) {
    let pos = gstreamer::ClockTime::from_mseconds(offset_ms as u64);
    if pipeline
        .seek_simple(
            gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::ACCURATE,
            pos,
        )
        .is_err()
    {
        tracing::warn!("Playback seek to {offset_ms}ms failed");
    } else {
        tracing::debug!("Playback seek to {offset_ms}ms");
    }
}

fn apply_rate_change(pipeline: &gstreamer::Pipeline, rate: f64) {
    // Seek to the current position at the new rate via a manual Seek event.
    // seek_simple is always rate=1.0, so we must use the GstEvent seek API.
    let pos = pipeline
        .query_position::<gstreamer::ClockTime>()
        .unwrap_or(gstreamer::ClockTime::ZERO);
    // Build a seek event that changes only the rate, keeping the current position.
    // SeekType::Set + pos  → start from current position
    // SeekType::None + 0   → keep existing stop (play to end of file)
    let evt = gstreamer::event::Seek::new(
        rate,
        gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::ACCURATE,
        gstreamer::SeekType::Set,
        pos,
        gstreamer::SeekType::None,
        gstreamer::ClockTime::ZERO,
    );
    if pipeline.send_event(evt) {
        tracing::debug!("Playback rate changed to {rate}");
    } else {
        tracing::warn!(
            "Playback rate change to {rate} failed; pipeline will continue at previous rate"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use gstreamer::prelude::*;
    use tokio::sync::watch;

    use super::{run_pipeline, AudioState, PipelineControls, PlaybackOutput, PlaybackSegment};

    fn create_segment(path: &std::path::Path, pattern: u32) {
        create_segment_at(path, pattern, 0, 4);
    }

    /// A segment whose timestamps start at `offset_s`, as a recording's
    /// second and later fragments do (they continue the run's timeline).
    fn create_segment_at(path: &std::path::Path, pattern: u32, offset_s: u64, buffers: u32) {
        let pipeline = gstreamer::parse::launch(&format!(
            "videotestsrc num-buffers={buffers} pattern={pattern} timestamp-offset={offset} ! \
             video/x-raw,width=64,height=36,framerate=15/1 ! videoconvert ! \
             x264enc speed-preset=ultrafast tune=zerolatency key-int-max=4 ! \
             h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display(),
            offset = offset_s * 1_000_000_000,
        ))
        .expect("build test segment pipeline")
        .downcast::<gstreamer::Pipeline>()
        .expect("test pipeline");
        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("play test segment pipeline");
        let bus = pipeline.bus().expect("test pipeline bus");
        let message = bus.timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(5),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        );
        assert!(
            message.is_some_and(|message| message.type_() == gstreamer::MessageType::Eos),
            "test segment generation must reach EOS"
        );
        let _ = pipeline.set_state(gstreamer::State::Null);
    }

    #[cfg(unix)]
    #[test]
    fn playback_rgb_callback_packs_854_pixel_rows() {
        use nw_core::frame::shm::{HEADER_BYTES, SLOT_HEADER_BYTES, SLOT_SIZE};

        gstreamer::init().expect("initialize GStreamer");
        let key = format!("pb_rgb_test_{}", uuid::Uuid::new_v4());
        let writer = Arc::new(Mutex::new(
            crate::shm::ring_buffer::ShmRingBufferWriter::create(&key).expect("create ring"),
        ));
        let sink = super::SegmentSink::Shm(Arc::clone(&writer));
        let pipeline = gstreamer::parse::launch(
            "videotestsrc num-buffers=1 pattern=solid-color foreground-color=0xff123456 ! \
             video/x-raw,format=RGB,width=854,height=4 ! appsink name=frame_sink sync=false",
        )
        .expect("build RGB test pipeline")
        .downcast::<gstreamer::Pipeline>()
        .expect("test pipeline");
        assert!(super::wire_appsink(
            &pipeline,
            &key,
            &sink,
            &Arc::new(std::sync::atomic::AtomicBool::new(true))
        ));
        pipeline.set_state(gstreamer::State::Playing).expect("play");
        let message = pipeline.bus().expect("bus").timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(5),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        );
        pipeline.set_state(gstreamer::State::Null).expect("stop");
        assert!(message.is_some_and(|m| m.type_() == gstreamer::MessageType::Eos));

        let file = std::fs::File::open(format!("/dev/shm/neurowatch_{key}")).expect("open ring");
        let ring = unsafe { memmap2::Mmap::map(&file) }.expect("map ring");
        let u32_at =
            |offset: usize| u32::from_le_bytes(ring[offset..offset + 4].try_into().unwrap());
        let slot = HEADER_BYTES + u32_at(0) as usize * SLOT_SIZE;
        assert_eq!(
            (u32_at(slot), u32_at(slot + 4), u32_at(slot + 8)),
            (854, 4, 854 * 4 * 3)
        );
        let start = slot + SLOT_HEADER_BYTES;
        assert!(ring[start..start + 854 * 4 * 3]
            .chunks_exact(3)
            .all(|pixel| pixel == [0x12, 0x34, 0x56]));
    }

    #[test]
    fn a_recording_with_audio_is_played_with_its_audio() {
        gstreamer::init().expect("initialize GStreamer");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("nw-playback-audio-{nonce}.mkv"));
        // A recording as the recorder writes one for a camera with G.711 audio.
        let pipeline = gstreamer::parse::launch(&format!(
            "videotestsrc num-buffers=30 ! video/x-raw,width=64,height=36,framerate=15/1 \
               ! videoconvert ! x264enc speed-preset=ultrafast tune=zerolatency key-int-max=4 \
               ! h264parse ! matroskamux name=mux ! filesink location=\"{}\" \
             audiotestsrc num-buffers=20 ! audio/x-raw,rate=8000,channels=1 ! alawenc ! mux.",
            path.display()
        ))
        .expect("build test recording")
        .downcast::<gstreamer::Pipeline>()
        .expect("pipeline");
        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("record");
        let done = pipeline.bus().expect("bus").timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(10),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        );
        assert!(done.is_some_and(|m| m.type_() == gstreamer::MessageType::Eos));
        let _ = pipeline.set_state(gstreamer::State::Null);

        let segments = vec![PlaybackSegment {
            path: path.to_string_lossy().into_owned(),
            start_ts: 1_000,
            end_ts: 3_000,
        }];
        let audio = Arc::new(AudioState::new(1.0));
        audio
            .enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let (stop_tx, stop_rx) = watch::channel(false);
        let (_seek_tx, seek_rx) = watch::channel(None);
        let (_speed_tx, speed_rx) = watch::channel(1.0);
        let (_playing_tx, playing_rx) = watch::channel(true);
        let worker_audio = Arc::clone(&audio);
        let shm_key = format!("pb_test_audio_{nonce}");
        let worker = std::thread::spawn(move || {
            run_pipeline(
                PlaybackOutput::Shm { key: shm_key },
                segments,
                0,
                Arc::new(Mutex::new((1_000, 3_000))),
                PipelineControls {
                    stop_rx,
                    seek_rx,
                    speed_rx,
                    playing_rx,
                    audio: worker_audio,
                },
            );
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !audio.present.load(std::sync::atomic::Ordering::Relaxed) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let present = audio.present.load(std::sync::atomic::Ordering::Relaxed);
        stop_tx.send(true).expect("stop playback");
        worker.join().expect("join playback worker");
        let _ = std::fs::remove_file(path);
        // Found and played (on a machine with an audio output) or played
        // silently; either way the video plays and nothing fails.
        assert!(present, "the recording's audio track was found");
        assert!(!audio.muted(), "audio on at 1x is heard");
    }

    #[tokio::test]
    async fn turning_audio_on_never_blocks() {
        // No segments: the pipeline thread ends at once; the handle stays.
        let session = super::PlaybackSession::spawn_with(
            "audio-lock".into(),
            "cam".into(),
            Vec::new(),
            0,
            PlaybackOutput::Shm {
                key: "pb_audio_lock_test".into(),
            },
            1.0,
            true,
        );
        let toggled = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || {
                session.set_audio(true);
                session.set_audio(false);
                session.audio_enabled()
            }),
        )
        .await;
        assert!(
            matches!(toggled, Ok(Ok(false))),
            "set_audio returned at once: {toggled:?}"
        );
    }

    #[test]
    fn audio_is_heard_only_when_on_and_between_half_and_double_speed() {
        let audio = AudioState::new(1.0);
        assert!(audio.muted(), "off until the desktop turns it on");
        audio
            .enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        for (rate, muted) in [
            (1.0, false),
            (0.5, false),
            (2.0, false),
            (4.0, true),
            (0.25, true),
        ] {
            audio
                .rate_bits
                .store(f64::to_bits(rate), std::sync::atomic::Ordering::Relaxed);
            assert_eq!(audio.muted(), muted, "at {rate}x");
        }
    }

    /// A recording run as the recorder writes it: 10 s of video (and
    /// audio) split by `splitmuxsink` at keyframes into ~3 s segments, with
    /// the recorder's file names, indexed as the backend indexes them.
    fn recorded_run(name: &str) -> (std::path::PathBuf, Vec<PlaybackSegment>) {
        gstreamer::init().expect("initialize GStreamer");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nw-run-{name}-{nonce}"));
        std::fs::create_dir_all(&dir).expect("create dir");
        let cam = "cam-run";
        let run: i64 = 1_789_659_000_000;
        let location = dir.join(format!("{cam}_{run}_1_%05d.mkv"));
        let pipeline = gstreamer::parse::launch(&format!(
            "videotestsrc num-buffers=100 ! video/x-raw,width=64,height=36,framerate=10/1 \
             ! videoconvert ! x264enc speed-preset=ultrafast tune=zerolatency key-int-max=8 \
             ! h264parse ! splitmuxsink name=mux location=\"{}\" max-size-time=3000000000 \
               muxer-factory=matroskamux \
             audiotestsrc num-buffers=100 samplesperbuffer=4800 \
             ! audio/x-raw,rate=48000,channels=1 ! audioconvert ! opusenc ! mux.audio_0",
            location.display()
        ))
        .expect("build recording pipeline")
        .downcast::<gstreamer::Pipeline>()
        .expect("pipeline");
        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("record");
        let done = pipeline.bus().expect("bus").timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(30),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        );
        assert!(done.is_some_and(|m| m.type_() == gstreamer::MessageType::Eos));
        let _ = pipeline.set_state(gstreamer::State::Null);
        // Finished files, as recordings older than a few seconds are.
        let old = SystemTime::now() - Duration::from_secs(600);
        for entry in std::fs::read_dir(&dir).expect("read dir").flatten() {
            std::fs::File::options()
                .write(true)
                .open(entry.path())
                .and_then(|f| f.set_modified(old))
                .expect("age segment");
        }
        let db = Arc::new(Mutex::new(
            crate::db::connection::open_in_memory().expect("db"),
        ));
        crate::recording::segment_tracker::scan_and_record(&db, &cam.to_owned(), &dir, 3_000);
        let conn = db.lock().expect("db lock");
        let mut stmt = conn
            .prepare("SELECT path, start_ts, end_ts FROM segments ORDER BY start_ts")
            .expect("prepare");
        let segments = stmt
            .query_map([], |r| {
                Ok(PlaybackSegment {
                    path: r.get(0)?,
                    start_ts: r.get(1)?,
                    end_ts: r.get(2)?,
                })
            })
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows");
        assert!(segments.len() >= 3, "{segments:?}");
        (dir, segments)
    }

    /// Plays `paths` as one run, seeking first to `seek_ms` (if any), and
    /// returns the stream times (ms) of the video frames that reach the view.
    fn play_run(paths: &[String], seek_ms: Option<i64>) -> Vec<i64> {
        use super::{apply_seek, build_run_pipeline_str, wire_run_source};
        let pipeline = gstreamer::parse::launch(&build_run_pipeline_str())
            .expect("build run pipeline")
            .downcast::<gstreamer::Pipeline>()
            .expect("pipeline");
        assert!(wire_run_source(
            &pipeline,
            "pb_test_run",
            paths,
            &Arc::new(AudioState::default())
        ));
        let sink = pipeline.by_name("frame_sink").expect("frame sink");
        sink.set_property("sync", false);
        sink.set_property("drop", false);
        let times = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&times);
        sink.static_pad("sink").expect("sink pad").add_probe(
            gstreamer::PadProbeType::BUFFER,
            move |pad, info| {
                if let (Some(buffer), Some(segment)) = (
                    info.buffer(),
                    pad.sticky_event::<gstreamer::event::Segment>(0),
                ) {
                    let segment = segment.segment().clone();
                    if let (Ok(segment), Some(pts)) =
                        (segment.downcast::<gstreamer::ClockTime>(), buffer.pts())
                    {
                        if let Some(at) = segment.to_stream_time(pts) {
                            seen.lock().unwrap().push(at.mseconds() as i64);
                        }
                    }
                }
                gstreamer::PadProbeReturn::Ok
            },
        );
        // Drain the frames (no SHM in this test).
        let appsink = sink.downcast::<gstreamer_app::AppSink>().expect("appsink");
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(|sink| {
                    let _ = sink.pull_sample();
                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.set_state(gstreamer::State::Paused).expect("pause");
        let _ = pipeline.state(gstreamer::ClockTime::from_seconds(10));
        if let Some(offset) = seek_ms {
            times.lock().unwrap().clear();
            apply_seek(&pipeline, offset);
            let _ = pipeline.state(gstreamer::ClockTime::from_seconds(10));
        }
        pipeline.set_state(gstreamer::State::Playing).expect("play");
        let end = pipeline.bus().expect("bus").timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(30),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        );
        let ok = end.is_some_and(|m| m.type_() == gstreamer::MessageType::Eos);
        let _ = pipeline.set_state(gstreamer::State::Null);
        assert!(ok, "the run plays to its end");
        let mut times = times.lock().unwrap().clone();
        times.dedup();
        times
    }

    #[test]
    fn a_recordings_segments_play_as_one_stream_without_a_hitch() {
        let (dir, segments) = recorded_run("continuous");
        let units = super::play_units(&segments);
        assert_eq!(units.len(), 1, "one run, one stream: {units:?}");
        assert_eq!(units[0].paths.len(), segments.len());
        assert_eq!(units[0].start_ts, segments[0].start_ts);
        assert_eq!(units[0].end_ts, segments.last().expect("last").end_ts);

        let times = play_run(&units[0].paths, None);
        // Every frame of the 10 s, 100 ms apart, across the segment
        // boundaries: none skipped, none repeated.
        // splitmuxsrc lays the listed files back to back, so a lost file
        // would not show as a jump: the frame count catches it.
        assert!(times.len() >= 99, "{} frames", times.len());
        let worst = times.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
        assert!(worst <= 110, "a {worst} ms jump: {times:?}");
        assert!(times.last().copied().unwrap_or(0) >= 9_800);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_seek_into_a_later_segment_lands_on_the_asked_moment() {
        let (dir, segments) = recorded_run("seek");
        let units = super::play_units(&segments);
        // 6.5 s: past the first two ~3 s segments.
        let times = play_run(&units[0].paths, Some(6_500));
        let first = times.first().copied().expect("frames after the seek");
        assert!(
            (6_400..=6_600).contains(&first),
            "first frame at {first} ms"
        );
        assert!(times.windows(2).all(|w| w[1] - w[0] <= 110), "{times:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn other_files_and_other_runs_are_played_on_their_own() {
        use super::{play_units, run_of};
        assert_eq!(
            run_of("/rec/cam_1_1789659000000_2_00004.mkv"),
            Some((
                std::path::PathBuf::from("/rec"),
                "cam_1_1789659000000_2".to_owned()
            ))
        );
        assert_eq!(run_of("/rec/cam-test_00000.mkv"), None, "older names");
        assert_eq!(run_of("/rec/nw-playlist-1-a.mkv"), None);
        // Files that do not exist count as still being written: alone.
        let segment = |path: &str, start, end| PlaybackSegment {
            path: path.to_owned(),
            start_ts: start,
            end_ts: end,
        };
        let units = play_units(&[
            segment("/missing/cam_1_1_00000.mkv", 0, 1_000),
            segment("/missing/cam_1_1_00001.mkv", 1_000, 2_000),
        ]);
        assert_eq!(units.len(), 2);
    }

    #[test]
    fn playback_advances_to_the_next_physical_segment() {
        gstreamer::init().expect("initialize GStreamer");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let first_path = std::env::temp_dir().join(format!("nw-playlist-{nonce}-a.mkv"));
        let second_path = std::env::temp_dir().join(format!("nw-playlist-{nonce}-b.mkv"));
        create_segment(&first_path, 0);
        create_segment(&second_path, 1);

        let segments = vec![
            PlaybackSegment {
                path: first_path.to_string_lossy().into_owned(),
                start_ts: 1_000,
                end_ts: 2_000,
            },
            PlaybackSegment {
                path: second_path.to_string_lossy().into_owned(),
                start_ts: 2_000,
                end_ts: 3_000,
            },
        ];
        let current = Arc::new(Mutex::new((1_000, 2_000)));
        let current_worker = Arc::clone(&current);
        let (stop_tx, stop_rx) = watch::channel(false);
        let (_seek_tx, seek_rx) = watch::channel(None);
        let (_speed_tx, speed_rx) = watch::channel(1.0);
        let (_playing_tx, playing_rx) = watch::channel(true);
        let shm_key = format!("pb_test_{nonce}");

        let worker = std::thread::spawn(move || {
            run_pipeline(
                PlaybackOutput::Shm { key: shm_key },
                segments,
                0,
                current_worker,
                PipelineControls {
                    stop_rx,
                    seek_rx,
                    speed_rx,
                    playing_rx,
                    audio: Arc::new(AudioState::default()),
                },
            );
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while current.lock().expect("current range").0 != 2_000 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(*current.lock().expect("current range"), (2_000, 3_000));

        stop_tx.send(true).expect("stop playback");
        worker.join().expect("join playback worker");
        let _ = std::fs::remove_file(first_path);
        let _ = std::fs::remove_file(second_path);
    }
}
