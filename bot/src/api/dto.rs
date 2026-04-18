// dto.rs — wire-format types that match the Web UI's Zod schemas.
//
// Every REST response and (once PR-B2 lands) every WS frame is one of these
// DTOs. They intentionally diverge from the internal `state::*` types:
// `camelCase` field names, string ids, string symbols, lowercase enums,
// RFC 3339 timestamps. Conversions live in `impl From<&state::X> for X` or
// in explicit `from_state()` helpers where a `SymbolCatalog` is required.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::api::symbols::SymbolCatalog;
use crate::state;

/// Current unix time in milliseconds. Used for `lastHeartbeatAt` and the
/// `at` field on `WsFrame::Heartbeat`.
pub fn now_unix_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// Format a unix-ms instant as RFC 3339 (UTC, second precision, trailing Z).
pub fn rfc3339_from_ms(ms: i64) -> String {
    format_ms_rfc3339(Some(ms))
}

// ─────────────────────────────────────────────────────────── primitive enums

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Authenticated,
}

impl ConnectionState {
    pub fn from_raw(s: u8) -> Self {
        match s {
            state::STATUS_AUTHENTICATED => ConnectionState::Authenticated,
            state::STATUS_CONNECTED => ConnectionState::Connected,
            state::STATUS_CONNECTING => ConnectionState::Connecting,
            _ => ConnectionState::Disconnected,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn from_internal(s: &str) -> Self {
        match s {
            "SELL" => Side::Sell,
            _ => Side::Buy,
        }
    }

    pub fn as_internal(&self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderKind {
    Market,
    Limit,
    Stop,
}

impl OrderKind {
    pub fn from_internal(s: &str) -> Self {
        match s {
            "LIMIT" => OrderKind::Limit,
            "STOP" | "STOP_LIMIT" => OrderKind::Stop,
            _ => OrderKind::Market,
        }
    }

    pub fn as_internal(&self) -> &'static str {
        match self {
            OrderKind::Market => "MARKET",
            OrderKind::Limit => "LIMIT",
            OrderKind::Stop => "STOP",
        }
    }
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Pending,
    Filled,
    Cancelled,
    Rejected,
}

impl OrderStatus {
    pub fn from_internal(s: &str) -> Self {
        match s {
            "FILLED" => OrderStatus::Filled,
            "CANCELLED" => OrderStatus::Cancelled,
            "REJECTED" | "EXPIRED" => OrderStatus::Rejected,
            _ => OrderStatus::Pending,
        }
    }
}

// ─────────────────────────────────────────────────────────────── DTO structs

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub balance: f64,
    pub equity: f64,
    pub margin: f64,
    pub free_margin: f64,
    pub currency: String,
}

/// One closed trade — appended to `AppState::trades` on a closing execution.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Trade {
    pub id: String,
    pub position_id: String,
    pub symbol: String,
    pub side: Side,
    pub volume: f64,
    pub open_price: f64,
    pub close_price: f64,
    pub pnl: f64,
    pub opened_at: String,
    pub closed_at: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StrategyState {
    Stopped,
    Running,
    Paused,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StrategyStatus {
    pub name: String,
    pub state: StrategyState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
    pub params: serde_json::Value,
}

impl Default for StrategyStatus {
    fn default() -> Self {
        StrategyStatus {
            name: "none".into(),
            state: StrategyState::Stopped,
            uptime_seconds: None,
            params: serde_json::json!({}),
        }
    }
}

/// Shared error envelope for non-2xx HTTP responses. Matches the UI's
/// `ApiError` parser: `{ error: { code, message, details? } }`.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ApiErrorBody {
    pub error: ApiErrorDetail,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ApiErrorDetail {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiErrorBody {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiErrorBody {
            error: ApiErrorDetail {
                code: code.into(),
                message: message.into(),
                details: None,
            },
        }
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub connection: ConnectionState,
    pub uptime_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<String>,
}

impl Status {
    pub fn from_state(s: &state::AppState) -> Self {
        let info = s.status_info();
        let last_hb_ms = s.last_heartbeat_at();
        Status {
            connection: ConnectionState::from_raw(s.status()),
            uptime_seconds: info.uptime_secs,
            account_id: if info.account_id == 0 {
                None
            } else {
                Some(info.account_id.to_string())
            },
            last_heartbeat_at: if last_hb_ms > 0 {
                Some(rfc3339_from_ms(last_hb_ms))
            } else {
                None
            },
        }
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub symbol: String,
    pub bid: f64,
    pub ask: f64,
    pub time: i64,
}

impl Quote {
    pub fn from_state(q: &state::Quote, symbols: &SymbolCatalog) -> Self {
        Quote {
            symbol: symbols.name(q.symbol_id),
            bid: q.bid.unwrap_or(0.0),
            ask: q.ask.unwrap_or(0.0),
            time: q.timestamp_ms.unwrap_or(0).max(0),
        }
    }
}

/// Map keyed by symbol name — matches UI's `QuotesMapSchema = z.record(SymbolSchema, QuoteSchema)`.
pub type QuotesMap = HashMap<String, Quote>;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub id: String,
    pub symbol: String,
    pub side: Side,
    pub volume: f64,
    pub open_price: f64,
    pub pnl: f64,
    pub swap: f64,
    pub commission: f64,
    pub opened_at: String,
}

impl Position {
    pub fn from_state(p: &state::Position, symbols: &SymbolCatalog) -> Self {
        Position {
            id: p.position_id.to_string(),
            symbol: symbols.name(p.symbol_id),
            side: Side::from_internal(p.side),
            volume: p.volume,
            open_price: p.entry_price.unwrap_or(0.0),
            pnl: 0.0, // live PnL is computed on the UI from the tick stream;
            // a server-side value lands with B3 (account + trader events).
            swap: p.swap,
            commission: p.commission,
            opened_at: format_ms_rfc3339(p.open_timestamp_ms),
        }
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub id: String,
    pub symbol: String,
    pub side: Side,
    pub kind: OrderKind,
    pub volume: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<f64>,
    pub status: OrderStatus,
    pub created_at: String,
}

impl Order {
    pub fn from_state(o: &state::Order, symbols: &SymbolCatalog) -> Self {
        let kind = OrderKind::from_internal(o.order_type);
        let price = match kind {
            OrderKind::Limit => o.limit_price,
            OrderKind::Stop => o.stop_price,
            OrderKind::Market => None,
        };
        Order {
            id: o.order_id.to_string(),
            symbol: symbols.name(o.symbol_id),
            side: Side::from_internal(o.side),
            kind,
            volume: o.volume,
            price,
            stop_loss: o.stop_loss,
            take_profit: o.take_profit,
            status: OrderStatus::from_internal(o.status),
            created_at: format_ms_rfc3339(o.created_at_ms),
        }
    }
}

// ────────────────────────────────────────────────── WebSocket outbound frames

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Every frame the server pushes to WS clients. Matches the UI's
/// `WsMessageSchema` discriminated union: a `kind` tag + per-variant fields
/// in camelCase.
#[derive(Serialize, Clone, Debug)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum WsFrame {
    Tick {
        symbol: String,
        bid: f64,
        ask: f64,
        time: i64,
    },
    Execution {
        order_id: String,
        status: OrderStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    PositionUpdate {
        position: Position,
    },
    PositionClosed {
        position_id: String,
    },
    OrderUpdate {
        order: Order,
    },
    Status {
        connection: ConnectionState,
    },
    Heartbeat {
        at: i64,
    },
    // Wired up in PR-B4 (tracing Layer -> WS log channel).
    #[allow(dead_code)]
    Log {
        level: LogLevel,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        fields: Option<serde_json::Value>,
        at: String,
    },
}

// ────────────────────────────────────────────────── inbound request DTOs

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaceOrderRequest {
    pub symbol: String,
    pub side: Side,
    pub kind: OrderKind,
    pub volume: f64,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub stop_loss: Option<f64>,
    #[serde(default)]
    pub take_profit: Option<f64>,
}

impl PlaceOrderRequest {
    /// Translate to the internal `PlaceOrderArgs` by resolving the symbol
    /// string to an id via the catalog. Returns an error if the symbol is
    /// unknown (caller should surface as HTTP 400).
    pub fn into_internal(self, symbols: &SymbolCatalog) -> Result<state::PlaceOrderArgs, String> {
        let symbol_id = symbols
            .id(&self.symbol)
            .ok_or_else(|| format!("unknown symbol: {}", self.symbol))?;
        Ok(state::PlaceOrderArgs {
            symbol_id,
            side: self.side.as_internal().to_string(),
            order_type: self.kind.as_internal().to_string(),
            volume: self.volume,
            limit_price: if self.kind == OrderKind::Limit {
                self.price
            } else {
                None
            },
            stop_price: if self.kind == OrderKind::Stop {
                self.price
            } else {
                None
            },
            stop_loss: self.stop_loss,
            take_profit: self.take_profit,
        })
    }
}

// ─────────────────────────────────────────────────────────── helpers

fn format_ms_rfc3339(ms: Option<i64>) -> String {
    let ts = ms.unwrap_or(0);
    match Utc.timestamp_millis_opt(ts).single() {
        Some(dt) => dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        None => DateTime::<Utc>::from_timestamp(0, 0)
            .expect("epoch is a valid timestamp")
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_with(id: i64, name: &str) -> std::sync::Arc<SymbolCatalog> {
        let c = SymbolCatalog::new();
        c.populate([(id, name)]);
        c
    }

    #[test]
    fn side_roundtrips() {
        assert_eq!(Side::from_internal("BUY").as_internal(), "BUY");
        assert_eq!(Side::from_internal("SELL").as_internal(), "SELL");
        assert_eq!(Side::from_internal("garbage"), Side::Buy);
    }

    #[test]
    fn order_kind_normalises_stop_limit() {
        assert_eq!(OrderKind::from_internal("STOP_LIMIT"), OrderKind::Stop);
        assert_eq!(OrderKind::from_internal("MARKET"), OrderKind::Market);
        assert_eq!(OrderKind::from_internal("LIMIT"), OrderKind::Limit);
    }

    #[test]
    fn order_status_rejected_expired() {
        assert_eq!(OrderStatus::from_internal("EXPIRED"), OrderStatus::Rejected);
        assert_eq!(OrderStatus::from_internal("ACCEPTED"), OrderStatus::Pending);
    }

    #[test]
    fn quote_serialises_with_camel_case() {
        let cat = catalog_with(1, "EURUSD");
        let q = Quote::from_state(
            &state::Quote {
                symbol_id: 1,
                bid: Some(1.08),
                ask: Some(1.08002),
                timestamp_ms: Some(1_713_440_000_000),
            },
            &cat,
        );
        let s = serde_json::to_string(&q).unwrap();
        assert!(s.contains("\"symbol\":\"EURUSD\""));
        assert!(s.contains("\"bid\":1.08"));
        assert!(s.contains("\"time\":1713440000000"));
    }

    #[test]
    fn position_serialises_open_price_and_opened_at() {
        let cat = catalog_with(1, "EURUSD");
        let p = Position::from_state(
            &state::Position {
                position_id: 42,
                symbol_id: 1,
                side: "BUY",
                volume: 0.1,
                entry_price: Some(1.08234),
                stop_loss: None,
                take_profit: None,
                swap: 0.0,
                commission: 0.0,
                open_timestamp_ms: Some(1_713_440_000_000),
            },
            &cat,
        );
        assert_eq!(p.id, "42");
        assert_eq!(p.symbol, "EURUSD");
        assert_eq!(p.side, Side::Buy);
        assert_eq!(p.open_price, 1.08234);
        // RFC 3339 fragment check — stable year
        assert!(p.opened_at.starts_with("2024") || p.opened_at.starts_with("2026"));
    }

    #[test]
    fn order_price_follows_kind() {
        let cat = catalog_with(1, "EURUSD");
        let limit = Order::from_state(
            &state::Order {
                order_id: 1,
                symbol_id: 1,
                side: "BUY",
                volume: 0.1,
                order_type: "LIMIT",
                status: "ACCEPTED",
                limit_price: Some(1.08),
                stop_price: None,
                stop_loss: None,
                take_profit: None,
                created_at_ms: Some(0),
            },
            &cat,
        );
        assert_eq!(limit.kind, OrderKind::Limit);
        assert_eq!(limit.price, Some(1.08));

        let stop = Order::from_state(
            &state::Order {
                order_id: 2,
                symbol_id: 1,
                side: "SELL",
                volume: 0.1,
                order_type: "STOP",
                status: "ACCEPTED",
                limit_price: None,
                stop_price: Some(1.07),
                stop_loss: None,
                take_profit: None,
                created_at_ms: Some(0),
            },
            &cat,
        );
        assert_eq!(stop.kind, OrderKind::Stop);
        assert_eq!(stop.price, Some(1.07));
    }

    #[test]
    fn place_order_request_rejects_unknown_symbol() {
        let cat = SymbolCatalog::new();
        cat.populate([(1, "EURUSD")]);
        let req = PlaceOrderRequest {
            symbol: "XAUUSD".to_string(),
            side: Side::Buy,
            kind: OrderKind::Market,
            volume: 1.0,
            price: None,
            stop_loss: None,
            take_profit: None,
        };
        assert!(req.into_internal(&cat).is_err());
    }

    #[test]
    fn place_order_request_translates_market() {
        let cat = SymbolCatalog::new();
        cat.populate([(1, "EURUSD")]);
        let req = PlaceOrderRequest {
            symbol: "EURUSD".to_string(),
            side: Side::Sell,
            kind: OrderKind::Market,
            volume: 0.1,
            price: Some(999.0), // ignored for market
            stop_loss: None,
            take_profit: None,
        };
        let args = req.into_internal(&cat).unwrap();
        assert_eq!(args.symbol_id, 1);
        assert_eq!(args.side, "SELL");
        assert_eq!(args.order_type, "MARKET");
        assert!(args.limit_price.is_none());
        assert!(args.stop_price.is_none());
    }

    #[test]
    fn status_from_state_maps_authenticated() {
        // Using a bare state::AppState without a live connection — status_info()
        // only reads the atomic + started_at, no channels are touched.
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(1);
        let (ws_tx, _) = tokio::sync::broadcast::channel::<WsFrame>(1);
        let state = state::AppState::new(7, cmd_tx, ws_tx);
        state.set_status(state::STATUS_AUTHENTICATED);
        let s = Status::from_state(&state);
        assert_eq!(s.connection, ConnectionState::Authenticated);
        assert_eq!(s.account_id, Some("7".to_string()));
    }

    #[test]
    fn ws_frame_tick_serialises_with_kind_tag() {
        let f = WsFrame::Tick {
            symbol: "EURUSD".to_string(),
            bid: 1.08,
            ask: 1.08002,
            time: 1_713_440_000_000,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"kind\":\"tick\""));
        assert!(s.contains("\"symbol\":\"EURUSD\""));
        assert!(s.contains("\"bid\":1.08"));
    }

    #[test]
    fn ws_frame_execution_uses_camel_case() {
        let f = WsFrame::Execution {
            order_id: "42".to_string(),
            status: OrderStatus::Filled,
            detail: Some("broker ok".into()),
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"kind\":\"execution\""));
        assert!(s.contains("\"orderId\":\"42\""));
        assert!(s.contains("\"status\":\"filled\""));
        assert!(s.contains("\"detail\":\"broker ok\""));
    }

    #[test]
    fn ws_frame_position_closed_emits_position_id() {
        let f = WsFrame::PositionClosed {
            position_id: "7".to_string(),
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"kind\":\"position_closed\""));
        assert!(s.contains("\"positionId\":\"7\""));
    }

    #[test]
    fn ws_frame_heartbeat_carries_timestamp() {
        let f = WsFrame::Heartbeat {
            at: 1_713_440_000_000,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert_eq!(s, "{\"kind\":\"heartbeat\",\"at\":1713440000000}");
    }
}
