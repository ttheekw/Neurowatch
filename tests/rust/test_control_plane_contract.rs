use nw_core::control_plane::{
    CameraAssignment, CameraDesiredState, Lease, NodeCapabilities, NodeHeartbeat, NodeIdentity,
    NodeRegistrationRequest, NodeRegistrationResponse, CONTROL_PROTOCOL_VERSION,
};

fn valid_identity() -> NodeIdentity {
    NodeIdentity {
        node_id: "node-a".into(),
        site_id: "site-a".into(),
        display_name: "Primary recorder".into(),
    }
}

fn valid_capabilities() -> NodeCapabilities {
    NodeCapabilities {
        max_camera_assignments: 32,
        available_storage_bytes: 1_000_000,
        available_ingress_bps: 50_000_000,
        hardware_decode_slots: Some(8),
    }
}

fn valid_lease() -> Lease {
    Lease {
        token: "opaque-lease-token".into(),
        issued_at_utc: "2026-09-15T12:00:00Z".into(),
        expires_at_utc: "2026-09-15T12:01:00Z".into(),
        duration_seconds: 60,
    }
}

#[test]
fn control_contracts_serialize_versioned_non_sensitive_metadata() {
    let assignment = CameraAssignment {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        assignment_id: "assignment-a".into(),
        camera_id: "camera-a".into(),
        node_id: "node-a".into(),
        lease: valid_lease(),
        fencing_generation: 1,
        assigned_at_utc: "2026-09-15T12:00:00Z".into(),
    };
    let heartbeat = NodeHeartbeat {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        node: valid_identity(),
        observed_at_utc: "2026-09-15T12:00:01Z".into(),
        active_assignments: 1,
        capabilities: valid_capabilities(),
    };

    assignment.validate().expect("assignment is valid");
    heartbeat.validate().expect("heartbeat is valid");

    let serialized = serde_json::to_string(&(assignment, heartbeat)).expect("contracts serialize");
    assert!(!serialized.contains("rtsp"));
    assert!(!serialized.contains("password"));
}

#[test]
fn control_contract_validation_rejects_invalid_ownership_state() {
    let mut identity = valid_identity();
    identity.node_id.clear();
    assert!(
        identity.validate().is_err(),
        "node identity requires a node id"
    );

    let mut lease = valid_lease();
    lease.duration_seconds = 0;
    assert!(lease.validate().is_err(), "lease duration must be nonzero");

    let mut assignment = CameraAssignment {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        assignment_id: "assignment-a".into(),
        camera_id: "camera-a".into(),
        node_id: "node-a".into(),
        lease: valid_lease(),
        fencing_generation: 1,
        assigned_at_utc: "2026-09-15T12:00:00Z".into(),
    };
    assignment.protocol_version = CONTROL_PROTOCOL_VERSION + 1;
    assert!(
        assignment.validate().is_err(),
        "unknown protocol is rejected"
    );

    assignment.protocol_version = CONTROL_PROTOCOL_VERSION;
    assignment.fencing_generation = 0;
    assert!(
        assignment.validate().is_err(),
        "assignment requires a fencing generation"
    );
}

#[test]
fn lease_validation_requires_consistent_rfc3339_utc_times() {
    let mut lease = valid_lease();
    lease.issued_at_utc = "2026-99-99TbadZ".into();
    assert!(lease.validate().is_err(), "malformed UTC time is rejected");

    let mut lease = valid_lease();
    lease.expires_at_utc = "2026-09-15T11:59:00Z".into();
    assert!(
        lease.validate().is_err(),
        "expiry before issue time is rejected"
    );

    let mut lease = valid_lease();
    lease.expires_at_utc = "2026-09-15T12:00:30Z".into();
    assert!(
        lease.validate().is_err(),
        "lease timestamps must agree with the declared duration"
    );
}

#[test]
fn registration_and_desired_state_contracts_serialize_transport_neutral_metadata() {
    let registration = NodeRegistrationRequest {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        node: valid_identity(),
        capabilities: valid_capabilities(),
    };
    let accepted = NodeRegistrationResponse {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        accepted: true,
        error_message: None,
    };
    let rejected = NodeRegistrationResponse {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        accepted: false,
        error_message: Some("node capacity is unavailable".into()),
    };
    let desired_state = CameraDesiredState {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        camera_id: "camera-a".into(),
        desired_node_id: "node-a".into(),
        recording_enabled: true,
    };

    registration.validate().expect("registration is valid");
    accepted.validate().expect("acceptance is valid");
    rejected.validate().expect("rejection is valid");
    desired_state.validate().expect("desired state is valid");

    let serialized = serde_json::to_string(&(registration, accepted, rejected, desired_state))
        .expect("contracts serialize");
    assert!(!serialized.contains("rtsp"));
    assert!(!serialized.contains("url"));
    assert!(!serialized.contains("credential"));
    assert!(!serialized.contains("password"));
    assert!(!serialized.contains("frame"));
    assert!(!serialized.contains("media"));
}

#[test]
fn registration_response_and_desired_state_validation_reject_invalid_values() {
    let invalid_protocol = NodeRegistrationRequest {
        protocol_version: CONTROL_PROTOCOL_VERSION + 1,
        node: valid_identity(),
        capabilities: valid_capabilities(),
    };
    assert!(invalid_protocol.validate().is_err());

    let accepted_with_error = NodeRegistrationResponse {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        accepted: true,
        error_message: Some("unexpected".into()),
    };
    assert!(accepted_with_error.validate().is_err());

    let rejected_without_error = NodeRegistrationResponse {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        accepted: false,
        error_message: None,
    };
    assert!(rejected_without_error.validate().is_err());

    let invalid_desired_state = CameraDesiredState {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        camera_id: "camera-a".into(),
        desired_node_id: " ".into(),
        recording_enabled: false,
    };
    assert!(invalid_desired_state.validate().is_err());
}

#[test]
fn lease_validation_accepts_zero_offset_and_rejects_nonzero_offsets() {
    let mut zero_offset_lease = valid_lease();
    zero_offset_lease.issued_at_utc = "2026-09-15T12:00:00+00:00".into();
    zero_offset_lease.expires_at_utc = "2026-09-15T12:01:00+00:00".into();
    zero_offset_lease
        .validate()
        .expect("an explicit zero UTC offset is valid");

    let mut positive_offset_lease = valid_lease();
    positive_offset_lease.issued_at_utc = "2026-09-15T13:00:00+01:00".into();
    positive_offset_lease.expires_at_utc = "2026-09-15T13:01:00+01:00".into();
    assert!(
        positive_offset_lease.validate().is_err(),
        "a positive RFC3339 offset is not UTC"
    );

    let mut negative_offset_lease = valid_lease();
    negative_offset_lease.issued_at_utc = "2026-09-15T11:00:00-01:00".into();
    negative_offset_lease.expires_at_utc = "2026-09-15T11:01:00-01:00".into();
    assert!(
        negative_offset_lease.validate().is_err(),
        "a negative RFC3339 offset is not UTC"
    );
}

#[test]
fn public_contract_deserialization_rejects_unknown_credential_fields() {
    let error = serde_json::from_str::<NodeRegistrationRequest>(
        r#"{
            "protocol_version": 1,
            "node": {
                "node_id": "node-a",
                "site_id": "site-a",
                "display_name": "Primary recorder",
                "password": "do-not-accept"
            },
            "capabilities": {
                "max_camera_assignments": 32,
                "available_storage_bytes": 1000000,
                "available_ingress_bps": 50000000,
                "hardware_decode_slots": 8
            }
        }"#,
    )
    .expect_err("wire contracts must reject unknown credential fields");

    assert!(error.to_string().contains("password"));
}

#[test]
fn heartbeat_validation_rejects_excess_assignments_and_zero_capacity() {
    let mut heartbeat = NodeHeartbeat {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        node: valid_identity(),
        observed_at_utc: "2026-09-15T12:00:01Z".into(),
        active_assignments: 1,
        capabilities: valid_capabilities(),
    };
    heartbeat.capabilities.max_camera_assignments = 0;
    assert_eq!(
        heartbeat.validate().unwrap_err().to_string(),
        "invalid control-plane contract: max_camera_assignments must be nonzero"
    );

    heartbeat.capabilities.max_camera_assignments = 1;
    heartbeat.active_assignments = 2;
    assert_eq!(
        heartbeat.validate().unwrap_err().to_string(),
        "invalid control-plane contract: active_assignments exceeds max_camera_assignments"
    );
}

#[test]
fn registration_response_validation_has_consistent_rejection_messages() {
    let accepted_with_error = NodeRegistrationResponse {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        accepted: true,
        error_message: Some("unexpected".into()),
    };
    assert_eq!(
        accepted_with_error.validate().unwrap_err().to_string(),
        "invalid control-plane contract: accepted registration must not include an error_message"
    );

    let rejected_without_error = NodeRegistrationResponse {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        accepted: false,
        error_message: None,
    };
    assert_eq!(
        rejected_without_error.validate().unwrap_err().to_string(),
        "invalid control-plane contract: rejected registration requires an error_message"
    );

    let rejected_with_blank_error = NodeRegistrationResponse {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        accepted: false,
        error_message: Some(" \t".into()),
    };
    assert_eq!(
        rejected_with_blank_error
            .validate()
            .unwrap_err()
            .to_string(),
        "invalid control-plane contract: error_message must not be empty"
    );
}

#[test]
fn camera_assignment_validation_rejects_invalid_assigned_at_timestamp() {
    let assignment = CameraAssignment {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        assignment_id: "assignment-a".into(),
        camera_id: "camera-a".into(),
        node_id: "node-a".into(),
        lease: valid_lease(),
        fencing_generation: 1,
        assigned_at_utc: "not-a-timestamp".into(),
    };

    assert_eq!(
        assignment.validate().unwrap_err().to_string(),
        "invalid control-plane contract: assigned_at_utc must be a valid RFC3339 timestamp"
    );
}
