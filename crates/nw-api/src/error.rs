//! Error type for the hosted control plane.
//!
//! Responses carry a stable machine code and a short message. Messages are
//! written for an operator reading a log, never echo a credential, a proof, a
//! signature or a local path, and deliberately do not distinguish "no such
//! enrollment" from "wrong tenant": a caller must not be able to probe another
//! tenant's identifiers through the error text.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The request is malformed or breaks a contract rule.
    #[error("{0}")]
    BadRequest(String),
    /// The caller did not prove who it is.
    #[error("unauthenticated")]
    Unauthenticated,
    /// The caller proved who it is but may not do this.
    #[error("forbidden")]
    Forbidden,
    /// No such object within the caller's tenant.
    #[error("not found")]
    NotFound,
    /// The object exists but is not in a state that allows this (a replayed
    /// one-time proof, a consumed challenge, an already-claimed enrollment).
    #[error("{0}")]
    Conflict(String),
    /// The object existed and has expired.
    #[error("{0}")]
    Expired(String),
    /// A media-session decision, carrying the same stable reason code that is
    /// written to the audit log. The caller and the audit trail therefore
    /// cannot disagree about why something was refused.
    #[error("{}", .0.code())]
    Denied(nw_cloud_protocol::media_session::GrantDenialReason),
    /// A capability this deployment has not been configured for. Distinct from
    /// an internal fault: nothing is broken, the deployment is incomplete.
    #[error("{message}")]
    Unavailable {
        code: &'static str,
        message: &'static str,
    },
    #[error("storage error: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Expired(_) => StatusCode::GONE,
            Self::Denied(reason) => denial_status(*reason),
            Self::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Store(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "invalid_request",
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Expired(_) => "expired",
            Self::Denied(reason) => reason.code(),
            Self::Unavailable { code, .. } => code,
            Self::Store(_) | Self::Internal(_) => "internal_error",
        }
    }

    /// The text sent to the caller. Storage and internal faults are logged in
    /// full and reported as a bare code, so an internal detail never reaches
    /// the network.
    fn public_message(&self) -> String {
        match self {
            Self::Store(error) => {
                tracing::error!(error = %error, "Control-plane storage error");
                "internal error".into()
            }
            Self::Internal(error) => {
                tracing::error!(error = %error, "Control-plane internal error");
                "internal error".into()
            }
            other => other.to_string(),
        }
    }
}

fn denial_status(reason: nw_cloud_protocol::media_session::GrantDenialReason) -> StatusCode {
    use nw_cloud_protocol::media_session::GrantDenialReason as Reason;
    match reason {
        Reason::IdentityUnavailable | Reason::NodeUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        Reason::NotAuthenticated => StatusCode::UNAUTHORIZED,
        Reason::MalformedGrant | Reason::ForbiddenMaterial | Reason::GrantTtlTooLong => {
            StatusCode::BAD_REQUEST
        }
        Reason::ActionNotSupported => StatusCode::NOT_IMPLEMENTED,
        Reason::GrantExpired => StatusCode::GONE,
        Reason::GrantAlreadyConsumed => StatusCode::CONFLICT,
        Reason::UnknownCamera => StatusCode::NOT_FOUND,
        Reason::ConcurrentSessionLimit => StatusCode::TOO_MANY_REQUESTS,
        Reason::GrantSignatureInvalid
        | Reason::AudienceMismatch
        | Reason::CameraNotLiveEnabled
        | Reason::TenantMismatch
        | Reason::RepresentationNotPermitted
        | Reason::PolicyMissing
        | Reason::SessionRevoked => StatusCode::FORBIDDEN,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = json!({"error": self.code(), "message": self.public_message()});
        (status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
