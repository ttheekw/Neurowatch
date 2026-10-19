//! nw-api server entrypoint.
//!
//! Configuration is explicit: without `NW_API_BIND` and `NW_API_DATABASE` the
//! process refuses to start rather than guessing a listener or a data
//! directory, and without `NW_API_ADMIN_TOKEN` it runs with no administrator
//! surface at all.

use anyhow::Context as _;
use nw_api::{ApiConfig, Server};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = ApiConfig::from_env().context("control-plane configuration")?;
    let server = Server::bind(&config)
        .await
        .with_context(|| format!("binding {}", config.bind))?;
    tracing::info!(
        address = %server.local_addr(),
        database = %config.database.display(),
        admin_routes = config.admin_token.is_some(),
        "Starting the NeuroWatch control plane"
    );
    server
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("Shutting down");
        })
        .await
        .context("serving")?;
    Ok(())
}
