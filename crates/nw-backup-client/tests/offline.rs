//! The client without an ingest: what it says when there is no plan, no
//! readable ticket or no server to reach, and how it keeps a camera's units
//! meanwhile (in memory, then in the outage folder across a restart). The
//! sessions themselves are tested against the real ingest in its own
//! repository.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use nw_backup_client::{tls_config, AssignFuture, Assigner, Assignment, Backup, Config, Status};
use nw_backup_protocol::frame::{flags, MediaKind, MediaUnit};
use nw_backup_protocol::messages::{Codec, TrackFormat, TrackKind};

/// Hands out the same answer every time it is asked.
struct Fixed(Mutex<Result<Assignment, String>>);

impl Assigner for Fixed {
    fn assign(&self) -> AssignFuture<'_> {
        let answer = self.0.lock().unwrap().clone();
        Box::pin(async move { answer })
    }
}

fn config(outage_dir: Option<std::path::PathBuf>) -> Config {
    Config {
        node_key: SigningKey::from_bytes(&[7; 32]),
        recorder: "test".into(),
        tls: tls_config(None).unwrap(),
        outage_dir,
        memory_bytes_per_track: 1 << 20,
        disk_bytes_per_track: 4 << 20,
    }
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// A ticket the client can read (the ingest would check its signature).
fn ticket() -> String {
    let claims = serde_json::json!({
        "v": 1, "id": "t-offline", "org_id": "org", "node_id": "node",
        "node_key": "00".repeat(32), "audience": "ingest.example.test",
        "issued_at_ms": 0, "expires_at_ms": i64::MAX,
        "cameras": [{"camera_id": "cam-1", "retention_s": 3600,
                     "limits": {"max_long_side": 1920, "max_short_side": 1080,
                                "max_fps": 30.0, "max_bitrate_bps": 4000000}}]
    });
    format!(
        "{}.{}",
        base64url(claims.to_string().as_bytes()),
        "00".repeat(64)
    )
}

fn unit(seq: u64, keyframe: bool) -> MediaUnit {
    MediaUnit {
        media: MediaKind::Video,
        flags: if keyframe { flags::KEYFRAME } else { 0 },
        track: 0,
        seq,
        time_us: 1_790_000_000_000_000 + seq as i64 * 40_000,
        pts_ns: Some(seq * 40_000_000),
        dts_ns: None,
        data: vec![0, 0, 0, 1, 0x65, seq as u8],
    }
}

fn h264() -> TrackFormat {
    TrackFormat {
        codec: Codec::H264,
        width: Some(1920),
        height: Some(1080),
        sample_rate: None,
        channels: None,
        codec_data: None,
    }
}

/// The status once the session has tried and failed at least once.
async fn retrying(backup: &Backup) -> Status {
    for _ in 0..200 {
        let status = backup.status();
        if status.state == "retrying" {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never retried: {:?}", backup.status());
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn without_a_plan_it_says_so_and_keeps_trying() {
    let backup = Backup::start(
        config(None),
        Arc::new(Fixed(Mutex::new(Err("no plan for this recorder".into())))),
    );
    let status = retrying(&backup).await;
    assert_eq!(
        status.problem.as_deref(),
        Some("No backup ticket: no plan for this recorder")
    );
    assert_eq!(status.ingest, None);
    backup.stop().await;
}

#[tokio::test]
async fn a_ticket_it_cannot_read_is_reported() {
    let assignment = Assignment {
        ticket: "not a ticket".into(),
        server_name: "ingest.example.test".into(),
        quic: None,
        tcp: None,
        instance_key: None,
    };
    let backup = Backup::start(config(None), Arc::new(Fixed(Mutex::new(Ok(assignment)))));
    let status = retrying(&backup).await;
    assert_eq!(
        status.problem.as_deref(),
        Some("The backup ticket could not be read.")
    );
    backup.stop().await;
}

#[tokio::test]
async fn an_ingest_it_cannot_reach_is_reported_for_both_transports() {
    let port = closed_port();
    let assignment = Assignment {
        ticket: ticket(),
        server_name: "localhost".into(),
        quic: Some(format!("127.0.0.1:{port}")),
        tcp: Some(format!("127.0.0.1:{port}")),
        instance_key: None,
    };
    let backup = Backup::start(config(None), Arc::new(Fixed(Mutex::new(Ok(assignment)))));
    let status = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let status = retrying(&backup).await;
            if status
                .problem
                .as_deref()
                .is_some_and(|p| p.contains("could not be reached"))
            {
                return status;
            }
        }
    })
    .await
    .expect("the unreachable ingest is reported");
    let problem = status.problem.unwrap();
    assert!(
        problem.contains("QUIC") && problem.contains("TCP"),
        "{problem}"
    );
    backup.stop().await;
}

#[tokio::test]
async fn units_wait_for_the_ingest_and_outlive_a_restart_in_the_outage_folder() {
    let dir = tempfile::tempdir().unwrap();
    let assigner = || Arc::new(Fixed(Mutex::new(Err("offline".into()))));
    let backup = Backup::start(config(Some(dir.path().to_path_buf())), assigner());
    let track = backup.track("cam-1", TrackKind::Video, h264());
    assert_eq!(track.camera_id(), "cam-1");
    assert_eq!(track.refusal(), None);
    assert!(!track.over_plan());
    track.push(unit(1, true));
    track.push(unit(2, false));
    track.push(unit(3, false));
    track.set_format(h264());
    let status = retrying(&backup).await;
    let [camera] = status.tracks.as_slice() else {
        panic!("one track: {status:?}");
    };
    assert_eq!(
        (camera.camera_id.as_str(), camera.kind),
        ("cam-1", TrackKind::Video)
    );
    assert_eq!(camera.unconfirmed_units, 3);
    assert!(camera.memory_bytes > 0);
    // A clean stop keeps what was never confirmed in the outage folder…
    backup.stop().await;
    assert!(
        std::fs::read_dir(dir.path()).unwrap().count() > 0,
        "the unconfirmed units were kept"
    );
    // …and the next start takes it up again, to send when it can.
    let again = Backup::start(config(Some(dir.path().to_path_buf())), assigner());
    let status = retrying(&again).await;
    assert!(
        status
            .tracks
            .iter()
            .any(|t| t.camera_id == "cam-1" && t.unconfirmed_units == 3),
        "recovered: {status:?}"
    );
    again.stop().await;
}
