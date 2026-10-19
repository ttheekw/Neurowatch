//! Route table.
//!
//! The `/v1` paths are fixed by the node client and must not be renamed; see
//! [`crate::protocol`]. `/admin/v1` is the portal-facing surface and exists
//! only when an administrator credential is configured.

pub mod admin;
pub mod enrollment;
pub mod health;
pub mod media;
pub mod node;

use axum::routing::{delete, get, post, put};
use axum::Router;

use crate::error::{ApiError, ApiResult};
use crate::server::AppState;

pub fn router(state: AppState) -> Router {
    let node_api = Router::new()
        .route("/health", get(health::health))
        .route(
            "/v1/node-enrollments/{enrollment_id}/claim-parameters",
            post(enrollment::claim_parameters),
        )
        .route(
            "/v1/node-enrollments/{enrollment_id}/claim",
            post(enrollment::claim),
        )
        .route(
            "/v1/node-enrollments/{enrollment_id}/confirm",
            post(enrollment::confirm),
        )
        .route("/v1/node-auth/challenges", post(node::challenge))
        .route(
            "/v1/nodes/{node_id}/metadata-check-ins",
            post(node::check_in),
        )
        .route(
            "/v1/nodes/{node_id}/inventory-snapshots",
            post(node::inventory),
        )
        .route(
            "/v1/nodes/{node_id}/publish-directives",
            post(node::publish_directives),
        )
        .route("/v1/nodes/{node_id}/unlink-indications", post(node::unlink));
    let node_api = node_api
        .route("/v1/media/live-sessions", post(media::open))
        .route(
            "/v1/media/live-sessions/{session_id}",
            delete(media::revoke),
        )
        .route(
            "/v1/media/live-sessions/{session_id}/renewals",
            post(media::renew),
        )
        .route("/v1/media/grant-consumptions", post(media::consume))
        .route("/v1/media/signing-key", get(media::signing_key));

    // No administrator credential, no administrator surface: the routes are
    // absent rather than present-and-open.
    let admin_api = if state.admin_token.is_some() {
        Router::new()
            .route("/admin/v1/tenants", post(admin::create_tenant))
            .route(
                "/admin/v1/tenants/{tenant_id}/node-enrollments",
                post(admin::create_enrollment),
            )
            .route(
                "/admin/v1/tenants/{tenant_id}/nodes",
                get(admin::list_nodes),
            )
            .route(
                "/admin/v1/tenants/{tenant_id}/nodes/{node_id}/cameras",
                get(admin::list_cameras),
            )
            .route(
                "/admin/v1/tenants/{tenant_id}/media-policy",
                put(admin::put_media_policy),
            )
            .route(
                "/admin/v1/tenants/{tenant_id}/media-audit-events",
                get(admin::media_audit),
            )
    } else {
        Router::new()
    };

    node_api.merge(admin_api).with_state(state)
}

/// Decode a request body into its protocol type.
///
/// The deserialiser's own message is deliberately discarded: it quotes the
/// value it rejected, and quoting a rejected value back to the caller (or into
/// a log line) is exactly how an RTSP URL or a password would escape a service
/// that is otherwise careful never to store one.
pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> ApiResult<T> {
    serde_json::from_value(value)
        .map_err(|_| ApiError::BadRequest("the request body does not match the protocol".into()))
}

/// Run a synchronous store operation off the async runtime's worker threads.
pub(crate) async fn blocking<T, F>(work: F) -> ApiResult<T>
where
    F: FnOnce() -> ApiResult<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ApiError::Internal(format!("storage task failed: {error}")))?
}
