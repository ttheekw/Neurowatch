//! Authenticated node requests: challenge issue, metadata check-in, camera
//! inventory and unlink.
//!
//! Every route here is reachable only by a node holding the private key the
//! control plane registered during enrollment, and all four apply the same
//! rules:
//!
//! * the signed window must be well-formed, short, current and plausible;
//! * the node must exist **inside the tenant its own body names**, and still be
//!   linked;
//! * the signature must verify against the key on file, over the whole body
//!   minus `signature`;
//! * `auth_epoch` must be the one the control plane currently grants;
//! * a challenge is spent exactly once, for the single operation it was issued
//!   for.
//!
//! Authentication happens before a request's payload is looked at, so an
//! unauthenticated caller cannot use payload validation as an oracle. Nothing a
//! node sends is stored beyond the allowlist in [`crate::protocol`]: no stream
//! URL, no credential, no local path.

use std::collections::HashSet;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use nw_cloud_protocol::media_session::{
    GrantScope, MediaAction, MediaGrant, PublishDirective, PublishDirectiveBatch, Representation,
    SfuEndpoint, MAX_DIRECTIVES_PER_BATCH, MEDIA_SESSION_PROTOCOL_VERSION,
    PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
};
use serde::Deserialize;
use serde_json::Value;

use crate::clock;
use crate::error::{ApiError, ApiResult};
use crate::protocol::{
    self, CameraEntry, ChallengeRequest, ChallengeResponse, CheckInResponse, InventoryResponse,
    InventorySnapshot, NodeProof, PublishDirectivesRequest, SignedWindow, CHALLENGE_TTL_SECONDS,
    CHECK_IN_OPERATION, INVENTORY_OPERATION, NODE_AUDIENCE, NODE_CHALLENGE_AUDIENCE, OPERATIONS,
    PROTOCOL_VERSION, PUBLISH_DIRECTIVES_OPERATION, UNLINK_OPERATION,
};
use crate::routes::{blocking, parse_body};
use crate::server::AppState;
use crate::store::{CameraRecord, NewChallenge, NodeRow, MAX_CAMERAS};

/// Entropy behind an issued challenge.
const CHALLENGE_BYTES: usize = 32;

/// Longest accepted challenge-request nonce. The node sends a 22-character
/// encoded UUID.
const MAX_NONCE_LEN: usize = 128;

/// The fields every signed node request carries, read leniently so that a body
/// can be authenticated *before* its payload is inspected. The strict,
/// allowlisted shape is enforced afterwards by the handler's own type.
#[derive(Debug, Deserialize)]
struct SignedEnvelope {
    protocol_version: u8,
    tenant_id: String,
    node_id: String,
    audience: String,
    auth_epoch: u64,
    issued_at: String,
    expires_at: String,
    challenge: String,
    signature: String,
}

/// Whether an operation is still available to a node that has already retired.
///
/// Only `unlink` is. The node persists its pending unlink and clears it solely
/// on an exact `204` (`retry_unlink` in `crates/nw-backend/src/cloud/mod.rs`),
/// retrying on every scheduler tick and across restarts with no give-up path.
/// If a retired node could not obtain an unlink challenge, an unlink whose
/// answer was lost in transit would loop for ever. Every other operation ends
/// at retirement.
fn survives_retirement(operation: &str) -> bool {
    operation == UNLINK_OPERATION
}

/// A node that exists in this tenant and may still perform `operation`.
///
/// Everything else — no such node, another tenant's node, a retired node
/// attempting anything but unlink — is the same bare 404, so a caller cannot
/// learn that an id it guessed exists somewhere else.
async fn node_for(
    state: &AppState,
    tenant_id: &str,
    node_id: &str,
    operation: &str,
) -> ApiResult<NodeRow> {
    let store = state.store.clone();
    let (tenant, id) = (tenant_id.to_owned(), node_id.to_owned());
    let node = blocking(move || store.node(&tenant, &id).map_err(ApiError::from)).await?;
    node.filter(|node| node.linked || survives_retirement(operation))
        .ok_or(ApiError::NotFound)
}

/// The signature must cover the whole body minus `signature`, and the epoch
/// must be the one currently granted. A stale epoch is an authentication
/// failure, not a hint: it is reported exactly like a bad signature.
fn verify(node: &NodeRow, signature: &str, auth_epoch: u64, raw: &Value) -> ApiResult<()> {
    if !protocol::verify_signature(
        &node.public_key,
        signature,
        &protocol::without_signature(raw),
    ) {
        return Err(ApiError::Unauthenticated);
    }
    if auth_epoch != node.auth_epoch {
        return Err(ApiError::Unauthenticated);
    }
    Ok(())
}

/// A node request that proved who sent it.
struct Authenticated {
    envelope: SignedEnvelope,
    node: NodeRow,
    /// The request's own window, canonicalised. This is the form that may be
    /// stored, never the spelling the node happened to send.
    window: SignedWindow,
}

/// Authenticate a signed node request and spend the challenge it presents.
///
/// A challenge that was never issued, was issued for a different operation, to
/// a different node or tenant, or has already been spent, all fail the same
/// way.
async fn authenticated_node(
    state: &AppState,
    raw: &Value,
    path_node_id: &str,
    operation: &str,
) -> ApiResult<Authenticated> {
    let envelope: SignedEnvelope = parse_body(raw.clone())?;
    if envelope.protocol_version != PROTOCOL_VERSION || envelope.audience != NODE_AUDIENCE {
        return Err(ApiError::BadRequest("unsupported request envelope".into()));
    }
    if envelope.node_id != path_node_id {
        return Err(ApiError::BadRequest(
            "the path and the body name different nodes".into(),
        ));
    }
    if !protocol::is_canonical_uuid(&envelope.tenant_id)
        || !protocol::is_canonical_uuid(&envelope.node_id)
    {
        return Err(ApiError::BadRequest(
            "identifiers must be canonical UUIDs".into(),
        ));
    }
    let window =
        protocol::validate_window(&envelope.issued_at, &envelope.expires_at, clock::now())?;

    let node = node_for(state, &envelope.tenant_id, &envelope.node_id, operation).await?;
    verify(&node, &envelope.signature, envelope.auth_epoch, raw)?;

    let store = state.store.clone();
    let (tenant, id, operation, epoch, challenge, now) = (
        envelope.tenant_id.clone(),
        envelope.node_id.clone(),
        operation.to_owned(),
        envelope.auth_epoch,
        envelope.challenge.clone(),
        clock::now_string(),
    );
    let spent = blocking(move || {
        store
            .consume_challenge(&tenant, &id, &operation, epoch, &challenge, &now)
            .map_err(ApiError::from)
    })
    .await?;
    if !spent {
        return Err(ApiError::Unauthenticated);
    }
    Ok(Authenticated {
        envelope,
        node,
        window,
    })
}

/// `POST /v1/node-auth/challenges`
///
/// The answer is exactly the five fields the node's `NodeChallengeResponse`
/// accepts — it denies unknown fields, so an extra one here would break every
/// deployed node.
pub async fn challenge(
    State(state): State<AppState>,
    Json(raw): Json<Value>,
) -> ApiResult<Json<ChallengeResponse>> {
    let request: ChallengeRequest = parse_body(raw.clone())?;
    if request.protocol_version != PROTOCOL_VERSION || request.audience != NODE_CHALLENGE_AUDIENCE {
        return Err(ApiError::BadRequest(
            "unsupported challenge envelope".into(),
        ));
    }
    if !protocol::is_canonical_uuid(&request.tenant_id)
        || !protocol::is_canonical_uuid(&request.node_id)
    {
        return Err(ApiError::BadRequest(
            "identifiers must be canonical UUIDs".into(),
        ));
    }
    if !OPERATIONS.contains(&request.operation.as_str()) {
        return Err(ApiError::BadRequest(
            "no challenge is issued for that operation".into(),
        ));
    }
    if request.nonce.is_empty() || request.nonce.len() > MAX_NONCE_LEN {
        return Err(ApiError::BadRequest("invalid request nonce".into()));
    }
    let window = protocol::validate_window(&request.issued_at, &request.expires_at, clock::now())?;

    let node = node_for(
        &state,
        &request.tenant_id,
        &request.node_id,
        &request.operation,
    )
    .await?;
    verify(&node, &request.signature, request.auth_epoch, &raw)?;

    // The nonce is what makes a replay of this whole signed request useless, so
    // it is spent before a challenge is minted. It is kept for as long as the
    // request it covers could still be presented again.
    let store = state.store.clone();
    let (tenant, id, nonce, window_end, now) = (
        request.tenant_id.clone(),
        request.node_id.clone(),
        request.nonce.clone(),
        window.expires_at.clone(),
        clock::now_string(),
    );
    let fresh = blocking(move || {
        store
            .consume_request_nonce(&tenant, &id, &nonce, &window_end, &now)
            .map_err(ApiError::from)
    })
    .await?;
    if !fresh {
        return Err(ApiError::Conflict(
            "this challenge request has already been used".into(),
        ));
    }

    let challenge = protocol::random_token(CHALLENGE_BYTES)?;
    let now = clock::now();
    let expires_at = clock::format(now + time::Duration::seconds(CHALLENGE_TTL_SECONDS));
    let store = state.store.clone();
    let issued = NewChallenge {
        tenant_id: request.tenant_id.clone(),
        node_id: request.node_id.clone(),
        operation: request.operation.clone(),
        auth_epoch: request.auth_epoch,
        challenge: challenge.clone(),
        expires_at: expires_at.clone(),
    };
    let at = clock::format(now);
    blocking(move || store.issue_challenge(&issued, &at).map_err(ApiError::from)).await?;

    Ok(Json(ChallengeResponse {
        protocol_version: PROTOCOL_VERSION,
        audience: NODE_AUDIENCE,
        operation: request.operation,
        challenge,
        expires_at,
    }))
}

/// `POST /v1/nodes/{node_id}/metadata-check-ins`
///
/// A check-in reports liveness and nothing else. The answer carries the node's
/// own id, its current epoch and the timestamp the control plane recorded,
/// which is the same string the administrator surface reports.
pub async fn check_in(
    State(state): State<AppState>,
    Path(node_id): Path<String>,
    Json(raw): Json<Value>,
) -> ApiResult<Json<CheckInResponse>> {
    let Authenticated { envelope, node, .. } =
        authenticated_node(&state, &raw, &node_id, CHECK_IN_OPERATION).await?;
    // A check-in carries its envelope and nothing else; a field the node was
    // never meant to send is refused rather than ignored.
    let _: NodeProof = parse_body(raw)?;

    let checked_in_at = clock::now_string();
    let store = state.store.clone();
    let (tenant, id, at) = (
        envelope.tenant_id.clone(),
        node.id.clone(),
        checked_in_at.clone(),
    );
    let recorded = blocking(move || {
        store
            .record_check_in(&tenant, &id, &at)
            .map_err(ApiError::from)
    })
    .await?;
    if !recorded {
        return Err(ApiError::NotFound);
    }
    tracing::debug!(tenant_id = %envelope.tenant_id, node_id = %node.id, "A node checked in");
    Ok(Json(CheckInResponse {
        node_id: node.id,
        auth_epoch: node.auth_epoch,
        status: "linked",
        checked_in_at,
    }))
}

/// Return, once, the live-session publication obligations owned by this node.
pub async fn publish_directives(
    State(state): State<AppState>,
    Path(node_id): Path<String>,
    Json(raw): Json<Value>,
) -> ApiResult<Json<PublishDirectiveBatch>> {
    let edge = state.media_edge.clone().ok_or(ApiError::Unavailable {
        code: "media_edge_unavailable",
        message: "media edge is not configured",
    })?;
    let Authenticated { envelope, .. } =
        authenticated_node(&state, &raw, &node_id, PUBLISH_DIRECTIVES_OPERATION).await?;
    let request: PublishDirectivesRequest = parse_body(raw)?;
    if request.acknowledged_directive_ids.len() > MAX_DIRECTIVES_PER_BATCH
        || request
            .acknowledged_directive_ids
            .iter()
            .any(|id| uuid::Uuid::parse_str(id).is_err())
    {
        return Err(ApiError::BadRequest(
            "invalid directive acknowledgements".into(),
        ));
    }
    let now = clock::now();
    let now_string = clock::format(now);
    let store = state.store.clone();
    let tenant_id = envelope.tenant_id.clone();
    let claim_node_id = node_id.clone();
    let claimed = blocking(move || {
        store
            .take_publish_directives(
                &tenant_id,
                &claim_node_id,
                &now_string,
                MAX_DIRECTIVES_PER_BATCH,
                &request.acknowledged_directive_ids,
            )
            .map_err(ApiError::from)
    })
    .await?;
    let store = state.store.clone();
    let tenant_id = envelope.tenant_id.clone();
    let active_at = clock::format(now);
    let active_session_ids = blocking(move || {
        store
            .active_publish_sessions(&tenant_id, &node_id, &active_at)
            .map_err(ApiError::from)
    })
    .await?;
    let mut directives = Vec::with_capacity(claimed.len());
    for row in claimed {
        let directive_id = row.grant_id.clone();
        let representation = Representation::parse(&row.representation)
            .ok_or_else(|| ApiError::Internal("stored representation is invalid".into()))?;
        let grant = MediaGrant {
            protocol_version: MEDIA_SESSION_PROTOCOL_VERSION,
            grant_id: row.grant_id,
            session_id: row.session_id.clone(),
            audience: edge.sfu_region.clone(),
            scope: GrantScope {
                tenant_id: envelope.tenant_id.clone(),
                site_id: envelope.node_id.clone(),
                camera_id: row.camera_id,
                action: MediaAction::Live,
                representation,
            },
            issued_at_utc: clock::format(now),
            not_before_utc: clock::format(now),
            expires_at_utc: row.grant_expires_at,
            max_bitrate_kbps: row.max_bitrate_kbps,
        };
        grant
            .validate()
            .map_err(|error| ApiError::Denied(error.reason()))?;
        let ttl = clock::parse(&grant.expires_at_utc)
            .map(|end| (end - clock::now()).whole_seconds())
            .unwrap_or(0);
        let publisher_token = crate::routes::media::publisher_token(&grant, &edge, ttl)?;
        directives.push(PublishDirective {
            protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
            directive_id,
            session_id: row.session_id,
            grant: crate::routes::media::sign(&grant, &edge)?,
            sfu: SfuEndpoint {
                region_id: edge.sfu_region.clone(),
                signaling_url: edge.sfu_signaling_base.clone(),
                ice_servers: Vec::new(),
            },
            publisher_token,
            playback: None,
        });
    }
    let batch = PublishDirectiveBatch {
        protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
        directives,
        active_session_ids,
    };
    batch
        .validate()
        .map_err(|error| ApiError::Denied(error.reason()))?;
    Ok(Json(batch))
}

/// `POST /v1/nodes/{node_id}/inventory-snapshots`
pub async fn inventory(
    State(state): State<AppState>,
    Path(node_id): Path<String>,
    Json(raw): Json<Value>,
) -> ApiResult<Json<InventoryResponse>> {
    let Authenticated {
        envelope,
        node,
        window,
    } = authenticated_node(&state, &raw, &node_id, INVENTORY_OPERATION).await?;
    // Only now is the payload read. `deny_unknown_fields` on the snapshot and
    // on every camera is the allowlist: a snapshot carrying a stream URL, a
    // credential or a path fails to parse and is refused whole, so such a value
    // cannot reach the database by being quietly ignored.
    let snapshot: InventorySnapshot = parse_body(raw)?;
    validate_cameras(&snapshot.cameras)?;

    let cameras: Vec<CameraRecord> = snapshot
        .cameras
        .iter()
        .map(|camera| CameraRecord {
            camera_id: camera.camera_id.clone(),
            name: camera.name.clone(),
            status: camera.status.clone(),
            recording: camera.recording,
        })
        .collect();
    let submitted = cameras.len();
    let store = state.store.clone();
    // The canonicalised window, never the spelling the node happened to send:
    // snapshot ordering compares these strings, so two spellings of one instant
    // would otherwise order by punctuation.
    let (tenant, id, issued_at, now) = (
        envelope.tenant_id.clone(),
        node.id.clone(),
        window.issued_at.clone(),
        clock::now_string(),
    );
    let (accepted, camera_count) = blocking(move || {
        let accepted = store.replace_inventory(&tenant, &id, &issued_at, &cameras, &now)?;
        // When a newer snapshot is kept, report the inventory actually held,
        // not the one that was turned away.
        let held = if accepted {
            submitted
        } else {
            store.cameras(&tenant, &id)?.len()
        };
        Ok((accepted, held))
    })
    .await?;

    tracing::debug!(
        tenant_id = %envelope.tenant_id,
        node_id = %node.id,
        accepted,
        camera_count,
        "A node reported its camera inventory"
    );
    Ok(Json(InventoryResponse {
        node_id: node.id,
        accepted,
        camera_count,
    }))
}

/// `POST /v1/nodes/{node_id}/unlink-indications`
///
/// The node insists on an empty `204`, and treats anything else as a failure it
/// will retry.
pub async fn unlink(
    State(state): State<AppState>,
    Path(node_id): Path<String>,
    Json(raw): Json<Value>,
) -> ApiResult<StatusCode> {
    let Authenticated { envelope, node, .. } =
        authenticated_node(&state, &raw, &node_id, UNLINK_OPERATION).await?;
    let _: NodeProof = parse_body(raw)?;

    let store = state.store.clone();
    let (tenant, id, now) = (
        envelope.tenant_id.clone(),
        node.id.clone(),
        clock::now_string(),
    );
    // `false` means this node had already retired. The caller is the node
    // itself — `authenticated_node` proved that, and only a node that exists in
    // this tenant reaches here at all — so a repeat is the same settled outcome,
    // not a failure. Answering anything but 204 would leave a node whose first
    // answer was lost retrying for ever.
    let newly_retired = blocking(move || {
        store
            .unlink_node(&tenant, &id, &now)
            .map_err(ApiError::from)
    })
    .await?;
    if newly_retired {
        tracing::info!(
            tenant_id = %envelope.tenant_id,
            node_id = %node.id,
            "A node unlinked itself; its identity and cached inventory are gone"
        );
    } else {
        tracing::debug!(
            tenant_id = %envelope.tenant_id,
            node_id = %node.id,
            "A node repeated an unlink it had already completed"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Snapshot-level rules the per-camera allowlist cannot express on its own.
fn validate_cameras(cameras: &[CameraEntry]) -> ApiResult<()> {
    if cameras.len() > MAX_CAMERAS {
        return Err(ApiError::BadRequest(format!(
            "a snapshot may report at most {MAX_CAMERAS} cameras"
        )));
    }
    let mut seen: HashSet<&str> = HashSet::with_capacity(cameras.len());
    for camera in cameras {
        protocol::validate_camera(camera)?;
        if !seen.insert(camera.camera_id.as_str()) {
            return Err(ApiError::BadRequest(
                "a snapshot must not repeat a camera_id".into(),
            ));
        }
    }
    Ok(())
}
