//! Local camera-assignment authority used to fence pipeline ownership.
//!
//! The registry is deliberately transport-neutral so a durable control-plane
//! client can replace it without changing the pipeline lifecycle boundary.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use nw_core::control_plane::CameraAssignment;
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const REPLACEMENT_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AssignmentError {
    #[error("assignment is invalid")]
    Invalid,
    #[error("assignment lease is not yet valid")]
    LeaseNotYetValid,
    #[error("assignment lease has expired")]
    LeaseExpired,
    #[error("assignment is not the current fenced owner")]
    NotCurrent,
    #[error("assignment fencing generation is stale")]
    StaleFencingGeneration,
    #[error("prior assignment did not acknowledge teardown before the replacement deadline")]
    TeardownTimeout,
}

/// A registry-managed, exclusive lifecycle lease for one supervisor.
///
/// It is deliberately not released on drop: only a supervisor that has proved
/// its pipeline and callbacks stopped may explicitly acknowledge teardown.
pub trait ActiveAssignmentLease: Send {
    fn permits(&self, now: OffsetDateTime) -> bool;
    fn release(self: Box<Self>);
}

/// Provides current camera-assignment authority to pipeline supervisors.
pub trait AssignmentRegistry: Send + Sync {
    /// Install or renew an assignment as the candidate current owner.
    ///
    /// Replacing a different owner revokes it and waits a bounded period for
    /// the old supervisor's verified teardown acknowledgement.
    fn install(&self, assignment: CameraAssignment) -> Result<(), AssignmentError>;

    /// Acquire exclusive active ownership for a supervisor lifecycle.
    fn acquire(
        &self,
        assignment: &CameraAssignment,
        now: OffsetDateTime,
    ) -> Result<Box<dyn ActiveAssignmentLease>, AssignmentError>;
}

#[derive(Default)]
struct RegistryState {
    assignments: HashMap<String, CameraAssignment>,
    active: HashMap<String, ActiveOwner>,
}

struct ActiveOwner {
    assignment: CameraAssignment,
    revoked: bool,
}

#[derive(Default)]
struct SharedRegistry {
    state: Mutex<RegistryState>,
    owner_released: Condvar,
}

/// In-process assignment registry for local-mode ownership fencing.
#[derive(Clone, Default)]
pub struct LocalAssignmentRegistry {
    shared: Arc<SharedRegistry>,
}

impl LocalAssignmentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verify that an assignment is still current and has not expired at `now`.
    /// This probe is test-only; production supervisors hold an active lease.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn permits(
        &self,
        assignment: &CameraAssignment,
        now: OffsetDateTime,
    ) -> Result<(), AssignmentError> {
        let state = self
            .shared
            .state
            .lock()
            .expect("assignment registry lock poisoned");
        current_permits(&state.assignments, assignment, now)
    }
}

fn same_owner(left: &CameraAssignment, right: &CameraAssignment) -> bool {
    left.camera_id == right.camera_id
        && left.assignment_id == right.assignment_id
        && left.node_id == right.node_id
        && left.fencing_generation == right.fencing_generation
}

fn current_permits(
    assignments: &HashMap<String, CameraAssignment>,
    assignment: &CameraAssignment,
    now: OffsetDateTime,
) -> Result<(), AssignmentError> {
    let current = assignments
        .get(&assignment.camera_id)
        .ok_or(AssignmentError::NotCurrent)?;
    if !same_owner(current, assignment) {
        return Err(AssignmentError::NotCurrent);
    }
    let issued_at = OffsetDateTime::parse(&current.lease.issued_at_utc, &Rfc3339)
        .map_err(|_| AssignmentError::Invalid)?;
    let expires_at = OffsetDateTime::parse(&current.lease.expires_at_utc, &Rfc3339)
        .map_err(|_| AssignmentError::Invalid)?;
    if now < issued_at {
        return Err(AssignmentError::LeaseNotYetValid);
    }
    if now >= expires_at {
        return Err(AssignmentError::LeaseExpired);
    }
    Ok(())
}

struct LocalActiveAssignmentLease {
    shared: Arc<SharedRegistry>,
    camera_id: String,
    assignment: CameraAssignment,
}

impl ActiveAssignmentLease for LocalActiveAssignmentLease {
    fn permits(&self, now: OffsetDateTime) -> bool {
        let state = self
            .shared
            .state
            .lock()
            .expect("assignment registry lock poisoned");
        let Some(active) = state.active.get(&self.camera_id) else {
            return false;
        };
        !active.revoked
            && same_owner(&active.assignment, &self.assignment)
            && current_permits(&state.assignments, &self.assignment, now).is_ok()
    }

    fn release(self: Box<Self>) {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("assignment registry lock poisoned");
        if state
            .active
            .get(&self.camera_id)
            .is_some_and(|active| same_owner(&active.assignment, &self.assignment))
        {
            state.active.remove(&self.camera_id);
            self.shared.owner_released.notify_all();
        }
    }
}

impl AssignmentRegistry for LocalAssignmentRegistry {
    fn install(&self, assignment: CameraAssignment) -> Result<(), AssignmentError> {
        assignment
            .validate()
            .map_err(|_| AssignmentError::Invalid)?;
        let mut state = self
            .shared
            .state
            .lock()
            .expect("assignment registry lock poisoned");

        loop {
            // A newer installer may publish while this caller waits. Validate
            // under the mutex after *every* wake before publishing anything.
            if let Some(current) = state.assignments.get(&assignment.camera_id) {
                if assignment.fencing_generation < current.fencing_generation
                    || (assignment.fencing_generation == current.fencing_generation
                        && !same_owner(&assignment, current))
                {
                    return Err(AssignmentError::StaleFencingGeneration);
                }
            }

            let replacement = state
                .assignments
                .get(&assignment.camera_id)
                .is_some_and(|current| !same_owner(current, &assignment));
            if !replacement || !state.active.contains_key(&assignment.camera_id) {
                state
                    .assignments
                    .insert(assignment.camera_id.clone(), assignment);
                return Ok(());
            }

            if let Some(active) = state.active.get_mut(&assignment.camera_id) {
                active.revoked = true;
            }
            let (next_state, timeout) = self
                .shared
                .owner_released
                .wait_timeout(state, REPLACEMENT_WAIT)
                .expect("assignment registry lock poisoned");
            state = next_state;
            if timeout.timed_out() && state.active.contains_key(&assignment.camera_id) {
                return Err(AssignmentError::TeardownTimeout);
            }
        }
    }

    fn acquire(
        &self,
        assignment: &CameraAssignment,
        now: OffsetDateTime,
    ) -> Result<Box<dyn ActiveAssignmentLease>, AssignmentError> {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("assignment registry lock poisoned");
        current_permits(&state.assignments, assignment, now)?;
        if state.active.contains_key(&assignment.camera_id) {
            return Err(AssignmentError::NotCurrent);
        }
        state.active.insert(
            assignment.camera_id.clone(),
            ActiveOwner {
                assignment: assignment.clone(),
                revoked: false,
            },
        );
        Ok(Box::new(LocalActiveAssignmentLease {
            shared: Arc::clone(&self.shared),
            camera_id: assignment.camera_id.clone(),
            assignment: assignment.clone(),
        }))
    }
}
