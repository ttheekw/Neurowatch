//! Node enrollment: the three calls the node's Cloud Link makes with a portal
//! enrollment code.
//!
//! * `claim-parameters` hands back the binding (expiry + challenge) to a caller
//!   that already holds the one-time proof.
//! * `claim` binds a node id and its public key to the enrollment and consumes
//!   the proof. It is the only step that spends it, and it spends it exactly
//!   once.
//! * `confirm` completes the link once the cloud accepted the claim, and
//!   registers the node identity.
//!
//! The proof authenticates the first two calls; from `confirm` onwards the
//! node's own key does.

use axum::extract::{Path, State};
use axum::Json;
use serde_json::Value;

use crate::clock;
use crate::error::{ApiError, ApiResult};
use crate::protocol::{
    self, ClaimParametersRequest, ClaimParametersResponse, ClaimRequest, ClaimResponse,
    ConfirmRequest, ConfirmResponse, ENROLLMENT_AUDIENCE, PROTOCOL_VERSION,
};
use crate::routes::{blocking, parse_body};
use crate::server::AppState;
use crate::store::{ClaimRecord, ConfirmOutcome, EnrollmentRow, EnrollmentState};

/// An enrollment that exists, is within its lifetime, and whose one-time proof
/// the caller has just proved it holds.
fn check_proof(enrollment: &EnrollmentRow, transfer_proof: &str) -> ApiResult<()> {
    if protocol::hash_matches(
        &protocol::proof_hash(transfer_proof),
        &enrollment.proof_hash,
    ) {
        Ok(())
    } else {
        Err(ApiError::Unauthenticated)
    }
}

fn check_live(enrollment: &EnrollmentRow) -> ApiResult<()> {
    let expires = clock::parse(&enrollment.expires_at)
        .ok_or_else(|| ApiError::Internal("stored enrollment expiry is not RFC 3339".into()))?;
    if expires <= clock::now() {
        return Err(ApiError::Expired(
            "the enrollment expired; create a new enrollment code".into(),
        ));
    }
    Ok(())
}

fn check_pending(enrollment: &EnrollmentRow) -> ApiResult<()> {
    match enrollment.state {
        EnrollmentState::Pending => Ok(()),
        // The proof is single-use: once a node has claimed the enrollment, the
        // same code buys nothing, for that node or any other.
        EnrollmentState::Claimed | EnrollmentState::Consumed => Err(ApiError::Conflict(
            "this enrollment code has already been used".into(),
        )),
    }
}

pub async fn claim_parameters(
    State(state): State<AppState>,
    Path(enrollment_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<ClaimParametersResponse>> {
    let body: ClaimParametersRequest = parse_body(body)?;
    if !protocol::is_canonical_uuid(&enrollment_id) || !protocol::is_canonical_uuid(&body.tenant_id)
    {
        return Err(ApiError::NotFound);
    }
    let store = state.store.clone();
    let (tenant, id) = (body.tenant_id.clone(), enrollment_id.clone());
    let enrollment =
        blocking(move || store.enrollment(&tenant, &id)?.ok_or(ApiError::NotFound)).await?;

    check_proof(&enrollment, &body.transfer_proof)?;
    check_live(&enrollment)?;
    check_pending(&enrollment)?;

    Ok(Json(ClaimParametersResponse {
        protocol_version: PROTOCOL_VERSION,
        audience: ENROLLMENT_AUDIENCE,
        enrollment_id,
        tenant_id: body.tenant_id,
        expires_at: enrollment.expires_at,
        challenge: enrollment.challenge,
    }))
}

pub async fn claim(
    State(state): State<AppState>,
    Path(enrollment_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<ClaimResponse>> {
    let body: ClaimRequest = parse_body(body)?;
    if body.protocol_version != PROTOCOL_VERSION || body.audience != ENROLLMENT_AUDIENCE {
        return Err(ApiError::BadRequest("unsupported claim envelope".into()));
    }
    if body.enrollment_id != enrollment_id {
        return Err(ApiError::BadRequest(
            "the path and the body name different enrollments".into(),
        ));
    }
    for id in [&body.enrollment_id, &body.tenant_id, &body.node_id] {
        if !protocol::is_canonical_uuid(id) {
            return Err(ApiError::BadRequest(
                "identifiers must be canonical UUIDs".into(),
            ));
        }
    }
    protocol::validate_display_label(&body.display_label)?;
    if protocol::decode_public_key(&body.public_key).is_none() {
        return Err(ApiError::BadRequest(
            "public_key must be a url-safe base64 ed25519 key".into(),
        ));
    }

    let store = state.store.clone();
    let (tenant, id) = (body.tenant_id.clone(), enrollment_id.clone());
    let enrollment =
        blocking(move || store.enrollment(&tenant, &id)?.ok_or(ApiError::NotFound)).await?;

    check_proof(&enrollment, &body.transfer_proof)?;
    check_live(&enrollment)?;
    check_pending(&enrollment)?;
    // The node must be claiming the binding this enrollment actually issued.
    if body.expires_at != enrollment.expires_at || body.challenge != enrollment.challenge {
        return Err(ApiError::BadRequest(
            "the claim does not match the enrollment binding".into(),
        ));
    }
    if !protocol::verify_signature(&body.public_key, &body.signature, &body.signed_payload()) {
        return Err(ApiError::Unauthenticated);
    }

    let claim_id = uuid::Uuid::new_v4().to_string();
    let record = ClaimRecord {
        tenant_id: body.tenant_id.clone(),
        enrollment_id: enrollment_id.clone(),
        claim_id: claim_id.clone(),
        node_id: body.node_id.clone(),
        public_key: body.public_key.clone(),
        display_label: body.display_label.clone(),
    };
    let store = state.store.clone();
    let now = clock::now_string();
    // A single conditional write decides who claimed it, so two nodes racing
    // the same code cannot both win.
    let claimed = blocking(move || {
        store
            .claim_enrollment(&record, &now)
            .map_err(ApiError::from)
    })
    .await?;
    if !claimed {
        return Err(ApiError::Conflict(
            "this enrollment code has already been used".into(),
        ));
    }
    tracing::info!(
        tenant_id = %body.tenant_id,
        enrollment_id = %enrollment_id,
        node_id = %body.node_id,
        "A node claimed an enrollment"
    );
    Ok(Json(ClaimResponse { claim_id }))
}

pub async fn confirm(
    State(state): State<AppState>,
    Path(enrollment_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<ConfirmResponse>> {
    let body: ConfirmRequest = parse_body(body)?;
    if body.protocol_version != PROTOCOL_VERSION || body.audience != ENROLLMENT_AUDIENCE {
        return Err(ApiError::BadRequest(
            "unsupported confirmation envelope".into(),
        ));
    }
    if body.enrollment_id != enrollment_id {
        return Err(ApiError::BadRequest(
            "the path and the body name different enrollments".into(),
        ));
    }
    if !protocol::is_canonical_uuid(&body.tenant_id) || !protocol::is_canonical_uuid(&body.node_id)
    {
        return Err(ApiError::BadRequest(
            "identifiers must be canonical UUIDs".into(),
        ));
    }

    let store = state.store.clone();
    let (tenant, id) = (body.tenant_id.clone(), enrollment_id.clone());
    let enrollment =
        blocking(move || store.enrollment(&tenant, &id)?.ok_or(ApiError::NotFound)).await?;
    check_live(&enrollment)?;

    // The node id and claim id must be the ones this enrollment issued, and the
    // signature must come from the key that claimed it.
    if enrollment.node_id.as_deref() != Some(body.node_id.as_str())
        || enrollment.claim_id.as_deref() != Some(body.claim_id.as_str())
    {
        return Err(ApiError::Conflict(
            "this enrollment has no such pending claim".into(),
        ));
    }
    let public_key = enrollment
        .public_key
        .clone()
        .ok_or_else(|| ApiError::Conflict("this enrollment has no pending claim".into()))?;
    if body.expires_at != enrollment.expires_at || body.challenge != enrollment.challenge {
        return Err(ApiError::BadRequest(
            "the confirmation does not match the enrollment binding".into(),
        ));
    }
    if !protocol::verify_signature(&public_key, &body.signature, &body.signed_payload()) {
        return Err(ApiError::Unauthenticated);
    }

    // A retried confirmation for the same claim is the same outcome, not a new
    // node: the node link is idempotent from here.
    if enrollment.state == EnrollmentState::Consumed {
        let store = state.store.clone();
        let (tenant, node) = (body.tenant_id.clone(), body.node_id.clone());
        let existing =
            blocking(move || store.node(&tenant, &node)?.ok_or(ApiError::NotFound)).await?;
        return Ok(Json(ConfirmResponse {
            status: "linked",
            node_id: existing.id,
            auth_epoch: existing.auth_epoch,
        }));
    }

    let store = state.store.clone();
    let (tenant, id, claim_id, now) = (
        body.tenant_id.clone(),
        enrollment_id.clone(),
        body.claim_id.clone(),
        clock::now_string(),
    );
    let outcome = blocking(move || {
        store
            .confirm_enrollment(&tenant, &id, &claim_id, &now)
            .map_err(ApiError::from)
    })
    .await?;
    match outcome {
        ConfirmOutcome::Registered => {}
        // The enrollment moved on between the read above and the write — a
        // replayed confirmation, or one racing another. Nothing changed.
        ConfirmOutcome::NotPending => {
            return Err(ApiError::Conflict(
                "this enrollment is no longer awaiting confirmation".into(),
            ))
        }
        // The node id is chosen by whoever holds the enrollment code, so it can
        // name a node this tenant already registered under a different key.
        // That is a refusal the caller can act on, not a storage fault, and the
        // node already holding the id is left exactly as it was.
        ConfirmOutcome::NodeIdTaken => {
            return Err(ApiError::Conflict(
                "this tenant already has a node with that id".into(),
            ))
        }
    }
    tracing::info!(
        tenant_id = %body.tenant_id,
        node_id = %body.node_id,
        "Linked a node"
    );
    Ok(Json(ConfirmResponse {
        status: "confirmed",
        node_id: body.node_id,
        auth_epoch: 1,
    }))
}
