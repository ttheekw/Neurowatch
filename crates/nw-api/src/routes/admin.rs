//! Administrator surface: create a tenant, mint a node enrollment code, and
//! read back what nodes reported.
//!
//! These routes exist only when the deployment was configured with an
//! administrator credential (see [`crate::config`]), and every one of them
//! checks that credential before it looks at the request body, so an
//! unauthenticated caller cannot use body validation as an oracle.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::clock;
use crate::error::{ApiError, ApiResult};
use crate::protocol;
use crate::routes::{blocking, parse_body};
use crate::server::AppState;
use crate::store::NewEnrollment;

/// Shortest and longest enrollment lifetime an operator may ask for. A code is
/// meant to be typed into a node within minutes.
pub const MIN_ENROLLMENT_TTL_SECONDS: i64 = 1;
pub const MAX_ENROLLMENT_TTL_SECONDS: i64 = 3_600;
pub const DEFAULT_ENROLLMENT_TTL_SECONDS: i64 = 900;

/// Bytes of entropy behind the one-time transfer proof and the enrollment
/// challenge.
const SECRET_BYTES: usize = 32;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTenant {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateEnrollment {
    label: String,
    #[serde(default)]
    ttl_seconds: Option<i64>,
}

/// Constant-time bearer check against the configured credential.
pub(crate) fn authorize(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let expected = state
        .admin_token
        .as_deref()
        .ok_or(ApiError::Unauthenticated)?;
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(ApiError::Unauthenticated)?;
    if protocol::hash_matches(
        &protocol::proof_hash(presented),
        &protocol::proof_hash(expected),
    ) {
        Ok(())
    } else {
        Err(ApiError::Unauthenticated)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaPolicy {
    live_enabled: bool,
    allow_main_representation: bool,
    max_concurrent_sessions: u32,
    max_bitrate_kbps: u32,
}

pub async fn put_media_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
    Json(value): Json<Value>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers)?;
    let body: MediaPolicy = parse_body(value)?;
    if body.max_concurrent_sessions == 0
        // A node pull carries the complete active set because omission means
        // cancellation. Do not permit a policy that can create more active
        // sessions than one authenticated batch can represent.
        || body.max_concurrent_sessions
            > nw_cloud_protocol::media_session::MAX_DIRECTIVES_PER_BATCH as u32
        || body.max_bitrate_kbps == 0
        || body.max_bitrate_kbps > nw_cloud_protocol::media_session::MAX_GRANT_BITRATE_KBPS
    {
        return Err(ApiError::BadRequest(
            "media policy limits are outside the permitted range".into(),
        ));
    }
    let store = state.store.clone();
    let tenant = tenant_id.clone();
    let now = clock::now_string();
    let live = body.live_enabled;
    let main = body.allow_main_representation;
    let sessions = body.max_concurrent_sessions;
    let bitrate = body.max_bitrate_kbps;
    let ok = blocking(move || {
        store
            .upsert_media_policy(&tenant, live, main, sessions, bitrate, &now)
            .map_err(ApiError::from)
    })
    .await?;
    if !ok {
        return Err(ApiError::NotFound);
    }
    Ok(Json(
        json!({"tenant_id":tenant_id,"live_enabled":live,"allow_main_representation":main,"max_concurrent_sessions":sessions,"max_bitrate_kbps":bitrate}),
    ))
}

pub async fn media_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers)?;
    let store = state.store.clone();
    let tenant = tenant_id.clone();
    let rows = blocking(move || store.media_audit(&tenant).map_err(ApiError::from))
        .await?
        .ok_or(ApiError::NotFound)?;
    let events:Vec<Value>=rows.into_iter().map(|(id,principal,event,reason,session,created)|json!({"event_id":id,"principal_id":principal,"event":event,"reason":reason,"session_id":session,"created_at":created})).collect();
    Ok(Json(json!({"tenant_id":tenant_id,"events":events})))
}

fn validate_tenant_name(name: &str) -> ApiResult<()> {
    let acceptable = !name.trim().is_empty()
        && name.chars().count() <= 64
        && !name.chars().any(char::is_control);
    if acceptable {
        Ok(())
    } else {
        Err(ApiError::BadRequest("invalid tenant name".into()))
    }
}

pub async fn create_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorize(&state, &headers)?;
    let body: CreateTenant = parse_body(body)?;
    validate_tenant_name(&body.name)?;
    let tenant_id = uuid::Uuid::new_v4().to_string();
    let created_at = clock::now_string();
    let store = state.store.clone();
    let (id, name, at) = (tenant_id.clone(), body.name.clone(), created_at.clone());
    blocking(move || store.create_tenant(&id, &name, &at).map_err(ApiError::from)).await?;
    tracing::info!(tenant_id = %tenant_id, "Created a tenant");
    Ok((
        StatusCode::CREATED,
        Json(json!({"tenant_id": tenant_id, "name": body.name, "created_at": created_at})),
    ))
}

/// Mint a one-time enrollment code. The proof is returned here and nowhere
/// else: only its hash is stored, so a leaked database cannot enroll a node.
pub async fn create_enrollment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorize(&state, &headers)?;
    let body: CreateEnrollment = parse_body(body)?;
    protocol::validate_display_label(&body.label)?;
    if !protocol::is_canonical_uuid(&tenant_id) {
        return Err(ApiError::NotFound);
    }
    let ttl = body.ttl_seconds.unwrap_or(DEFAULT_ENROLLMENT_TTL_SECONDS);
    if !(MIN_ENROLLMENT_TTL_SECONDS..=MAX_ENROLLMENT_TTL_SECONDS).contains(&ttl) {
        return Err(ApiError::BadRequest(format!(
            "ttl_seconds must be between {MIN_ENROLLMENT_TTL_SECONDS} and {MAX_ENROLLMENT_TTL_SECONDS}"
        )));
    }

    let transfer_proof = protocol::random_token(SECRET_BYTES)?;
    let challenge = protocol::random_token(SECRET_BYTES)?;
    let enrollment_id = uuid::Uuid::new_v4().to_string();
    let now = clock::now();
    let expires_at = clock::format(now + time::Duration::seconds(ttl));
    let new = NewEnrollment {
        tenant_id: tenant_id.clone(),
        label: body.label,
        proof_hash: protocol::proof_hash(&transfer_proof),
        challenge: challenge.clone(),
        expires_at: expires_at.clone(),
    };

    let store = state.store.clone();
    let (tenant, id, created_at) = (tenant_id.clone(), enrollment_id.clone(), clock::format(now));
    blocking(move || {
        if !store.tenant_exists(&tenant)? {
            return Err(ApiError::NotFound);
        }
        store.create_enrollment(&id, &new, &created_at)?;
        Ok(())
    })
    .await?;

    tracing::info!(
        tenant_id = %tenant_id,
        enrollment_id = %enrollment_id,
        expires_at = %expires_at,
        "Created a node enrollment"
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({
            // The single code the operator types into the node; the proof
            // inside it is never retrievable again.
            "enrollment_code": format!("{tenant_id}.{enrollment_id}.{transfer_proof}"),
            "enrollment_id": enrollment_id,
            "tenant_id": tenant_id,
            "expires_at": expires_at,
        })),
    ))
}

pub async fn list_nodes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers)?;
    let store = state.store.clone();
    let tenant = tenant_id.clone();
    let nodes = blocking(move || store.nodes(&tenant).map_err(ApiError::from)).await?;
    let nodes: Vec<Value> = nodes
        .into_iter()
        .map(|node| {
            json!({
                "node_id": node.id,
                "display_label": node.display_label,
                "auth_epoch": node.auth_epoch,
                "linked": node.linked,
                "last_check_in": node.last_check_in,
            })
        })
        .collect();
    Ok(Json(json!({"tenant_id": tenant_id, "nodes": nodes})))
}

pub async fn list_cameras(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_id, node_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers)?;
    let store = state.store.clone();
    let (tenant, node) = (tenant_id.clone(), node_id.clone());
    // Tenant-scoped: another tenant's node id is simply not found here.
    let cameras = blocking(move || {
        if store.node(&tenant, &node)?.is_none() {
            return Err(ApiError::NotFound);
        }
        store.cameras(&tenant, &node).map_err(ApiError::from)
    })
    .await?;
    let cameras: Vec<Value> = cameras
        .into_iter()
        .map(|camera| {
            json!({
                "camera_id": camera.camera_id,
                "name": camera.name,
                "status": camera.status,
                "recording": camera.recording,
            })
        })
        .collect();
    Ok(Json(
        json!({"tenant_id": tenant_id, "node_id": node_id, "cameras": cameras}),
    ))
}
