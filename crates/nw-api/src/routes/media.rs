//! Browser/SFU media authorization. This authorizes a future media path; it
//! does not publish or relay video.

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer as _, Verifier as _};
use hmac::{Hmac, Mac};
use livekit_api::access_token::{AccessToken, VideoGrants};
use nw_cloud_protocol::media_session::{
    GrantDenialReason as Denial, GrantScope, IceServer, MediaAction, MediaGrant,
    MediaSessionHandoff, Representation, SfuEndpoint, MEDIA_SESSION_PROTOCOL_VERSION,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

use crate::{
    clock,
    config::{MediaEdgeConfig, MIN_RENEWAL_WINDOW_SECONDS},
    error::{ApiError, ApiResult},
    identity::{Principal, PERMISSION_VIEW_LIVE, PERMISSION_VIEW_LIVE_MAIN},
    routes::{blocking, parse_body},
    server::AppState,
    store::{MediaSessionCreateOutcome, MediaSessionRenewOutcome},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    node_id: String,
    camera_id: String,
    representation: String,
    #[serde(default)]
    action: Option<MediaAction>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsumeRequest {
    grant: String,
}

/// Audit namespace for authenticated edge requests whose untrusted grant could
/// not be verified far enough to supply a tenant or session safely.
const UNATTRIBUTED_EDGE_AUDIT_TENANT: &str = "__media_edge__";

fn bearer(headers: &HeaderMap) -> ApiResult<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ApiError::Unauthenticated)
}
fn edge(state: &AppState) -> ApiResult<Arc<MediaEdgeConfig>> {
    state.media_edge.clone().ok_or(ApiError::Unavailable {
        code: "media_edge_unavailable",
        message: "media edge is not configured",
    })
}
async fn principal(state: &AppState, headers: &HeaderMap) -> ApiResult<Principal> {
    let verifier = state
        .principal_verifier
        .as_ref()
        .ok_or(ApiError::Unavailable {
            code: "identity_unavailable",
            message: "browser identity provider is not configured",
        })?;
    verifier.verify(bearer(headers)?).await
}
pub(crate) fn sign(grant: &MediaGrant, edge: &MediaEdgeConfig) -> ApiResult<String> {
    let payload = grant
        .canonical_json()
        .map_err(|e| ApiError::Denied(e.reason()))?;
    let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let key = ed25519_dalek::SigningKey::from_bytes(&edge.grant_signing_seed);
    let signature =
        URL_SAFE_NO_PAD.encode(key.sign(format!("nwg1.{encoded}").as_bytes()).to_bytes());
    Ok(format!("nwg1.{encoded}.{signature}"))
}
/// The SFU token a node publishes one session with, minted here so the
/// LiveKit API secret never leaves the control plane. For this session's
/// room only: publish, data to the viewer, and in a live room subscribe (a
/// viewer's talk track is the one track the node takes); no metadata. It
/// lapses with the grant it travels with; a refreshed directive carries a new
/// one.
pub(crate) fn publisher_token(
    grant: &MediaGrant,
    edge: &MediaEdgeConfig,
    ttl_seconds: i64,
) -> ApiResult<String> {
    if ttl_seconds <= 0 {
        return Err(ApiError::Denied(Denial::SessionRevoked));
    }
    AccessToken::with_api_key(&edge.livekit_api_key, &edge.livekit_api_secret)
        .with_identity(&format!(
            "node:{}:{}",
            grant.scope.site_id, grant.session_id
        ))
        .with_ttl(Duration::from_secs(ttl_seconds as u64))
        .with_grants(VideoGrants {
            room_join: true,
            room: grant.session_id.clone(),
            can_publish: Some(true),
            can_subscribe: Some(grant.scope.action == MediaAction::Live),
            can_publish_data: Some(true),
            can_update_own_metadata: Some(false),
            ..Default::default()
        })
        .to_jwt()
        .map_err(|_| ApiError::Internal("LiveKit token issuance failed".into()))
}

fn verify(token: &str, edge: &MediaEdgeConfig) -> ApiResult<MediaGrant> {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 || parts[0] != "nwg1" {
        return Err(ApiError::Denied(Denial::MalformedGrant));
    }
    let signature = URL_SAFE_NO_PAD
        .decode(parts[2])
        .ok()
        .and_then(|v| <[u8; 64]>::try_from(v).ok())
        .ok_or(ApiError::Denied(Denial::GrantSignatureInvalid))?;
    let key = ed25519_dalek::SigningKey::from_bytes(&edge.grant_signing_seed).verifying_key();
    key.verify(
        format!("nwg1.{}", parts[1]).as_bytes(),
        &ed25519_dalek::Signature::from_bytes(&signature),
    )
    .map_err(|_| ApiError::Denied(Denial::GrantSignatureInvalid))?;
    let raw = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| ApiError::Denied(Denial::MalformedGrant))?;
    let raw = std::str::from_utf8(&raw).map_err(|_| ApiError::Denied(Denial::MalformedGrant))?;
    MediaGrant::from_canonical_json(raw).map_err(|e| ApiError::Denied(e.reason()))
}
async fn audit(
    state: &AppState,
    tenant: String,
    principal: Option<String>,
    event: &'static str,
    reason: Option<&'static str>,
    session: Option<String>,
) -> ApiResult<()> {
    let store = state.store.clone();
    let now = clock::now_string();
    blocking(move || {
        store
            .append_media_audit(
                &tenant,
                principal.as_deref(),
                event,
                reason,
                session.as_deref(),
                &now,
            )
            .map_err(ApiError::from)
    })
    .await
}

pub async fn open(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let edge = edge(&state)?;
    let who = principal(&state, &headers).await?;
    let value = match payload {
        Ok(Json(value)) => value,
        Err(_) => {
            audit(
                &state,
                who.tenant_id.clone(),
                Some(who.principal_id.clone()),
                "grant_denied",
                Some(Denial::MalformedGrant.code()),
                None,
            )
            .await?;
            return Err(ApiError::Denied(Denial::MalformedGrant));
        }
    };
    let body: OpenRequest = match parse_body(value) {
        Ok(body) => body,
        Err(_) => {
            audit(
                &state,
                who.tenant_id.clone(),
                Some(who.principal_id.clone()),
                "grant_denied",
                Some(Denial::MalformedGrant.code()),
                None,
            )
            .await?;
            return Err(ApiError::Denied(Denial::MalformedGrant));
        }
    };
    let action = body.action.unwrap_or(MediaAction::Live);
    // This reference control plane has no playback path; only live is served.
    if action != MediaAction::Live {
        audit(
            &state,
            who.tenant_id.clone(),
            Some(who.principal_id.clone()),
            "grant_denied",
            Some(Denial::ActionNotSupported.code()),
            None,
        )
        .await?;
        return Err(ApiError::Denied(Denial::ActionNotSupported));
    }
    let representation = Representation::parse(&body.representation)
        .ok_or_else(|| ApiError::Denied(Denial::MalformedGrant));
    let representation = match representation {
        Ok(representation) => representation,
        Err(error) => {
            audit(
                &state,
                who.tenant_id.clone(),
                Some(who.principal_id.clone()),
                "grant_denied",
                Some(Denial::MalformedGrant.code()),
                None,
            )
            .await?;
            return Err(error);
        }
    };
    if !who.has(PERMISSION_VIEW_LIVE)
        || (representation == Representation::Main && !who.has(PERMISSION_VIEW_LIVE_MAIN))
    {
        audit(
            &state,
            who.tenant_id.clone(),
            Some(who.principal_id.clone()),
            "grant_denied",
            Some(Denial::RepresentationNotPermitted.code()),
            None,
        )
        .await?;
        return Err(ApiError::Denied(Denial::RepresentationNotPermitted));
    }
    let store = state.store.clone();
    let tenant = who.tenant_id.clone();
    let node = body.node_id.clone();
    let camera = body.camera_id.clone();
    let exists = blocking(move || {
        store
            .camera_exists(&tenant, &node, &camera)
            .map_err(ApiError::from)
    })
    .await?;
    if !exists {
        audit(
            &state,
            who.tenant_id.clone(),
            Some(who.principal_id.clone()),
            "grant_denied",
            Some(Denial::UnknownCamera.code()),
            None,
        )
        .await?;
        return Err(ApiError::Denied(Denial::UnknownCamera));
    }
    let now = clock::now();
    let issued = clock::format(now);
    let expires = clock::format(now + time::Duration::seconds(edge.grant_ttl_seconds));
    let session_expires = clock::format(now + time::Duration::hours(1));
    let session_id = uuid::Uuid::new_v4().to_string();
    let grant_id = uuid::Uuid::new_v4().to_string();
    let store = state.store.clone();
    let tenant = who.tenant_id.clone();
    let node = body.node_id.clone();
    let camera = body.camera_id.clone();
    let representation_code = representation.code();
    let sid = session_id.clone();
    let gid = grant_id.clone();
    let grant_expires = expires.clone();
    let pid = who.principal_id.clone();
    let created = issued.clone();
    let create_outcome = blocking(move || {
        store
            .create_media_session(
                &tenant,
                &pid,
                &sid,
                &gid,
                &node,
                &camera,
                representation_code,
                &grant_expires,
                &session_expires,
                &created,
            )
            .map_err(ApiError::from)
    })
    .await?;
    let bitrate = match create_outcome {
        MediaSessionCreateOutcome::Created { bitrate_kbps } => bitrate_kbps,
        outcome => {
            let denial = match outcome {
                MediaSessionCreateOutcome::ConcurrentLimit => Denial::ConcurrentSessionLimit,
                MediaSessionCreateOutcome::CameraUnavailable => Denial::UnknownCamera,
                MediaSessionCreateOutcome::PolicyMissing => Denial::PolicyMissing,
                MediaSessionCreateOutcome::CameraNotLiveEnabled => Denial::CameraNotLiveEnabled,
                MediaSessionCreateOutcome::RepresentationNotPermitted => {
                    Denial::RepresentationNotPermitted
                }
                MediaSessionCreateOutcome::Created { .. } => unreachable!(),
            };
            audit(
                &state,
                who.tenant_id.clone(),
                Some(who.principal_id.clone()),
                "grant_denied",
                Some(denial.code()),
                None,
            )
            .await?;
            return Err(ApiError::Denied(denial));
        }
    };
    let grant = MediaGrant {
        protocol_version: MEDIA_SESSION_PROTOCOL_VERSION,
        grant_id,
        session_id: session_id.clone(),
        audience: edge.sfu_region.clone(),
        scope: GrantScope {
            tenant_id: who.tenant_id.clone(),
            site_id: body.node_id,
            camera_id: body.camera_id,
            action,
            representation,
        },
        issued_at_utc: issued.clone(),
        not_before_utc: issued.clone(),
        expires_at_utc: expires.clone(),
        max_bitrate_kbps: bitrate,
    };
    grant.validate().map_err(|e| ApiError::Denied(e.reason()))?;
    let token = sign(&grant, &edge)?;
    let turn_exp = (now.unix_timestamp() + edge.grant_ttl_seconds) as u64;
    let username = format!("{turn_exp}:{session_id}");
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(edge.turn_secret.as_bytes())
        .map_err(|_| ApiError::Internal("TURN signing failed".into()))?;
    mac.update(username.as_bytes());
    let credential = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    let handoff = MediaSessionHandoff {
        protocol_version: MEDIA_SESSION_PROTOCOL_VERSION,
        session_id,
        grant: token,
        grant_expires_at_utc: expires,
        sfu: SfuEndpoint {
            region_id: edge.sfu_region.clone(),
            signaling_url: edge.sfu_signaling_base.clone(),
            ice_servers: vec![IceServer {
                urls: edge.turn_urls.clone(),
                username: Some(username),
                credential: Some(credential),
                credential_ttl_seconds: Some(edge.grant_ttl_seconds as u64),
            }],
        },
        livekit_access_token: AccessToken::with_api_key(
            &edge.livekit_api_key,
            &edge.livekit_api_secret,
        )
        .with_identity(&format!("viewer:{}:{}", who.principal_id, grant.session_id))
        .with_ttl(Duration::from_secs(edge.grant_ttl_seconds as u64))
        .with_grants(VideoGrants {
            room_join: true,
            room: grant.session_id.clone(),
            can_publish: Some(false),
            can_subscribe: Some(true),
            can_publish_data: Some(false),
            can_update_own_metadata: Some(false),
            ..Default::default()
        })
        .to_jwt()
        .map_err(|_| ApiError::Internal("LiveKit token issuance failed".into()))?,
        max_duration_seconds: 3600,
    };
    handoff
        .validate()
        .map_err(|e| ApiError::Denied(e.reason()))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(handoff).map_err(|e| ApiError::Internal(e.to_string()))?),
    ))
}

pub async fn consume(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    let edge = edge(&state)?;
    let presented = bearer(&headers)?;
    if !crate::protocol::hash_matches(
        &crate::protocol::proof_hash(presented),
        &crate::protocol::proof_hash(&edge.sfu_token),
    ) {
        return Err(ApiError::Unauthenticated);
    }
    let value = match payload {
        Ok(Json(value)) => value,
        Err(_) => {
            audit(
                &state,
                UNATTRIBUTED_EDGE_AUDIT_TENANT.into(),
                None,
                "grant_denied",
                Some(Denial::MalformedGrant.code()),
                None,
            )
            .await?;
            return Err(ApiError::Denied(Denial::MalformedGrant));
        }
    };
    let body: ConsumeRequest = match parse_body(value) {
        Ok(body) => body,
        Err(_) => {
            audit(
                &state,
                UNATTRIBUTED_EDGE_AUDIT_TENANT.into(),
                None,
                "grant_denied",
                Some(Denial::MalformedGrant.code()),
                None,
            )
            .await?;
            return Err(ApiError::Denied(Denial::MalformedGrant));
        }
    };
    let grant = match verify(&body.grant, &edge) {
        Ok(grant) => grant,
        Err(ApiError::Denied(reason)) => {
            audit(
                &state,
                UNATTRIBUTED_EDGE_AUDIT_TENANT.into(),
                None,
                "grant_denied",
                Some(reason.code()),
                None,
            )
            .await?;
            return Err(ApiError::Denied(reason));
        }
        Err(error) => return Err(error),
    };
    if grant.audience != edge.sfu_region {
        audit(
            &state,
            grant.scope.tenant_id.clone(),
            None,
            "grant_denied",
            Some(Denial::AudienceMismatch.code()),
            Some(grant.session_id.clone()),
        )
        .await?;
        return Err(ApiError::Denied(Denial::AudienceMismatch));
    }
    if grant.is_usable_at(clock::now()).is_err() {
        audit(
            &state,
            grant.scope.tenant_id.clone(),
            None,
            "grant_denied",
            Some(Denial::GrantExpired.code()),
            Some(grant.session_id.clone()),
        )
        .await?;
        return Err(ApiError::Denied(Denial::GrantExpired));
    }
    let store = state.store.clone();
    let g = grant.clone();
    let now = clock::now_string();
    let result = blocking(move || {
        store
            .consume_media_grant(&g.scope.tenant_id, &g.session_id, &g.grant_id, &now)
            .map_err(ApiError::from)
    })
    .await?;
    let denial = match result {
        0 => None,
        3 => Some(Denial::GrantAlreadyConsumed),
        4 => Some(Denial::GrantExpired),
        _ => Some(Denial::SessionRevoked),
    };
    if let Some(reason) = denial {
        return Err(ApiError::Denied(reason));
    }
    Ok(Json(
        json!({"protocol_version":1,"session_id":grant.session_id,"tenant_id":grant.scope.tenant_id,"site_id":grant.scope.site_id,"camera_id":grant.scope.camera_id,"action":grant.scope.action,"representation":grant.scope.representation,"max_bitrate_kbps":grant.max_bitrate_kbps}),
    ))
}

pub async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> ApiResult<StatusCode> {
    let _edge = edge(&state)?;
    let who = principal(&state, &headers).await?;
    let store = state.store.clone();
    let tenant = who.tenant_id.clone();
    let pid = who.principal_id.clone();
    let sid = session_id.clone();
    let now = clock::now_string();
    let ok = blocking(move || {
        store
            .revoke_media_session(&tenant, &pid, &sid, &now)
            .map_err(ApiError::from)
    })
    .await?;
    if !ok {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn renew(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let edge = edge(&state)?;
    let who = principal(&state, &headers).await?;
    let now = clock::now();
    let issued = clock::format(now);
    let expires = clock::format(now + time::Duration::seconds(edge.grant_ttl_seconds));
    let earliest_usable = clock::format(now + time::Duration::seconds(MIN_RENEWAL_WINDOW_SECONDS));
    let viewer_grant_id = uuid::Uuid::new_v4().to_string();
    let publish_grant_id = uuid::Uuid::new_v4().to_string();
    let store = state.store.clone();
    let tenant = who.tenant_id.clone();
    let principal_id = who.principal_id.clone();
    let sid = session_id.clone();
    let viewer_id = viewer_grant_id.clone();
    let publish_id = publish_grant_id;
    let grant_expiry = expires.clone();
    let now_string = issued.clone();
    let principal_live_allowed = who.has(PERMISSION_VIEW_LIVE);
    let principal_main_allowed = who.has(PERMISSION_VIEW_LIVE_MAIN);
    let outcome = blocking(move || {
        store
            .renew_media_session(
                &tenant,
                &principal_id,
                &sid,
                &viewer_id,
                &publish_id,
                &grant_expiry,
                &earliest_usable,
                &now_string,
                principal_live_allowed,
                principal_main_allowed,
            )
            .map_err(ApiError::from)
    })
    .await?;
    let renewed = match outcome {
        MediaSessionRenewOutcome::Renewed(value) => value,
        MediaSessionRenewOutcome::PolicyDenied => {
            return Err(ApiError::Denied(Denial::RepresentationNotPermitted));
        }
        MediaSessionRenewOutcome::NodeUnavailable => {
            return Err(ApiError::Denied(Denial::NodeUnavailable));
        }
        MediaSessionRenewOutcome::RevokedOrExpired => {
            return Err(ApiError::Denied(Denial::SessionRevoked));
        }
    };
    let representation = Representation::parse(&renewed.representation)
        .ok_or_else(|| ApiError::Internal("stored representation is invalid".into()))?;
    let expires = renewed.grant_expires_at.clone();
    let credential_end = clock::parse(&expires)
        .ok_or_else(|| ApiError::Internal("stored grant expiry is invalid".into()))?;
    let credential_ttl = (credential_end - now).whole_seconds();
    if credential_ttl <= 0 {
        return Err(ApiError::Denied(Denial::SessionRevoked));
    }
    let grant = MediaGrant {
        protocol_version: MEDIA_SESSION_PROTOCOL_VERSION,
        grant_id: viewer_grant_id,
        session_id: session_id.clone(),
        audience: edge.sfu_region.clone(),
        scope: GrantScope {
            tenant_id: who.tenant_id.clone(),
            site_id: renewed.node_id,
            camera_id: renewed.camera_id,
            action: MediaAction::Live,
            representation,
        },
        issued_at_utc: issued.clone(),
        not_before_utc: issued,
        expires_at_utc: expires.clone(),
        max_bitrate_kbps: renewed.max_bitrate_kbps,
    };
    grant.validate().map_err(|e| ApiError::Denied(e.reason()))?;
    let token = sign(&grant, &edge)?;
    let turn_exp = credential_end.unix_timestamp() as u64;
    let username = format!("{turn_exp}:{session_id}");
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(edge.turn_secret.as_bytes())
        .map_err(|_| ApiError::Internal("TURN signing failed".into()))?;
    mac.update(username.as_bytes());
    let credential = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    // `AccessToken` takes a lifetime, not an instant, and stamps `exp` from the
    // clock at `to_jwt()`. Measured from the `now` read at the top of the
    // handler, the token would outlive the clamped end by however long the store
    // call took — the one way a credential here could survive the session. So it
    // is measured as late as possible, against the same clamped instant every
    // other credential is derived from.
    let session_end = clock::parse(&renewed.session_expires_at);
    let at_issue = clock::now();
    let livekit_ttl = (credential_end - at_issue).whole_seconds();
    let remaining = session_end
        .map(|end| (end - at_issue).whole_seconds())
        .unwrap_or(0);
    if livekit_ttl <= 0 || remaining <= 0 {
        return Err(ApiError::Denied(Denial::SessionRevoked));
    }
    let remaining = remaining as u64;
    let handoff = MediaSessionHandoff {
        protocol_version: MEDIA_SESSION_PROTOCOL_VERSION,
        session_id: session_id.clone(),
        grant: token,
        grant_expires_at_utc: expires,
        sfu: SfuEndpoint {
            region_id: edge.sfu_region.clone(),
            signaling_url: edge.sfu_signaling_base.clone(),
            ice_servers: vec![IceServer {
                urls: edge.turn_urls.clone(),
                username: Some(username),
                credential: Some(credential),
                credential_ttl_seconds: Some(credential_ttl as u64),
            }],
        },
        livekit_access_token: AccessToken::with_api_key(
            &edge.livekit_api_key,
            &edge.livekit_api_secret,
        )
        .with_identity(&format!("viewer:{}:{}", who.principal_id, session_id))
        .with_ttl(Duration::from_secs(livekit_ttl as u64))
        .with_grants(VideoGrants {
            room_join: true,
            room: session_id,
            can_publish: Some(false),
            can_subscribe: Some(true),
            can_publish_data: Some(false),
            can_update_own_metadata: Some(false),
            ..Default::default()
        })
        .to_jwt()
        .map_err(|_| ApiError::Internal("LiveKit token issuance failed".into()))?,
        max_duration_seconds: remaining,
    };
    handoff
        .validate()
        .map_err(|e| ApiError::Denied(e.reason()))?;
    Ok(Json(
        serde_json::to_value(handoff).map_err(|e| ApiError::Internal(e.to_string()))?,
    ))
}

pub async fn signing_key(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let edge = edge(&state)?;
    let public = ed25519_dalek::SigningKey::from_bytes(&edge.grant_signing_seed).verifying_key();
    Ok(Json(
        json!({"protocol_version":1,"algorithm":"ed25519","public_key":URL_SAFE_NO_PAD.encode(public.as_bytes())}),
    ))
}
