// ws.rs — WebSocket endpoint.
//
// Frames follow the UI's WsMessageSchema (kind-tagged, camelCase fields).
// On connect we replay the current view of the world: status + every open
// position + every pending order. Then we forward the live `ws_tx` stream.

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::IntoResponse,
};
use tokio::sync::broadcast;
use tracing::debug;

use crate::api::dto::{self, WsFrame};
use crate::state::AppState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Drops the `ws_clients_connected` gauge on scope exit. Using an RAII
/// guard (rather than trailing `decrement` calls) covers every early
/// return path in `handle_socket` without manual bookkeeping.
struct ConnectedGuard;

impl Drop for ConnectedGuard {
    fn drop(&mut self) {
        metrics::gauge!("ctrader_bot_ws_clients_connected").decrement(1.0);
    }
}

async fn send_frame(socket: &mut WebSocket, frame: &WsFrame) -> bool {
    match serde_json::to_string(frame) {
        Ok(text) => socket.send(Message::Text(text)).await.is_ok(),
        Err(_) => true, // a ser error shouldn't kill the socket
    }
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    metrics::gauge!("ctrader_bot_ws_clients_connected").increment(1.0);
    metrics::counter!("ctrader_bot_ws_connections_total").increment(1);
    let _guard = ConnectedGuard;

    // 1) Status snapshot.
    let status_frame = WsFrame::Status {
        connection: dto::ConnectionState::from_raw(state.status()),
    };
    if !send_frame(&mut socket, &status_frame).await {
        return;
    }

    // 2) Replay open positions.
    {
        let positions = state.positions.read().await;
        for p in positions.iter() {
            let frame = WsFrame::PositionUpdate {
                position: dto::Position::from_state(p, &state.symbols),
            };
            if !send_frame(&mut socket, &frame).await {
                return;
            }
        }
    }

    // 3) Replay pending orders.
    {
        let orders = state.orders.read().await;
        for o in orders.iter() {
            let frame = WsFrame::OrderUpdate {
                order: dto::Order::from_state(o, &state.symbols),
            };
            if !send_frame(&mut socket, &frame).await {
                return;
            }
        }
    }

    // 4) Live stream. Client messages (subscribe etc.) are accepted and ignored
    //    for now — the server already pushes everything.
    let mut rx = state.ws_tx.subscribe();
    loop {
        tokio::select! {
            frame = rx.recv() => match frame {
                Ok(f) => {
                    if !send_frame(&mut socket, &f).await {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("WS subscriber lagged by {} frames", n);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}
