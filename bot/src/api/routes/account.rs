use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};

use crate::api::dto;
use crate::state::AppState;

pub async fn get_account(
    State(state): State<Arc<AppState>>,
) -> Result<Json<dto::Account>, (StatusCode, Json<dto::ApiErrorBody>)> {
    let account = state.account.read().await;
    match account.clone() {
        Some(a) => Ok(Json(a)),
        None => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(dto::ApiErrorBody::new(
                "account_unavailable",
                "Trader info has not arrived yet; try again once the bot is authenticated.",
            )),
        )),
    }
}
