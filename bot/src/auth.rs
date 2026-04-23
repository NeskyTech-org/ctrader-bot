// auth.rs — двухэтапная авторизация в cTrader Open API.
//
// Все сообщения оборачиваются в ProtoMessage (envelope):
//   ProtoMessage { payload_type, payload: Some(inner.encode_to_vec()), client_msg_id }
//
// Шаг 1: ProtoOAApplicationAuthReq → ProtoOAApplicationAuthRes
// Шаг 2: ProtoOAAccountAuthReq     → ProtoOAAccountAuthRes
//
// If Step 2 comes back with `CH_ACCESS_TOKEN_INVALID` AND a refresh
// token is configured, we transparently send `ProtoOaRefreshTokenReq`,
// update the in-memory config (and best-effort the on-disk `.env`),
// and retry account auth exactly once. After two consecutive failures
// we give up and surface the original error.

use crate::connection::CTraderConnection;
use crate::Config;
use anyhow::{bail, Result};
use prost::Message;
use tracing::{debug, info, warn};

mod proto {
    include!(concat!(env!("OUT_DIR"), "/_.rs"));
}

/// Оборачивает inner-сообщение в ProtoMessage envelope
fn wrap(payload_type: u32, inner: &impl Message) -> proto::ProtoMessage {
    proto::ProtoMessage {
        payload_type,
        payload: Some(inner.encode_to_vec()),
        client_msg_id: None,
    }
}

/// Reads a ProtoMessage off the wire and raises on any error payload.
/// Use this when you don't care about discriminating error codes —
/// just want the success response or a bail.
async fn recv_msg(conn: &mut CTraderConnection) -> Result<proto::ProtoMessage> {
    let raw = conn.recv_raw().await?;
    let msg = proto::ProtoMessage::decode(raw.as_slice())?;

    if msg.payload_type == proto::ProtoPayloadType::ErrorRes as u32 {
        if let Some(ref payload) = msg.payload {
            let err = proto::ProtoErrorRes::decode(payload.as_slice())?;
            bail!(
                "Сервер вернул ошибку: {:?} — {}",
                err.error_code,
                err.description.unwrap_or_default()
            );
        }
        bail!("Сервер вернул ErrorRes без payload");
    }

    if msg.payload_type == proto::ProtoOaPayloadType::ProtoOaErrorRes as u32 {
        if let Some(ref payload) = msg.payload {
            let err = proto::ProtoOaErrorRes::decode(payload.as_slice())?;
            bail!(
                "OA ошибка: {:?} — {}",
                err.error_code,
                err.description.unwrap_or_default()
            );
        }
        bail!("Сервер вернул ProtoOAErrorRes без payload");
    }

    Ok(msg)
}

/// Result of a successful account-auth attempt. The boolean lets the
/// caller tell us to handle `CH_ACCESS_TOKEN_INVALID` by refreshing,
/// which the caller then persists.
enum AccountAuthOutcome {
    Ok,
    AccessTokenInvalid { description: String },
}

/// Send AccountAuthReq and classify the response. Swallows the generic
/// OA-error path that `recv_msg` would normally `bail!` on — the caller
/// needs the error code to decide whether to refresh.
async fn try_account_auth(
    conn: &mut CTraderConnection,
    config: &Config,
) -> Result<AccountAuthOutcome> {
    let req = proto::ProtoOaAccountAuthReq {
        payload_type: Some(proto::ProtoOaPayloadType::ProtoOaAccountAuthReq as i32),
        ctid_trader_account_id: config.account_id,
        access_token: config.access_token.clone(),
    };
    let envelope = wrap(
        proto::ProtoOaPayloadType::ProtoOaAccountAuthReq as u32,
        &req,
    );
    conn.send(&envelope).await?;

    let raw = conn.recv_raw().await?;
    let msg = proto::ProtoMessage::decode(raw.as_slice())?;

    if msg.payload_type == proto::ProtoOaPayloadType::ProtoOaAccountAuthRes as u32 {
        return Ok(AccountAuthOutcome::Ok);
    }

    if msg.payload_type == proto::ProtoOaPayloadType::ProtoOaErrorRes as u32 {
        if let Some(payload) = msg.payload.as_ref() {
            let err = proto::ProtoOaErrorRes::decode(payload.as_slice())?;
            let description = err.description.clone().unwrap_or_default();
            if err.error_code == "CH_ACCESS_TOKEN_INVALID" {
                return Ok(AccountAuthOutcome::AccessTokenInvalid { description });
            }
            bail!("OA ошибка: {} — {}", err.error_code, description);
        }
        bail!("Сервер вернул ProtoOAErrorRes без payload");
    }

    bail!(
        "Ожидали AccountAuthRes (2103), получили payload_type={}",
        msg.payload_type
    );
}

pub async fn authenticate(conn: &mut CTraderConnection, config: &mut Config) -> Result<()> {
    // ── Шаг 1: Авторизация приложения ──
    debug!("Шаг 1: авторизация приложения...");

    let app_auth_req = proto::ProtoOaApplicationAuthReq {
        payload_type: Some(proto::ProtoOaPayloadType::ProtoOaApplicationAuthReq as i32),
        client_id: config.client_id.clone(),
        client_secret: config.client_secret.clone(),
    };

    let envelope = wrap(
        proto::ProtoOaPayloadType::ProtoOaApplicationAuthReq as u32,
        &app_auth_req,
    );
    conn.send(&envelope).await?;

    let resp = recv_msg(conn).await?;
    if resp.payload_type != proto::ProtoOaPayloadType::ProtoOaApplicationAuthRes as u32 {
        bail!(
            "Ожидали ApplicationAuthRes (2101), получили payload_type={}",
            resp.payload_type
        );
    }
    debug!("Приложение авторизовано");

    // ── Шаг 2: Авторизация торгового аккаунта ──
    debug!("Шаг 2: авторизация аккаунта {}...", config.account_id);

    match try_account_auth(conn, config).await? {
        AccountAuthOutcome::Ok => {
            info!("Аккаунт {} авторизован успешно", config.account_id);
            return Ok(());
        }
        AccountAuthOutcome::AccessTokenInvalid { description } => {
            let Some(refresh_token) = config.refresh_token.clone() else {
                bail!(
                    "OA ошибка: CH_ACCESS_TOKEN_INVALID — {description}. \
                     Set CTRADER_REFRESH_TOKEN in .env to enable auto-refresh."
                );
            };
            warn!(
                "access_token expired; attempting refresh via CTRADER_REFRESH_TOKEN"
            );
            let rotated = refresh_access_token(conn, &refresh_token).await?;
            config.access_token = rotated.access_token;
            config.refresh_token = Some(rotated.refresh_token);
            if let Err(e) = persist_rotated_tokens(config) {
                // Best-effort: keep the fresh tokens in memory even if we
                // can't rewrite .env. The bot stays up this run; operator
                // will see the warning and can update .env by hand.
                warn!(error = %e, "failed to persist rotated tokens to .env");
            }
        }
    }

    // Retry Step 2 with the refreshed access_token.
    match try_account_auth(conn, config).await? {
        AccountAuthOutcome::Ok => {
            info!(
                "Аккаунт {} авторизован успешно (после refresh токена)",
                config.account_id
            );
            Ok(())
        }
        AccountAuthOutcome::AccessTokenInvalid { description } => {
            bail!(
                "OA ошибка: CH_ACCESS_TOKEN_INVALID after refresh — {description}. \
                 Refresh token may also be expired; regenerate via OAuth flow."
            )
        }
    }
}

struct RefreshedTokens {
    access_token: String,
    refresh_token: String,
    #[allow(dead_code)] // will drive proactive refresh in a follow-up
    expires_in: i64,
}

async fn refresh_access_token(
    conn: &mut CTraderConnection,
    refresh_token: &str,
) -> Result<RefreshedTokens> {
    let req = proto::ProtoOaRefreshTokenReq {
        payload_type: Some(proto::ProtoOaPayloadType::ProtoOaRefreshTokenReq as i32),
        refresh_token: refresh_token.to_string(),
    };
    let envelope = wrap(
        proto::ProtoOaPayloadType::ProtoOaRefreshTokenReq as u32,
        &req,
    );
    conn.send(&envelope).await?;

    let resp = match recv_msg(conn).await {
        Ok(m) => m,
        Err(e) => {
            metrics::counter!(
                "ctrader_bot_token_refresh_total",
                "result" => "error",
            )
            .increment(1);
            return Err(e);
        }
    };

    if resp.payload_type != proto::ProtoOaPayloadType::ProtoOaRefreshTokenRes as u32 {
        metrics::counter!(
            "ctrader_bot_token_refresh_total",
            "result" => "error",
        )
        .increment(1);
        bail!(
            "Ожидали RefreshTokenRes (2174), получили payload_type={}",
            resp.payload_type
        );
    }

    let payload = resp.payload.unwrap_or_default();
    let decoded = proto::ProtoOaRefreshTokenRes::decode(payload.as_slice())?;
    metrics::counter!(
        "ctrader_bot_token_refresh_total",
        "result" => "success",
    )
    .increment(1);
    info!(
        expires_in = decoded.expires_in,
        "access_token refreshed"
    );
    Ok(RefreshedTokens {
        access_token: decoded.access_token,
        refresh_token: decoded.refresh_token,
        expires_in: decoded.expires_in,
    })
}

/// Write refreshed `access_token` / `refresh_token` back to the `.env`
/// file shipped with the source tree. Best-effort: if the file doesn't
/// exist (e.g. running with env vars set from systemd / k8s) we silently
/// skip. If it exists, we replace the two specific lines in place,
/// preserving every other key and comment.
fn persist_rotated_tokens(config: &Config) -> Result<()> {
    use std::path::Path;

    // `CARGO_MANIFEST_DIR` is baked in at compile time; in a shipped
    // binary it points at the build host's source tree, which usually
    // won't exist at runtime — and if it does, the early `exists()`
    // check bails without writing foreign files.
    let env_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    if !env_path.exists() {
        debug!(path = %env_path.display(), "skipping token persist: .env not found");
        return Ok(());
    }

    let original = std::fs::read_to_string(&env_path)?;
    let mut updated = replace_env_key(&original, "CTRADER_ACCESS_TOKEN", &config.access_token);
    if let Some(ref rt) = config.refresh_token {
        updated = replace_env_key(&updated, "CTRADER_REFRESH_TOKEN", rt);
    }
    // Avoid needless writes if nothing actually changed.
    if updated != original {
        std::fs::write(&env_path, updated)?;
        info!(path = %env_path.display(), "rotated tokens persisted to .env");
    }
    Ok(())
}

/// Replace (or append) a single `KEY=value` line inside a .env-style
/// file body. Comments and blank lines are preserved unchanged; a
/// commented-out `# KEY=` is never treated as the live entry.
fn replace_env_key(content: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}=");
    let mut found = false;
    let mut out_lines: Vec<String> = Vec::with_capacity(content.lines().count() + 1);
    for line in content.lines() {
        if line.trim_start().starts_with(&prefix) && !line.trim_start().starts_with('#') {
            out_lines.push(format!("{prefix}{value}"));
            found = true;
        } else {
            out_lines.push(line.to_string());
        }
    }
    if !found {
        out_lines.push(format!("{prefix}{value}"));
    }
    let mut joined = out_lines.join("\n");
    if content.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::replace_env_key;

    #[test]
    fn replaces_existing_key_preserving_other_lines_and_trailing_newline() {
        let original = "\
# comment
CTRADER_CLIENT_ID=abc
CTRADER_ACCESS_TOKEN=old_token
CTRADER_ACCOUNT_ID=1234
";
        let updated = replace_env_key(original, "CTRADER_ACCESS_TOKEN", "new_token");
        assert_eq!(
            updated,
            "\
# comment
CTRADER_CLIENT_ID=abc
CTRADER_ACCESS_TOKEN=new_token
CTRADER_ACCOUNT_ID=1234
"
        );
    }

    #[test]
    fn appends_key_when_missing() {
        let original = "FOO=bar\n";
        let updated = replace_env_key(original, "CTRADER_REFRESH_TOKEN", "rt");
        assert_eq!(updated, "FOO=bar\nCTRADER_REFRESH_TOKEN=rt\n");
    }

    #[test]
    fn skips_commented_out_lines() {
        let original = "# CTRADER_REFRESH_TOKEN=oldcomment\nFOO=bar\n";
        let updated = replace_env_key(original, "CTRADER_REFRESH_TOKEN", "rt");
        assert_eq!(
            updated,
            "# CTRADER_REFRESH_TOKEN=oldcomment\nFOO=bar\nCTRADER_REFRESH_TOKEN=rt\n"
        );
    }

    #[test]
    fn preserves_absence_of_trailing_newline() {
        let original = "FOO=bar";
        let updated = replace_env_key(original, "BAZ", "qux");
        assert_eq!(updated, "FOO=bar\nBAZ=qux");
    }

    #[test]
    fn does_not_match_key_as_substring_of_another() {
        let original = "CTRADER_ACCESS_TOKEN_BACKUP=keep\n";
        let updated = replace_env_key(original, "CTRADER_ACCESS_TOKEN", "new");
        assert_eq!(
            updated,
            "CTRADER_ACCESS_TOKEN_BACKUP=keep\nCTRADER_ACCESS_TOKEN=new\n"
        );
    }
}
