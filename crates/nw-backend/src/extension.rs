//! Optional backend extensions.
//!
//! The backend runs on its own: live view, recording, playback, export and
//! discovery need no extension. A build can add one through
//! [`crate::runtime::main_with`], which calls it at three points:
//!
//! 1. [`Extension::prepare`], before any camera starts, to install
//!    [`crate::taps`] on the state;
//! 2. [`Extension::start`], once the local services run and before the IPC
//!    server accepts connections;
//! 3. [`RunningExtension::shutdown`], when the backend stops, before the
//!    cameras stop (so their recordings are still being finalized).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use nw_core::ipc::RpcError;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::app_state::AppState;
use crate::discovery::DiscoveryService;
use crate::stats::PerformanceAggregator;
use crate::viewer::ViewerManager;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The backend's running services, handed to [`Extension::start`].
pub struct Services {
    pub state: Arc<RwLock<AppState>>,
    pub viewer_manager: Arc<ViewerManager>,
    pub stats: Arc<PerformanceAggregator>,
    /// The one camera-search service (its cache and one-search-at-a-time limit).
    pub discovery: Arc<DiscoveryService>,
    /// Clip export jobs, shared by every client.
    pub exports: Arc<crate::export::ExportService>,
    pub config_path: PathBuf,
    /// No desktop UI (`--headless`).
    pub headless: bool,
}

pub trait Extension: Send {
    /// Log directives for the extension's own targets when `RUST_LOG` is
    /// unset, e.g. `"my_extension=info"`.
    fn log_directives(&self) -> Vec<String> {
        Vec::new()
    }

    /// Before any camera starts.
    fn prepare(&mut self, _state: &mut AppState) {}

    /// Once the local services run, before the IPC server starts.
    fn start(self: Box<Self>, services: Services) -> Box<dyn RunningExtension>;
}

pub trait RunningExtension: Send + Sync {
    /// IPC methods this extension serves, if any.
    fn rpc(&self) -> Option<Arc<dyn RpcExtension>> {
        None
    }

    /// The backend is stopping.
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

/// IPC methods served by an extension: every method that starts with
/// [`RpcExtension::namespace`]. They are served only over the protected local
/// socket, never over a TCP endpoint.
pub trait RpcExtension: Send + Sync {
    /// The method prefix, with its dot, e.g. `"example."`. It must not be one
    /// of the backend's own namespaces.
    fn namespace(&self) -> &str;

    fn dispatch<'a>(
        &'a self,
        method: &'a str,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<Value, RpcError>>;
}
