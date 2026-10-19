//! Browser playback: recorded video replayed into a LiveKit room.
//!
//! The web portal asks for playback by sending a playback publish directive;
//! [`crate::cloud::media_publish::PublisherSupervisor`] opens it through this opener,
//! which replays the camera's recordings, still compressed, into the
//! publication's unit queue ([`crate::cloud::encoded_replay`]). The browser
//! decodes; the node does not.

use std::sync::Arc;

use time::OffsetDateTime;

use crate::cloud::encoded_replay::EncodedReplay;
use crate::cloud::media_publish::{
    BoxFuture, EncodedSubmitter, PlaybackOpener, PlaybackRun, PublishError,
};
use crate::playback::handler::PlaybackHandler;

pub struct RecordedPlaybackOpener {
    handler: PlaybackHandler,
}

impl RecordedPlaybackOpener {
    pub fn new(handler: PlaybackHandler) -> Self {
        Self { handler }
    }
}

impl PlaybackOpener for RecordedPlaybackOpener {
    fn open<'a>(
        &'a self,
        camera_id: &'a str,
        position: OffsetDateTime,
        speed_percent: u16,
        playing: bool,
        units: Arc<EncodedSubmitter>,
    ) -> BoxFuture<'a, Result<Box<dyn PlaybackRun>, PublishError>> {
        Box::pin(async move {
            let ts_ms = i64::try_from(position.unix_timestamp_nanos() / 1_000_000)
                .map_err(|_| PublishError::NoRecording)?;
            let (segments, offset_ms) = self
                .handler
                .playlist_from(camera_id, ts_ms)
                .await
                .map_err(|_| PublishError::NoRecording)?;
            let replay = EncodedReplay::spawn(segments, offset_ms, speed_percent, playing, units);
            Ok(Box::new(ReplayRun(replay)) as Box<dyn PlaybackRun>)
        })
    }
}

struct ReplayRun(EncodedReplay);

impl PlaybackRun for ReplayRun {
    fn set_speed_percent(&self, percent: u16) {
        self.0.set_speed_percent(percent);
    }

    fn set_playing(&self, playing: bool) {
        self.0.set_playing(playing);
    }

    fn stop(&self) {
        self.0.stop();
    }

    fn position(&self) -> Option<OffsetDateTime> {
        let ms = self.0.position_ms()?;
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
    }
}
