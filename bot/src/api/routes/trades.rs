use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};
use serde::Deserialize;

use crate::api::dto;
use crate::state::AppState;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TradesQuery {
    #[serde(default)]
    pub page: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Paginated view of the in-memory trade history.
/// Newest trade first. `page` starts at 0. `limit` capped at `MAX_LIMIT`.
pub async fn list_trades(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TradesQuery>,
) -> Json<Vec<dto::Trade>> {
    let page = q.page.unwrap_or(0);
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let trades = state.trades.read().await;
    // VecDeque is oldest-first; we want newest-first in the response.
    let start = page.saturating_mul(limit);
    let out: Vec<dto::Trade> = trades
        .iter()
        .rev()
        .skip(start)
        .take(limit)
        .cloned()
        .collect();
    Json(out)
}
