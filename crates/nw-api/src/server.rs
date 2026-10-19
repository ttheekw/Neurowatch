//! HTTP service assembly.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;

use crate::config::ApiConfig;
use crate::config::MediaEdgeConfig;
use crate::error::{ApiError, ApiResult};
use crate::identity::PrincipalVerifier;
use crate::store::Store;

/// Shared handler state.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    /// Present only when this deployment was configured with an administrator
    /// credential. When absent the admin routes do not exist.
    pub admin_token: Option<Arc<String>>,
    pub media_edge: Option<Arc<MediaEdgeConfig>>,
    pub principal_verifier: Option<Arc<dyn PrincipalVerifier>>,
}

/// A bound, not yet running, service.
pub struct Server {
    listener: tokio::net::TcpListener,
    router: Router,
    local_addr: SocketAddr,
}

impl Server {
    /// Open the configured database and bind the configured address.
    pub async fn bind(config: &ApiConfig) -> ApiResult<Self> {
        Self::bind_with(config, None).await
    }

    pub async fn bind_with(
        config: &ApiConfig,
        verifier: Option<Arc<dyn PrincipalVerifier>>,
    ) -> ApiResult<Self> {
        let store = Store::open(&config.database)?;
        Self::bind_with_store_and_verifier(config, store, verifier).await
    }

    pub async fn bind_with_store(config: &ApiConfig, store: Store) -> ApiResult<Self> {
        Self::bind_with_store_and_verifier(config, store, None).await
    }

    async fn bind_with_store_and_verifier(
        config: &ApiConfig,
        store: Store,
        verifier: Option<Arc<dyn PrincipalVerifier>>,
    ) -> ApiResult<Self> {
        let state = AppState {
            store,
            admin_token: config.admin_token.clone().map(Arc::new),
            media_edge: config.media_edge.clone().map(Arc::new),
            principal_verifier: verifier,
        };
        if state.admin_token.is_none() {
            tracing::warn!(
                "No administrator credential configured; the admin routes are not mounted"
            );
        }
        let listener = tokio::net::TcpListener::bind(config.bind)
            .await
            .map_err(|error| ApiError::Internal(format!("cannot bind {}: {error}", config.bind)))?;
        let local_addr = listener
            .local_addr()
            .map_err(|error| ApiError::Internal(error.to_string()))?;
        Ok(Self {
            listener,
            router: crate::routes::router(state),
            local_addr,
        })
    }

    /// The address actually bound, which is what a test that asked for port 0
    /// needs.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn run(self, shutdown: impl Future<Output = ()> + Send + 'static) -> ApiResult<()> {
        tracing::info!(address = %self.local_addr, "Control plane listening");
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(|error| ApiError::Internal(error.to_string()))
    }
}
