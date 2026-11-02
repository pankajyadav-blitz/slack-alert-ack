//! Press a Block Kit button the way the Slack client does. Port of slack_click.py.
//!
//! An interactive button (one with an `action_id` and no `url`) cannot be pressed
//! through Slack's public API -- there is no method for it, by design: an app is
//! only ever *told* about clicks. The Slack web client itself posts the interaction
//! to an internal endpoint, `blocks.actions`, authenticated not with a bot or user
//! OAuth token but with the browser session: an `xoxc-...` client token plus the
//! `d` cookie. Sending that same request is the only way to make the app on the
//! other side believe a human clicked, so it updates the message and acknowledges
//! the incident exactly as it would normally.
//!
//! Consequences worth knowing before relying on this:
//!
//!   * Undocumented. Slack can change the payload shape without notice, so the
//!     caller verifies the effect (see `AckBot::press`) rather than trusting `ok: true`.
//!   * Session-bound. The pair of credentials expires when you sign out of that
//!     browser session, and has to be re-copied. Nothing here can renew them.
//!   * It is you. The click is attributed to the account that owns the session, the
//!     same as if you had pressed it yourself.
//!
//! Getting the credentials, once per session:
//!
//!     Open Slack in a browser, DevTools -> Network, click any button in a message.
//!     Find the `blocks.actions` request.
//!       SLACK_CLIENT_TOKEN -- the `token` form field (xoxc-...)
//!       SLACK_D_COOKIE     -- the `d` cookie from the request's Cookie header
//!                             (xoxd-...); copy it exactly, including any %xx
//!
//!     `ack-bot import-session capture.txt --write` does both from a
//!     DevTools "Copy as cURL" capture.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::log_debug;
use crate::util::now;

const API: &str = "https://slack.com/api";

/// The endpoint is only served to something that looks like the web client.
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct ClickError(pub String);

impl fmt::Display for ClickError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ClickError {}

/// Browser-session credentials for the internal endpoints.
pub struct ClientSession {
    pub token: String,
    pub cookie_d: String,
    /// Some SSO/enterprise workspaces also require the `d-s` cookie; sending only
    /// `d` there looks exactly like an expired session (invalid_auth).
    pub cookie_ds: Option<String>,
    team_id: Mutex<Option<String>>,
    timeout: u64,
    seq: AtomicU64,
    bots: Mutex<HashMap<String, Value>>,
    http: reqwest::blocking::Client,
}

impl ClientSession {
    pub fn new(
        token: &str,
        cookie_d: &str,
        cookie_ds: Option<&str>,
    ) -> Result<ClientSession, ClickError> {
        if !token.starts_with("xoxc-") {
            let prefix = token.split('-').next().unwrap_or(token);
            return Err(ClickError(format!(
                "client token must be an xoxc- token, got {prefix}-... \
                 -- see the docs at the top of click.rs for where to copy it from"
            )));
        }
        // DevTools shortens long strings when it prints them inside an array, and
        // the elided form pastes as a plausible-looking token with a "…" in the
        // middle. Catching it here beats a bewildering invalid_auth later.
        for ellipsis in ["…", "..."] {
            if token.contains(ellipsis) {
                return Err(ClickError(format!(
                    "the client token contains {ellipsis:?}, so it is the shortened form \
                     DevTools printed, not the token. Get the whole value with:\n  \
                     copy(Object.values(JSON.parse(localStorage.localConfig_v2).teams)[0].token)\n\
                     then paste from the clipboard."
                )));
            }
        }
        if !cookie_d.starts_with("xoxd-") {
            return Err(ClickError("the `d` cookie must start with xoxd-".to_string()));
        }

        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| ClickError(format!("cannot build an http client: {e}")))?;

        Ok(ClientSession {
            token: token.to_string(),
            cookie_d: cookie_d.to_string(),
            cookie_ds: cookie_ds.filter(|c| !c.is_empty()).map(str::to_string),
            team_id: Mutex::new(None),
            timeout: 15,
            seq: AtomicU64::new(0),
            bots: Mutex::new(HashMap::new()),
            http,
        })
    }

    pub fn team_id(&self) -> Option<String> {
        self.team_id.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Set the team id unless one is already known -- `sess.team_id = sess.team_id or ...`.
    pub fn set_team_id_if_unset(&self, team_id: Option<&str>) {
        let mut slot = self.team_id.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_deref().is_none_or(str::is_empty) {
            *slot = team_id.filter(|t| !t.is_empty()).map(str::to_string);
        }
    }

    pub fn set_team_id(&self, team_id: Option<&str>) {
        let mut slot = self.team_id.lock().unwrap_or_else(|e| e.into_inner());
        *slot = team_id.filter(|t| !t.is_empty()).map(str::to_string);
    }

    /// Client-side idempotency key; the real client counts up per session.
    pub fn client_token(&self) -> String {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        format!("uid-{seq}-{:.3}", now())
    }

    pub fn cookie_header(&self) -> String {
        let mut jar = format!("d={}", self.cookie_d);
        if let Some(ds) = &self.cookie_ds {
            jar.push_str(&format!("; d-s={ds}"));
        }
        jar
    }

    pub fn call(
        &self,
        method: &str,
        fields: &[(&str, Option<String>)],
    ) -> Result<Value, ClickError> {
        // Sent as multipart with an explicit Cookie header: letting the client
        // build the cookie would re-encode a value that is already encoded.
        let mut form = reqwest::blocking::multipart::Form::new();
        for (key, value) in fields {
            if let Some(value) = value {
                form = form.text(key.to_string(), value.clone());
            }
        }

        let resp = self
            .http
            .post(format!("{API}/{method}"))
            .multipart(form)
            .header("Cookie", self.cookie_header())
            .header("User-Agent", UA)
            .header("Origin", "https://app.slack.com")
            .header("Referer", "https://app.slack.com/")
            .timeout(Duration::from_secs(self.timeout))
            .send()
            .map_err(|e| ClickError(format!("{method} could not be sent: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .map_err(|e| ClickError(format!("{method} response could not be read: {e}")))?;
        let data: Value = serde_json::from_str(&text).map_err(|_| {
            let snippet: String = text.chars().take(200).collect();
            ClickError(format!("{method} returned {}, not JSON: {snippet}", status.as_u16()))
        })?;

        if data.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = data.get("error").and_then(Value::as_str).unwrap_or("unknown");
            let mut hint = String::new();
            if matches!(err, "invalid_auth" | "not_authed" | "token_revoked") {
                hint.push_str(
                    " -- the browser session behind SLACK_CLIENT_TOKEN/SLACK_D_COOKIE is no \
                     longer valid; re-copy both from DevTools",
                );
                if self.cookie_ds.is_none() {
                    hint.push_str(
                        ". If your workspace uses SSO it may also need the `d-s` cookie: \
                         set SLACK_DS_COOKIE",
                    );
                }
            }
            let dump: String = data.to_string().chars().take(300).collect();
            return Err(ClickError(format!("{method}: {err}{hint} | {dump}")));
        }
        Ok(data)
    }

    pub fn whoami(&self) -> Result<Value, ClickError> {
        self.call("auth.test", &[("token", Some(self.token.clone()))])
    }

    /// Resolve a bot_id to its user id / app id (cached; used to address the app).
    pub fn bot_info(&self, bot_id: &str) -> Value {
        {
            let cache = self.bots.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(hit) = cache.get(bot_id) {
                return hit.clone();
            }
        }
        let resolved = match self.call(
            "bots.info",
            &[("token", Some(self.token.clone())), ("bot", Some(bot_id.to_string()))],
        ) {
            Ok(data) => data.get("bot").cloned().unwrap_or_else(|| json!({})),
            Err(exc) => {
                log_debug!("bots.info({bot_id}) failed: {exc}");
                json!({})
            }
        };
        let mut cache = self.bots.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(bot_id.to_string(), resolved.clone());
        resolved
    }
}

/// The one entry of `actions[]`, mirroring what the client sends on a click.
fn action_payload(block_id: &str, element: &Value) -> Value {
    let mut action = serde_json::Map::new();
    action.insert(
        "type".to_string(),
        element
            .get("type")
            .cloned()
            .unwrap_or_else(|| Value::String("button".to_string())),
    );
    action.insert("block_id".to_string(), Value::String(block_id.to_string()));
    action.insert(
        "action_id".to_string(),
        element.get("action_id").cloned().unwrap_or(Value::Null),
    );
    action.insert("action_ts".to_string(), Value::String(format!("{:.6}", now())));
    for key in ["text", "value", "style", "confirm", "url"] {
        if let Some(v) = element.get(key).filter(|v| !v.is_null()) {
            action.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(action)
}

pub struct ClickTarget<'a> {
    pub channel: &'a str,
    pub message_ts: &'a str,
    pub block_id: &'a str,
    pub element: &'a Value,
    pub service_id: &'a str,
    pub blocks: Option<&'a Value>,
    pub thread_ts: Option<&'a str>,
    pub bot_user_id: Option<&'a str>,
    pub is_ephemeral: bool,
}

/// Fire one block action. Returns Slack's response; errors on refusal.
///
/// `service_id` identifies the app to deliver the interaction to, and it is the
/// message's **bot_id** (`B...`) -- not its `app_id`. That is the one field worth
/// stating plainly, because all three wrong answers fail differently and none of
/// them says so (verified against a live workspace):
///
///     service_id = app_id  (A094EK9M8BD)  -> invalid_service_id
///     service_id omitted                  -> invalid_arguments
///     service_id = bot_id  (B094L10NUSZ)  -> ok, and the app acts
///
/// An `ok: true` only means Slack accepted and forwarded the interaction -- it says
/// nothing about what the receiving app did with it. Callers that care (we do) must
/// re-read the message and check it actually changed.
pub fn click(sess: &ClientSession, target: ClickTarget<'_>) -> Result<Value, ClickError> {
    let mut container = json!({
        "type": "message",
        "message_ts": target.message_ts,
        "channel_id": target.channel,
        "is_ephemeral": target.is_ephemeral,
        "is_app_unfurl": false,
    });
    if let Some(thread_ts) = target.thread_ts.filter(|t| !t.is_empty()) {
        container["thread_ts"] = Value::String(thread_ts.to_string());
    }

    let actions = Value::Array(vec![action_payload(target.block_id, target.element)]);

    let fields: Vec<(&str, Option<String>)> = vec![
        ("token", Some(sess.token.clone())),
        ("client_token", Some(sess.client_token())),
        ("channel", Some(target.channel.to_string())),
        ("message_ts", Some(target.message_ts.to_string())),
        ("container", Some(container.to_string())),
        ("actions", Some(actions.to_string())),
        ("state", Some(json!({"values": {}}).to_string())),
        ("service_team_id", sess.team_id()),
        ("bot_user_id", target.bot_user_id.map(str::to_string)),
        ("service_id", Some(target.service_id.to_string())),
        ("blocks", target.blocks.filter(|b| !b.is_null()).map(|b| b.to_string())),
        ("_x_mode", Some("online".to_string())),
        ("_x_sonic", Some("true".to_string())),
        ("_x_app_name", Some("client".to_string())),
    ];

    log_debug!(
        "blocks.actions {}/{} action_id={:?} value={:?}",
        target.channel,
        target.message_ts,
        target.element.get("action_id").and_then(Value::as_str).unwrap_or("-"),
        target.element.get("value").and_then(Value::as_str).unwrap_or("-")
    );
    sess.call("blocks.actions", &fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_the_wrong_token_shape() {
        assert!(ClientSession::new("xoxb-123", "xoxd-abc", None).is_err());
        assert!(ClientSession::new("xoxc-abc…def", "xoxd-abc", None).is_err());
        assert!(ClientSession::new("xoxc-abc", "abc", None).is_err());
        assert!(ClientSession::new("xoxc-abc", "xoxd-abc", None).is_ok());
    }

    #[test]
    fn cookie_header_adds_ds_only_when_present() {
        let sess = ClientSession::new("xoxc-a", "xoxd-b", None).unwrap();
        assert_eq!(sess.cookie_header(), "d=xoxd-b");
        let sess = ClientSession::new("xoxc-a", "xoxd-b", Some("ds")).unwrap();
        assert_eq!(sess.cookie_header(), "d=xoxd-b; d-s=ds");
    }

    #[test]
    fn client_token_counts_up() {
        let sess = ClientSession::new("xoxc-a", "xoxd-b", None).unwrap();
        assert!(sess.client_token().starts_with("uid-1-"));
        assert!(sess.client_token().starts_with("uid-2-"));
    }

    #[test]
    fn action_payload_carries_the_pressable_fields() {
        let element = json!({
            "type": "button", "action_id": "acknowledge_incident", "value": "INC-1",
            "style": "primary", "text": {"type": "plain_text", "text": "Acknowledge"}
        });
        let action = action_payload("blk", &element);
        assert_eq!(action["block_id"], "blk");
        assert_eq!(action["action_id"], "acknowledge_incident");
        assert_eq!(action["value"], "INC-1");
        assert_eq!(action["style"], "primary");
        assert!(action["action_ts"].as_str().unwrap().contains('.'));
        assert!(action.get("url").is_none());
    }
}
