// bot_loop.rs — основной event loop бота.
//
// Один select!:
//   1) входящие сообщения с cTrader
//   2) команды от API
//   3) heartbeat по таймеру
//
// Все исходящие клиенту (WS) события — это `dto::WsFrame`, которые
// отправляются через `state.ws_tx`. API слой просто форвардит их в сокет.

use crate::api::dto::{self, OrderStatus, Side as DtoSide, WsFrame};
use crate::connection::CTraderConnection;
use crate::state::{
    f64_to_proto_volume, order_from_proto, parse_order_type, parse_side, position_from_proto,
    proto, proto_price_to_f64, proto_volume_to_f64, AppState, BotCommand, PlaceOrderArgs,
    PlaceOrderResult, Position, Quote, STATUS_DISCONNECTED, TRADES_CAPACITY,
};
use anyhow::Result;
use prost::Message;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const SUBSCRIBE_SYMBOL_ID: i64 = 1311; // ETHEREUM

fn wrap(payload_type: u32, inner: &impl Message) -> proto::ProtoMessage {
    proto::ProtoMessage {
        payload_type,
        payload: Some(inner.encode_to_vec()),
        client_msg_id: None,
    }
}

/// Fire-and-forget broadcast. `SendError` just means no subscribers.
fn emit(state: &Arc<AppState>, frame: WsFrame) {
    let _ = state.ws_tx.send(frame);
}

fn position_frame(p: &Position, state: &Arc<AppState>) -> WsFrame {
    WsFrame::PositionUpdate {
        position: dto::Position::from_state(p, &state.symbols),
    }
}

fn order_frame(o: &crate::state::Order, state: &Arc<AppState>) -> WsFrame {
    WsFrame::OrderUpdate {
        order: dto::Order::from_state(o, &state.symbols),
    }
}

/// Append a closed-deal Trade to `state.trades`, evicting the oldest entry
/// if we're at capacity. The closing-deal side is the opposite of the
/// position side, so we flip it for the Trade record.
async fn append_trade(
    state: &Arc<AppState>,
    deal: &proto::ProtoOaDeal,
    detail: &proto::ProtoOaClosePositionDetail,
    position_snapshot: &Position,
) {
    let money_digits = detail.money_digits.unwrap_or(2) as i32;
    let scale = 10f64.powi(money_digits);

    // Closing deal's trade_side is the opposite of the position's.
    let position_side = if deal.trade_side == proto::ProtoOaTradeSide::Buy as i32 {
        DtoSide::Sell
    } else {
        DtoSide::Buy
    };

    let volume = detail
        .closed_volume
        .map(proto_volume_to_f64)
        .unwrap_or_else(|| proto_volume_to_f64(deal.filled_volume));

    let trade = dto::Trade {
        id: deal.deal_id.to_string(),
        position_id: deal.position_id.to_string(),
        symbol: state.symbols.name(deal.symbol_id),
        side: position_side,
        volume,
        open_price: detail.entry_price,
        close_price: deal.execution_price.unwrap_or(0.0),
        pnl: detail.gross_profit as f64 / scale,
        opened_at: dto::rfc3339_from_ms(position_snapshot.open_timestamp_ms.unwrap_or(0)),
        closed_at: dto::rfc3339_from_ms(deal.execution_timestamp),
    };

    let mut trades = state.trades.write().await;
    if trades.len() >= TRADES_CAPACITY {
        trades.pop_front();
    }
    trades.push_back(trade);
}

/// Первоначальная синхронизация состояния: позиции и pending ордера.
pub async fn initial_reconcile(conn: &mut CTraderConnection, state: &Arc<AppState>) -> Result<()> {
    debug!("Запрос ProtoOAReconcileReq");

    let req = proto::ProtoOaReconcileReq {
        payload_type: Some(proto::ProtoOaPayloadType::ProtoOaReconcileReq as i32),
        ctid_trader_account_id: state.account_id,
        return_protection_orders: Some(false),
    };
    let envelope = wrap(proto::ProtoOaPayloadType::ProtoOaReconcileReq as u32, &req);
    conn.send(&envelope).await?;

    // Читаем сообщения, пока не придёт ProtoOaReconcileRes (может прийти heartbeat/spot до него)
    loop {
        let raw = conn.recv_raw().await?;
        let msg = proto::ProtoMessage::decode(raw.as_slice())?;
        if msg.payload_type == proto::ProtoOaPayloadType::ProtoOaReconcileRes as u32 {
            if let Some(ref payload) = msg.payload {
                let res = proto::ProtoOaReconcileRes::decode(payload.as_slice())?;
                let positions: Vec<Position> =
                    res.position.iter().map(position_from_proto).collect();
                let orders: Vec<_> = res.order.iter().map(order_from_proto).collect();
                info!(
                    "Reconcile: {} позиций, {} ордеров",
                    positions.len(),
                    orders.len()
                );
                *state.positions.write().await = positions;
                *state.orders.write().await = orders;
            }
            return Ok(());
        }
        // Промежуточные сообщения просто обрабатываем как обычно
        handle_incoming(&msg, state, conn).await?;
    }
}

/// Fetch the full symbol list and populate `AppState::symbols`. Run once
/// after authentication, before `subscribe_to_spots`, so the UI's symbol
/// lookups never hit the fallback placeholder.
pub async fn fetch_symbols(conn: &mut CTraderConnection, state: &Arc<AppState>) -> Result<()> {
    debug!("ProtoOaSymbolsListReq");
    let req = proto::ProtoOaSymbolsListReq {
        payload_type: Some(proto::ProtoOaPayloadType::ProtoOaSymbolsListReq as i32),
        ctid_trader_account_id: state.account_id,
        include_archived_symbols: Some(false),
    };
    let envelope = wrap(
        proto::ProtoOaPayloadType::ProtoOaSymbolsListReq as u32,
        &req,
    );
    conn.send(&envelope).await?;

    loop {
        let raw = conn.recv_raw().await?;
        let msg = proto::ProtoMessage::decode(raw.as_slice())?;
        if msg.payload_type == proto::ProtoOaPayloadType::ProtoOaSymbolsListRes as u32 {
            if let Some(ref payload) = msg.payload {
                let res = proto::ProtoOaSymbolsListRes::decode(payload.as_slice())?;
                let n = state.symbols.populate(
                    res.symbol
                        .iter()
                        .filter_map(|s| s.symbol_name.as_deref().map(|n| (s.symbol_id, n))),
                );
                info!("Symbol catalog populated: {} entries", n);
            }
            return Ok(());
        }
        handle_incoming(&msg, state, conn).await?;
    }
}

pub async fn subscribe_to_spots(conn: &mut CTraderConnection, state: &Arc<AppState>) -> Result<()> {
    let req = proto::ProtoOaSubscribeSpotsReq {
        payload_type: Some(proto::ProtoOaPayloadType::ProtoOaSubscribeSpotsReq as i32),
        ctid_trader_account_id: state.account_id,
        symbol_id: vec![SUBSCRIBE_SYMBOL_ID],
        subscribe_to_spot_timestamp: Some(true),
    };
    let envelope = wrap(
        proto::ProtoOaPayloadType::ProtoOaSubscribeSpotsReq as u32,
        &req,
    );
    conn.send(&envelope).await?;
    info!(
        "Подписка на spot events отправлена (symbol_id={})",
        SUBSCRIBE_SYMBOL_ID
    );
    Ok(())
}

/// Главный event loop. Владеет соединением, получает команды через cmd_rx.
pub async fn run(
    mut conn: CTraderConnection,
    state: Arc<AppState>,
    mut cmd_rx: mpsc::Receiver<BotCommand>,
) -> Result<()> {
    // Status is set to AUTHENTICATED by main.rs after a successful account auth.
    // Broadcast the transition so fresh WS subscribers learn about it.
    emit(
        &state,
        WsFrame::Status {
            connection: dto::ConnectionState::from_raw(state.status()),
        },
    );

    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));

    loop {
        tokio::select! {
            // входящее сообщение
            raw = conn.recv_raw() => {
                let raw = match raw {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("Соединение оборвано: {}", e);
                        state.set_status(STATUS_DISCONNECTED);
                        emit(&state, WsFrame::Status {
                            connection: dto::ConnectionState::Disconnected,
                        });
                        return Err(e);
                    }
                };
                let msg = proto::ProtoMessage::decode(raw.as_slice())?;
                handle_incoming(&msg, &state, &mut conn).await?;
            }

            // команда от API
            Some(cmd) = cmd_rx.recv() => {
                handle_command(cmd, &state, &mut conn).await;
            }

            // heartbeat: cтавим ping в cTrader и одновременно пушим клиенту.
            _ = heartbeat.tick() => {
                let pong = proto::ProtoHeartbeatEvent {
                    payload_type: Some(proto::ProtoPayloadType::HeartbeatEvent as i32),
                };
                let envelope = wrap(proto::ProtoPayloadType::HeartbeatEvent as u32, &pong);
                if let Err(e) = conn.send(&envelope).await {
                    warn!("Не удалось отправить heartbeat: {}", e);
                }
                let now_ms = state.mark_heartbeat();
                emit(&state, WsFrame::Heartbeat { at: now_ms });
            }
        }
    }
}

async fn handle_incoming(
    msg: &proto::ProtoMessage,
    state: &Arc<AppState>,
    conn: &mut CTraderConnection,
) -> Result<()> {
    let pt = msg.payload_type;

    if pt == proto::ProtoPayloadType::HeartbeatEvent as u32 {
        // Сервер пингует — отвечаем. Не трогаем last_heartbeat_at здесь:
        // это broker-пинг, не клиент-пинг.
        let pong = proto::ProtoHeartbeatEvent {
            payload_type: Some(proto::ProtoPayloadType::HeartbeatEvent as i32),
        };
        let envelope = wrap(proto::ProtoPayloadType::HeartbeatEvent as u32, &pong);
        conn.send(&envelope).await?;
    } else if pt == proto::ProtoOaPayloadType::ProtoOaSpotEvent as u32 {
        if let Some(payload) = msg.payload.as_ref() {
            let spot = proto::ProtoOaSpotEvent::decode(payload.as_slice())?;
            let bid = spot.bid.map(proto_price_to_f64);
            let ask = spot.ask.map(proto_price_to_f64);
            let quote = Quote {
                symbol_id: spot.symbol_id,
                bid,
                ask,
                timestamp_ms: spot.timestamp,
            };
            state.quotes.insert(spot.symbol_id, quote);
            // Emit only when we have both sides; otherwise clients can't compute spread.
            if let (Some(bid), Some(ask)) = (bid, ask) {
                emit(
                    state,
                    WsFrame::Tick {
                        symbol: state.symbols.name(spot.symbol_id),
                        bid,
                        ask,
                        time: spot.timestamp.unwrap_or(0).max(0),
                    },
                );
            }
        }
    } else if pt == proto::ProtoOaPayloadType::ProtoOaExecutionEvent as u32 {
        if let Some(payload) = msg.payload.as_ref() {
            let exec = proto::ProtoOaExecutionEvent::decode(payload.as_slice())?;
            apply_execution(&exec, state).await;
        }
    } else if pt == proto::ProtoOaPayloadType::ProtoOaOrderErrorEvent as u32 {
        if let Some(payload) = msg.payload.as_ref() {
            let err = proto::ProtoOaOrderErrorEvent::decode(payload.as_slice())?;
            let description = err.description.clone().unwrap_or_default();
            warn!("OrderError: {} — {}", err.error_code, description);
            if let Some(order_id) = err.order_id {
                emit(
                    state,
                    WsFrame::Execution {
                        order_id: order_id.to_string(),
                        status: OrderStatus::Rejected,
                        detail: Some(format!("{}: {}", err.error_code, description)),
                    },
                );
            }
        }
    } else if pt == proto::ProtoOaPayloadType::ProtoOaErrorRes as u32 {
        if let Some(payload) = msg.payload.as_ref() {
            let err = proto::ProtoOaErrorRes::decode(payload.as_slice())?;
            warn!(
                "OA error: {:?} — {}",
                err.error_code,
                err.description.clone().unwrap_or_default()
            );
        }
    } else {
        debug!("Неизвестный payload_type={}", pt);
    }

    Ok(())
}

async fn apply_execution(exec: &proto::ProtoOaExecutionEvent, state: &Arc<AppState>) {
    // Position side — update or remove, and broadcast the corresponding frame.
    if let Some(pos) = exec.position.as_ref() {
        let closed =
            pos.position_status == proto::ProtoOaPositionStatus::PositionStatusClosed as i32;
        let updated = position_from_proto(pos);

        // Capture a Trade for closing deals before we drop the position.
        if closed {
            if let Some(deal) = exec.deal.as_ref() {
                if let Some(detail) = deal.close_position_detail.as_ref() {
                    append_trade(state, deal, detail, &updated).await;
                }
            }
        }

        {
            let mut positions = state.positions.write().await;
            if let Some(existing) = positions
                .iter_mut()
                .find(|p| p.position_id == updated.position_id)
            {
                *existing = updated.clone();
            } else if !closed {
                positions.push(updated.clone());
            }
            if closed {
                positions.retain(|p| p.position_id != pos.position_id);
            }
        }
        if closed {
            emit(
                state,
                WsFrame::PositionClosed {
                    position_id: pos.position_id.to_string(),
                },
            );
        } else {
            emit(state, position_frame(&updated, state));
        }
    }

    // Order side — update or remove, and emit order_update + execution frames.
    if let Some(order) = exec.order.as_ref() {
        let updated = order_from_proto(order);
        let terminal = matches!(
            order.order_status,
            s if s == proto::ProtoOaOrderStatus::OrderStatusFilled as i32
                || s == proto::ProtoOaOrderStatus::OrderStatusCancelled as i32
                || s == proto::ProtoOaOrderStatus::OrderStatusExpired as i32
                || s == proto::ProtoOaOrderStatus::OrderStatusRejected as i32
        );
        {
            let mut orders = state.orders.write().await;
            if let Some(existing) = orders.iter_mut().find(|o| o.order_id == updated.order_id) {
                *existing = updated.clone();
            } else if !terminal {
                orders.push(updated.clone());
            }
            if terminal {
                orders.retain(|o| o.order_id != order.order_id);
            }
        }
        emit(state, order_frame(&updated, state));
        emit(
            state,
            WsFrame::Execution {
                order_id: order.order_id.to_string(),
                status: OrderStatus::from_internal(updated.status),
                detail: None,
            },
        );
    }
}

async fn handle_command(cmd: BotCommand, state: &Arc<AppState>, conn: &mut CTraderConnection) {
    match cmd {
        BotCommand::PlaceOrder { args, resp } => {
            let result = place_order(args, state, conn).await;
            let _ = resp.send(result);
        }
        BotCommand::CancelOrder { order_id, resp } => {
            let req = proto::ProtoOaCancelOrderReq {
                payload_type: Some(proto::ProtoOaPayloadType::ProtoOaCancelOrderReq as i32),
                ctid_trader_account_id: state.account_id,
                order_id,
            };
            let envelope = wrap(
                proto::ProtoOaPayloadType::ProtoOaCancelOrderReq as u32,
                &req,
            );
            let out = conn.send(&envelope).await.map_err(|e| e.to_string());
            let _ = resp.send(out);
        }
        BotCommand::ClosePosition {
            position_id,
            volume,
            resp,
        } => {
            // Если volume не задан — закрываем всю позицию.
            let vol_proto = match volume {
                Some(v) => f64_to_proto_volume(v),
                None => {
                    let positions = state.positions.read().await;
                    match positions.iter().find(|p| p.position_id == position_id) {
                        Some(p) => f64_to_proto_volume(p.volume),
                        None => {
                            let _ = resp.send(Err("position not found".into()));
                            return;
                        }
                    }
                }
            };
            let req = proto::ProtoOaClosePositionReq {
                payload_type: Some(proto::ProtoOaPayloadType::ProtoOaClosePositionReq as i32),
                ctid_trader_account_id: state.account_id,
                position_id,
                volume: vol_proto,
            };
            let envelope = wrap(
                proto::ProtoOaPayloadType::ProtoOaClosePositionReq as u32,
                &req,
            );
            let out = conn.send(&envelope).await.map_err(|e| e.to_string());
            let _ = resp.send(out);
        }
    }
}

async fn place_order(
    args: PlaceOrderArgs,
    state: &Arc<AppState>,
    conn: &mut CTraderConnection,
) -> Result<PlaceOrderResult, String> {
    let side = parse_side(&args.side).ok_or_else(|| format!("invalid side: {}", args.side))?;
    let order_type = parse_order_type(&args.order_type)
        .ok_or_else(|| format!("invalid order_type: {}", args.order_type))?;

    let req = proto::ProtoOaNewOrderReq {
        payload_type: Some(proto::ProtoOaPayloadType::ProtoOaNewOrderReq as i32),
        ctid_trader_account_id: state.account_id,
        symbol_id: args.symbol_id,
        order_type: order_type as i32,
        trade_side: side as i32,
        volume: f64_to_proto_volume(args.volume),
        limit_price: args.limit_price,
        stop_price: args.stop_price,
        time_in_force: None,
        expiration_timestamp: None,
        stop_loss: args.stop_loss,
        take_profit: args.take_profit,
        comment: None,
        base_slippage_price: None,
        slippage_in_points: None,
        label: None,
        position_id: None,
        client_order_id: None,
        relative_stop_loss: None,
        relative_take_profit: None,
        guaranteed_stop_loss: None,
        trailing_stop_loss: None,
        stop_trigger_method: None,
    };
    let envelope = wrap(proto::ProtoOaPayloadType::ProtoOaNewOrderReq as u32, &req);
    conn.send(&envelope).await.map_err(|e| e.to_string())?;

    Ok(PlaceOrderResult {
        accepted: true,
        message: "order sent to server".into(),
    })
}
