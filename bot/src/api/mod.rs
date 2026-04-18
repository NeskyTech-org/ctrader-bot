// api/mod.rs — сборка Axum router-а.

use crate::state::AppState;
use axum::{
    routing::{delete, get, post, put},
    Router,
};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

pub mod dto;
pub mod routes;
pub mod symbols;
pub mod ws;

pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/api/status", get(routes::status::get_status))
        .route("/api/account", get(routes::account::get_account))
        .route("/api/quotes", get(routes::quotes::get_quotes))
        .route("/api/positions", get(routes::positions::list_positions))
        .route(
            "/api/positions/:id/close",
            post(routes::positions::close_position),
        )
        .route("/api/orders", get(routes::orders::list_orders))
        .route("/api/orders", post(routes::orders::place_order))
        .route("/api/orders/:id", delete(routes::orders::cancel_order))
        .route("/api/trades", get(routes::trades::list_trades))
        .route("/api/strategy", get(routes::strategy::get_strategy))
        .route(
            "/api/strategy/:action",
            post(routes::strategy::control_strategy),
        )
        .route(
            "/api/strategy/params",
            put(routes::strategy::update_strategy_params),
        )
        .route("/ws", get(ws::ws_handler))
        .with_state(state)
        .layer(cors)
}
