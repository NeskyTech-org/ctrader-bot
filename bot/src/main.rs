// prost-generated code contains idiomatic-for-protobuf patterns that clippy
// objects to (enum_variant_names on ProtoOA*, large variants, etc.). Those
// are upstream schema concerns, not ours.
#![allow(clippy::enum_variant_names)]
#![allow(clippy::large_enum_variant)]

mod api;
mod auth;
mod bot_loop;
mod connection;
mod state;

#[cfg(test)]
mod tests;

use anyhow::Result;
use std::net::SocketAddr;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info};

// Конфиг загружается из переменных окружения (.env файл).
//
// `access_token` is mutable: after `auth::authenticate` rotates it via
// `ProtoOaRefreshTokenReq`, the in-memory value is updated and (best
// effort) persisted back to `.env` so the next restart picks up the
// fresh token instead of crashing on CH_ACCESS_TOKEN_INVALID. Same for
// `refresh_token`, which cTrader may also rotate on every refresh.
//
// Access is deliberately a plain `String` rather than a redacting
// newtype — secrets in `.env` get redacted at log sites instead
// (`access_token = %"***"`), which is simpler than refactoring every
// call site that needs the raw string.
#[derive(Debug)]
pub struct Config {
    pub client_id: String,
    pub client_secret: String,
    pub access_token: String,
    /// Long-lived refresh token paired with `access_token`. When present
    /// the bot can recover from an expired access token at startup (or
    /// later — see `auth::authenticate`).
    pub refresh_token: Option<String>,
    pub account_id: i64,
    pub host: String, // demo.ctraderapi.com или live.ctraderapi.com
    pub port: u16,    // 5035
    pub api_addr: SocketAddr,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::from_filename(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"));
        dotenvy::dotenv().ok();
        // `CTRADER_REFRESH_TOKEN` treated as absent when empty (placeholder)
        // so the `.env.example` stub doesn't trip the startup check.
        let refresh_token = std::env::var("CTRADER_REFRESH_TOKEN")
            .ok()
            .filter(|v| !v.is_empty() && v != "your_refresh_token_here");
        Ok(Self {
            client_id: std::env::var("CTRADER_CLIENT_ID")?,
            client_secret: std::env::var("CTRADER_CLIENT_SECRET")?,
            access_token: std::env::var("CTRADER_ACCESS_TOKEN")?,
            refresh_token,
            account_id: std::env::var("CTRADER_ACCOUNT_ID")?.parse()?,
            host: std::env::var("CTRADER_HOST")
                .unwrap_or_else(|_| "demo.ctraderapi.com".to_string()),
            port: std::env::var("CTRADER_PORT")
                .unwrap_or_else(|_| "5035".to_string())
                .parse()?,
            api_addr: std::env::var("API_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:3000".to_string())
                .parse()?,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Layer stack:
    //   fmt       → stdout (human-readable or JSON, toggled by LOG_FORMAT)
    //   WsLogLayer → WS /ws `log` frames for the UI's Logs panel
    //
    // EnvFilter applies to both. Default: info everywhere, debug for this
    // crate. Set `RUST_LOG` to override.
    use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,ctrader_bot=debug"));

    // LOG_FORMAT=json → structured line-delimited JSON, ready for log
    // shippers (Vector, Promtail, filebeat). Anything else (or unset) →
    // the default human-readable ANSI output we've been using for dev.
    let log_format = std::env::var("LOG_FORMAT").unwrap_or_default();
    let fmt_layer: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> = if log_format == "json" {
        Box::new(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false),
        )
    } else {
        Box::new(tracing_subscriber::fmt::layer())
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(api::log_layer::WsLogLayer)
        .init();

    // Install the global Prometheus recorder before any `metrics::*!`
    // macros fire. After this, every counter/gauge/histogram the bot
    // emits is scrape-able at GET /metrics.
    api::metrics::install()?;

    info!("Запуск cTrader Bot");

    let mut config = Config::from_env()?;
    info!("Конфиг загружен, аккаунт: {}", config.account_id);

    // 1. Подключаемся + авторизуемся. `authenticate` may rotate
    //    `config.access_token` / `.refresh_token` via
    //    `ProtoOaRefreshTokenReq` if the saved access_token has expired.
    let mut conn = connection::CTraderConnection::new(&config).await?;
    info!("TCP/TLS соединение установлено");
    auth::authenticate(&mut conn, &mut config).await?;
    info!("Авторизация прошла успешно");

    // 2. Каналы и общий state
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (ws_tx, _) = broadcast::channel(1024);
    // Wire the log layer to our ws channel so tracing events flow to WS.
    api::log_layer::attach_sender(ws_tx.clone());

    let state = state::AppState::new(config.account_id, cmd_tx, ws_tx);
    state.set_status(state::STATUS_AUTHENTICATED);

    // 3. Symbol catalog (populates state.symbols so REST/WS can resolve names)
    bot_loop::fetch_symbols(&mut conn, &state).await?;

    // 4. Trader / account info (seeds state.account for GET /api/account)
    bot_loop::fetch_trader(&mut conn, &state).await?;

    // 5. Initial reconcile + подписка на котировки (до запуска select!)
    bot_loop::initial_reconcile(&mut conn, &state).await?;
    bot_loop::subscribe_to_spots(&mut conn, &state).await?;

    // 4. Bot loop и API сервер в параллели
    let state_for_api = state.clone();
    let api_addr = config.api_addr;

    let bot_handle = tokio::spawn(bot_loop::run(conn, state.clone(), cmd_rx));

    let api_handle = tokio::spawn(async move {
        let router = api::build_router(state_for_api);
        let listener = tokio::net::TcpListener::bind(api_addr).await?;
        info!("API сервер слушает на http://{}", api_addr);
        axum::serve(listener, router).await?;
        Ok::<_, anyhow::Error>(())
    });

    // 5. Ждём первого падения
    tokio::select! {
        res = bot_handle => match res {
            Ok(Ok(())) => info!("Bot loop завершён"),
            Ok(Err(e)) => error!("Bot loop упал: {}", e),
            Err(e) => error!("Bot task panic: {}", e),
        },
        res = api_handle => match res {
            Ok(Ok(())) => info!("API сервер завершён"),
            Ok(Err(e)) => error!("API сервер упал: {}", e),
            Err(e) => error!("API task panic: {}", e),
        },
    }

    Ok(())
}
