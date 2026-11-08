//! The protocol between a NeuroWatch recorder and the cloud backup ingest
//! service. Design: `docs/plans/2026-11-07-cloud-backup.md`.
//!
//! The recorder connects (QUIC, or TLS over TCP where UDP is blocked; ALPN
//! [`ALPN`]) and streams its backed-up cameras' compressed video and sound as
//! it arrives. The ingest writes it to disk and confirms what is safe there;
//! the recorder keeps only what is not yet confirmed.
//!
//! A session:
//!
//! 1. recorder → [`Hello`](messages::Hello): a [ticket](ticket) the portal
//!    signed, and the recorder's signature binding it to this connection;
//!    ingest → [`Welcome`](messages::Welcome) or [`Refused`](messages::Refused);
//! 2. recorder → [`OpenTrack`](messages::OpenTrack) per camera track (video,
//!    and sound when the camera has it) → `TrackOpened` with what this ingest
//!    already holds of it, or `TrackRefused` (not in the plan, over its
//!    [limits](limits), unsupported);
//! 3. recorder → [media units](frame::MediaUnit), numbered per track without
//!    gaps; `Gap` when it lost some or gave them up;
//! 4. ingest → `Ack` with each track's durable ranges, about once a second;
//! 5. recorder → `Renew` before the ticket expires; ingest → `Move` when it is
//!    being drained, and the recorder reconnects elsewhere.
//!
//! This crate does no I/O: both sides bring their own transport.
pub mod frame;
pub mod limits;
pub mod messages;
pub mod ranges;
pub mod sps;
pub mod ticket;

/// This crate's protocol version, sent in `Hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// The TLS application protocol both transports negotiate.
pub const ALPN: &[u8] = b"nw-backup/1";

/// TLS exporter label of the channel binding the recorder signs
/// ([`ticket::hello_payload`]); 32 bytes, empty context.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-nw-backup/1";

/// Length of the channel binding taken from the TLS exporter.
pub const CHANNEL_BINDING_BYTES: usize = 32;
