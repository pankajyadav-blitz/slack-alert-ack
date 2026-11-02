//! Button extraction and message rendering. Port of the middle of ack_bot.py.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default)]
pub struct Button {
    pub text: String,
    pub url: Option<String>,
    pub action_id: Option<String>,
    pub value: Option<String>,
    /// `block_id` and the raw element are what blocks.actions needs to identify
    /// which button of which block was pressed; only Block Kit buttons have them.
    pub block_id: Option<String>,
    pub element: Value,
}

impl Button {
    /// True for link buttons -- their ack is a plain HTTP request.
    pub fn clickable(&self) -> bool {
        self.url.as_ref().is_some_and(|u| !u.is_empty())
    }

    /// True for interactive buttons we can address with blocks.actions.
    pub fn pressable(&self) -> bool {
        self.action_id.as_ref().is_some_and(|a| !a.is_empty())
            && self.block_id.as_ref().is_some_and(|b| !b.is_empty())
            && !self.clickable()
    }
}

/// Python truthiness, for the `or`/`if x` checks this code is a port of.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `str(x)` for a JSON value, near enough for the strings Slack sends.
fn as_string(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    }
}

/// `str(x or "")`.
fn as_string_or_empty(value: Option<&Value>) -> String {
    if truthy(value) {
        as_string(value).unwrap_or_default()
    } else {
        String::new()
    }
}

fn button_text(el: &Value) -> String {
    match el.get("text") {
        Some(Value::Object(o)) => as_string(o.get("text")).unwrap_or_default(),
        other => as_string_or_empty(other),
    }
}

fn empty_object() -> &'static Value {
    static EMPTY: OnceLock<Value> = OnceLock::new();
    EMPTY.get_or_init(|| Value::Object(Map::new()))
}

fn walk_blocks(blocks: &[Value], parent_block_id: Option<&str>, found: &mut Vec<Button>) {
    for block in blocks {
        let Some(obj) = block.as_object() else { continue };
        // Nested containers carry no block_id of their own; a click is still
        // addressed by the id of the top-level block it sits in.
        let block_id = obj
            .get("block_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or(parent_block_id);

        let mut elements: Vec<&Value> = obj
            .get("elements")
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        if let Some(accessory) = obj.get("accessory").filter(|v| v.is_object()) {
            elements.push(accessory);
        }

        for el in elements {
            let Some(el_obj) = el.as_object() else { continue };
            if el_obj.get("type").and_then(Value::as_str) == Some("button") {
                found.push(Button {
                    text: button_text(el),
                    url: as_string(el_obj.get("url")),
                    action_id: as_string(el_obj.get("action_id")),
                    value: as_string(el_obj.get("value")),
                    block_id: block_id.map(str::to_string),
                    element: el.clone(),
                });
            } else if truthy(el_obj.get("elements")) {
                // rich_text / nested containers
                walk_blocks(std::slice::from_ref(el), block_id, found);
            }
        }
    }
}

/// Pull every button out of a message's blocks and (legacy) attachments.
pub fn extract_buttons(msg: &Value) -> Vec<Button> {
    let mut found: Vec<Button> = Vec::new();

    if let Some(blocks) = msg.get("blocks").and_then(Value::as_array) {
        walk_blocks(blocks, None, &mut found);
    }

    for att in msg.get("attachments").and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[]) {
        let Some(att_obj) = att.as_object() else { continue };
        if let Some(blocks) = att_obj.get("blocks").and_then(Value::as_array) {
            walk_blocks(blocks, None, &mut found);
        }
        // legacy attachment actions
        for action in att_obj.get("actions").and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[]) {
            let Some(act) = action.as_object() else { continue };
            let ty = act.get("type");
            let is_button = match ty {
                None | Some(Value::Null) => true,
                Some(Value::String(s)) => s == "button",
                _ => false,
            };
            if is_button {
                found.push(Button {
                    text: as_string_or_empty(act.get("text")),
                    url: as_string(act.get("url")),
                    action_id: as_string(act.get("action_id")).or_else(|| as_string(act.get("name"))),
                    value: as_string(act.get("value")),
                    block_id: None,
                    element: empty_object().clone(),
                });
            }
        }
    }
    found
}

fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"https?://[^\s"'<>|\\)\]]+"#).unwrap())
}

/// Every URL anywhere in the message -- text, blocks, attachments.
pub fn message_urls(msg: &Value) -> Vec<String> {
    let mut probe = Map::new();
    for key in ["text", "blocks", "attachments"] {
        probe.insert(key.to_string(), msg.get(key).cloned().unwrap_or(Value::Null));
    }
    let blob = Value::Object(probe).to_string();
    url_re()
        .find_iter(&blob)
        .map(|m| m.as_str().trim_end_matches(['.', ',', ';']).to_string())
        .collect()
}

/// One-line log form: the tail of a long alert is rarely the useful part.
pub fn clip(text: &str, limit: usize) -> String {
    let text = if text.is_empty() { "(no text)" } else { text };
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{head}…")
}

pub fn clip_default(text: &str) -> String {
    clip(text, 220)
}

const SKIP_ELEMENTS: [&str; 8] = [
    "button",
    "users_select",
    "static_select",
    "conversations_select",
    "channels_select",
    "external_select",
    "overflow",
    "datepicker",
];

fn walk_text(node: &Value, parts: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            let ty = map.get("type").and_then(Value::as_str).unwrap_or("");
            if SKIP_ELEMENTS.contains(&ty) {
                return;
            }
            if ty == "mrkdwn" || ty == "plain_text" {
                if let Some(t) = map.get("text").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        parts.push(t.trim().to_string());
                        return;
                    }
                }
            }
            for value in map.values() {
                walk_text(value, parts);
            }
        }
        Value::Array(items) => {
            for value in items {
                walk_text(value, parts);
            }
        }
        _ => {}
    }
}

/// The human-readable body of a Block Kit message, as one line.
///
/// Alert tools put nothing useful in `text` (it is a notification fallback) and
/// everything in `blocks`, so the interesting content -- alarm name, state,
/// threshold, reason -- has to be gathered out of section/field/context blocks.
/// Interactive elements are skipped because their labels are logged separately.
pub fn message_text(msg: &Value) -> String {
    static MAILTO: OnceLock<Regex> = OnceLock::new();
    static LINK: OnceLock<Regex> = OnceLock::new();
    static MARKUP: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();

    let mut parts: Vec<String> = Vec::new();
    if let Some(blocks) = msg.get("blocks").and_then(Value::as_array) {
        walk_text(&Value::Array(blocks.clone()), &mut parts);
    }
    if parts.is_empty() {
        if let Some(text) = msg.get("text").and_then(Value::as_str) {
            parts.push(text.to_string());
        }
    }
    for att in msg.get("attachments").and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[]) {
        for key in ["title", "text", "fallback"] {
            if let Some(v) = att.get(key).and_then(Value::as_str) {
                if !v.trim().is_empty() {
                    parts.push(v.trim().to_string());
                    break;
                }
            }
        }
    }

    // dedupe, keep order
    let mut seen: HashSet<&str> = HashSet::new();
    let mut unique: Vec<&str> = Vec::with_capacity(parts.len());
    for part in &parts {
        if seen.insert(part.as_str()) {
            unique.push(part.as_str());
        }
    }
    let out = unique.join(" | ");

    let mailto = MAILTO.get_or_init(|| Regex::new(r"<mailto:[^|>]+\|([^>]+)>").unwrap());
    let link = LINK.get_or_init(|| Regex::new(r"<(https?://[^|>]+)\|([^>]+)>").unwrap());
    let markup = MARKUP.get_or_init(|| Regex::new(r"[*`]").unwrap());
    let spaces = SPACES.get_or_init(|| Regex::new(r"\s+").unwrap());

    let out = mailto.replace_all(&out, "${1}").to_string();
    let out = link.replace_all(&out, "${2}").to_string();
    let out = out.replace("&gt;", "").replace("&lt;", "<").replace("&amp;", "&");
    // Underscores are left alone: mangling :rotating_light: into :rotatinglight:
    // is worse than leaving the odd _italic_ marker in a log line.
    let out = markup.replace_all(&out, "").to_string();
    spaces.replace_all(&out, " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_link_and_interactive_buttons() {
        let msg = json!({
            "blocks": [
                {"type": "actions", "block_id": "b1", "elements": [
                    {"type": "button", "text": {"type": "plain_text", "text": "Acknowledge"},
                     "action_id": "acknowledge_incident", "value": "INC-1"},
                    {"type": "button", "text": {"type": "plain_text", "text": "Runbook"},
                     "url": "https://alerts.example.com/ack/1"}
                ]}
            ]
        });
        let buttons = extract_buttons(&msg);
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0].text, "Acknowledge");
        assert!(buttons[0].pressable() && !buttons[0].clickable());
        assert_eq!(buttons[0].block_id.as_deref(), Some("b1"));
        assert!(buttons[1].clickable() && !buttons[1].pressable());
    }

    #[test]
    fn nested_containers_inherit_the_outer_block_id() {
        let msg = json!({
            "blocks": [
                {"type": "section", "block_id": "outer", "elements": [
                    {"type": "rich_text_section", "elements": [
                        {"type": "button", "text": "Ack", "action_id": "ack"}
                    ]}
                ]}
            ]
        });
        let buttons = extract_buttons(&msg);
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].block_id.as_deref(), Some("outer"));
        assert!(buttons[0].pressable());
    }

    #[test]
    fn accessory_and_legacy_attachment_actions_count() {
        let msg = json!({
            "blocks": [{"type": "section", "block_id": "s", "text": {"type": "mrkdwn", "text": "hi"},
                        "accessory": {"type": "button", "text": {"type": "plain_text", "text": "Ack"},
                                      "action_id": "a"}}],
            "attachments": [{"actions": [{"name": "ack", "text": "Acknowledge", "value": "42"}]}]
        });
        let buttons = extract_buttons(&msg);
        assert_eq!(buttons.len(), 2);
        assert!(buttons[0].pressable());
        // A legacy action has no block_id, so it cannot be addressed by blocks.actions.
        assert_eq!(buttons[1].action_id.as_deref(), Some("ack"));
        assert!(!buttons[1].pressable());
    }

    #[test]
    fn message_text_reads_blocks_not_the_fallback() {
        let msg = json!({
            "text": "notification fallback",
            "blocks": [
                {"type": "section", "text": {"type": "mrkdwn", "text": "*CPU high* on `web-1`"}},
                {"type": "section", "fields": [
                    {"type": "mrkdwn", "text": "State: ALARM"},
                    {"type": "mrkdwn", "text": "State: ALARM"}
                ]},
                {"type": "actions", "elements": [
                    {"type": "button", "text": {"type": "plain_text", "text": "Acknowledge"}, "action_id": "a"}
                ]}
            ]
        });
        assert_eq!(message_text(&msg), "CPU high on web-1 | State: ALARM");
    }

    #[test]
    fn message_text_unwraps_slack_links() {
        let msg = json!({"blocks": [{"type": "section", "text": {"type": "mrkdwn",
            "text": "see <https://pagerduty.com/incidents/PABC123|incident> &gt; now"}}]});
        assert_eq!(message_text(&msg), "see incident now");
    }

    #[test]
    fn message_urls_scans_everything() {
        let msg = json!({
            "text": "https://one.example.com/a.",
            "blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": "<https://two.example.com/b|b>"}}]
        });
        let urls = message_urls(&msg);
        assert!(urls.contains(&"https://one.example.com/a".to_string()));
        assert!(urls.contains(&"https://two.example.com/b".to_string()));
    }

    /// Shape taken from a real the-warden CloudWatch alert. The expected strings are
    /// the Python's actual output for the same input, captured side by side.
    fn warden_alert() -> Value {
        json!({
            "text": "",
            "blocks": [
                {"type": "section", "text": {"type": "mrkdwn",
                                             "text": "*:rotating_light:ALARM · TG-Latency*"}},
                {"type": "section", "fields": [
                    {"type": "mrkdwn", "text": "*State:* OK → ALARM"},
                    {"type": "mrkdwn", "text": "*Threshold:* AVERAGE"}
                ]},
                {"type": "section", "text": {"type": "mrkdwn", "text": "*Reason*\n&gt;Crossed"}},
                {"type": "context", "elements": [
                    {"type": "mrkdwn", "text": "*Acknowledged* by <mailto:a@b.co|a@b.co>"}
                ]},
                {"type": "actions", "block_id": "acts", "elements": [
                    {"type": "button", "action_id": "acknowledge_incident",
                     "text": {"type": "plain_text", "text": "Acknowledge"}, "value": "500708"},
                    {"type": "users_select", "action_id": "assign_incident__500708",
                     "placeholder": {"type": "plain_text", "text": "Assign to"}}
                ]}
            ]
        })
    }

    #[test]
    fn matches_python_on_a_real_alert() {
        let alert = warden_alert();
        assert_eq!(
            message_text(&alert),
            ":rotating_light:ALARM · TG-Latency | State: OK → ALARM | Threshold: AVERAGE \
             | Reason Crossed | Acknowledged by a@b.co"
        );
        let buttons = extract_buttons(&alert);
        assert_eq!(buttons.len(), 1); // the users_select is not a button
        assert_eq!(buttons[0].text, "Acknowledge");
        assert_eq!(buttons[0].action_id.as_deref(), Some("acknowledge_incident"));
        assert_eq!(buttons[0].value.as_deref(), Some("500708"));
        assert_eq!(buttons[0].block_id.as_deref(), Some("acts"));
        assert!(buttons[0].pressable() && !buttons[0].clickable());
    }

    #[test]
    fn malformed_input_does_not_panic() {
        assert_eq!(message_text(&json!({})), "");
        assert_eq!(message_text(&json!({"blocks": [null, 3, {"type": "section"}]})), "");
        assert!(extract_buttons(&json!({"text": "just a message"})).is_empty());
        assert!(extract_buttons(&json!({})).is_empty());
        assert!(
            extract_buttons(&json!({"blocks": ["nonsense", null,
                {"type": "actions", "elements": ["x", null]}]}))
            .is_empty()
        );
    }

    #[test]
    fn falls_back_to_text_then_attachments() {
        assert_eq!(message_text(&json!({"text": "plain alert"})), "plain alert");
        assert_eq!(
            message_text(&json!({"attachments": [{"text": "from attachment"}]})),
            "from attachment"
        );
    }

    #[test]
    fn plain_string_button_text_is_read() {
        let msg = json!({"blocks": [{"type": "actions", "elements": [
            {"type": "button", "text": "Ack now"}]}]});
        assert_eq!(extract_buttons(&msg)[0].text, "Ack now");
    }

    #[test]
    fn clip_keeps_the_head() {
        // The Python's own assertion: 10 chars in, 10 chars out, ellipsis last.
        let clipped = clip(&"x".repeat(50), 10);
        assert_eq!(clipped.chars().count(), 10);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 4), "abc");
        assert_eq!(clip("", 220), "(no text)");
    }
}
