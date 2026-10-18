use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::{error::ApiError, AppState};

/// Reports healthy only if the database answers; otherwise a real 503
/// (see `ApiError::Database`) instead of a cheerful empty response.
pub async fn health(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok(Json(json!({ "isSynced": true, "database": "ok" })))
}
