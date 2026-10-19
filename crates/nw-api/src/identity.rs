//! The browser-identity boundary.
//!
//! Media grants may only be issued to a **validated principal** — a specific
//! user, in a specific tenant, with a specific permission set. This crate does
//! not know how to produce one. Deciding that is [EXT-5] in
//! `docs/plans/2026-09-25-production-web-viewing.md` and it is unresolved: no
//! identity provider has been chosen, no accounts exist, and nothing here
//! authenticates anybody.
//!
//! So this module ships the *boundary* and nothing behind it:
//!
//! - [`Principal`] — what a verified identity has to look like.
//! - [`PrincipalVerifier`] — the one interface an identity provider implements.
//! - No implementation, and no fallback.
//!
//! Until a host injects a verifier through [`crate::Server::bind_with`], every
//! grant route answers `503 identity_unavailable`. That is deliberate: the two
//! tempting shortcuts — treating the administrator bearer token as a browser
//! user, or accepting a tenant id from the request body — would each hand every
//! caller every camera, and neither is reachable from this design.
//!
//! [EXT-5]: ../../../docs/plans/2026-09-25-production-web-viewing.md

use std::future::Future;
use std::pin::Pin;

use crate::error::ApiResult;

/// May watch a camera's `sub` representation live.
pub const PERMISSION_VIEW_LIVE: &str = "media.live.view";
/// May watch a camera's `main` representation live. Separate on purpose: the
/// full-rate stream costs bandwidth at the node and the edge.
pub const PERMISSION_VIEW_LIVE_MAIN: &str = "media.live.view_main";

/// A verified browser identity.
///
/// `tenant_id` comes from the verifier, never from the request: it is the
/// single fact every tenant-scoping decision downstream is built on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// Stable, auditable identifier. Must not be, or contain, a credential.
    pub principal_id: String,
    pub tenant_id: String,
    pub permissions: Vec<String>,
}

impl Principal {
    pub fn has(&self, permission: &str) -> bool {
        self.permissions.iter().any(|held| held == permission)
    }
}

/// The future a verifier returns. Spelled out rather than pulled from `futures`
/// so the trait stays object-safe with no extra dependency; a real provider
/// will need to do network work (JWKS fetch, introspection) inside it.
pub type VerifyFuture<'a> = Pin<Box<dyn Future<Output = ApiResult<Principal>> + Send + 'a>>;

/// Turns a presented credential into a [`Principal`], or refuses.
///
/// A verifier must fail closed: any error, including one it cannot classify,
/// has to surface as [`crate::ApiError::Unauthenticated`] rather than as a
/// principal with empty permissions.
pub trait PrincipalVerifier: Send + Sync + 'static {
    /// `credential` is the raw bearer value, with `Bearer ` already stripped.
    fn verify<'a>(&'a self, credential: &'a str) -> VerifyFuture<'a>;

    /// A short name for logs. Never include configuration or secrets.
    fn describe(&self) -> &'static str;
}
