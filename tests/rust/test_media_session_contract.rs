//! Contract tests for the versioned media-session grant types in `nw-core`.
//!
//! A media-session grant is the only thing the hosted control plane will ever
//! hand a browser or an SFU to authorise live video. These tests pin the two
//! properties that make that safe to do:
//!
//! 1. **Shape.** The wire form is versioned, canonical and round-trips exactly,
//!    so the control plane, the node and the SFU cannot drift apart silently.
//! 2. **Secrecy.** A grant — and the signalling handoff that carries it —
//!    cannot be constructed carrying an RTSP URL, a camera credential, a node
//!    IPC endpoint, a local path, a storage credential or a LAN address. The
//!    rejection is in the type's own validation, not in a caller's discipline.

use nw_cloud_protocol::media_session::{
    contains_local_material, EndpointPolicy, GrantDenialReason, GrantScope, IceServer, MediaAction,
    MediaGrant, MediaSessionHandoff, PlaybackControl, PublishDirective, PublishDirectiveBatch,
    Representation, SfuEndpoint, MAX_GRANT_TTL_SECONDS, MEDIA_SESSION_PROTOCOL_VERSION,
    PLAYBACK_SPEEDS_PERCENT, PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
};

const VECTOR: &str = include_str!("../../crates/nw-cloud-protocol/fixtures/media_grant_v1.json");

fn grant() -> MediaGrant {
    MediaGrant {
        protocol_version: MEDIA_SESSION_PROTOCOL_VERSION,
        grant_id: "1f4f4f6c8c2a4e0fa1b6e1c2d3e4f506".into(),
        session_id: "sess-9a2c".into(),
        audience: "sfu-eu-west-1".into(),
        scope: GrantScope {
            tenant_id: "5f2b".into(),
            site_id: "node-7c1e".into(),
            camera_id: "cam-lobby-01".into(),
            action: MediaAction::Live,
            representation: Representation::Sub,
        },
        issued_at_utc: "2026-09-25T12:00:00Z".into(),
        not_before_utc: "2026-09-25T12:00:00Z".into(),
        expires_at_utc: "2026-09-25T12:00:45Z".into(),
        max_bitrate_kbps: 2500,
    }
}

fn instant(raw: &str) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .expect("test timestamp")
}

fn endpoint() -> SfuEndpoint {
    SfuEndpoint {
        region_id: "eu-west-1".into(),
        signaling_url: "https://sfu.example.test/whep/sess-9a2c".into(),
        ice_servers: vec![IceServer {
            urls: vec!["turns:turn.example.test:5349?transport=tcp".into()],
            username: Some("1790000000:sess-9a2c".into()),
            credential: Some("Zm9yLXRoaXMtc2Vzc2lvbi1vbmx5".into()),
            credential_ttl_seconds: Some(120),
        }],
    }
}

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

#[test]
fn a_live_grant_round_trips_through_its_canonical_form() {
    let original = grant();
    original.validate().expect("the fixture grant is valid");
    let canonical = original.canonical_json().expect("canonical json");
    let parsed = MediaGrant::from_canonical_json(&canonical).expect("parse");
    assert_eq!(parsed, original);
}

#[test]
fn the_canonical_form_matches_the_stored_protocol_vector() {
    // Byte-for-byte: the SFU and the node verify a signature over exactly these
    // bytes, so a field rename or a reordering is a protocol break.
    assert_eq!(grant().canonical_json().expect("canonical"), VECTOR.trim());
    assert_eq!(
        MediaGrant::from_canonical_json(VECTOR.trim()).expect("vector parses"),
        grant()
    );
}

#[test]
fn an_unsupported_protocol_version_is_rejected() {
    let mut future = grant();
    future.protocol_version = MEDIA_SESSION_PROTOCOL_VERSION + 1;
    assert!(future.validate().is_err());
}

#[test]
fn a_grant_with_an_unexpected_field_is_rejected_rather_than_ignored() {
    // The allowlist is the defence: a field the control plane does not know
    // about must not survive a decode and reach storage or a log.
    let mut value: serde_json::Value = serde_json::from_str(VECTOR.trim()).expect("vector");
    value["rtsp_url"] = serde_json::Value::String("rtsp://10.0.0.5/stream".into());
    let raw = serde_json::to_string(&value).expect("json");
    assert!(MediaGrant::from_canonical_json(&raw).is_err());
}

#[test]
fn export_is_rejected_until_it_exists_and_playback_grants_validate() {
    let mut reserved = grant();
    reserved.scope.action = MediaAction::Export;
    let error = reserved.validate().expect_err("reserved action");
    assert_eq!(error.reason(), GrantDenialReason::ActionNotSupported);

    let mut playback = grant();
    playback.scope.action = MediaAction::Playback;
    playback.validate().expect("playback grant");
}

#[test]
fn a_grant_is_bound_to_exactly_one_camera_action_and_representation() {
    let issued = grant();
    // The binding is a value comparison, not a prefix or wildcard match.
    assert!(issued.authorises(
        "5f2b",
        "node-7c1e",
        "cam-lobby-01",
        MediaAction::Live,
        Representation::Sub
    ));
    assert!(!issued.authorises(
        "5f2c",
        "node-7c1e",
        "cam-lobby-01",
        MediaAction::Live,
        Representation::Sub
    ));
    assert!(!issued.authorises(
        "5f2b",
        "node-7c1f",
        "cam-lobby-01",
        MediaAction::Live,
        Representation::Sub
    ));
    assert!(!issued.authorises(
        "5f2b",
        "node-7c1e",
        "cam-lobby-02",
        MediaAction::Live,
        Representation::Sub
    ));
    assert!(!issued.authorises(
        "5f2b",
        "node-7c1e",
        "cam-lobby-01",
        MediaAction::Live,
        Representation::Main
    ));
    assert!(!issued.authorises(
        "5f2b",
        "node-7c1e",
        "cam-lobby-0",
        MediaAction::Live,
        Representation::Sub
    ));
}

// ---------------------------------------------------------------------------
// Expiry
// ---------------------------------------------------------------------------

#[test]
fn a_grant_may_not_outlive_the_maximum_ttl() {
    let mut long = grant();
    long.expires_at_utc = "2026-09-25T12:05:00Z".into();
    let error = long.validate().expect_err("over-long grant");
    assert_eq!(error.reason(), GrantDenialReason::GrantTtlTooLong);
    const _: () = assert!(MAX_GRANT_TTL_SECONDS <= 60);
}

#[test]
fn a_grant_is_usable_only_between_not_before_and_expiry() {
    let issued = grant();
    assert!(issued
        .is_usable_at(instant("2026-09-25T11:59:59Z"))
        .is_err());
    issued
        .is_usable_at(instant("2026-09-25T12:00:00Z"))
        .expect("usable at nbf");
    issued
        .is_usable_at(instant("2026-09-25T12:00:44Z"))
        .expect("usable just before expiry");
    let expired = issued
        .is_usable_at(instant("2026-09-25T12:00:45Z"))
        .expect_err("expired at exp");
    assert_eq!(expired.reason(), GrantDenialReason::GrantExpired);
    assert_eq!(
        issued
            .is_usable_at(instant("2026-09-25T13:00:00Z"))
            .expect_err("long expired")
            .reason(),
        GrantDenialReason::GrantExpired
    );
}

#[test]
fn a_grant_whose_timestamps_disagree_is_rejected() {
    let mut backwards = grant();
    backwards.expires_at_utc = "2026-09-25T11:59:00Z".into();
    assert!(backwards.validate().is_err());

    let mut local_offset = grant();
    local_offset.expires_at_utc = "2026-09-25T12:00:45+02:00".into();
    assert!(local_offset.validate().is_err());

    let mut fractional = grant();
    fractional.issued_at_utc = "2026-09-25T12:00:00.250Z".into();
    assert!(
        fractional.validate().is_err(),
        "only whole-second UTC is signable"
    );
}

// ---------------------------------------------------------------------------
// Secrecy
// ---------------------------------------------------------------------------

#[test]
fn a_grant_cannot_be_built_around_local_material() {
    for poison in [
        "rtsp://user:pass@192.168.1.40/Streaming/Channels/101",
        "/dev/shm/neurowatch_cam-1",
        "/run/user/1000/neurowatch/control.sock",
        "s3://backup-bucket/segments",
        "192.168.1.40",
        "admin:hunter2",
    ] {
        let mut poisoned = grant();
        poisoned.scope.camera_id = poison.to_owned();
        assert!(
            poisoned.validate().is_err(),
            "camera_id {poison:?} must not validate"
        );

        let mut poisoned = grant();
        poisoned.audience = poison.to_owned();
        assert!(
            poisoned.validate().is_err(),
            "audience {poison:?} must not validate"
        );
    }
}

#[test]
fn the_secrecy_scan_names_what_it_found() {
    assert_eq!(
        contains_local_material("rtsp://10.1.2.3/stream"),
        Some("rtsp://")
    );
    assert_eq!(
        contains_local_material("/dev/shm/neurowatch_cam-1"),
        Some("/dev/shm/")
    );
    assert_eq!(contains_local_material("cam-lobby-01"), None);
    assert_eq!(contains_local_material("sfu.example.test"), None);
}

// ---------------------------------------------------------------------------
// Signalling handoff
// ---------------------------------------------------------------------------

#[test]
fn a_browser_handoff_carries_only_a_grant_and_opaque_sfu_metadata() {
    let handoff = MediaSessionHandoff {
        protocol_version: MEDIA_SESSION_PROTOCOL_VERSION,
        session_id: "sess-9a2c".into(),
        grant: "nwg1.eyJ9.c2ln".into(),
        grant_expires_at_utc: "2026-09-25T12:00:45Z".into(),
        sfu: endpoint(),
        livekit_access_token: "header.payload.signature".into(),
        max_duration_seconds: 3_600,
    };
    handoff.validate().expect("a public SFU handoff is valid");

    // Round-trips, and its serialised form names no camera and no node.
    let encoded = serde_json::to_string(&handoff).expect("encode");
    let decoded: MediaSessionHandoff = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, handoff);
    for forbidden in ["rtsp", "camera_id", "node_id", "/dev/shm", ".sock"] {
        assert!(
            !encoded.contains(forbidden),
            "handoff must not mention {forbidden}: {encoded}"
        );
    }
}

#[test]
fn a_handoff_pointing_at_a_lan_address_is_rejected() {
    for url in [
        "https://192.168.1.40/whep",
        "https://10.0.0.5:8443/whep",
        "https://127.0.0.1/whep",
        "rtsp://sfu.example.test/whep",
        "http://sfu.example.test/whep",
    ] {
        let mut handoff = MediaSessionHandoff {
            protocol_version: MEDIA_SESSION_PROTOCOL_VERSION,
            session_id: "sess-9a2c".into(),
            grant: "nwg1.eyJ9.c2ln".into(),
            grant_expires_at_utc: "2026-09-25T12:00:45Z".into(),
            sfu: endpoint(),
            livekit_access_token: "header.payload.signature".into(),
            max_duration_seconds: 3_600,
        };
        handoff.sfu.signaling_url = url.to_owned();
        assert!(
            handoff.validate().is_err(),
            "{url} must not be offered to a browser"
        );
    }

    let mut lan_ice = MediaSessionHandoff {
        protocol_version: MEDIA_SESSION_PROTOCOL_VERSION,
        session_id: "sess-9a2c".into(),
        grant: "nwg1.eyJ9.c2ln".into(),
        grant_expires_at_utc: "2026-09-25T12:00:45Z".into(),
        sfu: endpoint(),
        livekit_access_token: "header.payload.signature".into(),
        max_duration_seconds: 3_600,
    };
    lan_ice.sfu.ice_servers[0].urls = vec!["turn:192.168.1.40:3478".into()];
    assert!(lan_ice.validate().is_err(), "a LAN TURN relay is a leak");
}

#[test]
fn a_node_publish_directive_round_trips_and_stays_opaque() {
    let directive = PublishDirective {
        protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
        directive_id: "directive-9a2c".into(),
        session_id: "sess-9a2c".into(),
        grant: "nwg1.eyJ9.c2ln".into(),
        sfu: SfuEndpoint {
            region_id: "eu-west-1".into(),
            signaling_url: "https://ingest.example.test/whip/sess-9a2c".into(),
            ice_servers: endpoint().ice_servers,
        },
        publisher_token: PUBLISHER_TOKEN.into(),
        playback: None,
    };
    directive.validate().expect("publish directive is valid");
    let encoded = serde_json::to_string(&directive).expect("encode");
    assert!(
        !encoded.contains("playback"),
        "a live directive carries no playback control: {encoded}"
    );
    let decoded: PublishDirective = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, directive);
    assert!(contains_local_material(&encoded).is_none(), "{encoded}");
}

// ---------------------------------------------------------------------------
// The node-facing directive batch
// ---------------------------------------------------------------------------

fn directive(session: &str) -> PublishDirective {
    PublishDirective {
        protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
        directive_id: format!("directive-{session}"),
        session_id: session.into(),
        grant: "nwg1.eyJ9.c2ln".into(),
        sfu: SfuEndpoint {
            region_id: "eu-west-1".into(),
            signaling_url: "wss://ingest.example.test".into(),
            ice_servers: endpoint().ice_servers,
        },
        publisher_token: PUBLISHER_TOKEN.into(),
        playback: None,
    }
}

/// The shape of an SFU access token (a JWT); never a real one.
const PUBLISHER_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJub2RlIn0.c2lnbmF0dXJl";

#[test]
fn a_directive_without_a_well_formed_publisher_token_is_refused() {
    let mut missing = serde_json::to_value(directive("sess-1")).expect("encode");
    missing
        .as_object_mut()
        .expect("object")
        .remove("publisher_token");
    assert!(
        serde_json::from_value::<PublishDirective>(missing).is_err(),
        "a v2 directive always carries the node's SFU token"
    );
    for token in ["", "not-a-token", "a.b", "a.b.c.d", "a.b.c d"] {
        let mut bad = directive("sess-1");
        bad.publisher_token = token.into();
        assert_eq!(
            bad.validate().map_err(|e| e.reason()),
            Err(GrantDenialReason::MalformedGrant),
            "{token:?}"
        );
    }
}

#[test]
fn a_version_1_directive_is_refused() {
    let mut old = directive("sess-1");
    old.protocol_version = 1;
    assert_eq!(
        old.validate().map_err(|e| e.reason()),
        Err(GrantDenialReason::MalformedGrant),
        "v1 directives expected the node to hold the SFU secret"
    );
}

#[test]
fn a_directive_batch_round_trips_and_validates_every_member() {
    let batch = PublishDirectiveBatch {
        protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
        directives: vec![directive("sess-1"), directive("sess-2")],
        active_session_ids: vec!["sess-1".into(), "sess-2".into()],
    };
    batch.validate().expect("batch is valid");
    let encoded = serde_json::to_string(&batch).expect("encode");
    let decoded: PublishDirectiveBatch = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, batch);
    assert!(contains_local_material(&encoded).is_none(), "{encoded}");

    // An empty batch is the normal "nothing to publish" answer, not an error.
    let idle = PublishDirectiveBatch {
        protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
        directives: vec![],
        active_session_ids: vec![],
    };
    idle.validate().expect("an empty batch is valid");
}

#[test]
fn a_directive_batch_is_rejected_whole_when_one_member_is_unsafe() {
    // One bad directive poisons the batch: a node must not pick the safe ones
    // out of an answer that was not wholly safe to send.
    let mut batch = PublishDirectiveBatch {
        protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
        directives: vec![directive("sess-1"), directive("sess-2")],
        active_session_ids: vec!["sess-1".into(), "sess-2".into()],
    };
    batch.directives[1].sfu.signaling_url = "wss://192.168.4.4:7880".into();
    assert_eq!(
        batch.validate().map_err(|e| e.reason()),
        Err(GrantDenialReason::ForbiddenMaterial)
    );

    batch.directives[1] = directive("sess-2");
    batch.protocol_version = PUBLISH_DIRECTIVE_PROTOCOL_VERSION + 1;
    assert_eq!(
        batch.validate().map_err(|e| e.reason()),
        Err(GrantDenialReason::MalformedGrant)
    );
}

#[test]
fn a_directive_batch_may_not_repeat_a_session() {
    let batch = PublishDirectiveBatch {
        protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
        directives: vec![directive("sess-1"), directive("sess-1")],
        active_session_ids: vec!["sess-1".into()],
    };
    assert_eq!(
        batch.validate().map_err(|e| e.reason()),
        Err(GrantDenialReason::MalformedGrant),
        "two directives for one session are contradictory, not merely redundant"
    );
}

// ---------------------------------------------------------------------------
// Local development: an explicitly opted-in loopback SFU
// ---------------------------------------------------------------------------

fn loopback_directive(signaling_url: &str) -> PublishDirective {
    PublishDirective {
        sfu: SfuEndpoint {
            region_id: "local-dev".into(),
            signaling_url: signaling_url.into(),
            ice_servers: vec![],
        },
        ..directive("sess-9a2c")
    }
}

#[test]
fn a_loopback_sfu_is_refused_unless_development_explicitly_allows_it() {
    let directive = loopback_directive("ws://127.0.0.1:7880");
    assert!(directive.validate().is_err());
    assert!(directive.validate_with(EndpointPolicy::PUBLIC).is_err());
    directive
        .validate_with(EndpointPolicy::DEVELOPMENT_LOOPBACK)
        .expect("development loopback SFU");

    let batch = PublishDirectiveBatch {
        protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
        directives: vec![directive],
        active_session_ids: vec!["sess-9a2c".into()],
    };
    assert!(batch.validate().is_err());
    batch
        .validate_with(EndpointPolicy::DEVELOPMENT_LOOPBACK)
        .expect("development loopback batch");
}

#[test]
fn the_development_allowance_is_exactly_numeric_loopback_with_a_port() {
    for url in [
        "ws://127.0.0.2:7880",
        "ws://10.0.0.5:7880",
        "ws://localhost:7880",
        "ws://127.0.0.1",
        "http://127.0.0.1:7880",
        "ws://127.0.0.1:7880/rtsp://camera",
        "ws://user@127.0.0.1:7880",
        "ws://[::1]:7880",
    ] {
        assert!(
            loopback_directive(url)
                .validate_with(EndpointPolicy::DEVELOPMENT_LOOPBACK)
                .is_err(),
            "{url} must stay refused"
        );
    }
    for url in ["ws://127.0.0.1:7880", "wss://127.0.0.1:7443/"] {
        loopback_directive(url)
            .validate_with(EndpointPolicy::DEVELOPMENT_LOOPBACK)
            .expect(url);
    }
}

#[test]
fn ice_servers_stay_public_even_in_development() {
    let mut directive = loopback_directive("ws://127.0.0.1:7880");
    directive.sfu.ice_servers = vec![IceServer {
        urls: vec!["turn:127.0.0.1:3478".into()],
        username: None,
        credential: None,
        credential_ttl_seconds: None,
    }];
    assert!(directive
        .validate_with(EndpointPolicy::DEVELOPMENT_LOOPBACK)
        .is_err());
}

// ---------------------------------------------------------------------------
// Playback directives
// ---------------------------------------------------------------------------

fn playback_control() -> PlaybackControl {
    PlaybackControl {
        position_at_utc: "2026-09-25T11:58:00Z".into(),
        speed_percent: 100,
        playing: true,
        revision: 3,
    }
}

#[test]
fn a_playback_directive_carries_its_position_speed_and_revision() {
    let directive = PublishDirective {
        playback: Some(playback_control()),
        ..directive("sess-pb")
    };
    directive.validate().expect("playback directive");
    let encoded = serde_json::to_string(&directive).expect("encode");
    let decoded: PublishDirective = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, directive);
}

#[test]
fn playback_controls_are_bounded() {
    for control in [
        PlaybackControl {
            speed_percent: 150,
            ..playback_control()
        },
        PlaybackControl {
            speed_percent: 0,
            ..playback_control()
        },
        PlaybackControl {
            position_at_utc: "2026-09-25T11:58:00.5Z".into(),
            ..playback_control()
        },
        PlaybackControl {
            position_at_utc: "yesterday".into(),
            ..playback_control()
        },
    ] {
        let directive = PublishDirective {
            playback: Some(control.clone()),
            ..directive("sess-pb")
        };
        assert!(directive.validate().is_err(), "{control:?} must be refused");
    }
    for speed in PLAYBACK_SPEEDS_PERCENT {
        PlaybackControl {
            speed_percent: speed,
            ..playback_control()
        }
        .validate()
        .expect("supported speed");
    }
}

#[test]
fn a_playback_control_with_unknown_fields_is_refused() {
    let raw = r#"{"position_at_utc":"2026-09-25T11:58:00Z","speed_percent":100,"playing":true,"revision":1,"path":"/srv/rec.mkv"}"#;
    assert!(serde_json::from_str::<PlaybackControl>(raw).is_err());
}
