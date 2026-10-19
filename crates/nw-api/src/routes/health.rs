//! Health.
//!
//! The endpoint answers from the database, not from a constant: if SQLite
//! cannot be read the service reports unhealthy instead of claiming to be up.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::routes::blocking;
use crate::server::AppState;

pub async fn health(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let store = state.store.clone();
    // The counts are queried and then deliberately dropped: running the query is
    // what proves the database answers, but this endpoint is unauthenticated, so
    // reporting how many tenants or nodes a deployment holds would make it a
    // census anyone can take. Only the schema version, which a deployment's own
    // operator needs and which reveals nothing about its contents, is reported.
    let report = blocking(move || store.health().map_err(ApiError::from)).await?;
    Ok(Json(json!({
        "status": "ok",
        "database": "ok",
        "schema_version": report.schema_version,
    })))
}
