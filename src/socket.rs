//! Socket Mode: what `SocketModeClient` did for the Python.
//!
//! Bot mode's event feed. `apps.connections.open` (app-level xapp- token) hands
//! back a single-use wss URL; every envelope has to be acknowledged by its
//! envelope_id or Slack redelivers it. Slack also cycles the connection every
//! ~hour and asks for a reconnect before doing so, so the loop reconnects rather
//! than treating a closed socket as an error.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::bot::AckBot;
use crate::util::Stop;
use crate::{log_debug, log_error, log_warn};

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// Ask Slack for a websocket URL. The app-level token is the only credential that
/// works here -- a bot token answers `invalid_auth`.
fn open_connection(app_token: &str) -> Result<String, String> {
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = http
        .post("https://slack.com/api/apps.connections.open")
        .bearer_auth(app_token)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send()
        .map_err(|e| format!("could not reach Slack: {e}"))?;
    let text = resp.text().map_err(|e| e.to_string())?;
    let data: Value = serde_json::from_str(&text).map_err(|_| {
        let snippet: String = text.chars().take(200).collect();
        format!("apps.connections.open returned non-JSON: {snippet}")
    })?;
    if data.get("ok").and_then(Value::as_bool) != Some(true) {
        let err = data.get("error").and_then(Value::as_str).unwrap_or("unknown");
        let hint = if matches!(err, "invalid_auth" | "not_authed") {
            " -- SLACK_APP_TOKEN must be an xapp- app-level token with connections:write"
        } else {
            ""
        };
        return Err(format!("apps.connections.open: {err}{hint}"));
    }
    data.get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "apps.connections.open returned no url".to_string())
}

/// Handle one envelope: acknowledge it, then dispatch an events_api payload.
/// Returns false when the connection should be re-established.
fn handle(raw: &str, socket: &mut Socket, bot: &AckBot) -> bool {
    let Ok(envelope) = serde_json::from_str::<Value>(raw) else {
        log_debug!("socket: ignoring a non-JSON frame");
        return true;
    };

    // Acknowledge first, exactly as the Python handler did -- an unacked envelope is
    // redelivered, which would ack the same alert twice were the dedupe table not
    // there to stop it.
    if let Some(envelope_id) = envelope.get("envelope_id").and_then(Value::as_str) {
        let response = json!({"envelope_id": envelope_id}).to_string();
        if let Err(e) = socket.send(Message::text(response)) {
            log_warn!("socket: could not acknowledge an envelope: {e}");
            return false;
        }
    }

    match envelope.get("type").and_then(Value::as_str) {
        Some("hello") => {
            log_debug!("socket: hello");
            true
        }
        Some("disconnect") => {
            let reason = envelope.get("reason").and_then(Value::as_str).unwrap_or("unknown");
            log_debug!("socket: Slack asked us to reconnect ({reason})");
            false
        }
        Some("events_api") => {
            let event = envelope
                .get("payload")
                .and_then(|payload| payload.get("event"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            // The Python wrapped this in try/except so one bad event could not kill
            // the listener. on_event() handles its own failures; a panic would take
            // the process down, so catch that too.
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| bot.on_event(&event)));
            if outcome.is_err() {
                log_error!(
                    "handler crashed on event {}",
                    event.get("type").and_then(Value::as_str).unwrap_or("unknown")
                );
            }
            true
        }
        _ => true,
    }
}

/// Reach through the TLS wrapper to the TCP socket so reads can time out -- that
/// timeout is what lets the loop notice `stop` between frames.
fn set_read_timeout(socket: &mut Socket, timeout: Duration) {
    let tcp = match socket.get_mut() {
        MaybeTlsStream::Plain(tcp) => Some(&*tcp),
        MaybeTlsStream::Rustls(tls) => Some(&tls.sock),
        _ => None,
    };
    match tcp {
        Some(tcp) => {
            if let Err(e) = tcp.set_read_timeout(Some(timeout)) {
                log_debug!("socket: could not set a read timeout: {e}");
            }
        }
        None => log_debug!("socket: unfamiliar stream type, no read timeout set"),
    }
}

/// Connect and pump events until `stop` is set. Reconnects on its own.
pub fn run(app_token: String, bot: Arc<AckBot>, stop: Arc<Stop>) {
    let mut attempt: u32 = 0;

    while !stop.is_set() {
        let url = match open_connection(&app_token) {
            Ok(url) => url,
            Err(why) => {
                attempt += 1;
                let wait = (1u64 << (attempt - 1).min(6)).min(60);
                log_error!("{why} -- retrying in {wait}s");
                stop.wait(wait as f64);
                continue;
            }
        };

        let mut socket: Socket = match tungstenite::connect(&url) {
            Ok((socket, _)) => socket,
            Err(e) => {
                attempt += 1;
                let wait = (1u64 << (attempt - 1).min(6)).min(60);
                log_error!("socket connect failed ({e}) -- retrying in {wait}s");
                stop.wait(wait as f64);
                continue;
            }
        };
        attempt = 0;
        log_debug!("connected; waiting for alerts (ctrl-c to stop)");

        // Slack pings every ~30s, so this only fires on a genuinely idle link.
        set_read_timeout(&mut socket, Duration::from_secs(45));

        while !stop.is_set() {
            match socket.read() {
                Ok(Message::Text(text)) => {
                    if !handle(text.as_str(), &mut socket, &bot) {
                        break; // reconnect
                    }
                }
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                    // tungstenite queues the pong itself; flush it.
                    let _ = socket.flush();
                }
                Ok(Message::Close(_)) => {
                    log_debug!("socket: closed by Slack -- reconnecting");
                    break;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    // Idle link: nothing to read, loop round and re-check `stop`.
                    let _ = socket.flush();
                }
                Err(e) => {
                    if !stop.is_set() {
                        log_warn!("socket: read failed ({e}) -- reconnecting");
                    }
                    break;
                }
            }
        }

        let _ = socket.close(None);
        if !stop.is_set() {
            stop.wait(1.0);
        }
    }
}
