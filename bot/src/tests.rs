//! Integration-style tests for the Axum router. They exercise the full
//! `build_router` pipeline in-process using `tower::ServiceExt::oneshot`
//! and assert response shapes by deserialising into the DTO types —
//! which IS the wire contract the UI consumes.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::Router;
use http::{Request, StatusCode};
use tokio::sync::{broadcast, mpsc};
use tower::util::ServiceExt;

use crate::api::{build_router, dto};
use crate::state::{AppState, BotCommand, Order, Position, TRADES_CAPACITY};

fn seeded_state() -> Arc<AppState> {
    let (cmd_tx, _cmd_rx) = mpsc::channel::<BotCommand>(8);
    let (ws_tx, _) = broadcast::channel::<dto::WsFrame>(16);
    let state = AppState::new(42, cmd_tx, ws_tx);
    state.symbols.populate([(1, "EURUSD"), (2, "GBPUSD")]);
    state
}

fn app(state: Arc<AppState>) -> Router {
    build_router(state)
}

async fn body_json(router: Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1_000_000).await.unwrap();
    let v: serde_json::Value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, v)
}

#[tokio::test]
async fn get_status_matches_ui_schema() {
    let state = seeded_state();
    let req = Request::builder()
        .uri("/api/status")
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app(state), req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("connection").is_some());
    assert!(body.get("uptimeSeconds").is_some());
    assert_eq!(body["accountId"], "42");
}

#[tokio::test]
async fn get_quotes_returns_map_by_symbol() {
    let state = seeded_state();
    state.quotes.insert(
        1,
        crate::state::Quote {
            symbol_id: 1,
            bid: Some(1.08),
            ask: Some(1.08002),
            timestamp_ms: Some(1_713_440_000_000),
        },
    );
    let req = Request::builder()
        .uri("/api/quotes")
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app(state), req).await;
    assert_eq!(status, StatusCode::OK);
    // Shape: { "EURUSD": { symbol, bid, ask, time } }
    assert!(body.is_object());
    let eurusd = body.get("EURUSD").expect("EURUSD key present");
    assert_eq!(eurusd["symbol"], "EURUSD");
    assert_eq!(eurusd["bid"], 1.08);
}

#[tokio::test]
async fn get_positions_serialises_camel_case() {
    let state = seeded_state();
    state.positions.write().await.push(Position {
        position_id: 7,
        symbol_id: 1,
        side: "BUY",
        volume: 0.1,
        entry_price: Some(1.08234),
        stop_loss: None,
        take_profit: None,
        swap: 0.0,
        commission: 0.0,
        open_timestamp_ms: Some(1_713_440_000_000),
    });
    let req = Request::builder()
        .uri("/api/positions")
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app(state), req).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let p = &arr[0];
    assert_eq!(p["id"], "7");
    assert_eq!(p["symbol"], "EURUSD");
    assert_eq!(p["side"], "buy");
    assert_eq!(p["openPrice"], 1.08234);
    assert!(p["openedAt"].as_str().unwrap().ends_with("Z"));
}

#[tokio::test]
async fn get_orders_renders_price_by_kind() {
    let state = seeded_state();
    state.orders.write().await.push(Order {
        order_id: 1,
        symbol_id: 2,
        side: "SELL",
        volume: 0.1,
        order_type: "LIMIT",
        status: "ACCEPTED",
        limit_price: Some(1.27),
        stop_price: None,
        stop_loss: None,
        take_profit: None,
        created_at_ms: Some(0),
    });
    let req = Request::builder()
        .uri("/api/orders")
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app(state), req).await;
    assert_eq!(status, StatusCode::OK);
    let o = &body[0];
    assert_eq!(o["id"], "1");
    assert_eq!(o["symbol"], "GBPUSD");
    assert_eq!(o["side"], "sell");
    assert_eq!(o["kind"], "limit");
    assert_eq!(o["price"], 1.27);
    assert_eq!(o["status"], "pending");
}

#[tokio::test]
async fn get_trades_paginates_newest_first() {
    let state = seeded_state();
    {
        let mut trades = state.trades.write().await;
        for i in 0..3_i64 {
            trades.push_back(dto::Trade {
                id: format!("d{i}"),
                position_id: format!("p{i}"),
                symbol: "EURUSD".into(),
                side: dto::Side::Buy,
                volume: 0.1,
                open_price: 1.0,
                close_price: 1.1,
                pnl: 10.0,
                opened_at: "2026-04-18T00:00:00Z".into(),
                closed_at: "2026-04-18T00:00:01Z".into(),
            });
        }
    }
    let req = Request::builder()
        .uri("/api/trades?page=0&limit=10")
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app(state), req).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    // Newest first — we pushed d0, d1, d2 oldest-first.
    assert_eq!(arr[0]["id"], "d2");
    assert_eq!(arr[2]["id"], "d0");
}

#[tokio::test]
async fn get_account_503_until_trader_response() {
    let state = seeded_state();
    let req = Request::builder()
        .uri("/api/account")
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app(state), req).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "account_unavailable");
}

#[tokio::test]
async fn get_strategy_returns_stub() {
    let state = seeded_state();
    let req = Request::builder()
        .uri("/api/strategy")
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app(state), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "none");
    assert_eq!(body["state"], "stopped");
    assert!(body["params"].is_object());
}

#[tokio::test]
async fn post_strategy_start_returns_not_implemented_envelope() {
    let state = seeded_state();
    let req = Request::builder()
        .method("POST")
        .uri("/api/strategy/start")
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app(state), req).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "strategy_not_implemented");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not wired up"));
}

#[tokio::test]
async fn post_strategy_unknown_action_returns_400() {
    let state = seeded_state();
    let req = Request::builder()
        .method("POST")
        .uri("/api/strategy/sabotage")
        .body(Body::empty())
        .unwrap();
    let (status, body) = body_json(app(state), req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_action");
}

#[tokio::test]
async fn post_orders_unknown_symbol_returns_400() {
    let state = seeded_state();
    let req = Request::builder()
        .method("POST")
        .uri("/api/orders")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"symbol":"XAUUSD","side":"buy","kind":"market","volume":0.1}"#,
        ))
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_orders_rejects_unknown_fields() {
    let state = seeded_state();
    let req = Request::builder()
        .method("POST")
        .uri("/api/orders")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"symbol":"EURUSD","side":"buy","kind":"market","volume":0.1,"mystery":42}"#,
        ))
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    // axum's Json extractor responds 422/400 on serde failures; accept either.
    let st = resp.status();
    assert!(
        st == StatusCode::BAD_REQUEST || st == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400 or 422, got {st}",
    );
}

#[tokio::test]
async fn post_orders_accepted_by_mocked_bot_loop() {
    // Spawn a minimal fake bot-loop consumer to unblock the POST roundtrip.
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<BotCommand>(8);
    let (ws_tx, _) = broadcast::channel::<dto::WsFrame>(16);
    let state = AppState::new(42, cmd_tx, ws_tx);
    state.symbols.populate([(1, "EURUSD")]);
    // Pretend the placed order is already in state so the route can echo it.
    state.orders.write().await.push(Order {
        order_id: 99,
        symbol_id: 1,
        side: "BUY",
        volume: 0.1,
        order_type: "MARKET",
        status: "ACCEPTED",
        limit_price: None,
        stop_price: None,
        stop_loss: None,
        take_profit: None,
        created_at_ms: Some(0),
    });

    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            if let BotCommand::PlaceOrder { resp, .. } = cmd {
                let _ = resp.send(Ok(crate::state::PlaceOrderResult {
                    accepted: true,
                    message: "ok".into(),
                }));
            }
        }
    });

    let req = Request::builder()
        .method("POST")
        .uri("/api/orders")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"symbol":"EURUSD","side":"buy","kind":"market","volume":0.1}"#,
        ))
        .unwrap();
    // Give the mocked handler room to respond even on slow CI.
    let resp = tokio::time::timeout(Duration::from_secs(2), app(state).oneshot(req))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[test]
fn trades_capacity_is_five_thousand() {
    // Guard against someone accidentally dropping the cap.
    assert_eq!(TRADES_CAPACITY, 5_000);
}

#[tokio::test]
async fn get_metrics_returns_prometheus_text() {
    // The recorder is a global — in the test harness we don't install it
    // (tests share a process, and re-installing panics). The handler
    // returns 200 with an empty body in that case, which is still a
    // valid Prometheus exposition. That's what this test pins:
    // endpoint reachable, correct content type, 2xx.
    let state = seeded_state();
    // Seed some positions + heartbeat so the snapshot path exercises
    // the async reads even without a recorder installed.
    state.positions.write().await.push(Position {
        position_id: 1,
        symbol_id: 1,
        side: "BUY",
        volume: 0.1,
        entry_price: Some(1.0),
        stop_loss: None,
        take_profit: None,
        swap: 0.0,
        commission: 0.0,
        open_timestamp_ms: Some(0),
    });
    state.mark_heartbeat();

    let req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.starts_with("text/plain"),
        "expected text/plain content type, got {ct}"
    );
    // Body is either empty (no recorder in tests) or valid Prometheus
    // text — both are acceptable. If a prior test did install a
    // recorder, we at least confirm the body is UTF-8.
    let body = to_bytes(resp.into_body(), 1_000_000).await.unwrap();
    assert!(std::str::from_utf8(&body).is_ok());
}

#[test]
fn ws_frame_kind_str_matches_serialised_tag() {
    // The Prometheus `kind` label values in `/metrics` must stay in sync
    // with the `kind` tag that serde emits on the wire — otherwise
    // dashboards built on log frames and dashboards built on metrics
    // disagree. Lock both sides by round-tripping one frame per variant.
    use crate::api::dto::{ConnectionState, LogLevel, OrderStatus, Side, WsFrame};

    let samples: Vec<(WsFrame, &'static str)> = vec![
        (
            WsFrame::Tick {
                symbol: "EURUSD".into(),
                bid: 1.0,
                ask: 1.1,
                time: 0,
            },
            "tick",
        ),
        (
            WsFrame::Execution {
                order_id: "1".into(),
                status: OrderStatus::Filled,
                detail: None,
            },
            "execution",
        ),
        (
            WsFrame::PositionUpdate {
                position: dto::Position {
                    id: "1".into(),
                    symbol: "EURUSD".into(),
                    side: Side::Buy,
                    volume: 0.1,
                    open_price: 1.0,
                    pnl: 0.0,
                    swap: 0.0,
                    commission: 0.0,
                    opened_at: "2026-04-18T00:00:00Z".into(),
                },
            },
            "position_update",
        ),
        (
            WsFrame::PositionClosed {
                position_id: "1".into(),
            },
            "position_closed",
        ),
        (
            WsFrame::OrderUpdate {
                order: dto::Order {
                    id: "1".into(),
                    symbol: "EURUSD".into(),
                    side: Side::Buy,
                    kind: dto::OrderKind::Market,
                    volume: 0.1,
                    price: None,
                    stop_loss: None,
                    take_profit: None,
                    status: OrderStatus::Filled,
                    created_at: "2026-04-18T00:00:00Z".into(),
                },
            },
            "order_update",
        ),
        (
            WsFrame::Status {
                connection: ConnectionState::Authenticated,
            },
            "status",
        ),
        (WsFrame::Heartbeat { at: 0 }, "heartbeat"),
        (
            WsFrame::Log {
                level: LogLevel::Info,
                message: "hi".into(),
                fields: None,
                at: "2026-04-18T00:00:00Z".into(),
            },
            "log",
        ),
    ];
    for (frame, expected) in samples {
        assert_eq!(frame.kind_str(), expected, "kind_str mismatch");
        let json: serde_json::Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            json["kind"], expected,
            "serde tag disagrees with kind_str for {expected}"
        );
    }
}
