//! Versioned contracts for browser-facing media sessions (`media-session-v1`).
//!
//! A **media-session grant** is a one-camera, one-action, one-representation,
//! expiring, one-time authority to receive live video through a hosted SFU. It
//! is the only object the control plane ever hands to a browser or to an SFU
//! edge for that purpose, and the types here are deliberately built so that it
//! *cannot* carry anything else.
//!
//! What a grant may name:
//!
//! - opaque tenant, site, camera and session identifiers minted by the control
//!   plane (`[A-Za-z0-9_-]{1,64}`),
//! - one [`MediaAction`], one [`Representation`], one audience (an SFU region),
//! - three whole-second UTC timestamps and a bitrate ceiling.
//!
//! What a grant may never name, enforced by [`GrantScope::validate`] and
//! [`contains_local_material`] rather than by a caller's discipline: an RTSP or
//! ONVIF URL, a camera credential, a node IPC endpoint, a local filesystem or
//! SHM path, a storage bucket or credential, or any IPv4 literal. There is no
//! field for them and free text that looks like them is refused.
//!
//! ## Site identity
//!
//! `site_id` is the identifier of the node that owns the camera. The hosted
//! model has exactly one site per node today; when sites become a separate
//! object the field keeps its meaning and only its source changes.
//!
//! ## Canonical form
//!
//! [`MediaGrant::canonical_json`] serialises through a sorted-key map, matching
//! the node link protocol in `crates/nw-backend/src/cloud`. Signatures are
//! taken over exactly those bytes, so `crates/nw-cloud-protocol/fixtures/media_grant_v1.json`
//! is a protocol vector: changing a field name or its order is a wire break.
//!
//! ## What this module is not
//!
//! It defines authority and signalling *metadata*. It carries no SDP, no media,
//! no transport, and it does not imply that a publisher, an SFU or a player
//! exists. See `docs/adr/0002-sfu-and-turn-selection.md`.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// Wire version of every type in this module except the publish directives.
pub const MEDIA_SESSION_PROTOCOL_VERSION: u16 = 1;

/// Wire version of [`PublishDirective`] and [`PublishDirectiveBatch`]. Version 2
/// added [`PublishDirective::publisher_token`]: the control plane mints the
/// node's SFU credential, so a node holds no SFU secret and needs no media
/// configuration of its own.
pub const PUBLISH_DIRECTIVE_PROTOCOL_VERSION: u16 = 2;

/// Longest life a grant may be issued for. A grant is a handoff token, not a
/// session: the session outlives it and is re-granted.
pub const MAX_GRANT_TTL_SECONDS: i64 = 60;

/// Longest `issued_at` → `not_before` gap. A grant is for now, not for later.
pub const MAX_GRANT_START_DELAY_SECONDS: i64 = 10;

/// Ceiling on the bitrate a grant may authorise, as a sanity bound on policy.
pub const MAX_GRANT_BITRATE_KBPS: u32 = 50_000;

/// Longest opaque identifier the control plane will mint or accept.
pub const MAX_IDENTIFIER_LEN: usize = 64;

/// Why a media session or grant was refused.
///
/// These codes are stable: they are written to the audit log and returned as
/// machine-readable denial reasons, so they may be added to but not renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantDenialReason {
    /// No identity provider is configured, so no browser principal can exist.
    IdentityUnavailable,
    /// The caller presented no credential, or one that did not verify.
    NotAuthenticated,
    /// The grant contract itself is malformed or of an unknown version.
    MalformedGrant,
    /// The requested action exists in the contract but is not implemented.
    ActionNotSupported,
    /// A field carried an RTSP URL, a credential, a path or a LAN address.
    ForbiddenMaterial,
    /// The requested lifetime is longer than [`MAX_GRANT_TTL_SECONDS`].
    GrantTtlTooLong,
    /// The grant has expired, or is not yet valid.
    GrantExpired,
    /// The grant's single use has already been spent.
    GrantAlreadyConsumed,
    /// The signature did not verify against the control plane's key.
    GrantSignatureInvalid,
    /// The grant was minted for a different SFU region.
    AudienceMismatch,
    /// The camera is unknown within the caller's tenant.
    UnknownCamera,
    /// The camera exists but live viewing is not enabled for it.
    CameraNotLiveEnabled,
    /// The principal's tenant does not own the requested object.
    TenantMismatch,
    /// This representation is not permitted by the tenant's policy.
    RepresentationNotPermitted,
    /// The tenant has no persisted media policy, so nothing is permitted.
    PolicyMissing,
    /// The tenant is already at its concurrent-session ceiling.
    ConcurrentSessionLimit,
    /// The session was revoked, or never existed for this principal.
    SessionRevoked,
    /// The node that owns the camera is not linked.
    NodeUnavailable,
}

impl GrantDenialReason {
    /// The stable wire code, as written to the audit log.
    pub fn code(self) -> &'static str {
        match self {
            Self::IdentityUnavailable => "identity_unavailable",
            Self::NotAuthenticated => "not_authenticated",
            Self::MalformedGrant => "malformed_grant",
            Self::ActionNotSupported => "action_not_supported",
            Self::ForbiddenMaterial => "forbidden_material",
            Self::GrantTtlTooLong => "grant_ttl_too_long",
            Self::GrantExpired => "grant_expired",
            Self::GrantAlreadyConsumed => "grant_already_consumed",
            Self::GrantSignatureInvalid => "grant_signature_invalid",
            Self::AudienceMismatch => "audience_mismatch",
            Self::UnknownCamera => "unknown_camera",
            Self::CameraNotLiveEnabled => "camera_not_live_enabled",
            Self::TenantMismatch => "tenant_mismatch",
            Self::RepresentationNotPermitted => "representation_not_permitted",
            Self::PolicyMissing => "policy_missing",
            Self::ConcurrentSessionLimit => "concurrent_session_limit",
            Self::SessionRevoked => "session_revoked",
            Self::NodeUnavailable => "node_unavailable",
        }
    }
}

/// A refusal, carrying a stable reason and an operator-facing note.
///
/// The note describes the *rule* that was broken and never quotes the value
/// that broke it: quoting is how an RTSP URL would escape into a log line.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{reason:?}: {note}")]
pub struct MediaGrantError {
    reason: GrantDenialReason,
    note: &'static str,
}

impl MediaGrantError {
    fn new(reason: GrantDenialReason, note: &'static str) -> Self {
        Self { reason, note }
    }

    pub fn reason(&self) -> GrantDenialReason {
        self.reason
    }

    pub fn note(&self) -> &'static str {
        self.note
    }
}

type Result<T> = core::result::Result<T, MediaGrantError>;

/// What a grant authorises the holder to do with one camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaAction {
    /// Receive the live stream.
    Live,
    /// Receive recorded video, played back by the node from its own
    /// recordings at the position a [`PlaybackControl`] names.
    Playback,
    /// Reserved for export. Refused: no such media path exists.
    Export,
}

impl MediaAction {
    /// Reserved actions are named in the contract so that a request for one is
    /// refused with a specific reason instead of a parse error, and so adding
    /// them later is not a wire break.
    pub fn is_implemented(self) -> bool {
        matches!(self, Self::Live | Self::Playback)
    }
}

/// Which encoding of a camera a grant authorises. One grant, one
/// representation: a `sub` grant can never be spent on `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Representation {
    Sub,
    Main,
}

impl Representation {
    pub fn code(self) -> &'static str {
        match self {
            Self::Sub => "sub",
            Self::Main => "main",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "sub" => Some(Self::Sub),
            "main" => Some(Self::Main),
            _ => None,
        }
    }
}

/// Exactly what one grant covers. Every field is an opaque control-plane
/// identifier or a closed enum; there is nowhere to put an address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantScope {
    pub tenant_id: String,
    /// The node that owns the camera; see the module documentation.
    pub site_id: String,
    /// The hosted opaque camera identifier, never a local source address.
    pub camera_id: String,
    pub action: MediaAction,
    pub representation: Representation,
}

impl GrantScope {
    pub fn validate(&self) -> Result<()> {
        require_identifier(&self.tenant_id)?;
        require_identifier(&self.site_id)?;
        require_identifier(&self.camera_id)?;
        if !self.action.is_implemented() {
            return Err(MediaGrantError::new(
                GrantDenialReason::ActionNotSupported,
                "only the live action is implemented",
            ));
        }
        Ok(())
    }

    /// Value equality on every dimension. There is no wildcard and no prefix
    /// match: a grant for `cam-1` does not cover `cam-10`.
    pub fn matches(
        &self,
        tenant_id: &str,
        site_id: &str,
        camera_id: &str,
        action: MediaAction,
        representation: Representation,
    ) -> bool {
        self.tenant_id == tenant_id
            && self.site_id == site_id
            && self.camera_id == camera_id
            && self.action == action
            && self.representation == representation
    }
}

/// A one-time, expiring authority to receive one camera's live stream.
///
/// Single use is not a property of this struct — it cannot be, since the struct
/// is copied to the browser and on to the SFU. It is enforced by the control
/// plane spending [`MediaGrant::grant_id`] exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaGrant {
    pub protocol_version: u16,
    /// Unique per grant. Spending it is what makes the grant one-time.
    pub grant_id: String,
    /// The media session this grant belongs to; sessions outlive grants.
    pub session_id: String,
    /// The SFU region allowed to accept this grant.
    pub audience: String,
    pub scope: GrantScope,
    pub issued_at_utc: String,
    pub not_before_utc: String,
    pub expires_at_utc: String,
    pub max_bitrate_kbps: u32,
}

impl MediaGrant {
    /// Every structural rule, independent of the current time.
    pub fn validate(&self) -> Result<()> {
        require_version(self.protocol_version)?;
        require_identifier(&self.grant_id)?;
        require_identifier(&self.session_id)?;
        require_identifier(&self.audience)?;
        self.scope.validate()?;

        let issued_at = require_canonical_utc(&self.issued_at_utc)?;
        let not_before = require_canonical_utc(&self.not_before_utc)?;
        let expires_at = require_canonical_utc(&self.expires_at_utc)?;
        if not_before < issued_at {
            return Err(MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "not_before_utc precedes issued_at_utc",
            ));
        }
        if (not_before - issued_at).whole_seconds() > MAX_GRANT_START_DELAY_SECONDS {
            return Err(MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "a grant may not be minted for a distant start time",
            ));
        }
        if expires_at <= not_before {
            return Err(MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "expires_at_utc must be after not_before_utc",
            ));
        }
        if (expires_at - issued_at).whole_seconds() > MAX_GRANT_TTL_SECONDS {
            return Err(MediaGrantError::new(
                GrantDenialReason::GrantTtlTooLong,
                "a grant may not outlive the maximum grant TTL",
            ));
        }
        if self.max_bitrate_kbps == 0 || self.max_bitrate_kbps > MAX_GRANT_BITRATE_KBPS {
            return Err(MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "max_bitrate_kbps is outside the permitted range",
            ));
        }
        Ok(())
    }

    /// Whether the grant is within its window at `now`. Separate from
    /// [`MediaGrant::validate`] so a verifier reports "expired" rather than
    /// "malformed" for a grant that was well-formed when it was minted.
    pub fn is_usable_at(&self, now: OffsetDateTime) -> Result<()> {
        let not_before = require_canonical_utc(&self.not_before_utc)?;
        let expires_at = require_canonical_utc(&self.expires_at_utc)?;
        if now < not_before || now >= expires_at {
            return Err(MediaGrantError::new(
                GrantDenialReason::GrantExpired,
                "the grant is outside its validity window",
            ));
        }
        Ok(())
    }

    /// Convenience over [`GrantScope::matches`].
    pub fn authorises(
        &self,
        tenant_id: &str,
        site_id: &str,
        camera_id: &str,
        action: MediaAction,
        representation: Representation,
    ) -> bool {
        self.scope
            .matches(tenant_id, site_id, camera_id, action, representation)
    }

    /// The exact bytes a signature covers: sorted-key JSON, matching the node
    /// link protocol.
    pub fn canonical_json(&self) -> Result<String> {
        let value = serde_json::to_value(self).map_err(|_| {
            MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "the grant could not be serialised",
            )
        })?;
        serde_json::to_string(&value).map_err(|_| {
            MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "the grant could not be serialised",
            )
        })
    }

    /// Parse and structurally validate in one step. An unknown field is a
    /// rejection, not something to ignore.
    pub fn from_canonical_json(raw: &str) -> Result<Self> {
        let grant: Self = serde_json::from_str(raw).map_err(|_| {
            MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "the grant does not match the contract",
            )
        })?;
        grant.validate()?;
        Ok(grant)
    }
}

// ---------------------------------------------------------------------------
// Signalling handoff
// ---------------------------------------------------------------------------

/// One ICE server offered to a peer. `username` and `credential` are the
/// standard WebRTC fields and carry **ephemeral, per-session** TURN credentials
/// only; the long-lived TURN secret never appears here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
    pub credential_ttl_seconds: Option<u64>,
}

impl IceServer {
    pub fn validate(&self) -> Result<()> {
        if self.urls.is_empty() {
            return Err(MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "an ICE server must offer at least one URL",
            ));
        }
        for url in &self.urls {
            require_public_endpoint(url, &["stun:", "stuns:", "turn:", "turns:"])?;
        }
        for value in [&self.username, &self.credential].into_iter().flatten() {
            require_no_local_material(value)?;
        }
        Ok(())
    }
}

/// Where a peer negotiates, and through what relays. Every field is a hosted,
/// publicly reachable endpoint: nothing here can point into a customer LAN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SfuEndpoint {
    /// Opaque region identifier; also the grant audience for this edge.
    pub region_id: String,
    /// HTTPS signalling URL — WHEP for a subscriber, WHIP for a publisher.
    pub signaling_url: String,
    pub ice_servers: Vec<IceServer>,
}

/// Which SFU signalling endpoints a verifier accepts.
///
/// Production is [`EndpointPolicy::PUBLIC`]: a named, public host. The only
/// relaxation is [`EndpointPolicy::DEVELOPMENT_LOOPBACK`], which additionally
/// admits exactly `ws(s)://127.0.0.1:<port>` as the signalling URL so a node
/// can publish to an SFU on the developer's own machine. It is selected only
/// by the node's explicit `NW_CLOUD_DEV_ALLOW_LOOPBACK_HTTP=1` opt-in; ICE
/// servers stay public under every policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndpointPolicy {
    pub allow_loopback_signaling: bool,
}

impl EndpointPolicy {
    pub const PUBLIC: Self = Self {
        allow_loopback_signaling: false,
    };
    pub const DEVELOPMENT_LOOPBACK: Self = Self {
        allow_loopback_signaling: true,
    };
}

/// `ws://127.0.0.1:<port>` or `wss://127.0.0.1:<port>`, optionally with a
/// trailing `/`, and nothing else.
fn is_loopback_signaling(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("ws://127.0.0.1:")
        .or_else(|| url.strip_prefix("wss://127.0.0.1:"))
    else {
        return false;
    };
    let port = rest.strip_suffix('/').unwrap_or(rest);
    !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0)
}

impl SfuEndpoint {
    pub fn validate(&self) -> Result<()> {
        self.validate_with(EndpointPolicy::PUBLIC)
    }

    pub fn validate_with(&self, policy: EndpointPolicy) -> Result<()> {
        require_identifier(&self.region_id)?;
        if !(policy.allow_loopback_signaling && is_loopback_signaling(&self.signaling_url)) {
            require_public_endpoint(&self.signaling_url, &["https:", "wss:"])?;
        }
        for server in &self.ice_servers {
            server.validate()?;
        }
        Ok(())
    }
}

/// What the control plane returns to a **browser** that opened a live session.
///
/// Deliberately only two things: a short-lived grant, and opaque SFU metadata.
/// There is no camera address, no node address and no local identifier — the
/// browser's only media peer is the SFU edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaSessionHandoff {
    pub protocol_version: u16,
    pub session_id: String,
    /// The compact signed grant token; see `docs/adr/0002-sfu-and-turn-selection.md`.
    pub grant: String,
    pub grant_expires_at_utc: String,
    pub sfu: SfuEndpoint,
    /// Short-lived LiveKit participant token, scoped to this session's room
    /// and to subscription only. It is a browser credential, never an API
    /// secret and never an administrator identity.
    pub livekit_access_token: String,
    /// How long the session may run before it must be recreated.
    pub max_duration_seconds: u64,
}

impl MediaSessionHandoff {
    pub fn validate(&self) -> Result<()> {
        require_version(self.protocol_version)?;
        require_identifier(&self.session_id)?;
        require_grant_token(&self.grant)?;
        require_canonical_utc(&self.grant_expires_at_utc)?;
        self.sfu.validate()?;
        require_grant_token(&self.livekit_access_token)?;
        if self.max_duration_seconds == 0 {
            return Err(MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "max_duration_seconds must be nonzero",
            ));
        }
        Ok(())
    }
}

/// What the control plane sends **outbound to a node** so it publishes one
/// camera to the SFU ingest.
///
/// The node already knows its own cameras; it is told which session and where
/// to publish, never how to reach a camera.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishDirective {
    pub protocol_version: u16,
    /// Opaque identifier for acknowledging this exact grant generation.
    pub directive_id: String,
    pub session_id: String,
    pub grant: String,
    pub sfu: SfuEndpoint,
    /// The SFU access token the node joins `sfu` with: minted by the control
    /// plane for this one room (the session), allowed to publish and nothing
    /// a viewer could not see, and expiring with the grant. The SFU's API
    /// secret never leaves the control plane.
    pub publisher_token: String,
    /// Present exactly on playback directives: where and how to play. Absent
    /// (and not serialised) on live directives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback: Option<PlaybackControl>,
}

/// Supported playback rates, in percent of real time.
pub const PLAYBACK_SPEEDS_PERCENT: [u16; 6] = [25, 50, 100, 200, 400, 800];

/// How a node plays one playback session. The control plane raises
/// `revision` with every viewer change, so a redelivered directive is applied
/// once and a newer one always wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackControl {
    /// Recording time to play from, canonical whole-second UTC.
    pub position_at_utc: String,
    /// One of [`PLAYBACK_SPEEDS_PERCENT`].
    pub speed_percent: u16,
    pub playing: bool,
    pub revision: u32,
}

impl PlaybackControl {
    pub fn validate(&self) -> Result<()> {
        require_canonical_utc(&self.position_at_utc)?;
        if !PLAYBACK_SPEEDS_PERCENT.contains(&self.speed_percent) {
            return Err(MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "unsupported playback speed",
            ));
        }
        Ok(())
    }

    /// The playback position as an instant.
    pub fn position(&self) -> Result<OffsetDateTime> {
        require_canonical_utc(&self.position_at_utc)
    }
}

impl PublishDirective {
    pub fn validate(&self) -> Result<()> {
        self.validate_with(EndpointPolicy::PUBLIC)
    }

    pub fn validate_with(&self, policy: EndpointPolicy) -> Result<()> {
        require_directive_version(self.protocol_version)?;
        require_identifier(&self.directive_id)?;
        require_identifier(&self.session_id)?;
        require_grant_token(&self.grant)?;
        require_compact_token(
            &self.publisher_token,
            "the publisher token is not a three-segment compact token",
        )?;
        if let Some(control) = &self.playback {
            control.validate()?;
        }
        self.sfu.validate_with(policy)
    }
}

/// Every directive a node currently has outstanding, as returned by the
/// node-facing pull.
///
/// The batch is the unit of trust: [`PublishDirectiveBatch::validate`] refuses
/// the whole answer if any member is malformed or carries site-local material,
/// so a node never picks the acceptable directives out of an answer that was
/// not wholly acceptable to send. An empty batch is the ordinary "nothing to
/// publish" reply and is valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishDirectiveBatch {
    pub protocol_version: u16,
    pub directives: Vec<PublishDirective>,
    /// Sessions that remain authorized for this node, including directives
    /// delivered by an earlier pull. Absence on a later pull is cancellation.
    #[serde(default)]
    pub active_session_ids: Vec<String>,
}

/// Bound on one pull's answer, so a node's work per pull is bounded by the
/// contract rather than by the control plane's good behaviour.
pub const MAX_DIRECTIVES_PER_BATCH: usize = 64;

impl PublishDirectiveBatch {
    pub fn validate(&self) -> Result<()> {
        self.validate_with(EndpointPolicy::PUBLIC)
    }

    pub fn validate_with(&self, policy: EndpointPolicy) -> Result<()> {
        require_directive_version(self.protocol_version)?;
        if self.directives.len() > MAX_DIRECTIVES_PER_BATCH
            || self.active_session_ids.len() > MAX_DIRECTIVES_PER_BATCH
        {
            return Err(MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "a batch may not carry more than the maximum number of directives",
            ));
        }
        for directive in &self.directives {
            directive.validate_with(policy)?;
        }
        for session_id in &self.active_session_ids {
            require_identifier(session_id)?;
        }
        for (index, session_id) in self.active_session_ids.iter().enumerate() {
            if self.active_session_ids[..index].contains(session_id) {
                return Err(MediaGrantError::new(
                    GrantDenialReason::MalformedGrant,
                    "a batch must not repeat an active session_id",
                ));
            }
        }
        if self
            .directives
            .iter()
            .any(|directive| !self.active_session_ids.contains(&directive.session_id))
        {
            return Err(MediaGrantError::new(
                GrantDenialReason::MalformedGrant,
                "every directive must name an active session",
            ));
        }
        // Two directives for one session would name two publications of the
        // same thing; there is no order that resolves which one is meant.
        for (index, directive) in self.directives.iter().enumerate() {
            if self.directives[..index].iter().any(|earlier| {
                earlier.session_id == directive.session_id
                    || earlier.directive_id == directive.directive_id
            }) {
                return Err(MediaGrantError::new(
                    GrantDenialReason::MalformedGrant,
                    "a batch must not repeat a session_id or directive_id",
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Shared rules
// ---------------------------------------------------------------------------

/// Markers of material that belongs to a customer site and must never appear in
/// a hosted contract. Returns the marker that matched, for an operator-facing
/// log that names the *rule* without quoting the offending value.
///
/// Deliberately not word-based: `credential` and `password` are legitimate
/// field names in the WebRTC ICE contract, so matching on them would refuse
/// correct data while catching nothing an attacker would send.
pub fn contains_local_material(text: &str) -> Option<&'static str> {
    const MARKERS: [&str; 8] = [
        "rtsp://",
        "rtsps://",
        "onvif",
        "/dev/shm/",
        ".sock",
        "unix:",
        "file://",
        "s3://",
    ];
    let lowered = text.to_ascii_lowercase();
    for marker in MARKERS {
        if lowered.contains(marker) {
            return Some(marker);
        }
    }
    if lowered.contains("-----begin") {
        return Some("private key block");
    }
    if contains_ipv4(text) {
        return Some("ipv4 address");
    }
    None
}

/// True if the text contains four dot-separated groups of 1–3 digits. Mirrors
/// the node-side and control-plane-side checks so the three agree.
fn contains_ipv4(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .any(|token| {
            let groups: Vec<&str> = token.split('.').collect();
            groups.len() >= 4
                && groups
                    .windows(4)
                    .any(|window| window.iter().all(|group| (1..=3).contains(&group.len())))
        })
}

fn require_no_local_material(text: &str) -> Result<()> {
    match contains_local_material(text) {
        Some(_) => Err(MediaGrantError::new(
            GrantDenialReason::ForbiddenMaterial,
            "the value carries site-local material",
        )),
        None => Ok(()),
    }
}

fn require_directive_version(version: u16) -> Result<()> {
    if version == PUBLISH_DIRECTIVE_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(MediaGrantError::new(
            GrantDenialReason::MalformedGrant,
            "unsupported publish-directive protocol version",
        ))
    }
}

fn require_version(version: u16) -> Result<()> {
    if version == MEDIA_SESSION_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(MediaGrantError::new(
            GrantDenialReason::MalformedGrant,
            "unsupported media-session protocol version",
        ))
    }
}

/// An opaque control-plane identifier: printable, bounded, and with no
/// punctuation that could form an address, a path or a URL.
fn require_identifier(value: &str) -> Result<()> {
    let shaped = !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !shaped {
        return Err(MediaGrantError::new(
            GrantDenialReason::ForbiddenMaterial,
            "identifiers must be opaque, bounded and free of address punctuation",
        ));
    }
    Ok(())
}

/// The compact grant token as it travels: three dot-separated url-safe base64
/// segments. Validated only for shape here; the signature is checked by
/// whoever holds the control plane's public key.
fn require_grant_token(token: &str) -> Result<()> {
    require_compact_token(
        token,
        "the grant token is not a three-segment compact token",
    )
}

/// Three dot-separated url-safe base64 segments (a NeuroWatch grant, or a
/// JWT such as an SFU access token), bounded in size.
fn require_compact_token(token: &str, note: &'static str) -> Result<()> {
    let segments: Vec<&str> = token.split('.').collect();
    let shaped = segments.len() == 3
        && !token.is_empty()
        && token.len() <= 4_096
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        });
    if !shaped {
        return Err(MediaGrantError::new(
            GrantDenialReason::MalformedGrant,
            note,
        ));
    }
    Ok(())
}

/// Canonical whole-second UTC RFC 3339, matching what the node signs.
///
/// The shape is checked lexically before parsing because stored and signed
/// timestamps are compared as text: accepting `+00:00`, a lowercase `z` or
/// fractional digits would make two spellings of one instant order by
/// punctuation and break signature verification.
fn require_canonical_utc(value: &str) -> Result<OffsetDateTime> {
    let canonical =
        value.len() == 20 && value.ends_with('Z') && value.is_ascii() && !value.contains('.');
    if !canonical {
        return Err(MediaGrantError::new(
            GrantDenialReason::MalformedGrant,
            "timestamps must be canonical whole-second UTC RFC 3339",
        ));
    }
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
        MediaGrantError::new(
            GrantDenialReason::MalformedGrant,
            "timestamps must be canonical whole-second UTC RFC 3339",
        )
    })
}

/// A hosted endpoint a browser or a node may be pointed at: an allowed scheme,
/// a name rather than an address, and nothing that resolves inside a LAN.
///
/// Loopback and private ranges are refused outright. There is no hosted
/// deployment yet, and the moment one exists it will have a real name; a local
/// stand-in must be an explicit, separately reviewed decision rather than
/// something this contract quietly permits.
fn require_public_endpoint(url: &str, allowed_schemes: &[&str]) -> Result<()> {
    require_no_local_material(url)?;
    let lowered = url.to_ascii_lowercase();
    if !allowed_schemes
        .iter()
        .any(|scheme| lowered.starts_with(scheme))
    {
        return Err(MediaGrantError::new(
            GrantDenialReason::ForbiddenMaterial,
            "the endpoint scheme is not permitted",
        ));
    }
    if url.len() > 512 || url.chars().any(char::is_control) {
        return Err(MediaGrantError::new(
            GrantDenialReason::MalformedGrant,
            "the endpoint is malformed",
        ));
    }
    let authority = match lowered.split_once("://") {
        Some((_, rest)) => rest,
        // `stun:host:port` and `turn:host:port` have no `//`.
        None => match lowered.split_once(':') {
            Some((_, rest)) => rest,
            None => &lowered,
        },
    };
    let host_port = authority.split(['/', '?', '#']).next().unwrap_or(authority);
    if host_port.contains('@') {
        return Err(MediaGrantError::new(
            GrantDenialReason::ForbiddenMaterial,
            "an endpoint must not embed credentials",
        ));
    }
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host.is_empty() {
        return Err(MediaGrantError::new(
            GrantDenialReason::MalformedGrant,
            "the endpoint has no host",
        ));
    }
    if is_private_host(host) {
        return Err(MediaGrantError::new(
            GrantDenialReason::ForbiddenMaterial,
            "the endpoint points at a loopback or private address",
        ));
    }
    Ok(())
}

/// Loopback, link-local and RFC 1918 hosts, plus bare IP literals in general:
/// a hosted media edge is named, not numbered.
fn is_private_host(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    if host.starts_with('[') {
        // Any IPv6 literal, including `[::1]`.
        return true;
    }
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return true;
    }
    let octets: Vec<&str> = host.split('.').collect();
    if octets.len() != 4 || !octets.iter().all(|part| part.parse::<u8>().is_ok()) {
        return false;
    }
    // Every IPv4 literal is refused; the private ranges are merely the reason
    // the rule exists.
    true
}
