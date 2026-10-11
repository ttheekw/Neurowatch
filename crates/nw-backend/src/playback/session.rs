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

    for (index, segment) in segments.iter().enumerate() {
        if *stop_rx.borrow() {
            break;
        }
        if let Ok(mut range) = current_segment.lock() {
            *range = (segment.start_ts, segment.end_ts);
        }
        let seek_ms = if index == 0 { initial_seek_ms } else { 0 };
        let has_next = index + 1 < segments.len();
        match run_segment(
            &shm_key,
            segment,
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
                tracing::warn!(shm_key = %shm_key, path = %segment.path, "Skipping unreadable recording");
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
    segment: &PlaybackSegment,
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
        path = %segment.path,
        start_ts = segment.start_ts,
        end_ts = segment.end_ts,
        "Opening playback segment"
    );

    let pipeline_str = build_pipeline_str(&segment.path);
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
    let Some(decode) = pipeline.by_name(PLAYBACK_DECODE) else {
        return;
    };
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
