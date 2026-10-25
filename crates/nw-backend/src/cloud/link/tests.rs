use super::*;
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc as StdArc, Mutex as StdMutex};
use std::thread;

const NODE_AUTH_FIXTURE: &str = include_str!("fixtures/node-auth-v1.json");
const ENROLLMENT_FIXTURE: &str = include_str!("fixtures/enrollment-v1.json");

fn temp_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nw-cloud-{name}-{}", uuid::Uuid::new_v4()))
}

/// A fresh install: neither the config directory nor the cloud directory
/// exists yet (nothing has saved a config).
#[cfg(unix)]
#[test]
fn opens_on_a_fresh_install_with_no_config_directory() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_dir("fresh-install");
    let dir = root.join("config").join("neurowatch").join("cloud-node");
    let node = CloudNode::open(dir.clone());
    assert!(node.is_ok(), "{:?}", node.err().map(|e| e.to_string()));
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "the cloud directory stays private");
    assert!(dir.join("node.key").is_file());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn consumed_directive_acknowledgements_are_durable_and_deduplicated() {
    let dir = temp_dir("directive-acks");
    let node = CloudNode::open(dir.clone()).unwrap();
    let batch = nw_cloud_protocol::media_session::PublishDirectiveBatch {
        protocol_version: nw_cloud_protocol::media_session::PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
        directives: vec![nw_cloud_protocol::media_session::PublishDirective {
            protocol_version: nw_cloud_protocol::media_session::PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
            directive_id: "directive-generation-1".into(),
            session_id: "session-1".into(),
            grant: "nwg1.payload.signature".into(),
            sfu: nw_cloud_protocol::media_session::SfuEndpoint {
                region_id: "region-1".into(),
                signaling_url: "wss://media.example.test".into(),
                ice_servers: vec![],
            },
            publisher_token: "header.payload.signature".into(),
            playback: None,
        }],
        active_session_ids: vec!["session-1".into()],
    };
    node.acknowledge_consumed_directives(&batch).unwrap();
    node.acknowledge_consumed_directives(&batch).unwrap();
    drop(node);

    let reopened = CloudNode::open(dir.clone()).unwrap();
    assert_eq!(
        reopened
            .lock()
            .unwrap()
            .persisted
            .pending_directive_acknowledgements,
        vec!["directive-generation-1"]
    );
    fs::remove_dir_all(dir).unwrap();
}

fn enrollment() -> EnrollmentInput {
    EnrollmentInput {
        endpoint: "https://cloud.example.test".into(),
        enrollment_id: "6ba7b810-9dad-11d1-80b4-00c04fd430c8".into(),
        tenant_id: "6ba7b811-9dad-11d1-80b4-00c04fd430c8".into(),
        expires_at: "2099-01-01T00:00:00+00:00".into(),
        challenge: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        transfer_proof: "one-time-proof".into(),
        display_label: "Front office node".into(),
    }
}

#[test]
fn canonical_enrollment_fixture_matches_webapp_pydantic_contract() {
    let fixture: Value = serde_json::from_str(ENROLLMENT_FIXTURE).unwrap();
    assert_eq!(
        canonical_json(&fixture).unwrap(),
        "{\"audience\":\"neurowatch-cloud-enrollment\",\"challenge\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",\"enrollment_id\":\"6ba7b810-9dad-11d1-80b4-00c04fd430c8\",\"expires_at\":\"2026-09-20T12:05:00Z\",\"extension\":{},\"node_id\":\"6ba7b812-9dad-11d1-80b4-00c04fd430c8\",\"protocol_version\":1,\"tenant_id\":\"6ba7b811-9dad-11d1-80b4-00c04fd430c8\"}"
    );
}

#[test]
fn real_node_auth_fixture_canonical_payloads_and_signatures_match() {
    let fixture: Value = serde_json::from_str(NODE_AUTH_FIXTURE).unwrap();
    for (payload_name, canonical_name) in [
        ("signed_challenge_request", "canonical_challenge_request"),
        ("signed_check_in", "canonical_check_in"),
        (
            "signed_unlink_challenge_request",
            "canonical_unlink_challenge_request",
        ),
        ("signed_unlink", "canonical_unlink"),
    ] {
        let signed = &fixture[payload_name];
        let payload = without_signature(signed);
        assert_eq!(canonical_json(&payload).unwrap(), fixture[canonical_name]);
        assert!(verify_signed(
            fixture["public_key"].as_str().unwrap(),
            signed["signature"].as_str().unwrap(),
            &payload,
        ));
    }

    let response = parse_challenge_response(
        fixture["challenge_response"].clone(),
        OPERATION,
        OffsetDateTime::parse(
            "2026-09-20T12:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(response.challenge, "challenge-vector-0000000000000001");

    let mut extra = fixture["challenge_response"].clone();
    extra["node_id"] = json!("6ba7b812-9dad-11d1-80b4-00c04fd430c8");
    assert!(parse_challenge_response(extra, OPERATION, OffsetDateTime::UNIX_EPOCH).is_err());
    assert!(parse_challenge_response(
        fixture["challenge_response"].clone(),
        OPERATION,
        OffsetDateTime::parse(
            "2026-09-20T12:01:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap(),
    )
    .is_err());
}

#[test]
fn local_unlink_is_immediate_durable_and_keeps_non_secret_retry_tombstone() {
    let dir = temp_dir("unlink-tombstone");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(
        "http://127.0.0.1:9/",
        "6ba7b811-9dad-11d1-80b4-00c04fd430c8",
        "6ba7b812-9dad-11d1-80b4-00c04fd430c8",
        7,
    )
    .unwrap();
    node.unlink_local().unwrap();
    assert_eq!(node.status().phase, LinkPhase::Unlinked);
    assert!(node.has_pending_unlink());
    let raw = fs::read_to_string(dir.join("cloud-state.json")).unwrap();
    assert!(!raw.contains("signature"));
    assert!(!raw.contains("challenge"));
    drop(node);
    let restarted = CloudNode::open(dir.clone()).unwrap();
    assert_eq!(restarted.status().phase, LinkPhase::Unlinked);
    assert!(restarted.has_pending_unlink());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn utc_timestamp_is_cloud_canonical_before_signing_and_persistence() {
    let dir = temp_dir("plus-zero");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    let claim = node.prepare_claim(enrollment()).unwrap();

    assert_eq!(claim.expires_at, "2099-01-01T00:00:00Z");
    assert_eq!(claim.signed_payload()["expires_at"], "2099-01-01T00:00:00Z");
    assert!(verify_signed(
        &claim.public_key,
        &claim.signature,
        &claim.signed_payload()
    ));
    let state = fs::read_to_string(dir.join("cloud-state.json")).unwrap();
    assert!(state.contains("2099-01-01T00:00:00Z"));
    assert_eq!(
        CloudNode::open(dir.clone()).unwrap().status().phase,
        LinkPhase::AwaitingClaim
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn enrollment_rejects_non_utc_and_expired_timestamps_without_state_change() {
    let dir = temp_dir("invalid-time");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    let mut input = enrollment();
    input.expires_at = "2099-01-01T01:00:00+01:00".into();
    assert!(node.prepare_claim(input).is_err());
    let mut input = enrollment();
    input.expires_at = "2000-01-01T00:00:00+00:00".into();
    assert!(node.prepare_claim(input).is_err());
    assert_eq!(node.status().phase, LinkPhase::Unlinked);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_node_key_survives_a_reopen_and_a_damaged_key_is_refused() {
    let dir = temp_dir("key-store");
    let public_key = |node: CloudNode| node.inner.lock().unwrap().key.verifying_key().to_bytes();
    let first = public_key(CloudNode::open(dir.clone()).unwrap());
    let again = public_key(CloudNode::open(dir.clone()).unwrap());
    assert_eq!(first, again, "the same identity after a restart");
    let key = dir.join("node.key");
    let stored = fs::read(&key).unwrap();
    // Unix: the seed in a private file; Windows: DPAPI-encrypted, never the bare seed.
    assert_eq!(stored.len() == 32, cfg!(not(windows)));
    let mut damaged = stored.clone();
    damaged.truncate(stored.len() - 1);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&key)
        .unwrap();
    file.write_all(&damaged).unwrap();
    drop(file);
    assert!(
        CloudNode::open(dir.clone()).is_err(),
        "never silently replaced"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn cloud_store_rejects_symlinks_and_unsafe_permissions() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let root = temp_dir("store-safety");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(CloudNode::open(root.clone()).is_err());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let target = root.join("target-key");
    fs::write(&target, [0_u8; 32]).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, root.join("node.key")).unwrap();
    assert!(CloudNode::open(root.clone()).is_err());
    fs::remove_dir_all(root).unwrap();

    let root = temp_dir("state-symlink");
    let node = CloudNode::open(root.clone()).unwrap();
    drop(node);
    let state_target = root.join("state-target");
    fs::write(&state_target, b"{}").unwrap();
    fs::set_permissions(&state_target, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&state_target, root.join("cloud-state.json")).unwrap();
    assert!(CloudNode::open(root.clone()).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn failed_persistence_does_not_change_memory_or_restart_state() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("rollback");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(node.prepare_claim(enrollment()).is_err());
    assert_eq!(node.status().phase, LinkPhase::Unlinked);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        CloudNode::open(dir.clone()).unwrap().status().phase,
        LinkPhase::Unlinked
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resolver_injection_rejects_every_non_global_or_mapped_address() {
    use std::net::IpAddr;

    for raw in [
        "127.0.0.1",
        "10.0.0.1",
        "169.254.1.1",
        "0.0.0.0",
        "224.0.0.1",
        "192.0.2.1",
        "100.64.0.1",
        "198.18.0.1",
        "::1",
        "::",
        "ff02::1",
        "fe80::1",
        "fc00::1",
        "2001:db8::1",
        "2001::1",
        "3fff::1",
        "::ffff:127.0.0.1",
    ] {
        let ip: IpAddr = raw.parse().unwrap();
        assert!(validate_resolved_ips(&[ip]).is_err(), "accepted {raw}");
    }
    assert!(validate_resolved_ips(&["93.184.216.34".parse().unwrap()]).is_ok());
    assert!(
        validate_resolved_ips(&["2606:2800:220:1:248:1893:25c8:1946".parse().unwrap()]).is_ok()
    );
    assert!(validate_resolved_ips(&[
        "93.184.216.34".parse().unwrap(),
        "127.0.0.1".parse().unwrap(),
    ])
    .is_err());
}

fn serve_json_sequence(responses: Vec<Value>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let task = thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let body = serde_json::to_vec(&response).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ).unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    (format!("http://{address}/"), task)
}

fn serve_scripted(
    responses: Vec<(u16, Option<Value>)>,
) -> (String, StdArc<StdMutex<Vec<Value>>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let captured = StdArc::new(StdMutex::new(Vec::new()));
    let output = StdArc::clone(&captured);
    let task = thread::spawn(move || {
        for (status, response) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end = loop {
                let read = stream.read(&mut buffer).unwrap();
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .parse::<usize>()
                .unwrap();
            while bytes.len() - header_end < content_length {
                let read = stream.read(&mut buffer).unwrap();
                bytes.extend_from_slice(&buffer[..read]);
            }
            output.lock().unwrap().push(
                serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap(),
            );
            let body = response
                .map(|value| serde_json::to_vec(&value).unwrap())
                .unwrap_or_default();
            let reason = if status == 204 {
                "No Content"
            } else if status == 500 {
                "Internal Server Error"
            } else {
                "OK"
            };
            write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    (format!("http://{address}/"), captured, task)
}

#[tokio::test]
async fn unlink_retry_after_uncertain_failure_uses_fresh_proof_and_clears_once() {
    let challenge = |value: &str| {
        json!({
            "protocol_version": 1,
            "audience": NODE_AUDIENCE,
            "operation": UNLINK_OPERATION,
            "challenge": value,
            "expires_at": "2099-01-01T00:01:00Z"
        })
    };
    let (endpoint, captured, server) = serve_scripted(vec![
        (201, Some(challenge("unlink-challenge-first-00000001"))),
        (500, Some(json!({"error":"uncertain"}))),
        (201, Some(challenge("unlink-challenge-second-0000001"))),
        (204, None),
    ]);
    let dir = temp_dir("unlink-retry");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(
        &endpoint,
        "6ba7b811-9dad-11d1-80b4-00c04fd430c8",
        "6ba7b812-9dad-11d1-80b4-00c04fd430c8",
        7,
    )
    .unwrap();
    node.unlink_local().unwrap();
    assert!(node.retry_pending_unlink().await.is_err());
    drop(node);
    let restarted = CloudNode::open(dir.clone()).unwrap();
    assert!(restarted.has_pending_unlink());
    restarted.retry_pending_unlink().await.unwrap();
    assert!(!restarted.has_pending_unlink());
    assert!(matches!(
        restarted.retry_pending_unlink().await,
        Err(CloudError::State)
    ));
    server.join().unwrap();
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0]["operation"], UNLINK_OPERATION);
    assert_ne!(requests[0]["nonce"], requests[2]["nonce"]);
    assert_eq!(requests[1]["challenge"], "unlink-challenge-first-00000001");
    assert_eq!(requests[3]["challenge"], "unlink-challenge-second-0000001");
    assert_eq!(restarted.status().phase, LinkPhase::Unlinked);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn restart_preserves_epoch_and_stale_epoch_response_cannot_advance_state() {
    let dir = temp_dir("stale-epoch");
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let tenant_id = "6ba7b811-9dad-11d1-80b4-00c04fd430c8";
    let (endpoint, server) = serve_json_sequence(vec![
        serde_json::json!({
            "protocol_version": 1,
            "audience": NODE_AUDIENCE,
            "operation": OPERATION,
            "expires_at": "2099-01-01T00:01:00+00:00",
            "challenge": "server-one-time-challenge"
        }),
        serde_json::json!({
            "node_id": node_id,
            "auth_epoch": 8,
            "checked_in_at": "2099-01-01T00:00:01+00:00"
        }),
    ]);
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, tenant_id, node_id, 7)
        .unwrap();
    drop(node);
    let restarted = CloudNode::open(dir.clone()).unwrap();
    assert_eq!(restarted.status().auth_epoch, Some(7));
    assert!(restarted.check_in_once().await.is_err());
    server.join().unwrap();
    assert_eq!(restarted.status().auth_epoch, Some(7));
    assert_eq!(restarted.status().last_check_in, None);
    drop(restarted);
    let after_second_restart = CloudNode::open(dir.clone()).unwrap();
    assert_eq!(after_second_restart.status().auth_epoch, Some(7));
    assert_eq!(after_second_restart.status().last_check_in, None);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn revoked_node_response_is_not_persisted_or_replayed_after_restart() {
    let dir = temp_dir("revoked");
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let tenant_id = "6ba7b811-9dad-11d1-80b4-00c04fd430c8";
    let (endpoint, _, server) = serve_scripted(vec![
        (
            201,
            Some(serde_json::json!({
                "protocol_version": 1, "audience": NODE_AUDIENCE, "operation": OPERATION,
                "expires_at": "2099-01-01T00:01:00+00:00",
                "challenge": "server-one-time-challenge"
            })),
        ),
        // The real cloud contract is nondisclosing: revoked/stale credentials
        // fail authentication instead of returning a fictional status body.
        (
            401,
            Some(serde_json::json!({"detail":"node authentication failed"})),
        ),
    ]);
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, tenant_id, node_id, 7)
        .unwrap();
    assert!(node.check_in_once().await.is_err());
    server.join().unwrap();
    assert_eq!(node.status().last_check_in, None);
    drop(node);
    assert_eq!(
        CloudNode::open(dir.clone()).unwrap().status().last_check_in,
        None
    );
    fs::remove_dir_all(dir).unwrap();
}

fn dev_options() -> CloudOptions {
    CloudOptions {
        policy: EndpointPolicy {
            allow_loopback_http: true,
            pinned: None,
        },
        ..CloudOptions::default()
    }
}

fn loopback_enrollment(endpoint: &str) -> EnrollmentInput {
    EnrollmentInput {
        endpoint: endpoint.into(),
        ..enrollment()
    }
}

#[test]
fn loopback_http_endpoint_is_rejected_without_the_dev_policy() {
    let dir = temp_dir("loopback-default");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    assert!(node
        .prepare_claim(loopback_enrollment("http://127.0.0.1:8000"))
        .is_err());
    assert_eq!(node.status().phase, LinkPhase::Unlinked);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn dev_policy_accepts_only_credential_free_http_on_127_0_0_1() {
    for rejected in [
        "http://localhost:8000",
        "http://192.168.1.5:8000",
        "http://10.0.0.1",
        "http://user:pw@127.0.0.1:8000",
        "http://127.0.0.1:8000/?x=1",
        "http://127.0.0.1:8000/#frag",
        "ftp://127.0.0.1:8000",
    ] {
        let dir = temp_dir("loopback-rejected");
        let mut node = CloudNode::open_with_options(dir.clone(), dev_options()).unwrap();
        assert!(
            node.prepare_claim(loopback_enrollment(rejected)).is_err(),
            "{rejected} must stay rejected"
        );
        assert_eq!(node.status().phase, LinkPhase::Unlinked);
        fs::remove_dir_all(dir).unwrap();
    }

    let dir = temp_dir("loopback-accepted");
    let mut node = CloudNode::open_with_options(dir.clone(), dev_options()).unwrap();
    node.prepare_claim(loopback_enrollment("http://127.0.0.1:8000"))
        .unwrap();
    assert_eq!(node.status().phase, LinkPhase::AwaitingClaim);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn dev_policy_reads_only_an_explicit_opt_in_value() {
    assert!(!EndpointPolicy::from_env_value(None).allow_loopback_http);
    assert!(!EndpointPolicy::from_env_value(Some("0")).allow_loopback_http);
    assert!(!EndpointPolicy::from_env_value(Some("yes")).allow_loopback_http);
    assert!(EndpointPolicy::from_env_value(Some("1")).allow_loopback_http);
}

#[tokio::test]
async fn dev_policy_claims_against_a_loopback_cloud() {
    let claim_id = uuid::Uuid::new_v4().to_string();
    let (base, server) = serve_json_sequence(vec![json!({
        "enrollment_id": enrollment().enrollment_id,
        "status": "claimed",
        "claim_id": claim_id,
    })]);
    let dir = temp_dir("loopback-claim");
    let mut node = CloudNode::open_with_options(dir.clone(), dev_options()).unwrap();
    let status = node
        .begin_remote(loopback_enrollment(base.trim_end_matches('/')))
        .await
        .unwrap();
    assert_eq!(status.phase, LinkPhase::AwaitingLocalConfirmation);
    server.join().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn signed_node_auth_times_are_whole_seconds_so_the_cloud_reserializes_them_identically() {
    // The cloud parses these into microsecond datetimes and re-serializes
    // them before verifying the signature; sub-second digits would differ.
    let t = OffsetDateTime::parse(
        "2026-09-24T14:12:01.123456789Z",
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert_eq!(fmt_time(t).unwrap(), "2026-09-24T14:12:01Z");
}

const TENANT: &str = "6ba7b811-9dad-11d1-80b4-00c04fd430c8";
const ENROLLMENT: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
const NODE: &str = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";

fn code() -> String {
    format!("{TENANT}.{ENROLLMENT}.KFPtAkVSlNOLCOT1Rdp634jsz4foAMaBcKzXXZBPZi0")
}

#[test]
fn enrollment_code_is_tenant_enrollment_and_proof() {
    let parsed = EnrollmentCode::parse(&format!("  {}\n", code())).unwrap();
    assert_eq!(parsed.tenant_id, TENANT);
    assert_eq!(parsed.enrollment_id, ENROLLMENT);
    assert_eq!(
        parsed.transfer_proof,
        "KFPtAkVSlNOLCOT1Rdp634jsz4foAMaBcKzXXZBPZi0"
    );
    for rejected in [
        String::new(),
        format!("{TENANT}.{ENROLLMENT}"),
        format!("{}.extra", code()),
        format!("{}.{ENROLLMENT}.proof-value", TENANT.to_uppercase()),
        format!("{TENANT}.not-a-uuid.proof-value"),
        format!("{TENANT}.{ENROLLMENT}."),
        format!("{TENANT}.{ENROLLMENT}.proof value"),
    ] {
        assert!(EnrollmentCode::parse(&rejected).is_err(), "{rejected:?}");
    }
}

#[test]
fn cloud_options_have_no_endpoint_until_one_is_configured() {
    assert_eq!(CloudOptions::from_env_values(None, None).endpoint, None);
    assert_eq!(
        CloudOptions::from_env_values(Some("  "), None).endpoint,
        None
    );
    assert!(
        !CloudOptions::from_env_values(None, None)
            .policy
            .allow_loopback_http
    );
    let custom = CloudOptions::from_env_values(Some("https://cloud.example.com/"), Some("1"));
    assert_eq!(
        custom.endpoint.as_deref(),
        Some("https://cloud.example.com")
    );
    assert!(custom.policy.allow_loopback_http);
}

#[tokio::test]
async fn enrollment_code_fetches_the_claim_binding_from_the_configured_cloud_then_claims() {
    let claim_id = uuid::Uuid::new_v4().to_string();
    let (base, captured, server) = serve_scripted(vec![
        (
            200,
            Some(json!({
                "protocol_version": 1,
                "audience": ENROLLMENT_AUDIENCE,
                "enrollment_id": ENROLLMENT,
                "tenant_id": TENANT,
                "expires_at": "2099-01-01T00:00:00Z",
                "challenge": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            })),
        ),
        (200, Some(json!({"claim_id": claim_id}))),
    ]);
    let dir = temp_dir("code-claim");
    let options = CloudOptions {
        endpoint: Some(base.trim_end_matches('/').to_owned()),
        ..dev_options()
    };
    let mut node = CloudNode::open_with_options(dir.clone(), options).unwrap();

    let status = node
        .begin_with_code(&code(), "Front office".into())
        .await
        .unwrap();

    assert_eq!(status.phase, LinkPhase::AwaitingLocalConfirmation);
    assert_eq!(status.endpoint.as_deref(), Some(base.trim_end_matches('/')));
    server.join().unwrap();
    let requests = captured.lock().unwrap();
    assert_eq!(
        requests[0],
        json!({"tenant_id": TENANT, "transfer_proof": "KFPtAkVSlNOLCOT1Rdp634jsz4foAMaBcKzXXZBPZi0"})
    );
    assert_eq!(requests[1]["enrollment_id"], ENROLLMENT);
    assert_eq!(requests[1]["display_label"], "Front office");
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_claim_binding_for_another_enrollment_is_rejected_without_state_change() {
    let (base, _captured, server) = serve_scripted(vec![(
        200,
        Some(json!({
            "protocol_version": 1,
            "audience": ENROLLMENT_AUDIENCE,
            "enrollment_id": uuid::Uuid::new_v4().to_string(),
            "tenant_id": TENANT,
            "expires_at": "2099-01-01T00:00:00Z",
            "challenge": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        })),
    )]);
    let dir = temp_dir("code-mismatch");
    let options = CloudOptions {
        endpoint: Some(base.trim_end_matches('/').to_owned()),
        ..dev_options()
    };
    let mut node = CloudNode::open_with_options(dir.clone(), options).unwrap();

    assert!(node
        .begin_with_code(&code(), "Front office".into())
        .await
        .is_err());
    assert_eq!(node.status().phase, LinkPhase::Unlinked);
    server.join().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn refusing_a_local_http_cloud_url_names_the_development_opt_in() {
    let error = CloudEndpoint::parse_with("http://127.0.0.1:8000", EndpointPolicy::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains(EndpointPolicy::DEV_ENV), "{error}");
    let other = CloudEndpoint::parse_with("http://example.com", EndpointPolicy::default())
        .unwrap_err()
        .to_string();
    assert!(!other.contains(EndpointPolicy::DEV_ENV), "{other}");
}

fn camera(id: &str, name: &str, recording: bool) -> nw_core::CameraConfig {
    let mut camera =
        nw_core::CameraConfig::new(name, "rtsp://admin:hunter2@192.168.1.64:554/stream1");
    camera.id = id.into();
    camera.username = Some("admin".into());
    camera.password_b64 = Some("aHVudGVyMg==".into());
    camera.recording.local.enabled = recording;
    camera
}

#[test]
fn camera_inventory_reports_which_streams_a_browser_can_be_sent() {
    let cameras = vec![
        camera("cam-a", "Front door", true),
        camera("cam-b", "Gate", false),
    ];
    let snapshot =
        camera_inventory_with_live(&cameras, &std::collections::HashMap::new(), |camera| {
            (camera.id == "cam-a")
                .then(|| std::collections::BTreeMap::from([("main", Some("h265")), ("sub", None)]))
        });
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(json[0]["live"], json!({"main": "h265", "sub": null}));
    // Not known: left out, so a cloud keeps its earlier assumption and older
    // signed payloads are unchanged.
    assert!(json[1].get("live").is_none());
}

#[test]
fn camera_inventory_is_allowlisted_and_never_carries_connection_details() {
    let cameras = vec![
        camera("cam-a", "Front door", true),
        camera("cam-b", "rtsp://admin@192.168.1.64/live", false),
        camera("cam-c", "192.168.1.20 gate", false),
        camera("cam/../d", "Yard", false),
        camera("cam-e", &"x".repeat(80), false),
    ];
    let states = std::collections::HashMap::from([
        ("cam-a".to_string(), "recording_ready".to_string()),
        ("cam-b".to_string(), "teardown_failed".to_string()),
        ("cam-c".to_string(), "reconnecting".to_string()),
    ]);

    let snapshot = camera_inventory(&cameras, &states);
    let json = serde_json::to_value(&snapshot).unwrap();

    assert_eq!(
        json[0],
        json!({"camera_id": "cam-a", "name": "Front door", "status": "streaming", "recording": true})
    );
    assert_eq!(json[1]["name"], "Camera 2");
    assert_eq!(json[1]["status"], "error");
    assert_eq!(json[2]["name"], "Camera 3");
    assert_eq!(json[2]["status"], "reconnecting");
    assert_eq!(json[3]["camera_id"], "camera-4");
    assert_eq!(json[3]["status"], "stopped");
    assert_eq!(json[4]["name"].as_str().unwrap().chars().count(), 64);
    let text = json.to_string();
    for secret in ["rtsp", "hunter2", "aHVudGVyMg", "192.168", "admin", "554"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    let many: Vec<_> = (0..300)
        .map(|i| camera(&format!("c{i}"), "Cam", false))
        .collect();
    assert_eq!(
        camera_inventory(&many, &std::collections::HashMap::new()).len(),
        256
    );
}

#[test]
fn diagnostics_name_cameras_by_the_ids_the_inventory_reports() {
    let cameras = vec![
        camera("cam-a", "Front door", true),
        camera("cam/../d", "Yard", false),
    ];
    let reported = reported_camera_ids(&cameras);
    let inventory = camera_inventory(&cameras, &std::collections::HashMap::new());
    for (camera, snapshot) in cameras.iter().zip(&inventory) {
        assert_eq!(reported[&camera.id], snapshot.camera_id);
    }
    assert_eq!(reported["cam/../d"], "camera-2");
}

fn diagnostics_challenge() -> Value {
    json!({
        "protocol_version": 1,
        "audience": NODE_AUDIENCE,
        "operation": DIAGNOSTICS_REPORT_OPERATION,
        "expires_at": "2099-01-01T00:01:00+00:00",
        "challenge": "server-one-time-challenge"
    })
}

#[tokio::test]
async fn a_diagnostics_report_is_signed_over_the_exact_snapshot_text() {
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let (endpoint, captured, server) = serve_scripted(vec![
        (201, Some(diagnostics_challenge())),
        (200, Some(json!({"wanted": true}))),
        (201, Some(diagnostics_challenge())),
        (200, Some(json!({"wanted": false}))),
    ]);
    let dir = temp_dir("diagnostics-push");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, TENANT, node_id, 3)
        .unwrap();
    let snapshot = json!({"process": {"cpu_percent": 12.5}, "drives": []});

    assert!(node
        .push_diagnostics(Some(&snapshot), Duration::ZERO)
        .await
        .unwrap());
    assert!(!node.push_diagnostics(None, Duration::ZERO).await.unwrap());
    server.join().unwrap();

    let requests = captured.lock().unwrap();
    assert_eq!(requests[0]["operation"], DIAGNOSTICS_REPORT_OPERATION);
    let report = &requests[1];
    assert_eq!(report["challenge"], "server-one-time-challenge");
    assert_eq!(report["auth_epoch"], 3);
    assert_eq!(
        serde_json::from_str::<Value>(report["snapshot_json"].as_str().unwrap()).unwrap(),
        snapshot
    );
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().as_bytes());
    assert!(verify_signed(
        &public,
        report["signature"].as_str().unwrap(),
        &without_signature(report)
    ));
    // Nobody watching: the report says so explicitly, with no snapshot.
    assert_eq!(requests[3]["snapshot_json"], Value::Null);
    assert!(requests[3]
        .as_object()
        .unwrap()
        .contains_key("snapshot_json"));
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_oversized_diagnostics_snapshot_is_not_sent() {
    let dir = temp_dir("diagnostics-oversized");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(
        "http://127.0.0.1:9",
        TENANT,
        "6ba7b812-9dad-11d1-80b4-00c04fd430c8",
        3,
    )
    .unwrap();
    let huge = json!({"padding": "x".repeat(MAX_DIAGNOSTICS_BYTES)});
    assert!(matches!(
        node.push_diagnostics(Some(&huge), Duration::ZERO).await,
        Err(CloudError::Invalid(_))
    ));
    fs::remove_dir_all(dir).unwrap();
}

fn inventory_challenge() -> Value {
    json!({
        "protocol_version": 1,
        "audience": NODE_AUDIENCE,
        "operation": INVENTORY_OPERATION,
        "expires_at": "2099-01-01T00:01:00+00:00",
        "challenge": "server-one-time-challenge"
    })
}

#[tokio::test]
async fn inventory_snapshot_is_signed_over_its_own_fresh_challenge() {
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let (endpoint, captured, server) = serve_scripted(vec![
        (201, Some(inventory_challenge())),
        (
            200,
            Some(json!({"node_id": node_id, "accepted": true, "camera_count": 1})),
        ),
    ]);
    let dir = temp_dir("inventory-push");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, TENANT, node_id, 3)
        .unwrap();
    let cameras = camera_inventory(
        &[camera("cam-a", "Café door", true)],
        &std::collections::HashMap::new(),
    );

    assert!(node.push_inventory(cameras).await.unwrap());
    server.join().unwrap();

    let requests = captured.lock().unwrap();
    assert_eq!(requests[0]["operation"], INVENTORY_OPERATION);
    let snapshot = &requests[1];
    assert_eq!(snapshot["challenge"], "server-one-time-challenge");
    assert_eq!(snapshot["audience"], NODE_AUDIENCE);
    assert_eq!(snapshot["auth_epoch"], 3);
    assert_eq!(snapshot["cameras"][0]["name"], "Café door");
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().as_bytes());
    assert!(verify_signed(
        &public,
        snapshot["signature"].as_str().unwrap(),
        &without_signature(snapshot)
    ));
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn scheduler_reports_the_camera_inventory_right_after_start() {
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let check_in_challenge = json!({
        "protocol_version": 1, "audience": NODE_AUDIENCE, "operation": OPERATION,
        "expires_at": "2099-01-01T00:01:00+00:00", "challenge": "check-in-challenge"
    });
    let (endpoint, captured, server) = serve_scripted(vec![
        (201, Some(check_in_challenge)),
        (
            200,
            Some(
                json!({"node_id": node_id, "auth_epoch": 1, "checked_in_at": "2099-01-01T00:00:01Z"}),
            ),
        ),
        (201, Some(inventory_challenge())),
        (
            200,
            Some(json!({"node_id": node_id, "accepted": true, "camera_count": 1})),
        ),
    ]);
    let dir = temp_dir("inventory-scheduler");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, TENANT, node_id, 1)
        .unwrap();
    let source: InventorySource = Arc::new(|| {
        Box::pin(async {
            camera_inventory(
                &[camera("cam-a", "Front door", false)],
                &std::collections::HashMap::new(),
            )
        })
    });

    let handle = CloudScheduler::new(
        node.shared(),
        Duration::from_secs(3600),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
    .with_inventory(source)
    .spawn();
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    handle.shutdown().await;

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[3]["cameras"][0]["name"], "Front door");
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_saved_endpoint_rejected_by_current_settings_keeps_the_cloud_subsystem_usable() {
    let dir = temp_dir("saved-endpoint-policy");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.prepare_claim(enrollment()).unwrap();
    drop(node);
    // Simulate a link made with the development opt-in, reopened without it.
    let state_path = dir.join("cloud-state.json");
    let state = fs::read_to_string(&state_path)
        .unwrap()
        .replace("https://cloud.example.test", "http://localhost:8000");
    fs::write(&state_path, state).unwrap();

    let node = CloudNode::open(dir.clone()).unwrap();

    assert_eq!(node.status().phase, LinkPhase::AwaitingClaim);
    let error = node.check_in_once().await.unwrap_err().to_string();
    assert!(error.contains("http://localhost:8000"), "{error}");
    fs::remove_dir_all(dir).unwrap();
}

fn soon_expiring_enrollment(seconds: i64) -> EnrollmentInput {
    let expires =
        OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() + TimeDuration::seconds(seconds);
    EnrollmentInput {
        expires_at: expires
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap(),
        ..enrollment()
    }
}

#[test]
fn status_never_erases_a_pending_link_that_outlived_its_enrollment() {
    let dir = temp_dir("expired-pending-status");
    let state_path = dir.join("cloud-state.json");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.prepare_claim(soon_expiring_enrollment(1)).unwrap();
    let saved = fs::read(&state_path).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let status = node.status();

    assert_eq!(status.phase, LinkPhase::AwaitingClaim);
    assert!(node.lock().unwrap().persisted.pending.is_some());
    assert_eq!(
        fs::read(&state_path).unwrap(),
        saved,
        "cloud.status rewrote persisted cloud state"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_expired_pending_link_stays_visible_after_restart_and_a_new_code_clears_it() {
    let dir = temp_dir("expired-pending-restart");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.prepare_claim(soon_expiring_enrollment(1)).unwrap();
    drop(node);
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let mut node = CloudNode::open(dir.clone()).unwrap();

    assert_eq!(node.status().phase, LinkPhase::AwaitingClaim);
    assert!(node.lock().unwrap().persisted.pending.is_some());
    // Only an explicit new link attempt discards the expired attempt.
    node.prepare_claim(enrollment()).unwrap();
    assert_eq!(node.status().phase, LinkPhase::AwaitingClaim);
    assert_eq!(
        node.lock()
            .unwrap()
            .persisted
            .pending
            .clone()
            .unwrap()
            .expires_at,
        "2099-01-01T00:00:00Z"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn confirming_after_the_enrollment_expired_asks_for_a_new_enrollment_code() {
    let dir = temp_dir("expired-pending-confirm");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.prepare_claim(soon_expiring_enrollment(1)).unwrap();
    node.record_claim_response(ClaimResponse {
        claim_id: uuid::Uuid::new_v4().to_string(),
    })
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let error = node.confirm().unwrap_err().to_string();

    assert!(error.contains("new enrollment code"), "{error}");
    assert_eq!(
        node.status().phase,
        LinkPhase::AwaitingLocalConfirmation,
        "a failed confirmation must not rewrite state"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn linking_without_a_configured_cloud_is_a_distinct_not_configured_state() {
    let dir = temp_dir("cloud-not-configured");
    let mut node = CloudNode::open(dir.clone()).unwrap();

    let error = node
        .begin_with_code(&code(), "Front office".into())
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("not configured"), "{error}");
    assert!(!error.contains("127.0.0.1"), "{error}");
    assert!(!error.contains(EndpointPolicy::DEV_ENV), "{error}");
    assert_eq!(node.status().phase, LinkPhase::Unlinked);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_status_report_names_the_configured_cloud_and_an_expired_enrollment() {
    let dir = temp_dir("status-report");
    let options = CloudOptions {
        endpoint: Some("https://cloud.example.com".into()),
        ..CloudOptions::default()
    };
    let mut node = CloudNode::open_with_options(dir.clone(), options).unwrap();

    let report = node.report().unwrap();

    assert!(report.cloud_configured);
    assert!(report.new_link_configured);
    assert_eq!(
        report.cloud_endpoint.as_deref(),
        Some("https://cloud.example.com")
    );
    assert!(!report.pending_expired);
    assert_eq!(report.error, None);

    node.prepare_claim(soon_expiring_enrollment(1)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let report = node.report().unwrap();

    assert_eq!(report.status.phase, LinkPhase::AwaitingClaim);
    assert!(report.pending_expired);
    assert_eq!(report.error.as_deref(), Some("enrollment_expired"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_fresh_node_with_no_cloud_anywhere_reports_a_safe_unconfigured_state() {
    let dir = temp_dir("status-report-unconfigured");
    let node = CloudNode::open(dir.clone()).unwrap();

    let report = node.report().unwrap();

    assert_eq!(report.status.phase, LinkPhase::Unlinked);
    assert!(!report.cloud_configured);
    assert!(
        !report.new_link_configured,
        "a new link has no cloud to use"
    );
    assert_eq!(report.cloud_endpoint, None);
    assert_eq!(report.error, None);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_linked_node_stays_linked_and_unlinkable_without_a_configured_cloud() {
    // A node linked earlier, then restarted without NW_CLOUD_ENDPOINT: unlink
    // and check-in still use the saved endpoint, so the status must say so
    // instead of claiming the cloud was never set up here.
    let dir = temp_dir("status-report-linked-no-env");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test("http://127.0.0.1:9", TENANT, NODE, 4)
        .unwrap();
    drop(node);

    let mut node = CloudNode::open(dir.clone()).unwrap();
    let report = node.report().unwrap();

    assert_eq!(report.status.phase, LinkPhase::Linked);
    assert!(report.cloud_configured, "a linked node has a cloud");
    assert!(
        !report.new_link_configured,
        "no cloud is configured for a new link"
    );
    assert_eq!(report.cloud_endpoint.as_deref(), Some("http://127.0.0.1:9"));
    node.unlink_local().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_awaiting_confirmation_node_stays_confirmable_without_a_configured_cloud() {
    let dir = temp_dir("status-report-awaiting-no-env");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.prepare_claim(enrollment()).unwrap();
    node.record_claim_response(ClaimResponse {
        claim_id: uuid::Uuid::new_v4().to_string(),
    })
    .unwrap();
    drop(node);

    let mut node = CloudNode::open(dir.clone()).unwrap();
    let report = node.report().unwrap();

    assert_eq!(report.status.phase, LinkPhase::AwaitingLocalConfirmation);
    assert!(report.cloud_configured);
    assert!(!report.new_link_configured);
    assert!(!report.pending_expired);
    assert_eq!(
        report.cloud_endpoint.as_deref(),
        Some("https://cloud.example.test")
    );
    // The confirmation the status offers is really available.
    node.confirm().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_poisoned_cloud_lock_is_an_error_not_a_pretend_unconfigured_cloud() {
    let dir = temp_dir("status-report-poisoned");
    let node = CloudNode::open_with_options(
        dir.clone(),
        CloudOptions {
            endpoint: Some("https://cloud.example.com".into()),
            ..CloudOptions::default()
        },
    )
    .unwrap();
    let poisoner = node.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let _ = thread::spawn(move || {
        let _guard = poisoner.inner.lock().unwrap();
        panic!("poison the cloud state lock");
    })
    .join();
    std::panic::set_hook(previous);

    let error = node.report().unwrap_err();

    assert!(matches!(error, CloudError::Store(_)), "{error}");
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_legacy_link_request_may_only_target_the_configured_cloud() {
    let dir = temp_dir("legacy-begin-mismatch");
    let mut node = CloudNode::open_with_options(
        dir.clone(),
        CloudOptions {
            endpoint: Some("https://cloud.example.com".into()),
            ..CloudOptions::default()
        },
    )
    .unwrap();

    let error = node
        .begin_legacy(EnrollmentInput {
            endpoint: "https://attacker.example.net".into(),
            ..enrollment()
        })
        .await
        .unwrap_err();

    assert!(matches!(error, CloudError::Invalid(_)), "{error}");
    assert_eq!(
        node.status().phase,
        LinkPhase::Unlinked,
        "a rejected legacy request must not half-start a link"
    );
    assert_eq!(node.status().endpoint, None);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_legacy_link_request_on_an_unconfigured_node_is_refused_not_persisted() {
    let dir = temp_dir("legacy-begin-unconfigured");
    let mut node = CloudNode::open(dir.clone()).unwrap();

    let error = node.begin_legacy(enrollment()).await.unwrap_err();

    assert!(matches!(error, CloudError::NotConfigured), "{error}");
    assert_eq!(node.status().phase, LinkPhase::Unlinked);
    assert_eq!(
        node.status().endpoint,
        None,
        "a caller-provided endpoint must never be persisted"
    );
    let report = node.report().unwrap();
    assert!(!report.cloud_configured);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_legacy_link_request_for_the_configured_cloud_still_claims() {
    let claim_id = uuid::Uuid::new_v4().to_string();
    let (base, server) = serve_json_sequence(vec![json!({
        "enrollment_id": enrollment().enrollment_id,
        "status": "claimed",
        "claim_id": claim_id,
    })]);
    let dir = temp_dir("legacy-begin-match");
    let mut node = CloudNode::open_with_options(
        dir.clone(),
        CloudOptions {
            endpoint: Some(base.trim_end_matches('/').to_owned()),
            ..dev_options()
        },
    )
    .unwrap();

    // A trailing slash is the same cloud, not a different one.
    let status = node.begin_legacy(loopback_enrollment(&base)).await.unwrap();

    assert_eq!(status.phase, LinkPhase::AwaitingLocalConfirmation);
    assert_eq!(
        status.endpoint.as_deref(),
        Some(base.trim_end_matches('/')),
        "the configured cloud is what gets persisted"
    );
    server.join().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(start_paused = true)]
async fn scheduler_ticks_do_not_burst_after_a_slow_cycle() {
    let period = Duration::from_secs(60);
    let mut tick = CloudScheduler::ticker(period);

    tick.tick().await; // fires at once so a node reports right after start
    tokio::time::advance(Duration::from_secs(185)).await; // a cycle ran long
    tick.tick().await;

    let resumed = tokio::time::Instant::now();
    tick.tick().await;
    assert!(
        resumed.elapsed() >= period,
        "missed ticks burst after {:?}",
        resumed.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn rest_keeps_an_early_event_until_the_minimum_cycle_and_falls_back_without_one() {
    let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
    let wake = tokio::sync::Notify::new();
    let second = Duration::from_secs(1);

    // An event that came in during the minimum cycle is acted on when it ends.
    wake.notify_one();
    let start = tokio::time::Instant::now();
    assert!(
        rest(
            start + 4 * second,
            start + 15 * second,
            Some(&wake),
            &mut stop
        )
        .await
    );
    assert_eq!(start.elapsed(), 4 * second);

    // Without an event, the fallback deadline.
    let start = tokio::time::Instant::now();
    assert!(rest(start, start + 15 * second, Some(&wake), &mut stop).await);
    assert_eq!(start.elapsed(), 15 * second);

    // An event ends the wait at once.
    let start = tokio::time::Instant::now();
    let later = async {
        tokio::time::sleep(3 * second).await;
        wake.notify_one();
    };
    let (rested, ()) = tokio::join!(
        rest(start, start + 15 * second, Some(&wake), &mut stop),
        later
    );
    assert!(rested);
    assert_eq!(start.elapsed(), 3 * second);

    // Without events, only time counts.
    wake.notify_one();
    let start = tokio::time::Instant::now();
    assert!(rest(start + second, start + 2 * second, None, &mut stop).await);
    assert_eq!(start.elapsed(), 2 * second);

    stop_tx.send(true).unwrap();
    let start = tokio::time::Instant::now();
    assert!(!rest(start, start + 15 * second, Some(&wake), &mut stop).await);
}

#[test]
fn every_step_of_a_cycle_is_bounded_so_the_whole_cycle_fits_in_a_period() {
    for seconds in [1, 30, 60, 3600] {
        let period = Duration::from_secs(seconds);
        let budget = CloudScheduler::request_budget(period);
        assert!(budget > Duration::ZERO, "{seconds}s period");
        // A cycle runs three bounded steps: the check-in, collecting the
        // camera inventory, and reporting it. All three must finish inside a
        // period, or a slow cloud or a busy backend delays every later cycle.
        assert!(
            budget.mul_f64(CloudScheduler::CYCLE_STEPS as f64) < period,
            "{seconds}s period"
        );
    }
}

#[tokio::test]
async fn a_stalled_inventory_source_is_abandoned_within_its_budget() {
    // The source reads shared backend state, so a busy backend must not be
    // able to stretch a scheduler cycle past its period.
    let stalled: InventorySource = Arc::new(|| Box::pin(std::future::pending()));
    let started = tokio::time::Instant::now();

    let cameras = collect_inventory(&stalled, Duration::from_millis(20)).await;

    assert!(
        cameras.is_none(),
        "a stalled source must not block the cycle"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "waited {:?}",
        started.elapsed()
    );
    let ready: InventorySource = Arc::new(|| {
        Box::pin(async {
            camera_inventory(
                &[camera("cam-a", "Front door", false)],
                &std::collections::HashMap::new(),
            )
        })
    });
    assert_eq!(
        collect_inventory(&ready, Duration::from_secs(5))
            .await
            .map(|cameras| cameras.len()),
        Some(1)
    );
}

#[tokio::test]
async fn an_inventory_report_that_times_out_is_reported_for_logging() {
    let elapsed = tokio::time::timeout(
        Duration::from_millis(1),
        std::future::pending::<Result<bool>>(),
    )
    .await
    .unwrap_err();

    assert_eq!(
        inventory_failure_reason(&Err(elapsed)).as_deref(),
        Some("timed out")
    );
    assert_eq!(inventory_failure_reason(&Ok(Ok(true))), None);
    assert_eq!(
        inventory_failure_reason(&Ok(Err(CloudError::Network("HTTP 503".into())))).as_deref(),
        Some("cloud request failed: HTTP 503")
    );
}

#[test]
fn the_test_only_constructor_behaves_like_the_binary_entry_point_with_no_cloud() {
    // Every test that calls `CloudNode::open` is meant to exercise what the
    // binary does through `open_with_options`; if the shorthand diverged, the
    // suite would be proving a policy production never runs.
    let shorthand = temp_dir("open-shorthand");
    let explicit = temp_dir("open-explicit");
    let mut shorthand_node = CloudNode::open(shorthand.clone()).unwrap();
    let mut explicit_node =
        CloudNode::open_with_options(explicit.clone(), CloudOptions::default()).unwrap();

    for node in [&mut shorthand_node, &mut explicit_node] {
        let report = node.report().unwrap();
        assert!(!report.cloud_configured);
        assert!(!report.new_link_configured);
        assert_eq!(report.cloud_endpoint, None);
        // Production policy: a plain-HTTP development cloud stays refused.
        let error = node
            .prepare_claim(loopback_enrollment("http://127.0.0.1:8000"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(EndpointPolicy::DEV_ENV), "{error}");
        assert_eq!(node.status().phase, LinkPhase::Unlinked);
    }
    fs::remove_dir_all(shorthand).unwrap();
    fs::remove_dir_all(explicit).unwrap();
}

#[test]
fn a_tampered_pending_timestamp_is_still_rejected_on_load() {
    let dir = temp_dir("tampered-pending");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.prepare_claim(enrollment()).unwrap();
    drop(node);
    let state_path = dir.join("cloud-state.json");
    let state = fs::read_to_string(&state_path)
        .unwrap()
        .replace("2099-01-01T00:00:00Z", "2099-01-01T00:00:00+00:00");
    fs::write(&state_path, state).unwrap();
    assert!(CloudNode::open(dir.clone()).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn replacement_camera_ids_never_collide_with_real_ones() {
    // The cloud rejects a whole snapshot that repeats a camera id, so a
    // replacement for an unsafe id must not reuse an id another camera has.
    let snapshot = camera_inventory(
        &[
            camera("rtsp://not-an-id", "Lobby", true),
            camera("camera-1", "Yard", false),
            camera("camera-1-2", "Dock", false),
        ],
        &std::collections::HashMap::new(),
    );
    let ids: Vec<&str> = snapshot.iter().map(|c| c.camera_id.as_str()).collect();
    let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len(), "duplicate ids in {ids:?}");
    assert_eq!(ids[1], "camera-1", "a safe id is reported unchanged");
    assert_eq!(ids[2], "camera-1-2");
}

#[tokio::test]
async fn a_stuck_unlink_of_an_earlier_link_does_not_stop_the_current_link_checking_in() {
    // An earlier link was revoked in the portal before the node unlinked it
    // locally, so its unlink tombstone is refused forever. The node has since
    // linked again; that link must still check in and report its cameras.
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let check_in_challenge = json!({
        "protocol_version": 1, "audience": NODE_AUDIENCE, "operation": OPERATION,
        "expires_at": "2099-01-01T00:01:00+00:00", "challenge": "check-in-challenge"
    });
    let (endpoint, captured, server) = serve_scripted(vec![
        (401, Some(json!({"code": "unauthenticated"}))),
        (201, Some(check_in_challenge)),
        (
            200,
            Some(
                json!({"node_id": node_id, "auth_epoch": 1, "checked_in_at": "2099-01-01T00:00:01Z"}),
            ),
        ),
        (201, Some(inventory_challenge())),
        (
            200,
            Some(json!({"node_id": node_id, "accepted": true, "camera_count": 1})),
        ),
    ]);
    let dir = temp_dir("stuck-unlink-scheduler");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, TENANT, node_id, 1)
        .unwrap();
    node.lock().unwrap().persisted.pending_unlink = Some(PendingUnlink {
        endpoint: endpoint.clone(),
        tenant_id: TENANT.into(),
        node_id: "a86871ae-8907-48ed-811a-d043da55f166".into(),
        auth_epoch: 1,
    });
    let source: InventorySource = Arc::new(|| {
        Box::pin(async {
            camera_inventory(
                &[camera("cam-a", "Front door", false)],
                &std::collections::HashMap::new(),
            )
        })
    });

    let handle = CloudScheduler::new(
        node.shared(),
        Duration::from_secs(3600),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
    .with_inventory(source)
    .spawn();
    // Poll rather than join: without the fix the scripted server waits for a
    // check-in that never comes, and joining it would hang the test.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while captured.lock().unwrap().len() < 5 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.shutdown().await;
    let finished = captured.lock().unwrap().len() >= 5;
    if finished {
        server.join().unwrap();
    }

    let requests = captured.lock().unwrap();
    assert!(
        finished,
        "the scheduler stopped after {} request(s)",
        requests.len()
    );
    assert_eq!(
        requests.len(),
        5,
        "unlink retry, then check-in and inventory"
    );
    assert_eq!(requests[0]["operation"], UNLINK_OPERATION);
    assert_eq!(requests[1]["operation"], OPERATION);
    assert_eq!(requests[4]["cameras"][0]["name"], "Front door");
    assert!(
        node.has_pending_unlink(),
        "a refused unlink is kept for retry"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn coverage_is_whole_second_utc_ranges_for_opaque_camera_ids_only() {
    let report = camera_coverage(vec![
        (
            "cam-a".to_string(),
            vec![
                (1_790_000_000_400, 1_790_000_060_900),
                (1_790_000_100_000, 1_790_000_100_500),
            ],
        ),
        (
            "rtsp://cam".to_string(),
            vec![(1_790_000_000_000, 1_790_000_060_000)],
        ),
    ]);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(
        json,
        json!([{"camera_id": "cam-a", "ranges": [
            {"start_at": "2026-09-21T14:13:21Z", "end_at": "2026-09-21T14:14:20Z"}
        ]}]),
        "ranges round inwards; one that rounds to nothing is dropped"
    );
}

#[tokio::test]
async fn coverage_report_is_signed_over_its_own_fresh_challenge() {
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let challenge = json!({
        "protocol_version": 1, "audience": NODE_AUDIENCE, "operation": COVERAGE_OPERATION,
        "expires_at": "2099-01-01T00:01:00+00:00", "challenge": "coverage-challenge"
    });
    let (endpoint, captured, server) = serve_scripted(vec![
        (201, Some(challenge)),
        (
            200,
            Some(json!({"node_id": node_id, "accepted": true, "camera_count": 1})),
        ),
    ]);
    let dir = temp_dir("coverage-push");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, TENANT, node_id, 2)
        .unwrap();
    let coverage = camera_coverage(vec![(
        "cam-a".to_string(),
        vec![(1_790_000_000_000, 1_790_000_060_000)],
    )]);

    assert!(node.push_coverage(coverage).await.unwrap());
    server.join().unwrap();

    let requests = captured.lock().unwrap();
    assert_eq!(requests[0]["operation"], COVERAGE_OPERATION);
    let report = &requests[1];
    assert_eq!(report["challenge"], "coverage-challenge");
    assert_eq!(report["auth_epoch"], 2);
    assert_eq!(
        report["cameras"][0]["ranges"][0]["start_at"],
        "2026-09-21T14:13:20Z"
    );
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().as_bytes());
    assert!(verify_signed(
        &public,
        report["signature"].as_str().unwrap(),
        &without_signature(report)
    ));
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn scheduler_reports_recording_coverage_after_the_inventory() {
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let challenge = |operation: &str, value: &str| {
        json!({"protocol_version": 1, "audience": NODE_AUDIENCE, "operation": operation,
               "expires_at": "2099-01-01T00:01:00+00:00", "challenge": value})
    };
    let (endpoint, captured, server) = serve_scripted(vec![
        (201, Some(challenge(OPERATION, "check-in"))),
        (
            200,
            Some(
                json!({"node_id": node_id, "auth_epoch": 1, "checked_in_at": "2099-01-01T00:00:01Z"}),
            ),
        ),
        (201, Some(challenge(INVENTORY_OPERATION, "inventory"))),
        (
            200,
            Some(json!({"node_id": node_id, "accepted": true, "camera_count": 1})),
        ),
        (201, Some(challenge(COVERAGE_OPERATION, "coverage"))),
        (
            200,
            Some(json!({"node_id": node_id, "accepted": true, "camera_count": 1})),
        ),
    ]);
    let dir = temp_dir("coverage-scheduler");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, TENANT, node_id, 1)
        .unwrap();
    let inventory: InventorySource = Arc::new(|| {
        Box::pin(async {
            camera_inventory(
                &[camera("cam-a", "Front door", true)],
                &std::collections::HashMap::new(),
            )
        })
    });
    let coverage: CoverageSource = Arc::new(|| {
        Box::pin(async {
            camera_coverage(vec![(
                "cam-a".to_string(),
                vec![(1_790_000_000_000, 1_790_000_060_000)],
            )])
        })
    });

    let handle = CloudScheduler::new(
        node.shared(),
        Duration::from_secs(3600),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
    .with_inventory(inventory)
    .with_coverage(coverage)
    .spawn();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while captured.lock().unwrap().len() < 6 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.shutdown().await;
    let finished = captured.lock().unwrap().len() >= 6;
    if finished {
        server.join().unwrap();
    }

    let requests = captured.lock().unwrap();
    assert!(
        finished,
        "the scheduler stopped after {} request(s)",
        requests.len()
    );
    assert_eq!(requests[4]["operation"], COVERAGE_OPERATION);
    assert_eq!(requests[5]["cameras"][0]["camera_id"], "cam-a");
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn departure_is_signed_over_its_own_fresh_challenge() {
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let (endpoint, captured, server) = serve_scripted(vec![
        (
            201,
            Some(json!({
                "protocol_version": 1,
                "audience": NODE_AUDIENCE,
                "operation": DEPARTURE_OPERATION,
                "expires_at": "2099-01-01T00:01:00+00:00",
                "challenge": "server-one-time-challenge"
            })),
        ),
        (204, None),
    ]);
    let dir = temp_dir("departure");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, TENANT, node_id, 3)
        .unwrap();

    node.announce_departure().await.unwrap();
    server.join().unwrap();

    let requests = captured.lock().unwrap();
    assert_eq!(requests[0]["operation"], DEPARTURE_OPERATION);
    let proof = &requests[1];
    assert_eq!(proof["challenge"], "server-one-time-challenge");
    assert_eq!(proof["audience"], NODE_AUDIENCE);
    assert_eq!(proof["node_id"], node_id);
    assert_eq!(proof["auth_epoch"], 3);
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().as_bytes());
    assert!(verify_signed(
        &public,
        proof["signature"].as_str().unwrap(),
        &without_signature(proof)
    ));
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_unlinked_node_announces_no_departure() {
    let dir = temp_dir("departure-unlinked");
    let node = CloudNode::open(dir.clone()).unwrap();
    assert!(matches!(
        node.announce_departure().await,
        Err(CloudError::State | CloudError::NotConfigured)
    ));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_newly_visible_camera_gets_a_snapshot_at_once_then_every_thirty_seconds() {
    let start = std::time::Instant::now();
    let at = |secs: u64| start + Duration::from_secs(secs);
    let ids = |list: &[&str]| list.iter().map(|id| id.to_string()).collect::<Vec<_>>();
    let mut schedule = ThumbnailSchedule::default();

    assert_eq!(schedule.due(&ids(&["a", "b"]), at(0)), ids(&["a", "b"]));
    schedule.uploaded(&ids(&["a", "b"]), at(0));
    assert!(schedule.due(&ids(&["a", "b"]), at(5)).is_empty());
    // Paging to a new camera must not wait for the others' refresh.
    assert_eq!(schedule.due(&ids(&["a", "c"]), at(10)), ids(&["c"]));
    schedule.uploaded(&ids(&["c"]), at(10));
    assert_eq!(schedule.due(&ids(&["a", "c"]), at(30)), ids(&["a"]));
    // A camera that left the page and came back is fresh demand again.
    assert_eq!(schedule.due(&ids(&["b"]), at(31)), ids(&["b"]));
}

#[test]
fn a_failed_capture_is_retried_on_the_next_poll() {
    let start = std::time::Instant::now();
    let mut schedule = ThumbnailSchedule::default();
    let demand = vec!["a".to_string()];
    assert_eq!(schedule.due(&demand, start), demand);
    // Nothing recorded as uploaded: still due five seconds later.
    assert_eq!(
        schedule.due(&demand, start + Duration::from_secs(5)),
        demand
    );
}

#[test]
fn a_waiting_demand_poll_ends_before_the_http_client_gives_up() {
    // The cloud client times requests out after 20 s.
    assert!(THUMBNAIL_DEMAND_WAIT < Duration::from_secs(20));
    assert_eq!(THUMBNAIL_REFRESH, Duration::from_secs(30));
}

#[test]
fn the_demand_poll_waits_only_until_the_next_snapshot_is_due() {
    let start = std::time::Instant::now();
    let demand = vec!["a".to_string(), "b".to_string()];
    let mut schedule = ThumbnailSchedule::default();
    assert_eq!(schedule.next_due_in(&[], start), None, "nothing on screen");
    assert_eq!(
        schedule.next_due_in(&demand, start),
        Some(Duration::ZERO),
        "new cameras"
    );
    schedule.uploaded(&demand, start);
    assert_eq!(
        schedule.next_due_in(&demand, start + Duration::from_secs(10)),
        Some(THUMBNAIL_REFRESH - THUMBNAIL_DUE_SLACK - Duration::from_secs(10))
    );
}

#[test]
fn a_camera_that_could_not_be_captured_is_retried_every_few_seconds_not_every_poll() {
    let start = std::time::Instant::now();
    let demand = vec!["down".to_string()];
    let mut schedule = ThumbnailSchedule::default();
    assert_eq!(schedule.due(&demand, start), demand);
    schedule.missed(&demand, start);
    assert!(schedule
        .due(&demand, start + Duration::from_secs(1))
        .is_empty());
    assert_eq!(
        schedule.due(&demand, start + THUMBNAIL_MISSED_RETRY),
        demand
    );
}

#[test]
fn the_demand_request_says_how_long_to_wait_and_what_the_node_already_has() {
    let url = thumbnail_demand_url(
        &reqwest::Url::parse("http://127.0.0.1:8000/").unwrap(),
        "node-1",
        Duration::from_secs(15),
        &["cam-b".to_string(), "cam-a".to_string()],
    )
    .unwrap();
    assert_eq!(
        url.as_str(),
        "http://127.0.0.1:8000/v1/nodes/node-1/thumbnail-demands?wait=15&known=cam-a%2Ccam-b"
    );
}

#[test]
fn a_snapshot_uploaded_just_after_a_poll_is_still_refreshed_on_the_thirty_second_poll() {
    let start = std::time::Instant::now();
    let demand = vec!["a".to_string()];
    let mut schedule = ThumbnailSchedule::default();
    schedule.uploaded(&demand, start + Duration::from_millis(600));
    assert!(schedule
        .due(&demand, start + THUMBNAIL_REFRESH - Duration::from_secs(5))
        .is_empty());
    assert_eq!(schedule.due(&demand, start + THUMBNAIL_REFRESH), demand);
}

#[test]
fn a_camera_status_change_is_reported_within_seconds_but_not_more_than_every_ten() {
    let start = std::time::Instant::now();
    let snap = |status: &'static str| {
        vec![CameraSnapshot {
            camera_id: "cam-a".into(),
            name: "Door".into(),
            status,
            recording: false,
            live: None,
        }]
    };
    let reported = (snap("connecting"), start);
    // Nothing reported yet: the regular check-in cycle sends the first one.
    assert!(!inventory_change_due(None, &snap("streaming"), start));
    assert!(!inventory_change_due(
        Some(&reported),
        &snap("connecting"),
        start + Duration::from_secs(30)
    ));
    assert!(!inventory_change_due(
        Some(&reported),
        &snap("streaming"),
        start + Duration::from_secs(5)
    ));
    assert!(inventory_change_due(
        Some(&reported),
        &snap("streaming"),
        start + INVENTORY_CHANGE_MIN_GAP
    ));
}

#[test]
fn a_camera_is_reported_recording_when_a_segment_is_open_not_from_its_own_switch() {
    // "Record all cameras" records cameras whose own switch is off.
    let cameras = vec![
        camera("all-cams", "Recorded by the global switch", false),
        camera("switched-on", "Own switch on, segment not open yet", true),
        camera("down", "Own switch on, camera offline", true),
    ];
    let states = std::collections::HashMap::from([
        ("all-cams".to_string(), "recording_ready".to_string()),
        ("switched-on".to_string(), "streaming".to_string()),
        ("down".to_string(), "offline".to_string()),
    ]);
    let recording: Vec<bool> = camera_inventory(&cameras, &states)
        .iter()
        .map(|camera| camera.recording)
        .collect();
    assert_eq!(recording, vec![true, false, false]);
}

#[tokio::test]
async fn the_cloud_counts_as_reachable_only_after_a_recent_successful_request() {
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let challenge = json!({
        "protocol_version": 1,
        "audience": NODE_AUDIENCE,
        "operation": THUMBNAIL_DEMAND_OPERATION,
        "expires_at": "2099-01-01T00:01:00+00:00",
        "challenge": "server-one-time-challenge"
    });
    let (endpoint, _captured, server) = serve_scripted(vec![
        (201, Some(challenge.clone())),
        (200, Some(json!({"camera_ids": []}))),
        (201, Some(challenge)),
        (500, Some(json!({"message": "down"}))),
    ]);
    let dir = temp_dir("reachable");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, TENANT, node_id, 3)
        .unwrap();
    assert!(!node.report().unwrap().cloud_reachable, "nothing heard yet");

    node.pull_thumbnail_demands(Duration::ZERO, &[])
        .await
        .unwrap();
    assert!(node.report().unwrap().cloud_reachable);

    // A failure does not refresh it, and it lapses once the last success is old.
    assert!(node
        .pull_thumbnail_demands(Duration::ZERO, &[])
        .await
        .is_err());
    server.join().unwrap();
    node.lock().unwrap().last_contact =
        Some(std::time::Instant::now() - CLOUD_REACHABLE_WITHIN - Duration::from_secs(1));
    assert!(!node.report().unwrap().cloud_reachable);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_cloud_that_refuses_this_node_is_reported_as_a_link_problem_until_it_accepts_again() {
    // Unlinking or revoking a recorder in the web portal leaves it "linked"
    // locally; the portal then answers every signed request with 401.
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let challenge = json!({
        "protocol_version": 1,
        "audience": NODE_AUDIENCE,
        "operation": THUMBNAIL_DEMAND_OPERATION,
        "expires_at": "2099-01-01T00:01:00+00:00",
        "challenge": "server-one-time-challenge"
    });
    let (endpoint, _captured, server) = serve_scripted(vec![
        (401, Some(json!({"code": "unauthenticated"}))),
        (201, Some(challenge)),
        (200, Some(json!({"camera_ids": []}))),
    ]);
    let dir = temp_dir("refused");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, TENANT, node_id, 3)
        .unwrap();
    assert_eq!(node.report().unwrap().cloud_problem, None);

    let refused = node.pull_thumbnail_demands(Duration::ZERO, &[]).await;
    assert!(
        matches!(refused, Err(CloudError::Refused(401))),
        "{refused:?}"
    );
    let report = node.report().unwrap();
    assert_eq!(report.cloud_problem, Some("link_refused"));
    assert!(!report.cloud_reachable);

    node.pull_thumbnail_demands(Duration::ZERO, &[])
        .await
        .unwrap();
    server.join().unwrap();
    let report = node.report().unwrap();
    assert_eq!(report.cloud_problem, None);
    assert!(report.cloud_reachable);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_sealed_portal_request_runs_in_plaintext_here_and_goes_back_sealed() {
    let node_id = "6ba7b812-9dad-11d1-80b4-00c04fd430c8";
    let (endpoint, captured, server) = serve_scripted(vec![
        (
            201,
            Some(json!({
                "protocol_version": 1, "audience": NODE_AUDIENCE, "operation": CONFIG_RESULT_OPERATION,
                "expires_at": "2099-01-01T00:01:00+00:00", "challenge": "server-one-time-challenge"
            })),
        ),
        (204, None),
    ]);
    let dir = temp_dir("sealed-request");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&endpoint, TENANT, node_id, 3)
        .unwrap();
    let node_public = sealed::node_public_key(&node.lock().unwrap().key);
    let browser_scalar = [21u8; 32];
    let reply_key =
        curve25519_dalek::montgomery::MontgomeryPoint::mul_base_clamped(browser_scalar).to_bytes();
    let request_id = "feedfacefeedfacefeedfacefeedface";
    let envelope = sealed::seal_request_with(
        [22u8; 32],
        [23u8; 12],
        node_public,
        node_id,
        "camera.discovery.profiles",
        &json!({
            "id": request_id,
            "exp": OffsetDateTime::now_utc().unix_timestamp() + 60,
            "reply_key": URL_SAFE_NO_PAD.encode(reply_key),
            "params": {"candidate_id": "c1", "username": "admin", "password_b64": "c2VjcmV0"}
        }),
    );
    let seen = StdArc::new(StdMutex::new(Vec::new()));
    let record = StdArc::clone(&seen);
    let handler: RemoteConfigHandler = Arc::new(move |method, params, was_sealed| {
        record.lock().unwrap().push((method, params, was_sealed));
        Box::pin(async { Ok(json!({"streams": [{"stream_id": "s1", "name": "10.0.10.23 main"}]})) })
    });

    run_remote_request(
        node.shared(),
        handler,
        RemoteConfigRequest {
            request_id: "a".repeat(32),
            method: "camera.discovery.profiles".into(),
            params: json!({"sealed": envelope}),
        },
    )
    .await;
    server.join().unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, "camera.discovery.profiles");
    assert_eq!(seen[0].1["password_b64"], "c2VjcmV0");
    assert!(seen[0].2, "the handler is told it arrived sealed");
    let body = captured.lock().unwrap()[1].clone();
    let text = body.to_string();
    assert!(
        !text.contains("10.0.10.23") && !text.contains("c2VjcmV0"),
        "{text}"
    );
    let result: Value = serde_json::from_str(body["result_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        sealed::open_reply(browser_scalar, node_id, request_id, &result["sealed"]).unwrap(),
        json!({"ok": true, "result": {"streams": [{"stream_id": "s1", "name": "10.0.10.23 main"}]}})
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_pinned_build_accepts_only_its_own_cloud() {
    let options = CloudOptions::pinned_to(Some("https://cloud.example.com/"));
    assert_eq!(
        options.endpoint.as_deref(),
        Some("https://cloud.example.com")
    );
    assert!(!options.policy.allow_loopback_http);

    for own in ["https://cloud.example.com", "https://cloud.example.com/"] {
        assert!(
            CloudEndpoint::parse_with(own, options.policy).is_ok(),
            "{own}"
        );
    }
    for other in [
        "https://other.example.com",
        "https://cloud.example.com.evil.example",
        "http://127.0.0.1:8000",
    ] {
        let error = CloudEndpoint::parse_with(other, options.policy).unwrap_err();
        assert!(
            error.to_string().contains("links only to NeuroWatch Cloud"),
            "{other}: {error}"
        );
    }
}

#[test]
fn a_release_build_links_only_to_neurowatch_cloud_whatever_its_settings() {
    for (endpoint, loopback) in [
        (None, None),
        (Some("http://127.0.0.1:8000"), Some("1")),
        (Some("https://other.example.com"), None),
    ] {
        let options = CloudOptions::for_build(false, endpoint, loopback);
        assert_eq!(options, CloudOptions::pinned_to(Some(NEUROWATCH_CLOUD)));
        assert_eq!(options.endpoint.as_deref(), Some("https://neurowatch.app"));
        assert!(!options.policy.allow_loopback_http);
    }
}

#[test]
fn a_development_build_uses_its_development_cloud_else_neurowatch_cloud() {
    let dev = CloudOptions::for_build(true, Some("http://127.0.0.1:8000/"), Some("1"));
    assert_eq!(dev.endpoint.as_deref(), Some("http://127.0.0.1:8000"));
    assert!(dev.policy.allow_loopback_http);
    assert_eq!(dev.policy.pinned, None);
    for unset in [None, Some(""), Some("  ")] {
        assert_eq!(
            CloudOptions::for_build(true, unset, None),
            CloudOptions::pinned_to(Some(NEUROWATCH_CLOUD))
        );
    }
}

#[test]
fn a_release_built_without_a_cloud_url_links_nowhere() {
    let options = CloudOptions::pinned_to(None);
    assert_eq!(options.endpoint, None);
    let error = CloudEndpoint::parse_with("https://cloud.example.com", options.policy).unwrap_err();
    assert!(
        error.to_string().contains("not linked to a cloud"),
        "{error}"
    );
}

#[test]
fn a_pinned_node_refuses_a_link_started_with_another_url() {
    let dir = temp_dir("pinned-claim");
    let mut node = CloudNode::open_with_options(
        dir.clone(),
        CloudOptions::pinned_to(Some("https://cloud.example.com")),
    )
    .unwrap();
    let refused = node.prepare_claim(EnrollmentInput {
        endpoint: "https://other.example.com".into(),
        enrollment_id: "enr".into(),
        tenant_id: "ten".into(),
        expires_at: "2099-01-01T00:00:00Z".into(),
        challenge: "challenge".into(),
        transfer_proof: "proof".into(),
        display_label: "Office".into(),
    });
    assert!(refused.is_err());
    assert_eq!(node.status().phase, LinkPhase::Unlinked);
    let _ = fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// A recorder whose clock is wrong (e.g. a Windows PC with the wrong time zone)
// ---------------------------------------------------------------------------

type CloudReply = Box<dyn Fn(&str, &Value) -> (u16, Value) + Send>;

/// A cloud that behaves like the portal on time: every answer carries a `Date`
/// header from the real clock, enrollment codes expire five minutes from now,
/// and signed requests whose `issued_at` is more than 15 s ahead of it or
/// whose `expires_at` has passed are refused with 401.
fn serve_real_clock_cloud(
    replies: Vec<CloudReply>,
) -> (String, StdArc<StdMutex<Vec<Value>>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let captured = StdArc::new(StdMutex::new(Vec::new()));
    let output = StdArc::clone(&captured);
    let task = thread::spawn(move || {
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end = loop {
                let read = stream.read(&mut buffer).unwrap();
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(position) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]).to_string();
            let path = headers
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .parse::<usize>()
                .unwrap();
            while bytes.len() - header_end < length {
                let read = stream.read(&mut buffer).unwrap();
                bytes.extend_from_slice(&buffer[..read]);
            }
            let request: Value =
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
            output.lock().unwrap().push(request.clone());
            let now = OffsetDateTime::now_utc();
            let parse = |key: &str| {
                request[key].as_str().map(|raw| {
                    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
                        .unwrap()
                })
            };
            let skewed = parse("issued_at")
                .is_some_and(|issued| issued > now + TimeDuration::seconds(15))
                || parse("expires_at")
                    .is_some_and(|expires| request.get("issued_at").is_some() && expires <= now);
            let (status, body) = if skewed {
                (401, json!({"detail": "node authentication failed"}))
            } else {
                reply(&path, &request)
            };
            let body = serde_json::to_vec(&body).unwrap();
            let date = now
                .format(&time::format_description::well_known::Rfc2822)
                .unwrap()
                .replace("+0000", "GMT");
            write!(
                stream,
                "HTTP/1.1 {status} X\r\nDate: {date}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    (format!("http://{address}/"), captured, task)
}

fn in_minutes(minutes: i64) -> String {
    fmt_time(OffsetDateTime::now_utc() + TimeDuration::minutes(minutes)).unwrap()
}

fn claim_parameters_reply() -> CloudReply {
    Box::new(|_, _| {
        (
            200,
            json!({
                "protocol_version": 1, "audience": ENROLLMENT_AUDIENCE,
                "enrollment_id": ENROLLMENT, "tenant_id": TENANT,
                "expires_at": in_minutes(5),
                "challenge": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            }),
        )
    })
}

fn challenge_reply(operation: &'static str) -> CloudReply {
    Box::new(move |_, _| {
        (
            201,
            json!({"protocol_version": 1, "audience": NODE_AUDIENCE, "operation": operation,
                   "challenge": "server-one-time-challenge", "expires_at": in_minutes(2)}),
        )
    })
}

fn check_in_reply() -> CloudReply {
    Box::new(|_, request| {
        (
            200,
            json!({"node_id": request["node_id"], "auth_epoch": request["auth_epoch"],
                   "checked_in_at": fmt_time(OffsetDateTime::now_utc()).unwrap()}),
        )
    })
}

/// Sri Lanka is UTC+5:30: a PC showing the right local time with its time
/// zone set to UTC has a UTC clock 5 h 30 min ahead.
const WRONG_TIME_ZONE: TimeDuration = TimeDuration::minutes(330);

#[tokio::test]
async fn a_recorder_whose_clock_is_hours_ahead_links_and_checks_in_by_the_clouds_clock() {
    let replies: Vec<CloudReply> = vec![
        claim_parameters_reply(),
        Box::new(|_, _| {
            (
                200,
                json!({"claim_id": "6ba7b813-9dad-11d1-80b4-00c04fd430c8"}),
            )
        }),
        Box::new(|_, request| {
            (
                200,
                json!({"status": "linked", "node_id": request["node_id"]}),
            )
        }),
        challenge_reply(OPERATION),
        check_in_reply(),
    ];
    let (base, captured, server) = serve_real_clock_cloud(replies);
    let dir = temp_dir("skewed-link");
    let options = CloudOptions {
        endpoint: Some(base.trim_end_matches('/').to_owned()),
        ..dev_options()
    };
    let mut node = CloudNode::open_with_options(dir.clone(), options).unwrap();
    node.websocket = None;
    node.clock.set_local_skew(WRONG_TIME_ZONE);

    node.begin_with_code(&code(), "Colombo office".into())
        .await
        .expect("the code's expiry is judged by the cloud's clock");
    node.confirm_remote().await.unwrap();
    assert_eq!(node.status().phase, LinkPhase::Linked);
    node.check_in_once()
        .await
        .expect("signed times follow the cloud's clock");
    server.join().unwrap();

    let requests = captured.lock().unwrap();
    let issued = OffsetDateTime::parse(
        requests[4]["issued_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert!((issued - OffsetDateTime::now_utc()).abs() < TimeDuration::seconds(5));
    assert!(requests[4]["issued_at"].as_str().unwrap().ends_with('Z'));

    let report = node.report().unwrap();
    let offset = report.clock_offset_sec.unwrap();
    assert!((offset + 330 * 60).abs() <= 2, "{offset}");
    let warning = report.clock_warning.unwrap();
    assert!(
        warning.contains("5 h 30 min ahead of NeuroWatch Cloud"),
        "{warning}"
    );
    assert!(report.status.last_check_in.is_some());
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_portals_server_time_is_preferred_over_the_date_header() {
    let replies: Vec<CloudReply> = vec![
        Box::new(|_, _| {
            (
                200,
                json!({
                    "protocol_version": 1, "audience": ENROLLMENT_AUDIENCE,
                    "enrollment_id": ENROLLMENT, "tenant_id": TENANT,
                    "expires_at": in_minutes(5),
                    "challenge": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    "server_time": OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339).unwrap(),
                }),
            )
        }),
        Box::new(|_, _| {
            (
                200,
                json!({"claim_id": "6ba7b813-9dad-11d1-80b4-00c04fd430c8"}),
            )
        }),
    ];
    let (base, _captured, server) = serve_real_clock_cloud(replies);
    let dir = temp_dir("skewed-server-time");
    let options = CloudOptions {
        endpoint: Some(base.trim_end_matches('/').to_owned()),
        ..dev_options()
    };
    let mut node = CloudNode::open_with_options(dir.clone(), options).unwrap();
    node.websocket = None;
    node.clock.set_local_skew(-WRONG_TIME_ZONE);

    node.begin_with_code(&code(), "Colombo office".into())
        .await
        .unwrap();
    server.join().unwrap();
    let estimate = node.clock.measured_offset().unwrap();
    // Precise to the round trip, not just to the Date header's second.
    assert!((estimate - WRONG_TIME_ZONE).abs() < TimeDuration::milliseconds(500));
    assert!(node
        .report()
        .unwrap()
        .clock_warning
        .unwrap()
        .contains("5 h 30 min behind"));
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_refusal_for_a_wrong_clock_teaches_the_offset_and_the_next_check_in_succeeds() {
    // Linked earlier, then this computer's clock fell 10 minutes behind.
    let replies: Vec<CloudReply> = vec![
        // Never reached: the cloud refuses the stale challenge request first.
        Box::new(|_, _| (500, json!({}))),
        challenge_reply(OPERATION),
        check_in_reply(),
    ];
    let (base, captured, server) = serve_real_clock_cloud(replies);
    let dir = temp_dir("skewed-refusal");
    let mut node = CloudNode::open(dir.clone()).unwrap();
    node.install_linked_for_test(&base, TENANT, NODE, 3)
        .unwrap();
    node.clock.set_local_skew(-TimeDuration::minutes(10));

    assert!(matches!(
        node.check_in_once().await,
        Err(CloudError::Refused(401))
    ));
    node.check_in_once().await.unwrap();
    server.join().unwrap();
    assert_eq!(captured.lock().unwrap().len(), 3);
    let report = node.report().unwrap();
    assert!(report.cloud_reachable);
    assert!(report.clock_warning.unwrap().contains("10 min behind"));
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn without_any_cloud_time_an_expired_looking_code_says_to_check_the_clock() {
    let (base, _captured, server) = serve_scripted(vec![(
        200,
        Some(json!({
            "protocol_version": 1, "audience": ENROLLMENT_AUDIENCE,
            "enrollment_id": ENROLLMENT, "tenant_id": TENANT,
            "expires_at": in_minutes(5),
            "challenge": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        })),
    )]);
    let dir = temp_dir("skewed-no-date");
    let options = CloudOptions {
        endpoint: Some(base.trim_end_matches('/').to_owned()),
        ..dev_options()
    };
    let mut node = CloudNode::open_with_options(dir.clone(), options).unwrap();
    node.clock.set_local_skew(WRONG_TIME_ZONE);

    let error = node
        .begin_with_code(&code(), "Colombo office".into())
        .await
        .unwrap_err()
        .to_string();
    server.join().unwrap();
    assert!(error.contains("enrollment code has expired"), "{error}");
    assert!(error.contains("time zone"), "{error}");
    assert_eq!(node.status().phase, LinkPhase::Unlinked);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_correct_clock_reports_no_offset_until_the_cloud_answers_and_no_warning() {
    let dir = temp_dir("clock-status");
    let node = CloudNode::open(dir.clone()).unwrap();
    let report = node.report().unwrap();
    assert_eq!(report.clock_offset_sec, None);
    assert_eq!(report.clock_warning, None);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["clock_offset_sec"], Value::Null);
    assert!(json.get("clock_warning").is_none());
    fs::remove_dir_all(dir).unwrap();
}
