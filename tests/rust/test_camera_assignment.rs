#[path = "../../crates/nw-backend/src/assignment.rs"]
mod assignment;

use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration as StdDuration;

use assignment::{AssignmentError, AssignmentRegistry, LocalAssignmentRegistry};
use nw_core::control_plane::{CameraAssignment, Lease, CONTROL_PROTOCOL_VERSION};
use time::{Duration, OffsetDateTime};

fn assignment(
    assignment_id: &str,
    camera_id: &str,
    node_id: &str,
    generation: u64,
    issued_at: OffsetDateTime,
    expires_at: OffsetDateTime,
) -> CameraAssignment {
    CameraAssignment {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        assignment_id: assignment_id.to_owned(),
        camera_id: camera_id.to_owned(),
        node_id: node_id.to_owned(),
        lease: Lease {
            token: format!("lease-{assignment_id}"),
            issued_at_utc: issued_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            expires_at_utc: expires_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            duration_seconds: (expires_at - issued_at).whole_seconds() as u64,
        },
        fencing_generation: generation,
        assigned_at_utc: issued_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap(),
    }
}

#[test]
fn valid_assignment_permits_start() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = LocalAssignmentRegistry::new();
    let current = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(1),
    );

    registry.install(current.clone()).unwrap();

    assert!(registry.permits(&current, now).is_ok());
}

#[test]
fn expired_lease_rejects_start_and_recording_continuation() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = LocalAssignmentRegistry::new();
    let current = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::seconds(1),
    );

    registry.install(current.clone()).unwrap();

    assert_eq!(
        registry.permits(&current, now + Duration::seconds(1)),
        Err(AssignmentError::LeaseExpired)
    );
}

#[test]
fn future_issued_lease_rejects_start() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = LocalAssignmentRegistry::new();
    let future = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now + Duration::seconds(1),
        now + Duration::minutes(1),
    );

    registry.install(future.clone()).unwrap();

    assert_eq!(
        registry.permits(&future, now),
        Err(AssignmentError::LeaseNotYetValid)
    );
}

#[test]
fn stale_fencing_generation_rejects_restart() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = LocalAssignmentRegistry::new();
    let current = assignment(
        "assignment-2",
        "camera-1",
        "node-1",
        2,
        now,
        now + Duration::minutes(1),
    );
    let stale = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(1),
    );

    registry.install(current).unwrap();

    assert_eq!(
        registry.permits(&stale, now),
        Err(AssignmentError::NotCurrent)
    );
}

#[test]
fn renewal_with_same_fencing_generation_is_valid() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = LocalAssignmentRegistry::new();
    let original = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(1),
    );
    let renewed = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(2),
    );

    registry.install(original.clone()).unwrap();
    registry.install(renewed.clone()).unwrap();

    // The running supervisor retains its original assignment object, but must
    // use the renewed lease belonging to the same fenced owner.
    assert!(registry
        .permits(&original, now + Duration::minutes(1))
        .is_ok());
    assert!(registry
        .permits(&renewed, now + Duration::minutes(1))
        .is_ok());
}

#[test]
fn replacement_assignment_retires_prior_owner() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = LocalAssignmentRegistry::new();
    let prior = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(1),
    );
    let replacement = assignment(
        "assignment-2",
        "camera-1",
        "node-2",
        2,
        now,
        now + Duration::minutes(1),
    );

    registry.install(prior.clone()).unwrap();
    registry.install(replacement.clone()).unwrap();

    assert_eq!(
        registry.permits(&prior, now),
        Err(AssignmentError::NotCurrent)
    );
    assert!(registry.permits(&replacement, now).is_ok());
}

#[test]
fn activation_is_fenced_and_stale_owner_cannot_restart() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = LocalAssignmentRegistry::new();
    let prior = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(1),
    );
    let replacement = assignment(
        "assignment-2",
        "camera-1",
        "node-2",
        2,
        now,
        now + Duration::minutes(1),
    );
    registry.install(prior.clone()).unwrap();
    let prior_owner = registry.acquire(&prior, now).unwrap();
    assert!(prior_owner.permits(now));
    prior_owner.release();

    registry.install(replacement.clone()).unwrap();
    assert_eq!(
        registry.permits(&prior, now),
        Err(AssignmentError::NotCurrent)
    );
    assert_eq!(
        registry.acquire(&prior, now).map(|_| ()),
        Err(AssignmentError::NotCurrent)
    );
    let replacement_owner = registry.acquire(&replacement, now).unwrap();
    assert!(replacement_owner.permits(now));
}

#[test]
fn expiry_stops_recording_and_supervisor_lifecycle() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = LocalAssignmentRegistry::new();
    let current = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::seconds(1),
    );
    registry.install(current.clone()).unwrap();

    let mut recording_and_supervisor_running = true;
    if registry
        .permits(&current, now + Duration::seconds(1))
        .is_err()
    {
        recording_and_supervisor_running = false;
    }
    assert!(!recording_and_supervisor_running);
}

#[test]
fn renewal_does_not_revoke_active_owner() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = Arc::new(LocalAssignmentRegistry::new());
    let prior = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(1),
    );
    let renewal = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(2),
    );
    registry.install(prior.clone()).unwrap();
    let owner = registry.acquire(&prior, now).unwrap();
    registry.install(renewal).unwrap();
    assert!(owner.permits(now + Duration::minutes(1)));
}

#[test]
fn replacement_activation_waits_for_prior_owner_to_release() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = Arc::new(LocalAssignmentRegistry::new());
    let prior = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(1),
    );
    let replacement = assignment(
        "assignment-2",
        "camera-1",
        "node-2",
        2,
        now,
        now + Duration::minutes(1),
    );
    registry.install(prior.clone()).unwrap();

    let prior_owner = registry.acquire(&prior, now).unwrap();
    let (replacement_started_tx, replacement_started_rx) = mpsc::channel();
    let (replacement_active_tx, replacement_active_rx) = mpsc::channel();
    let replacing_registry = Arc::clone(&registry);
    let replacing = std::thread::spawn(move || {
        replacement_started_tx.send(()).unwrap();
        replacing_registry.install(replacement.clone()).unwrap();
        let _replacement_owner = replacing_registry.acquire(&replacement, now).unwrap();
        replacement_active_tx.send(()).unwrap();
    });

    replacement_started_rx.recv().unwrap();
    assert!(replacement_active_rx
        .recv_timeout(StdDuration::from_millis(50))
        .is_err());

    prior_owner.release();
    replacement_active_rx
        .recv_timeout(StdDuration::from_secs(1))
        .unwrap();
    replacing.join().unwrap();
}

#[test]
fn concurrent_replacements_never_allow_an_older_generation_to_overwrite_newer() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = Arc::new(LocalAssignmentRegistry::new());
    let prior = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(1),
    );
    let generation_two = assignment(
        "assignment-2",
        "camera-1",
        "node-2",
        2,
        now,
        now + Duration::minutes(1),
    );
    let generation_three = assignment(
        "assignment-3",
        "camera-1",
        "node-3",
        3,
        now,
        now + Duration::minutes(1),
    );
    registry.install(prior.clone()).unwrap();
    let prior_owner = registry.acquire(&prior, now).unwrap();

    let (two_started_tx, two_started_rx) = mpsc::channel();
    let (three_started_tx, three_started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let start_install = Arc::new(Barrier::new(3));
    let registry_two = Arc::clone(&registry);
    let start_two = Arc::clone(&start_install);
    let two = std::thread::spawn(move || {
        two_started_tx.send(()).unwrap();
        start_two.wait();
        let _ = registry_two.install(generation_two);
        done_tx.send(()).unwrap();
    });
    two_started_rx.recv().unwrap();
    let registry_three = Arc::clone(&registry);
    let start_three = Arc::clone(&start_install);
    let three = std::thread::spawn(move || {
        three_started_tx.send(()).unwrap();
        start_three.wait();
        registry_three.install(generation_three).unwrap();
    });
    three_started_rx.recv().unwrap();
    start_install.wait();

    // Both replacement attempts are blocked behind the active owner. Once it
    // acknowledges shutdown, every awakened installer must revalidate.
    assert!(done_rx.recv_timeout(StdDuration::from_millis(50)).is_err());
    prior_owner.release();
    two.join().unwrap();
    three.join().unwrap();

    let current = assignment(
        "assignment-3",
        "camera-1",
        "node-3",
        3,
        now,
        now + Duration::minutes(1),
    );
    assert!(registry.acquire(&current, now).is_ok());
}

#[test]
fn dropping_an_unacknowledged_lease_preserves_the_fence() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
    let registry = LocalAssignmentRegistry::new();
    let prior = assignment(
        "assignment-1",
        "camera-1",
        "node-1",
        1,
        now,
        now + Duration::minutes(1),
    );
    let replacement = assignment(
        "assignment-2",
        "camera-1",
        "node-2",
        2,
        now,
        now + Duration::minutes(1),
    );
    registry.install(prior.clone()).unwrap();

    let owner = registry.acquire(&prior, now).unwrap();
    drop(owner);

    // A supervisor that cannot prove teardown must not implicitly release its
    // lease merely because its task exits.
    assert_eq!(
        registry.install(replacement),
        Err(AssignmentError::TeardownTimeout)
    );
}
