//! Versioned, transport-neutral contracts between the control plane and media nodes.
//!
//! These contracts deliberately carry compact control metadata only. Camera source
//! URLs, credentials, raw frames, and other media payloads do not belong here.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub const CONTROL_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid control-plane contract: {message}")]
pub struct ControlContractValidationError {
    message: String,
}

impl ControlContractValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Stable identity of a deployed media node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeIdentity {
    pub node_id: String,
    pub site_id: String,
    pub display_name: String,
}

impl NodeIdentity {
    pub fn validate(&self) -> Result<(), ControlContractValidationError> {
        require_nonempty(&self.node_id, "node_id")?;
        require_nonempty(&self.site_id, "site_id")
    }
}

/// Scheduler-relevant capacity advertised by a media node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCapabilities {
    pub max_camera_assignments: u32,
    pub available_storage_bytes: u64,
    pub available_ingress_bps: u64,
    pub hardware_decode_slots: Option<u32>,
}

impl NodeCapabilities {
    pub fn validate(&self) -> Result<(), ControlContractValidationError> {
        if self.max_camera_assignments == 0 {
            return Err(ControlContractValidationError::new(
                "max_camera_assignments must be nonzero",
            ));
        }
        Ok(())
    }
}

/// A media node's versioned request to join the control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRegistrationRequest {
    pub protocol_version: u16,
    pub node: NodeIdentity,
    pub capabilities: NodeCapabilities,
}

impl NodeRegistrationRequest {
    pub fn validate(&self) -> Result<(), ControlContractValidationError> {
        require_supported_protocol(self.protocol_version)?;
        self.node.validate()?;
        self.capabilities.validate()
    }
}

/// The control plane's versioned decision on a node registration request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRegistrationResponse {
    pub protocol_version: u16,
    pub accepted: bool,
    pub error_message: Option<String>,
}

impl NodeRegistrationResponse {
    pub fn validate(&self) -> Result<(), ControlContractValidationError> {
        require_supported_protocol(self.protocol_version)?;
        match (self.accepted, self.error_message.as_deref()) {
            (true, None) => Ok(()),
            (true, Some(_)) => Err(ControlContractValidationError::new(
                "accepted registration must not include an error_message",
            )),
            (false, Some(error_message)) => require_nonempty(error_message, "error_message"),
            (false, None) => Err(ControlContractValidationError::new(
                "rejected registration requires an error_message",
            )),
        }
    }
}

/// A versioned instruction for the intended owner and recording state of a camera.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraDesiredState {
    pub protocol_version: u16,
    pub camera_id: String,
    pub desired_node_id: String,
    pub recording_enabled: bool,
}

impl CameraDesiredState {
    pub fn validate(&self) -> Result<(), ControlContractValidationError> {
        require_supported_protocol(self.protocol_version)?;
        require_nonempty(&self.camera_id, "camera_id")?;
        require_nonempty(&self.desired_node_id, "desired_node_id")
    }
}

/// Opaque, time-bound authority for one media-node assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub token: String,
    pub issued_at_utc: String,
    pub expires_at_utc: String,
    pub duration_seconds: u64,
}

impl Lease {
    pub fn validate(&self) -> Result<(), ControlContractValidationError> {
        require_nonempty(&self.token, "lease token")?;
        let issued_at = require_utc_timestamp(&self.issued_at_utc, "issued_at_utc")?;
        let expires_at = require_utc_timestamp(&self.expires_at_utc, "expires_at_utc")?;
        if self.duration_seconds == 0 {
            return Err(ControlContractValidationError::new(
                "lease duration_seconds must be nonzero",
            ));
        }
        if expires_at <= issued_at {
            return Err(ControlContractValidationError::new(
                "expires_at_utc must be after issued_at_utc",
            ));
        }
        let declared_duration = i64::try_from(self.duration_seconds).map_err(|_| {
            ControlContractValidationError::new("lease duration_seconds is too large")
        })?;
        if expires_at - issued_at != time::Duration::seconds(declared_duration) {
            return Err(ControlContractValidationError::new(
                "lease timestamps must agree with duration_seconds",
            ));
        }
        Ok(())
    }
}

/// A fenced instruction for a node to own one camera pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraAssignment {
    pub protocol_version: u16,
    pub assignment_id: String,
    pub camera_id: String,
    pub node_id: String,
    pub lease: Lease,
    pub fencing_generation: u64,
    pub assigned_at_utc: String,
}

impl CameraAssignment {
    pub fn validate(&self) -> Result<(), ControlContractValidationError> {
        require_supported_protocol(self.protocol_version)?;
        require_nonempty(&self.assignment_id, "assignment_id")?;
        require_nonempty(&self.camera_id, "camera_id")?;
        require_nonempty(&self.node_id, "node_id")?;
        self.lease.validate()?;
        if self.fencing_generation == 0 {
            return Err(ControlContractValidationError::new(
                "fencing_generation must be nonzero",
            ));
        }
        require_utc_timestamp(&self.assigned_at_utc, "assigned_at_utc").map(|_| ())
    }
}

/// A point-in-time liveness and capacity report from a media node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeHeartbeat {
    pub protocol_version: u16,
    pub node: NodeIdentity,
    pub observed_at_utc: String,
    pub active_assignments: u32,
    pub capabilities: NodeCapabilities,
}

impl NodeHeartbeat {
    pub fn validate(&self) -> Result<(), ControlContractValidationError> {
        require_supported_protocol(self.protocol_version)?;
        self.node.validate()?;
        require_utc_timestamp(&self.observed_at_utc, "observed_at_utc")?;
        self.capabilities.validate()?;
        if self.active_assignments > self.capabilities.max_camera_assignments {
            return Err(ControlContractValidationError::new(
                "active_assignments exceeds max_camera_assignments",
            ));
        }
        Ok(())
    }
}

fn require_supported_protocol(version: u16) -> Result<(), ControlContractValidationError> {
    if version == CONTROL_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ControlContractValidationError::new(format!(
            "unsupported protocol version {version}",
        )))
    }
}

fn require_nonempty(value: &str, field: &str) -> Result<(), ControlContractValidationError> {
    if value.trim().is_empty() {
        Err(ControlContractValidationError::new(format!(
            "{field} must not be empty",
        )))
    } else {
        Ok(())
    }
}

fn require_utc_timestamp(
    value: &str,
    field: &str,
) -> Result<OffsetDateTime, ControlContractValidationError> {
    let timestamp = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
        ControlContractValidationError::new(format!("{field} must be a valid RFC3339 timestamp"))
    })?;
    if !timestamp.offset().is_utc() {
        return Err(ControlContractValidationError::new(format!(
            "{field} must use a zero UTC offset",
        )));
    }
    Ok(timestamp)
}
