//! SQLite persistence for the hosted control plane.
//!
//! Two rules hold for every statement in this file.
//!
//! 1. **Tenant scoping.** Every read and every write of tenant-owned data
//!    carries `tenant_id` in its `WHERE` clause, including lookups by a primary
//!    key that is already unique. A node id or enrollment id learned from
//!    another tenant therefore resolves to nothing rather than to somebody
//!    else's row.
//! 2. **No local secrets.** The schema has no column for an RTSP or ONVIF URL,
//!    a camera credential, a filesystem path or a local IPC endpoint, and the
//!    enrollment proof is stored only as a hash. There is nothing here to leak.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};

/// Bumped whenever the schema changes; reported by the health endpoint.
pub const SCHEMA_VERSION: i64 = 3;

/// Matches `MAX_REPORTED_CAMERAS` in the node's cloud inventory module.
pub const MAX_CAMERAS: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("database lock poisoned")]
    Poisoned,
    #[error("unsupported schema version {found}; this build expects {SCHEMA_VERSION}")]
    Schema { found: i64 },
}

type Result<T> = std::result::Result<T, StoreError>;

/// Lifecycle of an enrollment. The proof is usable only in `Pending`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnrollmentState {
    Pending,
    Claimed,
    Consumed,
}

impl EnrollmentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Consumed => "consumed",
        }
    }
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "claimed" => Some(Self::Claimed),
            "consumed" => Some(Self::Consumed),
            _ => None,
        }
    }
}

/// A media session joined with its node's state and its tenant's policy:
/// node, camera, representation, session expiry, revoked at, node state,
/// policy tenant, live enabled, main allowed, max bitrate (kbps).
type SessionPolicyRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    bool,
    bool,
    u32,
);

/// One media audit entry: id, principal, event, reason, session, created at.
pub type MediaAuditRow = (
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
);

#[derive(Clone, Debug)]
pub struct EnrollmentRow {
    pub id: String,
    pub tenant_id: String,
    pub proof_hash: String,
    pub challenge: String,
    pub expires_at: String,
    pub state: EnrollmentState,
    pub claim_id: Option<String>,
    pub node_id: Option<String>,
    pub public_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NewEnrollment {
    pub tenant_id: String,
    pub label: String,
    pub proof_hash: String,
    pub challenge: String,
    pub expires_at: String,
}

/// How finishing an enrollment ended. Only `Registered` changed anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// The enrollment was consumed and the node is now registered.
    Registered,
    /// The enrollment was not awaiting confirmation — already finished, never
    /// claimed, or claimed by a different claim id.
    NotPending,
    /// This tenant already has a node with that id, registered through some
    /// other enrollment.
    NodeIdTaken,
}

#[derive(Clone, Debug)]
pub struct ClaimRecord {
    pub tenant_id: String,
    pub enrollment_id: String,
    pub claim_id: String,
    pub node_id: String,
    pub public_key: String,
    pub display_label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRow {
    pub id: String,
    pub tenant_id: String,
    pub display_label: String,
    pub public_key: String,
    pub auth_epoch: u64,
    pub linked: bool,
    pub last_check_in: Option<String>,
}

/// A challenge about to be issued, bound to the one node, tenant, operation and
/// epoch that may spend it.
#[derive(Clone, Debug)]
pub struct NewChallenge {
    pub tenant_id: String,
    pub node_id: String,
    pub operation: String,
    pub auth_epoch: u64,
    pub challenge: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraRecord {
    pub camera_id: String,
    pub name: String,
    pub status: String,
    pub recording: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthReport {
    pub schema_version: i64,
    pub tenants: i64,
    pub linked_nodes: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaSessionCreateOutcome {
    Created { bitrate_kbps: u32 },
    ConcurrentLimit,
    CameraUnavailable,
    PolicyMissing,
    CameraNotLiveEnabled,
    RepresentationNotPermitted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPublishDirective {
    pub session_id: String,
    pub grant_id: String,
    pub camera_id: String,
    pub representation: String,
    pub grant_expires_at: String,
    pub max_bitrate_kbps: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenewedMediaSession {
    pub node_id: String,
    pub camera_id: String,
    pub representation: String,
    pub session_expires_at: String,
    pub grant_expires_at: String,
    pub max_bitrate_kbps: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaSessionRenewOutcome {
    Renewed(RenewedMediaSession),
    RevokedOrExpired,
    PolicyDenied,
    NodeUnavailable,
}

/// A handle to the control-plane database. Cheap to clone; every method is
/// synchronous and is expected to be called from a blocking context.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl Store {
    /// Open (creating if needed) the database at `path` and bring the schema up
    /// to date.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    StoreError::Database(rusqlite::Error::InvalidPath(
                        format!("{}: {error}", parent.display()).into(),
                    ))
                })?;
            }
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| StoreError::Poisoned)
    }

    fn migrate(&self) -> Result<()> {
        let connection = self.lock()?;
        let found: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::Schema { found });
        }
        if found == 0 {
            connection.execute_batch(&format!(
                "BEGIN IMMEDIATE; {SCHEMA_V1} PRAGMA user_version={SCHEMA_VERSION}; COMMIT;"
            ))?;
        } else if found == 1 {
            connection.execute_batch(&format!(
                "BEGIN IMMEDIATE; {SCHEMA_V2_MEDIA} {SCHEMA_V3_DIRECTIVES} PRAGMA user_version={SCHEMA_VERSION}; COMMIT;"
            ))?;
        } else if found == 2 {
            connection.execute_batch(&format!(
                "BEGIN IMMEDIATE; {SCHEMA_V3_DIRECTIVES} PRAGMA user_version={SCHEMA_VERSION}; COMMIT;"
            ))?;
        }
        Ok(())
    }

    /// A real query, not a constant: the endpoint is only healthy if the
    /// database answers.
    pub fn health(&self) -> Result<HealthReport> {
        let connection = self.lock()?;
        let schema_version: i64 =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let tenants: i64 =
            connection.query_row("SELECT count(*) FROM tenants", [], |row| row.get(0))?;
        let linked_nodes: i64 = connection.query_row(
            "SELECT count(*) FROM nodes WHERE state = 'linked'",
            [],
            |row| row.get(0),
        )?;
        Ok(HealthReport {
            schema_version,
            tenants,
            linked_nodes,
        })
    }

    pub fn create_tenant(&self, id: &str, name: &str, now: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO tenants (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![id, name, now],
        )?;
        Ok(())
    }

    pub fn tenant_exists(&self, tenant_id: &str) -> Result<bool> {
        let connection = self.lock()?;
        Ok(connection
            .query_row(
                "SELECT 1 FROM tenants WHERE id = ?1",
                params![tenant_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn create_enrollment(&self, id: &str, new: &NewEnrollment, now: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO node_enrollments \
             (id, tenant_id, label, proof_hash, challenge, expires_at, state, created_at) \
             SELECT ?1, id, ?3, ?4, ?5, ?6, 'pending', ?7 FROM tenants WHERE id = ?2",
            params![
                id,
                new.tenant_id,
                new.label,
                new.proof_hash,
                new.challenge,
                new.expires_at,
                now
            ],
        )?;
        Ok(())
    }

    /// Tenant-scoped: an enrollment id belonging to another tenant is `None`.
    pub fn enrollment(
        &self,
        tenant_id: &str,
        enrollment_id: &str,
    ) -> Result<Option<EnrollmentRow>> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT id, tenant_id, proof_hash, challenge, expires_at, state, claim_id, node_id, public_key \
                 FROM node_enrollments WHERE tenant_id = ?1 AND id = ?2",
                params![tenant_id, enrollment_id],
                |row| {
                    let state: String = row.get(5)?;
                    Ok(EnrollmentRow {
                        id: row.get(0)?,
                        tenant_id: row.get(1)?,
                        proof_hash: row.get(2)?,
                        challenge: row.get(3)?,
                        expires_at: row.get(4)?,
                        state: EnrollmentState::parse(&state).unwrap_or(EnrollmentState::Consumed),
                        claim_id: row.get(6)?,
                        node_id: row.get(7)?,
                        public_key: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Consume the one-time proof by moving `pending -> claimed` in a single
    /// conditional write. `Ok(false)` means somebody else got there first, so
    /// two concurrent claims can never both succeed.
    pub fn claim_enrollment(&self, claim: &ClaimRecord, now: &str) -> Result<bool> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE node_enrollments \
             SET state = 'claimed', claim_id = ?3, node_id = ?4, public_key = ?5, \
                 display_label = ?6, claimed_at = ?7 \
             WHERE tenant_id = ?1 AND id = ?2 AND state = 'pending'",
            params![
                claim.tenant_id,
                claim.enrollment_id,
                claim.claim_id,
                claim.node_id,
                claim.public_key,
                claim.display_label,
                now
            ],
        )?;
        Ok(changed == 1)
    }

    /// Finish the enrollment and register the node, atomically.
    ///
    /// Anything other than [`ConfirmOutcome::Registered`] leaves the database
    /// exactly as it was, so a replayed confirmation cannot register a second
    /// node and a colliding node id cannot disturb the node already holding it.
    pub fn confirm_enrollment(
        &self,
        tenant_id: &str,
        enrollment_id: &str,
        claim_id: &str,
        now: &str,
    ) -> Result<ConfirmOutcome> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE node_enrollments SET state = 'consumed', consumed_at = ?4 \
             WHERE tenant_id = ?1 AND id = ?2 AND claim_id = ?3 AND state = 'claimed'",
            params![tenant_id, enrollment_id, claim_id, now],
        )?;
        if changed != 1 {
            transaction.rollback()?;
            return Ok(ConfirmOutcome::NotPending);
        }
        // The node id is chosen by whoever holds the enrollment code, so it can
        // collide with one this tenant already registered. Detect that here:
        // letting the primary key raise it would be an opaque storage fault,
        // and the node already registered must not be disturbed either way.
        let taken: bool = transaction.query_row(
            "SELECT EXISTS( \
               SELECT 1 FROM nodes WHERE tenant_id = ?1 AND id = ( \
                 SELECT node_id FROM node_enrollments WHERE tenant_id = ?1 AND id = ?2))",
            params![tenant_id, enrollment_id],
            |row| row.get(0),
        )?;
        if taken {
            transaction.rollback()?;
            return Ok(ConfirmOutcome::NodeIdTaken);
        }
        transaction.execute(
            "INSERT INTO nodes \
             (id, tenant_id, enrollment_id, public_key, display_label, auth_epoch, state, linked_at) \
             SELECT node_id, tenant_id, id, public_key, display_label, 1, 'linked', ?3 \
             FROM node_enrollments WHERE tenant_id = ?1 AND id = ?2",
            params![tenant_id, enrollment_id, now],
        )?;
        transaction.commit()?;
        Ok(ConfirmOutcome::Registered)
    }

    /// Tenant-scoped node lookup. A node id from another tenant is `None`.
    pub fn node(&self, tenant_id: &str, node_id: &str) -> Result<Option<NodeRow>> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT id, tenant_id, display_label, public_key, auth_epoch, state, last_check_in \
                 FROM nodes WHERE tenant_id = ?1 AND id = ?2",
                params![tenant_id, node_id],
                |row| {
                    let epoch: i64 = row.get(4)?;
                    let state: String = row.get(5)?;
                    Ok(NodeRow {
                        id: row.get(0)?,
                        tenant_id: row.get(1)?,
                        display_label: row.get(2)?,
                        public_key: row.get(3)?,
                        auth_epoch: epoch.max(0) as u64,
                        linked: state == "linked",
                        last_check_in: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn nodes(&self, tenant_id: &str) -> Result<Vec<NodeRow>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, tenant_id, display_label, public_key, auth_epoch, state, last_check_in \
             FROM nodes WHERE tenant_id = ?1 ORDER BY linked_at, id",
        )?;
        let rows = statement
            .query_map(params![tenant_id], |row| {
                let epoch: i64 = row.get(4)?;
                let state: String = row.get(5)?;
                Ok(NodeRow {
                    id: row.get(0)?,
                    tenant_id: row.get(1)?,
                    display_label: row.get(2)?,
                    public_key: row.get(3)?,
                    auth_epoch: epoch.max(0) as u64,
                    linked: state == "linked",
                    last_check_in: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Record a request nonce. `Ok(false)` means it was already used, which is
    /// how a replayed challenge request is refused.
    pub fn consume_request_nonce(
        &self,
        tenant_id: &str,
        node_id: &str,
        nonce: &str,
        expires_at: &str,
        now: &str,
    ) -> Result<bool> {
        let connection = self.lock()?;
        connection.execute(
            "DELETE FROM node_request_nonces WHERE expires_at <= ?1",
            params![now],
        )?;
        let inserted = connection.execute(
            "INSERT OR IGNORE INTO node_request_nonces (tenant_id, node_id, nonce, expires_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![tenant_id, node_id, nonce, expires_at],
        )?;
        Ok(inserted == 1)
    }

    pub fn issue_challenge(&self, new: &NewChallenge, now: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "DELETE FROM node_challenges WHERE expires_at <= ?1",
            params![now],
        )?;
        connection.execute(
            "INSERT INTO node_challenges \
             (challenge, tenant_id, node_id, operation, auth_epoch, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                new.challenge,
                new.tenant_id,
                new.node_id,
                new.operation,
                new.auth_epoch as i64,
                new.expires_at
            ],
        )?;
        Ok(())
    }

    /// Spend a challenge exactly once, for the tenant, node, operation and
    /// epoch it was issued to. `Ok(false)` covers unknown, expired, already
    /// spent and belonging-to-someone-else alike.
    pub fn consume_challenge(
        &self,
        tenant_id: &str,
        node_id: &str,
        operation: &str,
        auth_epoch: u64,
        challenge: &str,
        now: &str,
    ) -> Result<bool> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE node_challenges SET consumed_at = ?6 \
             WHERE challenge = ?5 AND tenant_id = ?1 AND node_id = ?2 AND operation = ?3 \
               AND auth_epoch = ?4 AND consumed_at IS NULL AND expires_at > ?6",
            params![
                tenant_id,
                node_id,
                operation,
                auth_epoch as i64,
                challenge,
                now
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn record_check_in(&self, tenant_id: &str, node_id: &str, at: &str) -> Result<bool> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE nodes SET last_check_in = ?3 WHERE tenant_id = ?1 AND id = ?2 AND state = 'linked'",
            params![tenant_id, node_id, at],
        )?;
        Ok(changed == 1)
    }

    /// Replace a node's camera inventory. `Ok(false)` means a newer snapshot is
    /// already stored and this one was deliberately kept out.
    pub fn replace_inventory(
        &self,
        tenant_id: &str,
        node_id: &str,
        issued_at: &str,
        cameras: &[CameraRecord],
        now: &str,
    ) -> Result<bool> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let newer: Option<String> = transaction
            .query_row(
                "SELECT inventory_issued_at FROM nodes \
                 WHERE tenant_id = ?1 AND id = ?2 AND state = 'linked'",
                params![tenant_id, node_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if newer.as_deref().is_some_and(|stored| stored > issued_at) {
            transaction.rollback()?;
            return Ok(false);
        }
        transaction.execute(
            "DELETE FROM cameras WHERE tenant_id = ?1 AND node_id = ?2",
            params![tenant_id, node_id],
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO cameras \
                 (tenant_id, node_id, camera_id, name, status, recording, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for camera in cameras {
                insert.execute(params![
                    tenant_id,
                    node_id,
                    camera.camera_id,
                    camera.name,
                    camera.status,
                    camera.recording as i64,
                    now
                ])?;
            }
        }
        transaction.execute(
            "UPDATE nodes SET inventory_issued_at = ?3 \
             WHERE tenant_id = ?1 AND id = ?2 AND state = 'linked'",
            params![tenant_id, node_id, issued_at],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn cameras(&self, tenant_id: &str, node_id: &str) -> Result<Vec<CameraRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT camera_id, name, status, recording FROM cameras \
             WHERE tenant_id = ?1 AND node_id = ?2 ORDER BY camera_id",
        )?;
        let rows = statement
            .query_map(params![tenant_id, node_id], |row| {
                let recording: i64 = row.get(3)?;
                Ok(CameraRecord {
                    camera_id: row.get(0)?,
                    name: row.get(1)?,
                    status: row.get(2)?,
                    recording: recording != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn camera_exists(&self, tenant_id: &str, node_id: &str, camera_id: &str) -> Result<bool> {
        let connection = self.lock()?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM cameras c JOIN nodes n ON n.tenant_id=c.tenant_id AND n.id=c.node_id WHERE c.tenant_id=?1 AND c.node_id=?2 AND c.camera_id=?3 AND n.state='linked')",
            params![tenant_id, node_id, camera_id], |row| row.get(0))?)
    }

    pub fn upsert_media_policy(
        &self,
        tenant_id: &str,
        live: bool,
        main: bool,
        max_sessions: u32,
        bitrate: u32,
        now: &str,
    ) -> Result<bool> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "INSERT INTO media_policies (tenant_id,live_enabled,allow_main,max_sessions,max_bitrate_kbps,updated_at) SELECT id,?2,?3,?4,?5,?6 FROM tenants WHERE id=?1 ON CONFLICT(tenant_id) DO UPDATE SET live_enabled=excluded.live_enabled,allow_main=excluded.allow_main,max_sessions=excluded.max_sessions,max_bitrate_kbps=excluded.max_bitrate_kbps,updated_at=excluded.updated_at",
            params![tenant_id, live as i64, main as i64, max_sessions, bitrate, now])?;
        Ok(changed == 1)
    }

    pub fn media_policy(&self, tenant_id: &str) -> Result<Option<(bool, bool, u32, u32)>> {
        let connection = self.lock()?;
        connection.query_row("SELECT live_enabled,allow_main,max_sessions,max_bitrate_kbps FROM media_policies WHERE tenant_id=?1", params![tenant_id], |r| Ok((r.get::<_,i64>(0)? != 0,r.get::<_,i64>(1)? != 0,r.get(2)?,r.get(3)?))).optional().map_err(Into::into)
    }

    #[allow(clippy::too_many_arguments)] // one argument per stored column
    pub fn create_media_session(
        &self,
        tenant_id: &str,
        principal_id: &str,
        session_id: &str,
        grant_id: &str,
        node_id: &str,
        camera_id: &str,
        representation: &str,
        expires_at: &str,
        session_expires_at: &str,
        now: &str,
    ) -> Result<MediaSessionCreateOutcome> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let camera_available: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM cameras c JOIN nodes n ON n.tenant_id=c.tenant_id AND n.id=c.node_id WHERE c.tenant_id=?1 AND c.node_id=?2 AND c.camera_id=?3 AND n.state='linked')",
            params![tenant_id, node_id, camera_id],
            |row| row.get(0),
        )?;
        if !camera_available {
            tx.rollback()?;
            return Ok(MediaSessionCreateOutcome::CameraUnavailable);
        }
        let policy: Option<(bool, bool, u32, u32)> = tx
            .query_row(
                "SELECT live_enabled,allow_main,max_sessions,max_bitrate_kbps FROM media_policies WHERE tenant_id=?1",
                params![tenant_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? != 0,
                        row.get::<_, i64>(1)? != 0,
                        row.get(2)?,
                        row.get(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((live, allow_main, limit, bitrate)) = policy else {
            tx.rollback()?;
            return Ok(MediaSessionCreateOutcome::PolicyMissing);
        };
        if !live {
            tx.rollback()?;
            return Ok(MediaSessionCreateOutcome::CameraNotLiveEnabled);
        }
        if representation == "main" && !allow_main {
            tx.rollback()?;
            return Ok(MediaSessionCreateOutcome::RepresentationNotPermitted);
        }
        let incompatible_representation: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM media_sessions \
             WHERE tenant_id=?1 AND node_id=?2 AND camera_id=?3 \
               AND revoked_at IS NULL AND session_expires_at>?4 AND representation<>?5)",
            params![tenant_id, node_id, camera_id, now, representation],
            |row| row.get(0),
        )?;
        if incompatible_representation {
            tx.rollback()?;
            return Ok(MediaSessionCreateOutcome::RepresentationNotPermitted);
        }
        let count: u32 = tx.query_row(
            "SELECT count(*) FROM media_sessions WHERE tenant_id=?1 AND revoked_at IS NULL AND session_expires_at > ?2",
            params![tenant_id, now],
            |r| r.get(0),
        )?;
        if count >= limit {
            tx.rollback()?;
            return Ok(MediaSessionCreateOutcome::ConcurrentLimit);
        }
        let publish_grant_id = uuid::Uuid::new_v4().to_string();
        tx.execute("INSERT INTO media_sessions (session_id,grant_id,publish_grant_id,tenant_id,principal_id,node_id,camera_id,representation,grant_expires_at,session_expires_at,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)", params![session_id,grant_id,publish_grant_id,tenant_id,principal_id,node_id,camera_id,representation,expires_at,session_expires_at,now])?;
        tx.execute(
            "INSERT INTO media_audit (tenant_id,principal_id,event,reason,session_id,created_at) VALUES (?1,?2,'session_opened',NULL,?3,?4)",
            params![tenant_id, principal_id, session_id, now],
        )?;
        tx.execute(
            "INSERT INTO media_audit (tenant_id,principal_id,event,reason,session_id,created_at) VALUES (?1,?2,'grant_issued',NULL,?3,?4)",
            params![tenant_id, principal_id, session_id, now],
        )?;
        tx.commit()?;
        Ok(MediaSessionCreateOutcome::Created {
            bitrate_kbps: bitrate,
        })
    }

    /// Atomically acknowledge exact prior generations and return every
    /// publication obligation this authenticated node still owes.
    ///
    /// Delivery is at-least-once and the *pull* is not the spend: an obligation
    /// stays outstanding, and is handed out again on every pull, until the node
    /// settles it one of two ways — by acknowledging the exact `directive_id` it
    /// received, or by spending that directive's publish grant at the ingest
    /// edge. A node whose response was lost therefore retries rather than losing
    /// the work, and a node that published successfully but whose
    /// acknowledgement was lost is not told to publish the same camera again.
    ///
    /// Each redelivery re-signs the *same* publish grant instead of minting a
    /// new one, so however many copies reach the node, exactly one is spendable:
    /// at-least-once delivery, at-most-once authority.
    pub fn take_publish_directives(
        &self,
        tenant_id: &str,
        node_id: &str,
        now: &str,
        limit: usize,
        acknowledged_directive_ids: &[String],
    ) -> Result<Vec<PendingPublishDirective>> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for directive_id in acknowledged_directive_ids {
            tx.execute(
                "UPDATE media_sessions SET directive_delivered_at=?4 \
                 WHERE tenant_id=?1 AND node_id=?2 AND publish_grant_id=?3 \
                   AND directive_delivered_at IS NULL",
                params![tenant_id, node_id, directive_id, now],
            )?;
        }
        let mut statement = tx.prepare(
            "SELECT s.session_id,s.publish_grant_id,s.camera_id,s.representation,\
                    s.grant_expires_at,p.max_bitrate_kbps \
             FROM media_sessions s \
             JOIN nodes n ON n.tenant_id=s.tenant_id AND n.id=s.node_id \
             JOIN media_policies p ON p.tenant_id=s.tenant_id \
             WHERE s.tenant_id=?1 AND s.node_id=?2 AND n.state='linked' \
               AND s.revoked_at IS NULL AND s.directive_delivered_at IS NULL \
               AND s.publish_consumed_at IS NULL \
               AND s.grant_expires_at>?3 AND s.session_expires_at>?3 \
             ORDER BY s.created_at,s.session_id LIMIT ?4",
        )?;
        let rows = statement
            .query_map(params![tenant_id, node_id, now, limit as i64], |row| {
                Ok(PendingPublishDirective {
                    session_id: row.get(0)?,
                    grant_id: row.get(1)?,
                    camera_id: row.get(2)?,
                    representation: row.get(3)?,
                    grant_expires_at: row.get(4)?,
                    max_bitrate_kbps: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        tx.commit()?;
        Ok(rows)
    }

    /// Re-grant a live session, or end it.
    ///
    /// `earliest_usable_expiry` is the instant a renewal must still reach to be
    /// worth issuing (`now + MIN_RENEWAL_WINDOW_SECONDS`). A session that cannot
    /// reach it, and one the tenant's policy no longer covers, are both settled
    /// verdicts rather than transient failures: the session is revoked here so
    /// that its concurrency slot returns to the tenant immediately instead of
    /// being held, unrenewable, until it times out.
    #[allow(clippy::too_many_arguments)]
    pub fn renew_media_session(
        &self,
        tenant_id: &str,
        principal_id: &str,
        session_id: &str,
        viewer_grant_id: &str,
        publish_grant_id: &str,
        grant_expires_at: &str,
        earliest_usable_expiry: &str,
        now: &str,
        principal_live_allowed: bool,
        principal_main_allowed: bool,
    ) -> Result<MediaSessionRenewOutcome> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<SessionPolicyRow> = tx
            .query_row(
                "SELECT s.node_id,s.camera_id,s.representation,s.session_expires_at,s.revoked_at,n.state,\
                        p.tenant_id,p.live_enabled,p.allow_main,p.max_bitrate_kbps \
                 FROM media_sessions s \
                 LEFT JOIN nodes n ON n.tenant_id=s.tenant_id AND n.id=s.node_id \
                 LEFT JOIN media_policies p ON p.tenant_id=s.tenant_id \
                 WHERE s.tenant_id=?1 AND s.principal_id=?2 AND s.session_id=?3",
                params![tenant_id, principal_id, session_id],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get::<_,i64>(7)? != 0,r.get::<_,i64>(8)? != 0,r.get(9)?)),
            )
            .optional()?;
        let outcome = match row {
            None => MediaSessionRenewOutcome::RevokedOrExpired,
            Some((_, _, _, expires, Some(_), _, _, _, _, _)) if expires.as_str() > now => {
                MediaSessionRenewOutcome::RevokedOrExpired
            }
            Some((_, _, _, expires, _, _, _, _, _, _)) if expires.as_str() <= now => {
                MediaSessionRenewOutcome::RevokedOrExpired
            }
            // In the future, but too near its end for a renewal to be worth
            // issuing: every credential a renewal hands out is clamped to the
            // session end, so what is left here is unusably short. End the
            // session rather than re-granting it, which also hands its slot back
            // now instead of when it would have timed out.
            Some((_, _, _, expires, _, _, _, _, _, _))
                if expires.as_str() < earliest_usable_expiry =>
            {
                end_session_in(
                    &tx,
                    tenant_id,
                    principal_id,
                    session_id,
                    "renewal_window_too_short",
                    now,
                )?;
                MediaSessionRenewOutcome::RevokedOrExpired
            }
            Some((_, _, _, _, _, state, _, _, _, _)) if state != "linked" => {
                MediaSessionRenewOutcome::NodeUnavailable
            }
            // The tenant's policy, or this principal's permissions, no longer
            // cover the session that is already running. That verdict cannot
            // reverse itself on a later renewal, so the session is ended rather
            // than left alive holding a slot nobody can use.
            Some((_, _, representation, _, _, _, _, live, allow_main, _))
                if !live
                    || !principal_live_allowed
                    || (representation == "main" && (!allow_main || !principal_main_allowed)) =>
            {
                end_session_in(
                    &tx,
                    tenant_id,
                    principal_id,
                    session_id,
                    "renewal_outside_policy",
                    now,
                )?;
                MediaSessionRenewOutcome::PolicyDenied
            }
            Some((
                node_id,
                camera_id,
                representation,
                session_expires_at,
                _,
                _,
                _,
                _,
                _,
                bitrate,
            )) => {
                // The single place a renewal's lifetime is decided:
                // `min(now + bounded grant TTL, session end)`. Every credential
                // the route hands out is derived from this one value, so none of
                // them can outlive the session.
                let bounded_grant_expires_at = if grant_expires_at < session_expires_at.as_str() {
                    grant_expires_at.to_owned()
                } else {
                    session_expires_at.clone()
                };
                // Subsumed by the floor arm above for any caller that passes a
                // future `grant_expires_at`; kept so that the invariant "a grant
                // is never issued already expired" holds here too, rather than
                // depending on the caller.
                if bounded_grant_expires_at.as_str() <= now {
                    tx.rollback()?;
                    return Ok(MediaSessionRenewOutcome::RevokedOrExpired);
                }
                let incompatible: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM media_sessions WHERE tenant_id=?1 AND node_id=?2 \
                     AND camera_id=?3 AND session_id<>?4 AND revoked_at IS NULL \
                     AND session_expires_at>?5 AND representation<>?6)",
                    params![
                        tenant_id,
                        node_id,
                        camera_id,
                        session_id,
                        now,
                        representation
                    ],
                    |row| row.get(0),
                )?;
                if incompatible {
                    tx.rollback()?;
                    return Ok(MediaSessionRenewOutcome::PolicyDenied);
                }
                tx.execute(
                    "UPDATE media_sessions SET grant_id=?4,publish_grant_id=?5,grant_expires_at=?6,\
                     consumed_at=NULL,publish_consumed_at=NULL,directive_delivered_at=NULL \
                     WHERE tenant_id=?1 AND principal_id=?2 AND session_id=?3 AND revoked_at IS NULL",
                    params![tenant_id,principal_id,session_id,viewer_grant_id,publish_grant_id,bounded_grant_expires_at],
                )?;
                tx.execute(
                    "INSERT INTO media_audit (tenant_id,principal_id,event,reason,session_id,created_at) VALUES (?1,?2,'grant_renewed',NULL,?3,?4)",
                    params![tenant_id,principal_id,session_id,now],
                )?;
                MediaSessionRenewOutcome::Renewed(RenewedMediaSession {
                    node_id,
                    camera_id,
                    representation,
                    session_expires_at,
                    grant_expires_at: bounded_grant_expires_at,
                    max_bitrate_kbps: bitrate,
                })
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    pub fn active_publish_sessions(
        &self,
        tenant_id: &str,
        node_id: &str,
        now: &str,
    ) -> Result<Vec<String>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT s.session_id FROM media_sessions s \
             JOIN nodes n ON n.tenant_id=s.tenant_id AND n.id=s.node_id \
             WHERE s.tenant_id=?1 AND s.node_id=?2 AND n.state='linked' \
               AND s.revoked_at IS NULL AND s.grant_expires_at>?3 \
               AND s.session_expires_at>?3 ORDER BY s.session_id",
        )?;
        let sessions = statement
            .query_map(params![tenant_id, node_id, now], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(sessions)
    }

    pub fn revoke_media_session(
        &self,
        tenant_id: &str,
        principal_id: &str,
        session_id: &str,
        now: &str,
    ) -> Result<bool> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let changed = tx.execute("UPDATE media_sessions SET revoked_at=?4 WHERE tenant_id=?1 AND principal_id=?2 AND session_id=?3 AND revoked_at IS NULL", params![tenant_id,principal_id,session_id,now])?;
        if changed == 1 {
            tx.execute(
                "INSERT INTO media_audit (tenant_id,principal_id,event,reason,session_id,created_at) VALUES (?1,?2,'session_revoked',NULL,?3,?4)",
                params![tenant_id, principal_id, session_id, now],
            )?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Atomically spends either the viewer or node grant independently.
    /// 0=spent, 1=unknown, 2=revoked, 3=replay, 4=expired.
    pub fn consume_media_grant(
        &self,
        tenant_id: &str,
        session_id: &str,
        grant_id: &str,
        now: &str,
    ) -> Result<u8> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let row: Option<(Option<String>,Option<String>,String,bool)> = tx.query_row(
            "SELECT revoked_at,CASE WHEN grant_id=?3 THEN consumed_at ELSE publish_consumed_at END,grant_expires_at,grant_id=?3 FROM media_sessions WHERE tenant_id=?1 AND session_id=?2 AND (grant_id=?3 OR publish_grant_id=?3)",
            params![tenant_id,session_id,grant_id],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
        ).optional()?;
        let outcome = match row {
            None => 1,
            Some((Some(_), _, _, _)) => 2,
            Some((_, Some(_), _, _)) => 3,
            Some((None, None, expires, _)) if expires.as_str() <= now => 4,
            Some((None, None, _, viewer)) => {
                let column = if viewer {
                    "consumed_at"
                } else {
                    "publish_consumed_at"
                };
                tx.execute(
                    &format!("UPDATE media_sessions SET {column}=?4 WHERE tenant_id=?1 AND session_id=?2 AND (grant_id=?3 OR publish_grant_id=?3) AND {column} IS NULL"),
                    params![tenant_id,session_id,grant_id,now],
                )?;
                0
            }
        };
        let reason = match outcome {
            0 => None,
            3 => Some("grant_already_consumed"),
            4 => Some("grant_expired"),
            _ => Some("session_revoked"),
        };
        tx.execute(
            "INSERT INTO media_audit (tenant_id,principal_id,event,reason,session_id,created_at) VALUES (?1,NULL,?2,?3,?4,?5)",
            params![
                tenant_id,
                if outcome == 0 { "grant_consumed" } else { "grant_denied" },
                reason,
                session_id,
                now
            ],
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    pub fn append_media_audit(
        &self,
        tenant_id: &str,
        principal_id: Option<&str>,
        event: &str,
        reason: Option<&str>,
        session_id: Option<&str>,
        now: &str,
    ) -> Result<()> {
        let connection = self.lock()?;
        connection.execute("INSERT INTO media_audit (tenant_id,principal_id,event,reason,session_id,created_at) VALUES (?1,?2,?3,?4,?5,?6)",params![tenant_id,principal_id,event,reason,session_id,now])?;
        Ok(())
    }

    pub fn media_audit(&self, tenant_id: &str) -> Result<Option<Vec<MediaAuditRow>>> {
        if !self.tenant_exists(tenant_id)? {
            return Ok(None);
        }
        let connection = self.lock()?;
        let mut stmt=connection.prepare("SELECT id,principal_id,event,reason,session_id,created_at FROM media_audit WHERE tenant_id=?1 ORDER BY rowid")?;
        let rows = stmt
            .query_map(params![tenant_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Some(rows))
    }

    /// Mark a node unlinked at the node's own request. Its cameras go with it:
    /// an unlinked node has no inventory in the hosted cache.
    ///
    /// Idempotent by design. `Ok(false)` means the node had already retired,
    /// which is a repeat of an outcome that already holds, not a failure — the
    /// node retries an unlink whose answer it never saw. The camera delete runs
    /// either way so a retry also settles a half-finished retirement.
    pub fn unlink_node(&self, tenant_id: &str, node_id: &str, now: &str) -> Result<bool> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE nodes SET state = 'unlinked', unlinked_at = ?3 \
             WHERE tenant_id = ?1 AND id = ?2 AND state = 'linked'",
            params![tenant_id, node_id, now],
        )?;
        transaction.execute(
            "DELETE FROM cameras WHERE tenant_id = ?1 AND node_id = ?2",
            params![tenant_id, node_id],
        )?;
        let sessions = {
            let mut statement = transaction.prepare(
                "SELECT session_id FROM media_sessions WHERE tenant_id = ?1 AND node_id = ?2 AND revoked_at IS NULL",
            )?;
            let sessions = statement
                .query_map(params![tenant_id, node_id], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            sessions
        };
        transaction.execute(
            "UPDATE media_sessions SET revoked_at = ?3 WHERE tenant_id = ?1 AND node_id = ?2 AND revoked_at IS NULL",
            params![tenant_id, node_id, now],
        )?;
        for session_id in sessions {
            transaction.execute(
                "INSERT INTO media_audit (tenant_id,principal_id,event,reason,session_id,created_at) VALUES (?1,NULL,'session_revoked','node_unlinked',?2,?3)",
                params![tenant_id, session_id, now],
            )?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }
}

/// Revoke a session from inside an in-flight transaction, recording why.
///
/// Free of the caller's control flow on purpose: a renewal decides a session is
/// over in more than one place, and each of those places must both release the
/// concurrency slot and leave an audit row saying which rule ended it. `reason`
/// names the rule and never quotes a value.
fn end_session_in(
    tx: &rusqlite::Transaction<'_>,
    tenant_id: &str,
    principal_id: &str,
    session_id: &str,
    reason: &str,
    now: &str,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE media_sessions SET revoked_at=?4 \
         WHERE tenant_id=?1 AND principal_id=?2 AND session_id=?3 AND revoked_at IS NULL",
        params![tenant_id, principal_id, session_id, now],
    )?;
    // Already revoked: the slot is released and the reason is already on record,
    // so a second audit row would only claim this call ended it.
    if changed == 1 {
        tx.execute(
            "INSERT INTO media_audit (tenant_id,principal_id,event,reason,session_id,created_at) \
             VALUES (?1,?2,'session_revoked',?3,?4,?5)",
            params![tenant_id, principal_id, reason, session_id, now],
        )?;
    }
    Ok(())
}

const SCHEMA_V1: &str = "
CREATE TABLE tenants (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE node_enrollments (
    id            TEXT PRIMARY KEY,
    tenant_id     TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    label         TEXT NOT NULL,
    proof_hash    TEXT NOT NULL,
    challenge     TEXT NOT NULL,
    expires_at    TEXT NOT NULL,
    state         TEXT NOT NULL CHECK (state IN ('pending', 'claimed', 'consumed')),
    claim_id      TEXT,
    node_id       TEXT,
    public_key    TEXT,
    display_label TEXT,
    created_at    TEXT NOT NULL,
    claimed_at    TEXT,
    consumed_at   TEXT
);
CREATE INDEX node_enrollments_tenant ON node_enrollments(tenant_id);

-- Keyed by (tenant_id, id), not by id alone. A node id is chosen by whoever
-- holds an enrollment code, so a global key would let one tenant's choice
-- collide with — and reveal the existence of — another tenant's node. The
-- composite key also indexes the tenant-scoped lookups below on its leftmost
-- column, so no separate tenant index is needed.
CREATE TABLE nodes (
    id                  TEXT NOT NULL,
    tenant_id           TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    enrollment_id       TEXT NOT NULL,
    public_key          TEXT NOT NULL,
    display_label       TEXT NOT NULL,
    auth_epoch          INTEGER NOT NULL,
    state               TEXT NOT NULL CHECK (state IN ('linked', 'unlinked')),
    linked_at           TEXT NOT NULL,
    unlinked_at         TEXT,
    last_check_in       TEXT,
    inventory_issued_at TEXT,
    PRIMARY KEY (tenant_id, id)
);

CREATE TABLE node_request_nonces (
    tenant_id  TEXT NOT NULL,
    node_id    TEXT NOT NULL,
    nonce      TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY (tenant_id, node_id, nonce)
);

CREATE TABLE node_challenges (
    challenge   TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    node_id     TEXT NOT NULL,
    operation   TEXT NOT NULL,
    auth_epoch  INTEGER NOT NULL,
    expires_at  TEXT NOT NULL,
    consumed_at TEXT
);

CREATE TABLE cameras (
    tenant_id  TEXT NOT NULL,
    node_id    TEXT NOT NULL,
    camera_id  TEXT NOT NULL,
    name       TEXT NOT NULL,
    status     TEXT NOT NULL,
    recording  INTEGER NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (tenant_id, node_id, camera_id)
);

CREATE TABLE media_policies (tenant_id TEXT PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE, live_enabled INTEGER NOT NULL, allow_main INTEGER NOT NULL, max_sessions INTEGER NOT NULL, max_bitrate_kbps INTEGER NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE media_sessions (session_id TEXT PRIMARY KEY, grant_id TEXT UNIQUE NOT NULL, publish_grant_id TEXT UNIQUE NOT NULL, tenant_id TEXT NOT NULL, principal_id TEXT NOT NULL, node_id TEXT NOT NULL, camera_id TEXT NOT NULL, representation TEXT NOT NULL, grant_expires_at TEXT NOT NULL, session_expires_at TEXT NOT NULL, created_at TEXT NOT NULL, consumed_at TEXT, publish_consumed_at TEXT, revoked_at TEXT, directive_delivered_at TEXT);
CREATE INDEX media_sessions_active ON media_sessions(tenant_id,revoked_at);
CREATE TABLE media_audit (id TEXT NOT NULL DEFAULT (lower(hex(randomblob(16)))), tenant_id TEXT NOT NULL, principal_id TEXT, event TEXT NOT NULL, reason TEXT, session_id TEXT, created_at TEXT NOT NULL);
";

const SCHEMA_V2_MEDIA: &str = "
CREATE TABLE media_policies (tenant_id TEXT PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE, live_enabled INTEGER NOT NULL, allow_main INTEGER NOT NULL, max_sessions INTEGER NOT NULL, max_bitrate_kbps INTEGER NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE media_sessions (session_id TEXT PRIMARY KEY, grant_id TEXT UNIQUE NOT NULL, tenant_id TEXT NOT NULL, principal_id TEXT NOT NULL, node_id TEXT NOT NULL, camera_id TEXT NOT NULL, representation TEXT NOT NULL, grant_expires_at TEXT NOT NULL, session_expires_at TEXT NOT NULL, created_at TEXT NOT NULL, consumed_at TEXT, revoked_at TEXT);
CREATE INDEX media_sessions_active ON media_sessions(tenant_id,revoked_at);
CREATE TABLE media_audit (id TEXT NOT NULL DEFAULT (lower(hex(randomblob(16)))), tenant_id TEXT NOT NULL, principal_id TEXT, event TEXT NOT NULL, reason TEXT, session_id TEXT, created_at TEXT NOT NULL);
";

const SCHEMA_V3_DIRECTIVES: &str = "
ALTER TABLE media_sessions ADD COLUMN publish_grant_id TEXT;
ALTER TABLE media_sessions ADD COLUMN directive_delivered_at TEXT;
ALTER TABLE media_sessions ADD COLUMN publish_consumed_at TEXT;
UPDATE media_sessions SET publish_grant_id = grant_id WHERE publish_grant_id IS NULL;
CREATE UNIQUE INDEX media_sessions_publish_grant ON media_sessions(publish_grant_id);
";
