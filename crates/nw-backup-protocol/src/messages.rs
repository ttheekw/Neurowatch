//! Control messages: JSON objects tagged by `type`.
//!
//! A type a side does not know decodes as `Unknown` and is ignored, so either
//! side may add messages; unknown fields are ignored too.
use crate::ranges::SeqRange;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Recorder → ingest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello(Hello),
    /// A fresh ticket for the same recorder, before the current one expires
    /// (plans change, so tickets are short-lived).
    Renew(Renew),
    OpenTrack(OpenTrack),
    CloseTrack(CloseTrack),
    Gap(Gap),
    Ping(Ping),
    /// Starts each QUIC media stream; on TLS over TCP, says which track the
    /// units that follow on the shared stream belong to.
    Chunk(Chunk),
    /// The recorder is stopping backup: the ingest finishes its files.
    Bye,
    #[serde(other)]
    Unknown,
}

/// Ingest → recorder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome(Welcome),
    /// The session is refused or ended; the connection closes after it.
    Refused(Refused),
    TicketAccepted(TicketAccepted),
    TrackOpened(TrackOpened),
    TrackRefused(TrackRefused),
    Ack(Ack),
    /// This ingest is being drained: reconnect through the portal's
    /// assignment, after `retry_after_ms`, resending what is not confirmed.
    Move(Move),
    Pong(Ping),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// The portal's [ticket](crate::ticket::Ticket), as issued.
    pub ticket: String,
    /// The recorder's Ed25519 signature (hex) of
    /// [`hello_payload`](crate::ticket::hello_payload).
    pub signature: String,
    /// The recorder's version, for the ingest's logs.
    #[serde(default)]
    pub recorder: String,
    /// The cloud's clock minus the recorder's, as the recorder learned it.
    #[serde(default)]
    pub clock_offset_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Renew {
    pub ticket: String,
    /// Signature of [`renew_payload`](crate::ticket::renew_payload).
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenTrack {
    /// The recorder's number for this track in this session (units carry it).
    pub track: u32,
    pub camera_id: String,
    pub kind: TrackKind,
    /// Chosen by the recorder when it starts backing up this track (random,
    /// new after a restart). `(camera, kind, epoch)` names a run of units
    /// across reconnects and ingests; their `seq` count within it.
    pub epoch: u64,
    pub format: TrackFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Audio,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackFormat {
    pub codec: Codec,
    /// Video size as the recorder knows it; the ingest reads the SPS itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u8>,
    /// Codec configuration where units do not carry it (AAC's
    /// AudioSpecificConfig), base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_data: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    H265,
    Aac,
    Opus,
    /// G.711 µ-law.
    Pcmu,
    /// G.711 A-law.
    Pcma,
    #[serde(other)]
    Unknown,
}

impl Codec {
    pub fn is_video(self) -> bool {
        matches!(self, Codec::H264 | Codec::H265)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseTrack {
    pub track: u32,
}

/// Units the recorder will never send: lost before it got them, dropped when
/// its outage folder filled, or given up because they are older than the
/// camera's backup window. The next unit after a gap is a DISCONTINUITY.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub track: u32,
    /// Unit numbers given up, when any had been numbered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seqs: Option<SeqRange>,
    /// The time the gap covers (recorder clock, µs since the Unix epoch).
    pub from_time_us: i64,
    pub to_time_us: i64,
    pub reason: GapReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    /// The camera's connection was lost.
    CameraLost,
    /// The recorder could not keep up (its queue overflowed).
    Overflow,
    /// The outage folder was full.
    OutageFull,
    /// Older than the backup window: the cloud would delete it at once.
    Expired,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub session_id: String,
    pub server_time_ms: i64,
    /// How often the ingest confirms durable units.
    pub ack_interval_ms: u32,
    /// Tracks this session may open.
    pub max_tracks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    pub code: RefusalCode,
    /// Plain words for logs and the recorder's status line.
    pub message: String,
    /// When to try again (through the portal: a new ticket or ingest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// A protocol version this ingest does not speak.
    UnsupportedProtocol,
    /// The ticket's signature, audience or times are wrong, or the
    /// recorder's signature does not match its key and this connection.
    BadTicket,
    TicketExpired,
    /// The ingest is full or draining.
    Busy,
    /// The recorder broke the protocol (malformed or unexpected frames).
    Protocol,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketAccepted {
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackOpened {
    pub track: u32,
    /// What this ingest already holds of `(camera, kind, epoch)` (after a
    /// reconnect to the same ingest); the recorder skips those units.
    #[serde(default)]
    pub durable: Vec<SeqRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRefused {
    pub track: u32,
    pub code: TrackRefusal,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackRefusal {
    /// The ticket grants no backup for this camera (or no sound for it).
    NotInPlan,
    /// Larger than the plan allows.
    OverResolution,
    /// More pictures a second than the plan allows.
    OverFrameRate,
    /// More data than the plan allows.
    OverBitrate,
    /// A codec the ingest does not store.
    UnsupportedCodec,
    /// The track number is in use, or too many tracks.
    BadTrack,
    #[serde(other)]
    Unknown,
}

/// What the ingest holds safely on disk of a track, as ranges of `seq`; the
/// recorder may forget those units. Each `Ack` carries the full current set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub track: u32,
    pub durable: Vec<SeqRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    pub reason: String,
    #[serde(default)]
    pub retry_after_ms: u32,
}

/// The first message of each QUIC media stream (`{"type":"chunk",…}`): the
/// units that follow are of this one track, in order. Live units and an
/// outage backlog travel on separate streams (the backlog at a lower
/// priority). On TLS over TCP, where everything shares one stream, a chunk
/// message comes before each run of one track's units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub track: u32,
    /// True for units sent as they arrive, false for a backlog being caught up.
    pub live: bool,
    /// When the keyframe that started the first unit's picture group arrived
    /// (µs since the Unix epoch), when the run does not start at that
    /// keyframe: the ingest files the run under that keyframe's minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gop_start_us: Option<i64>,
}

/// Encode a message as the JSON a [`Frame::Message`](crate::frame::Frame) carries.
pub fn encode<T: Serialize>(message: &T) -> Vec<u8> {
    // Serializing these types cannot fail: they hold no maps with non-string
    // keys and no failing Serialize impls.
    serde_json::to_vec(message).unwrap_or_default()
}

/// Decode a message's JSON.
pub fn decode<T: DeserializeOwned>(json: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_are_tagged_by_type() {
        let open = ClientMessage::OpenTrack(OpenTrack {
            track: 1,
            camera_id: "cam-1".into(),
            kind: TrackKind::Video,
            epoch: 42,
            format: TrackFormat {
                codec: Codec::H265,
                width: Some(1920),
                height: Some(1080),
                sample_rate: None,
                channels: None,
                codec_data: None,
            },
        });
        let wire: serde_json::Value = serde_json::from_slice(&encode(&open)).unwrap();
        assert_eq!(
            wire,
            json!({"type": "open_track", "track": 1, "camera_id": "cam-1", "kind": "video",
                   "epoch": 42, "format": {"codec": "h265", "width": 1920, "height": 1080}})
        );
        assert_eq!(decode::<ClientMessage>(&encode(&open)).unwrap(), open);

        let ack = ServerMessage::Ack(Ack {
            track: 1,
            durable: vec![SeqRange(1, 90)],
        });
        assert_eq!(
            String::from_utf8(encode(&ack)).unwrap(),
            r#"{"type":"ack","track":1,"durable":[[1,90]]}"#
        );
        assert_eq!(
            String::from_utf8(encode(&ClientMessage::Bye)).unwrap(),
            r#"{"type":"bye"}"#
        );
    }

    #[test]
    fn unknown_types_values_and_fields_are_tolerated() {
        let future = br#"{"type":"stats","bytes":1}"#;
        assert_eq!(
            decode::<ClientMessage>(future).unwrap(),
            ClientMessage::Unknown
        );
        assert_eq!(
            decode::<ServerMessage>(future).unwrap(),
            ServerMessage::Unknown
        );

        let refusal =
            br#"{"type":"track_refused","track":2,"code":"quota_spent","message":"x","later":1}"#;
        let ServerMessage::TrackRefused(refused) = decode::<ServerMessage>(refusal).unwrap() else {
            panic!("not a refusal");
        };
        assert_eq!(refused.code, TrackRefusal::Unknown);

        let codec = br#"{"codec":"av1"}"#;
        assert_eq!(decode::<TrackFormat>(codec).unwrap().codec, Codec::Unknown);
    }

    #[test]
    fn malformed_messages_are_errors() {
        assert!(decode::<ClientMessage>(b"{}").is_err(), "no type");
        assert!(decode::<ClientMessage>(br#"{"type":"open_track","track":1}"#).is_err());
        assert!(decode::<ServerMessage>(b"[1]").is_err());
    }
}
