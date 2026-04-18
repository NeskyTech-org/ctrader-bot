use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde_json::Value;

use crate::api::dto;
use crate::state::AppState;

/// Stub: returns a minimal "no strategy" snapshot until the bot grows real
/// strategy plumbing. The UI renders a graceful "unavailable" card off this.
pub async fn get_strategy(State(_state): State<Arc<AppState>>) -> Json<dto::StrategyStatus> {
    Json(dto::StrategyStatus::default())
}

fn not_implemented(code: &str, message: &str) -> (StatusCode, Json<dto::ApiErrorBody>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(dto::ApiErrorBody::new(code, message)),
    )
}

pub async fn control_strategy(
    State(_state): State<Arc<AppState>>,
    Path(action): Path<String>,
) -> Result<Json<dto::StrategyStatus>, (StatusCode, Json<dto::ApiErrorBody>)> {
    let cmd = action.to_ascii_lowercase();
    if !matches!(cmd.as_str(), "start" | "stop" | "pause") {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(dto::ApiErrorBody::new(
                "invalid_action",
                format!("unknown strategy action: {action}"),
            )),
        ));
    }
    Err(not_implemented(
        "strategy_not_implemented",
        "Strategy control is not wired up yet on the bot side.",
    ))
}

pub async fn update_strategy_params(
    State(_state): State<Arc<AppState>>,
    Json(_params): Json<Value>,
) -> Result<Json<dto::StrategyStatus>, (StatusCode, Json<dto::ApiErrorBody>)> {
    Err(not_implemented(
        "strategy_not_implemented",
        "Strategy parameter updates are not supported yet.",
    ))
}
