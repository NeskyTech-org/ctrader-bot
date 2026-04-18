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

// Конфиг загружается из переменных окружения (.env файл)
#[derive(Debug)]
pub struct Config {
    pub client_id: String,
    pub client_secret: String,
    pub access_token: String,
    pub account_id: i64,
    pub host: String, // demo.ctraderapi.com или live.ctraderapi.com
    pub port: u16,    // 5035
    pub api_addr: SocketAddr,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        Ok(Self {
            client_id: std::env::var("CTRADER_CLIENT_ID")?,
            client_secret: std::env::var("CTRADER_CLIENT_SECRET")?,
            access_token: std::env::var("CTRADER_ACCESS_TOKEN")?,
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
    // fmt layer -> stdout (human or JSON depending on RUST_LOG).
    // WsLogLayer -> WS /ws log channel, gated by the same EnvFilter.
    // EnvFilter default: info for the world, debug for this crate.
    use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,ctrader_bot=debug"));
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(api::log_layer::WsLogLayer)
        .init();

    info!("Запуск cTrader Bot");

    let config = Config::from_env()?;
    info!("Конфиг загружен, аккаунт: {}", config.account_id);

    // 1. Подключаемся + авторизуемся
    let mut conn = connection::CTraderConnection::new(&config).await?;
    info!("TCP/TLS соединение установлено");
    auth::authenticate(&mut conn, &config).await?;
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
