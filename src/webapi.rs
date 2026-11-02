//! The Slack Web API client, with the retries a weeks-long run will need.
//!
//! Stands in for `make_web_client()` and slack_sdk's retry handlers. The Python
//! docstring is worth keeping, because the reasoning still applies here:
//!
//!   The bare constructor installs no retry handlers at all, so a reset connection
//!   or a 502 from Slack surfaced as a hard error and killed that poll. Retrying
//!   with exponential backoff and jitter belongs inside the client, which is where
//!   a transient network fault belongs -- the poll loop should only see errors that
//!   actually mean something.
//!
//!   The timeout is the one thing standing between a half-open socket and a poll
//!   loop that never ticks again, so it is set explicitly (30s).
//!
//! Every call this client makes is a read (history, list, auth_test,
//! getPermalink), so replaying one is free of side effects.

use std::fmt;
use std::sync::RwLock;
use std::time::Duration;

use serde_json::Value;

use crate::util::{now, sleep};

const API: &str = "https://slack.com/api";
const MAX_RETRIES: u32 = 3;

/// Either Slack said no (`ok: false`, with an `error`), or the call never got an
/// answer. The poll loop branches on exactly this distinction.
#[derive(Debug, Clone)]
pub enum SlackError {
    Api { error: String, retry_after: Option<u64> },
    Network { kind: &'static str, detail: String },
}

impl SlackError {
    /// `exc.response.get("error")` -- None for a transport failure.
    pub fn api_error(&self) -> Option<&str> {
        match self {
            SlackError::Api { error, .. } => Some(error),
            SlackError::Network { .. } => None,
        }
    }

    /// Seconds Slack asked us to wait, from a ratelimited response's header.
    /// Falls back to the caller's value when the header is absent or unparsable.
    pub fn retry_after(&self, default: u64) -> u64 {
        match self {
            SlackError::Api { retry_after, .. } => retry_after.unwrap_or(default).max(1),
            SlackError::Network { .. } => default,
        }
    }

}

impl fmt::Display for SlackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SlackError::Api { error, .. } => write!(f, "{error}"),
            SlackError::Network { kind, detail } => write!(f, "{kind}: {detail}"),
        }
    }
}

impl std::error::Error for SlackError {}

fn classify(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "TimeoutError"
    } else if err.is_connect() {
        "ConnectionError"
    } else if err.is_decode() {
        // Not a parse failure -- see transient(). Named for the Python log line so
        // the two implementations can be grepped together.
        "IncompleteRead"
    } else if err.is_body() {
        "RemoteDisconnected"
    } else {
        "URLError"
    }
}

fn transient(err: &reqwest::Error) -> bool {
    // urllib wrapped only some of these; reqwest reports them all here, and every
    // call this client makes is a read, so replaying one is free of side effects.
    //
    // is_decode() is on this list because reqwest files a *truncated response body*
    // under Kind::Decode rather than Kind::Body: Response::text() collects the body
    // and maps every failure through error::decode(), so hyper's IncompleteMessage
    // -- and a half-finished gzip stream, which the gzip feature makes the likelier
    // shape for a large conversations.list page -- both land there. is_body() only
    // ever covers request-body streaming and never fires on the read path, so
    // without is_decode() a cut-off response was fatal on the first try, exactly as
    // it was in Python before IncompleteRead joined that error list.
    //
    // This is safe only because call_with_token() takes text() and parses it
    // separately: a malformed-JSON decode error cannot reach this function, it is
    // reported as http_{status}. Switching to resp.json() would break that and make
    // the client retry responses that will never parse.
    err.is_timeout() || err.is_connect() || err.is_body() || err.is_request() || err.is_decode()
}

pub struct WebClient {
    token: RwLock<String>,
    http: reqwest::blocking::Client,
}

impl WebClient {
    pub fn new(token: &str) -> WebClient {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("slack-alert-ack (rust)")
            .build()
            .expect("http client");
        WebClient { token: RwLock::new(token.to_string()), http }
    }

    pub fn token(&self) -> String {
        self.token.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_token(&self, token: &str) {
        *self.token.write().unwrap_or_else(|e| e.into_inner()) = token.to_string();
    }

    pub fn call(&self, method: &str, params: &[(&str, String)]) -> Result<Value, SlackError> {
        self.call_with_token(method, params, &self.token())
    }

    pub fn call_with_token(
        &self,
        method: &str,
        params: &[(&str, String)],
        token: &str,
    ) -> Result<Value, SlackError> {
        let url = format!("{API}/{method}");
        let mut connection_tries = 0u32;
        let mut rate_tries = 0u32;

        loop {
            let sent = self
                .http
                .post(&url)
                .bearer_auth(token)
                .form(params)
                .send();

            let resp = match sent {
                Ok(resp) => resp,
                Err(err) => {
                    if transient(&err) && connection_tries < MAX_RETRIES {
                        connection_tries += 1;
                        // exponential backoff with jitter, as slack_sdk does
                        let jitter = (now().fract() * 0.5).abs();
                        sleep((1u64 << (connection_tries - 1)) as f64 + jitter);
                        continue;
                    }
                    return Err(SlackError::Network {
                        kind: classify(&err),
                        detail: err.to_string(),
                    });
                }
            };

            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());

            if status.as_u16() == 429 && rate_tries < MAX_RETRIES {
                rate_tries += 1;
                sleep(retry_after.unwrap_or(1).max(1) as f64);
                continue;
            }

            let text = match resp.text() {
                Ok(text) => text,
                Err(err) => {
                    if transient(&err) && connection_tries < MAX_RETRIES {
                        connection_tries += 1;
                        // Same backoff as the send() branch above, jitter included:
                        // a truncated body is most likely when Slack is under load,
                        // which is precisely when synchronised retries hurt.
                        let jitter = (now().fract() * 0.5).abs();
                        sleep((1u64 << (connection_tries - 1)) as f64 + jitter);
                        continue;
                    }
                    return Err(SlackError::Network {
                        kind: classify(&err),
                        detail: err.to_string(),
                    });
                }
            };

            let data: Value = match serde_json::from_str(&text) {
                Ok(data) => data,
                Err(_) => {
                    // A 5xx from in front of Slack answers HTML. Report it the way
                    // an unrecognised API error is reported, so the caller logs one
                    // line and carries on.
                    let snippet: String = text.chars().take(200).collect();
                    return Err(SlackError::Api {
                        error: format!("http_{}: {}", status.as_u16(), snippet.trim()),
                        retry_after,
                    });
                }
            };

            if data.get("ok").and_then(Value::as_bool) != Some(true) {
                let error = data
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                return Err(SlackError::Api { error, retry_after });
            }
            return Ok(data);
        }
    }

    pub fn auth_test(&self) -> Result<Value, SlackError> {
        self.call("auth.test", &[])
    }

    pub fn conversations_list(
        &self,
        types: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Value, SlackError> {
        let mut params = vec![("types", types.to_string()), ("limit", limit.to_string())];
        if let Some(cursor) = cursor.filter(|c| !c.is_empty()) {
            params.push(("cursor", cursor.to_string()));
        }
        self.call("conversations.list", &params)
    }

    pub fn conversations_history(
        &self,
        channel: &str,
        oldest: Option<&str>,
        latest: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
        inclusive: bool,
    ) -> Result<Value, SlackError> {
        let mut params = vec![
            ("channel", channel.to_string()),
            ("limit", limit.to_string()),
            ("inclusive", if inclusive { "true".to_string() } else { "false".to_string() }),
        ];
        if let Some(oldest) = oldest.filter(|v| !v.is_empty()) {
            params.push(("oldest", oldest.to_string()));
        }
        if let Some(latest) = latest.filter(|v| !v.is_empty()) {
            params.push(("latest", latest.to_string()));
        }
        if let Some(cursor) = cursor.filter(|c| !c.is_empty()) {
            params.push(("cursor", cursor.to_string()));
        }
        self.call("conversations.history", &params)
    }

    pub fn chat_get_permalink(&self, channel: &str, message_ts: &str) -> Result<Value, SlackError> {
        self.call(
            "chat.getPermalink",
            &[
                ("channel", channel.to_string()),
                ("message_ts", message_ts.to_string()),
            ],
        )
    }
}

/// The `messages` array of a history response, or an empty slice.
pub fn messages(resp: &Value) -> &[Value] {
    resp.get("messages")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

/// `response_metadata.next_cursor`, empty treated as absent.
pub fn next_cursor(resp: &Value) -> Option<String> {
    resp.get("response_metadata")?
        .get("next_cursor")?
        .as_str()
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}
