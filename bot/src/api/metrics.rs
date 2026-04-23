// metrics.rs — Prometheus metrics recorder + /metrics endpoint.
//
// We install a single global `PrometheusRecorder` at startup (main.rs).
// Every `metrics::counter!()` / `gauge!()` / `histogram!()` call in the
// crate flows into it. The `PrometheusHandle` it returns is kept in a
// `OnceLock` so the HTTP handler can call `.render()` to produce the
// scrape body on demand.
//
// Gauges that reflect current AppState (open positions, ws clients that
// we don't track per-frame) are refreshed in `snapshot_state_gauges()`
// right before each scrape — that keeps them accurate without a separate
// collector task.

use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::state::AppState;

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the global Prometheus recorder. Must be called once at startup,
/// before any `metrics::*!` macros fire. Idempotent — subsequent calls
/// return without reinstalling.
pub fn install() -> Result<()> {
    if HANDLE.get().is_some() {
        return Ok(());
    }
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow!("failed to install Prometheus recorder: {e}"))?;
    HANDLE
        .set(handle)
        .map_err(|_| anyhow!("Prometheus handle already initialised"))?;
    Ok(())
}

/// `GET /metrics` — Prometheus text exposition format.
///
/// Returns an empty body (still 200 OK) if the recorder hasn't been
/// installed — e.g. in integration tests that build a router without
/// going through `main`.
pub async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    snapshot_state_gauges(&state).await;
    let body = HANDLE.get().map(|h| h.render()).unwrap_or_default();
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

/// Refresh gauges whose value IS the current AppState, not an accumulated
/// counter. Called at scrape time so the handler doesn't need a
/// background ticker.
async fn snapshot_state_gauges(state: &Arc<AppState>) {
    use metrics::gauge;

    gauge!("ctrader_bot_uptime_seconds").set(state.started_at.elapsed().as_secs() as f64);
    gauge!("ctrader_bot_connection_state").set(state.status() as f64);

    let last_hb = state.last_heartbeat_at();
    let age_ms = if last_hb == 0 {
        -1.0
    } else {
        (crate::api::dto::now_unix_ms() - last_hb) as f64
    };
    gauge!("ctrader_bot_last_heartbeat_age_seconds").set(age_ms / 1000.0);

    gauge!("ctrader_bot_positions_open").set(state.positions.read().await.len() as f64);
    gauge!("ctrader_bot_orders_open").set(state.orders.read().await.len() as f64);
    gauge!("ctrader_bot_trades_buffered").set(state.trades.read().await.len() as f64);
    gauge!("ctrader_bot_quotes_tracked").set(state.quotes.len() as f64);
    gauge!("ctrader_bot_symbols_known").set(state.symbols.len() as f64);
}
