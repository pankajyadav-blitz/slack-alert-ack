//! Provider adapters -- used when the Ack button is interactive, not a link.
//! Scrape the incident id out of the message and ack through the source tool's
//! own API.

use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};

use crate::buttons::message_urls;
use crate::config::ProviderCfg;
use crate::envstore;

#[derive(Debug, Clone)]
pub struct ProviderError(pub String);

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ProviderError {}

fn http() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("http client")
    })
}

fn snippet(text: &str) -> String {
    text.chars().take(200).collect()
}

pub fn ack_pagerduty(cfg: &ProviderCfg, msg: &Value) -> Result<String, ProviderError> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"pagerduty\.com/incidents/([A-Z0-9]+)").unwrap());

    let token = envstore::get_ne(&cfg.get("token_env", "PAGERDUTY_TOKEN"));
    let from_email = cfg.get_opt("from_email");
    let (Some(token), Some(from_email)) = (token, from_email) else {
        return Err(ProviderError("pagerduty: token env or from_email missing".into()));
    };

    let mut ids: Vec<String> = Vec::new();
    for url in message_urls(msg) {
        if let Some(caps) = re.captures(&url) {
            ids.push(caps[1].to_string());
        }
    }
    let Some(incident_id) = ids.first().cloned() else {
        return Err(ProviderError("pagerduty: no incident id found in message".into()));
    };

    let resp = http()
        .put(format!("https://api.pagerduty.com/incidents/{incident_id}"))
        .header("Authorization", format!("Token token={token}"))
        .header("Accept", "application/vnd.pagerduty+json;version=2")
        .header("From", from_email)
        .header("Content-Type", "application/json")
        .json(&json!({"incident": {"type": "incident_reference", "status": "acknowledged"}}))
        .send()
        .map_err(|e| ProviderError(format!("pagerduty: request failed: {e}")))?;

    let status = resp.status().as_u16();
    if status >= 300 {
        let body = resp.text().unwrap_or_default();
        return Err(ProviderError(format!("pagerduty {status}: {}", snippet(&body))));
    }
    Ok(format!("pagerduty:{incident_id}"))
}

pub fn ack_opsgenie(cfg: &ProviderCfg, msg: &Value) -> Result<String, ProviderError> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"opsgenie\.com/alert/detail/([0-9a-fA-F-]{8,})").unwrap());

    let Some(token) = envstore::get_ne(&cfg.get("token_env", "OPSGENIE_TOKEN")) else {
        return Err(ProviderError("opsgenie: token env missing".into()));
    };

    let mut alert_id: Option<String> = None;
    for url in message_urls(msg) {
        if let Some(caps) = re.captures(&url) {
            alert_id = Some(caps[1].to_string());
            break;
        }
    }
    let Some(alert_id) = alert_id else {
        return Err(ProviderError("opsgenie: no alert id found in message".into()));
    };

    let base = cfg.get("api_base", "https://api.opsgenie.com");
    let note = cfg.get("note", "auto-acknowledged");
    let resp = http()
        .post(format!("{base}/v2/alerts/{alert_id}/acknowledge"))
        .header("Authorization", format!("GenieKey {token}"))
        .header("Content-Type", "application/json")
        .json(&json!({"source": "slack-alert-ack", "note": note}))
        .send()
        .map_err(|e| ProviderError(format!("opsgenie: request failed: {e}")))?;

    let status = resp.status().as_u16();
    if status >= 300 {
        let body = resp.text().unwrap_or_default();
        return Err(ProviderError(format!("opsgenie {status}: {}", snippet(&body))));
    }
    Ok(format!("opsgenie:{alert_id}"))
}

/// The dispatch table -- `PROVIDERS` in the Python.
pub fn provider_fn(name: &str) -> Option<fn(&ProviderCfg, &Value) -> Result<String, ProviderError>> {
    match name {
        "pagerduty" => Some(ack_pagerduty),
        "opsgenie" => Some(ack_opsgenie),
        _ => None,
    }
}
