//! Camera discovery wire types, shared by the node and every UI.
//!
//! The node that owns the cameras runs discovery; the desktop asks it over
//! local JSON-RPC and the web portal through the node's configuration
//! channel. Candidates and streams are named by short-lived opaque ids: a UI
//! never sends a device or stream URL back to the node, and the node resolves
//! the real addresses from its own scan cache.

use serde::{Deserialize, Serialize};

/// Version of the `camera.discovery.*` request/result contract.
pub const DISCOVERY_PROTOCOL_VERSION: u32 = 1;
/// Most IPv4 addresses one search may probe.
pub const MAX_SCAN_ADDRESSES: u32 = 1024;

/// How a configured camera was found, kept so the same device and channel
/// are recognised by later searches. An NVR answers with one endpoint
/// reference for many channels, so `source` (the media profile token)
/// distinguishes its streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryIdentity {
    /// WS-Discovery endpoint reference, e.g. `urn:uuid:…`.
    pub endpoint_reference: String,
    /// ONVIF media profile token of the main stream, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// A network interface a search can run on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryInterface {
    /// Stable for the node's lifetime: the interface name.
    pub id: String,
    pub name: String,
    /// The interface's IPv4 address.
    pub address: String,
    pub prefix: u8,
    /// The network it is on, e.g. `192.168.1.0/24`.
    pub cidr: String,
    /// The interface of the default route: what `scope: "current"` searches.
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanStatus {
    Running,
    Complete,
    Cancelled,
    Failed,
}

/// What adding a candidate would mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateState {
    /// Answers device queries without a password.
    Ready,
    /// Answers discovery but asks for a username and password.
    CredentialsNeeded,
    /// Same discovery identity or stream URL as a configured camera.
    AlreadyAdded,
    /// Same device or address as a configured camera, but maybe another stream.
    PossibleMatch,
    /// Answered discovery but its device service could not be reached.
    Unavailable,
}

/// A configured camera a candidate matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraRef {
    pub id: String,
    pub name: String,
}

/// One device that answered a search. Only what the device itself reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryCandidate {
    pub candidate_id: String,
    /// IPv4 address the device answered from, for display.
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    pub state: CandidateState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_camera: Option<CameraRef>,
    /// The device's WS-Discovery endpoint reference, stored with a camera
    /// added from it (see [`DiscoveryIdentity`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_reference: Option<String>,
}

/// A search and what it found so far.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanResults {
    pub scan_id: String,
    pub status: ScanStatus,
    /// The normalized network searched, e.g. `10.0.10.0/24`.
    pub scope: String,
    /// Addresses probed one by one so far, and in all (0 for a multicast-only search).
    pub probed: u32,
    pub total: u32,
    pub candidates: Vec<DiscoveryCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One stream a device offers, named by an opaque id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamChoice {
    pub stream_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// The ONVIF profile token: the `source` of a [`DiscoveryIdentity`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The stream URL, without credentials. Local desktop answers only; the
    /// node leaves it out of every answer sent to the web portal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
