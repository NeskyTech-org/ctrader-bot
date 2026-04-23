use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use tokio::sync::oneshot;

use crate::api::dto;
use crate::state::{AppState, BotCommand};

pub async fn list_orders(State(state): State<Arc<AppState>>) -> Json<Vec<dto::Order>> {
    let orders = state.orders.read().await;
    let out: Vec<dto::Order> = orders
        .iter()
        .map(|o| dto::Order::from_state(o, &state.symbols))
        .collect();
    Json(out)
}

pub async fn place_order(
    State(state): State<Arc<AppState>>,
    Json(req): Json<dto::PlaceOrderRequest>,
) -> Result<Json<dto::Order>, (StatusCode, String)> {
    let side_label = req.side.as_internal();
    let symbol_label = req.symbol.clone();
    let args = req.into_internal(&state.symbols).map_err(|e| {
        metrics::counter!(
            "ctrader_bot_orders_rejected_total",
            "reason" => "bad_request",
            "side" => side_label,
            "symbol" => symbol_label.clone(),
        )
        .increment(1);
        (StatusCode::BAD_REQUEST, e)
    })?;

    let (tx, rx) = oneshot::channel();
    state
        .cmd_tx
        .send(BotCommand::PlaceOrder { args, resp: tx })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rx.await {
        Ok(Ok(result)) => {
            if !result.accepted {
                metrics::counter!(
                    "ctrader_bot_orders_rejected_total",
                    "reason" => "broker_rejected",
                    "side" => side_label,
                    "symbol" => symbol_label.clone(),
                )
                .increment(1);
                return Err((StatusCode::BAD_REQUEST, result.message));
            }
            metrics::counter!(
                "ctrader_bot_orders_placed_total",
                "side" => side_label,
                "symbol" => symbol_label.clone(),
            )
            .increment(1);
            // The concrete Order appears in state.orders after the broker
            // emits an ExecutionEvent. Return the most recent order as a
            // best-effort synchronous handshake; the UI always waits for
            // the WS execution event before treating anything as final.
            let orders = state.orders.read().await;
            let latest = orders.last();
            match latest {
                Some(o) => Ok(Json(dto::Order::from_state(o, &state.symbols))),
                None => Err((
                    StatusCode::ACCEPTED,
                    "order accepted but not yet visible in state".into(),
                )),
            }
        }
        Ok(Err(e)) => {
            metrics::counter!(
                "ctrader_bot_orders_rejected_total",
                "reason" => "bot_error",
                "side" => side_label,
                "symbol" => symbol_label,
            )
            .increment(1);
            Err((StatusCode::BAD_REQUEST, e))
        }
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "bot loop dropped response".into(),
        )),
    }
}

pub async fn cancel_order(
    State(state): State<Arc<AppState>>,
    Path(order_id_str): Path<String>,
) -> Result<Json<dto::Order>, (StatusCode, String)> {
    let order_id: i64 = order_id_str
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid order id".into()))?;

    let (tx, rx) = oneshot::channel();
    state
        .cmd_tx
        .send(BotCommand::CancelOrder { order_id, resp: tx })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rx.await {
        Ok(Ok(())) => {
            let orders = state.orders.read().await;
            let found = orders.iter().find(|o| o.order_id == order_id);
            match found {
                Some(o) => Ok(Json(dto::Order::from_state(o, &state.symbols))),
                None => Err((
                    StatusCode::NOT_FOUND,
                    format!("order {order_id} not found after cancel request"),
                )),
            }
        }
        Ok(Err(e)) => Err((StatusCode::BAD_REQUEST, e)),
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "bot loop dropped response".into(),
        )),
    }
}
