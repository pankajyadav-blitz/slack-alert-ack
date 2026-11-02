//! Pull browser-session credentials out of a DevTools "Copy as cURL" capture.
//! Port of import_session.py, as `ack-bot import-session`.
//!
//! Two jobs, both for the same recurring chore -- the `xoxc` token and `d` cookie
//! that the click path needs expire with the browser session, so they have to be
//! re-copied by hand every so often:
//!
//!   1. Extract them and print the credential lines (or write them with --write).
//!   2. Report which form fields the real request carried, and diff that against the
//!      ones click.rs sends. `blocks.actions` is undocumented, so a capture from
//!      your own workspace is the only authority on its shape -- if Slack changes
//!      it, this is what tells you.
//!
//! Usage:
//!     # Slack in a browser -> DevTools -> Network -> click any message button ->
//!     # right-click the `blocks.actions` request -> Copy -> Copy as cURL
//!     ack-bot import-session capture.txt
//!     ack-bot import-session capture.txt --write
//!     pbpaste | ack-bot import-session -

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::PathBuf;

use regex::Regex;

use crate::tokens;
use crate::util::die;

/// What click.rs sends. Kept here rather than imported so this stays usable when
/// the two have drifted -- reporting that drift is the point.
const OURS: [&str; 14] = [
    "token", "client_token", "channel", "message_ts", "container", "actions", "state",
    "service_team_id", "bot_user_id", "service_id", "blocks", "_x_mode", "_x_sonic", "_x_app_name",
];

/// Fields that are pure client telemetry; absence is not worth reporting.
fn noise(field: &str) -> bool {
    field.starts_with("_x_") || field == "fp" || field == "slack_route"
}

/// Both quote styles, since the regex crate has no backreferences: `'([^']*)'` and
/// `"([^"]*)"` capture exactly what Python's `(['"])(.*?)\1` did.
fn quoted(prefix: &str) -> Vec<Regex> {
    vec![
        Regex::new(&format!(r"(?s){prefix}\s+'([^']*)'")).unwrap(),
        Regex::new(&format!(r#"(?s){prefix}\s+"([^"]*)""#)).unwrap(),
    ]
}

/// One cookie value, from either -b/--cookie or a -H 'Cookie: ...' header.
pub fn find_cookie(curl: &str, name: &str) -> Option<String> {
    let mut jars: Vec<String> = Vec::new();
    for re in quoted(r"(?:-b|--cookie)") {
        for caps in re.captures_iter(curl) {
            jars.push(caps[1].to_string());
        }
    }
    for re in [
        Regex::new(r"(?s)-H\s+'\s*[Cc]ookie:\s*([^']*)'").unwrap(),
        Regex::new(r#"(?s)-H\s+"\s*[Cc]ookie:\s*([^"]*)""#).unwrap(),
    ] {
        for caps in re.captures_iter(curl) {
            jars.push(caps[1].to_string());
        }
    }

    let pattern = Regex::new(&format!(r"(?:^|;\s*){}=([^;]+)", regex::escape(name))).unwrap();
    for jar in jars {
        if let Some(caps) = pattern.captures(&jar) {
            return Some(caps[1].trim().to_string());
        }
    }
    None
}

/// `ack-bot set-session`: store the xoxc token and `d` cookie pasted straight in,
/// for when they were copied from DevTools by hand instead of from a cURL capture.
/// Any value not given as a flag is asked for with hidden input, which keeps it out
/// of shell history.
pub fn set_session(
    token_argument: Option<String>,
    cookie_argument: Option<String>,
    ds_cookie_argument: Option<String>,
) -> i32 {
    let ask_hidden = |label: &str| -> String {
        rpassword::prompt_password(label)
            .unwrap_or_else(|e| die(format!("cannot read input: {e}")))
            .trim()
            .to_string()
    };
    let token = token_argument
        .unwrap_or_else(|| ask_hidden("xoxc- token (hidden, paste + Enter): "))
        .trim()
        .to_string();
    if !token.starts_with("xoxc-") {
        die("the token must start with xoxc- (Slack in a browser -> DevTools console, see README)");
    }
    let cookie = cookie_argument
        .unwrap_or_else(|| ask_hidden("d cookie, xoxd-... (hidden, paste + Enter): "))
        .trim()
        .to_string();
    if !cookie.starts_with("xoxd-") {
        die(
            "the cookie must start with xoxd- (DevTools -> Application -> Cookies -> https://app.slack.com -> d)",
        );
    }

    let mut lines: Vec<(String, String)> = vec![
        ("SLACK_CLIENT_TOKEN".to_string(), token),
        ("SLACK_D_COOKIE".to_string(), cookie),
    ];
    if let Some(ds_cookie) = ds_cookie_argument
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        lines.push(("SLACK_DS_COOKIE".to_string(), ds_cookie));
    }
    let path = tokens::env_path().to_path_buf();
    if let Err(e) = tokens::update_env(&lines, &path) {
        die(format!("cannot write {}: {e}", path.display()));
    }
    println!(
        "wrote {} to {} (chmod 600)",
        lines
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        path.display()
    );
    println!("check it with: ack-bot --test --dry-run");
    0
}

/// Form fields, whether the body was copied as multipart or urlencoded.
pub fn find_fields(curl: &str) -> HashMap<String, String> {
    let mut fields: HashMap<String, String> = HashMap::new();

    // -F name=value  (rare from DevTools, common when hand-editing)
    for re in quoted(r"(?:-F|--form)") {
        for caps in re.captures_iter(curl) {
            let raw = &caps[1];
            let (name, value) = raw.split_once('=').unwrap_or((raw, ""));
            fields.insert(name.trim().to_string(), value.to_string());
        }
    }

    let mut bodies: Vec<String> = Vec::new();
    for re in [
        Regex::new(r"(?s)(?:--data-raw|--data-binary|--data|-d)\s+\$?'([^']*)'").unwrap(),
        Regex::new(r#"(?s)(?:--data-raw|--data-binary|--data|-d)\s+\$?"([^"]*)""#).unwrap(),
    ] {
        for caps in re.captures_iter(curl) {
            bodies.push(caps[1].to_string());
        }
    }

    // multipart: ...name="token"\r\n\r\nxoxc-...\r\n------WebKitFormBoundary
    // `Copy as cURL` escapes the CRLFs, so accept both forms.
    let multipart =
        Regex::new(r#"(?s)name="([^"]+)"(?:\\r\\n|\r\n|\n){2}(.*?)(?:\\r\\n|\r\n|\n)-{6,}"#)
            .unwrap();
    for body in bodies {
        let pairs: Vec<(String, String)> = multipart
            .captures_iter(&body)
            .map(|c| (c[1].to_string(), c[2].to_string()))
            .collect();
        if !pairs.is_empty() {
            fields.extend(pairs);
            continue;
        }
        if body.contains('=') && !body.trim().contains('\n') {
            // urlencoded
            for (key, value) in url::form_urlencoded::parse(body.as_bytes()) {
                fields.insert(key.to_string(), value.to_string());
            }
        }
    }
    fields
}

pub fn main(capture: &str, write: Option<Option<PathBuf>>) -> i32 {
    let curl = if capture == "-" {
        let mut buf = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
            die(format!("cannot read stdin: {e}"));
        }
        buf
    } else {
        std::fs::read_to_string(capture)
            .unwrap_or_else(|e| die(format!("cannot read {capture}: {e}")))
    };

    if !curl.contains("curl") {
        die("that does not look like a copied cURL command");
    }

    let fields = find_fields(&curl);
    let token = fields.get("token").cloned().unwrap_or_default();
    let cookie = find_cookie(&curl, "d").unwrap_or_default();

    if !token.starts_with("xoxc-") {
        die("no xoxc- token in the capture -- copy the `blocks.actions` request itself, \
             not a different one (its `token` form field is what we need)");
    }
    if !cookie.starts_with("xoxd-") {
        die("no `d` cookie in the capture -- use Copy as cURL, which includes the Cookie \
             header, rather than Copy as fetch");
    }

    let endpoint = Regex::new(r"/api/([A-Za-z.]+)")
        .unwrap()
        .captures(&curl)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    println!("endpoint : {endpoint}");
    println!(
        "token    : {}...{}",
        &token[..token.len().min(14)],
        &token[token.len().saturating_sub(4)..]
    );
    println!(
        "cookie   : {}...{}",
        &cookie[..cookie.len().min(12)],
        &cookie[cookie.len().saturating_sub(4)..]
    );

    let theirs: BTreeSet<&str> = fields.keys().map(String::as_str).collect();
    let ours: BTreeSet<&str> = OURS.into_iter().collect();
    let missing: Vec<&str> = theirs.difference(&ours).copied().filter(|f| !noise(f)).collect();
    println!(
        "\nfields in the capture ({}): {}",
        theirs.len(),
        theirs.iter().copied().collect::<Vec<_>>().join(", ")
    );
    if !missing.is_empty() {
        println!("\nNOT sent by click(): {}", missing.join(", "));
        println!("  -> add them in click.rs if the click is being rejected");
    }
    let extra: Vec<&str> = ours.difference(&theirs).copied().filter(|f| !noise(f)).collect();
    if !extra.is_empty() {
        println!("\nsent by click() but absent here: {}", extra.join(", "));
        println!("  -> harmless if the click works; drop them if Slack complains");
    }
    if missing.is_empty() && extra.is_empty() {
        println!("\nclick() sends exactly this set of fields.");
    }

    let mut lines: Vec<(String, String)> = vec![
        ("SLACK_CLIENT_TOKEN".to_string(), token),
        ("SLACK_D_COOKIE".to_string(), cookie),
    ];
    // Only some (SSO) workspaces send it; without it those reject the session.
    if let Some(cookie_ds) = find_cookie(&curl, "d-s") {
        lines.push(("SLACK_DS_COOKIE".to_string(), cookie_ds));
    }

    let Some(target) = write else {
        println!();
        for (key, value) in &lines {
            println!("{key}={value}");
        }
        return 0;
    };
    // `--write` with no value means the credentials file this binary reads.
    let path = target.unwrap_or_else(|| tokens::env_path().to_path_buf());
    if let Err(e) = tokens::update_env(&lines, &path) {
        die(format!("cannot write {}: {e}", path.display()));
    }
    println!(
        "\nwrote {} to {} (chmod 600)",
        lines.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", "),
        path.display()
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURE: &str = r#"curl 'https://slack.com/api/blocks.actions' \
  -H 'Cookie: b=x; d=xoxd-ABCDEF%2Fgh; d-s=1712345678' \
  --data-raw $'------WebKitFormBoundary\r\nContent-Disposition: form-data; name="token"\r\n\r\nxoxc-1111-2222-3333\r\n------WebKitFormBoundary\r\nContent-Disposition: form-data; name="service_id"\r\n\r\nB094L10NUSZ\r\n------WebKitFormBoundary--'"#;

    #[test]
    fn finds_the_token_and_both_cookies() {
        let fields = find_fields(CAPTURE);
        assert_eq!(fields.get("token").unwrap(), "xoxc-1111-2222-3333");
        assert_eq!(fields.get("service_id").unwrap(), "B094L10NUSZ");
        assert_eq!(find_cookie(CAPTURE, "d").unwrap(), "xoxd-ABCDEF%2Fgh");
        assert_eq!(find_cookie(CAPTURE, "d-s").unwrap(), "1712345678");
    }

    #[test]
    fn does_not_confuse_d_with_d_s() {
        let curl = "curl -H 'Cookie: d-s=only' https://slack.com/api/blocks.actions";
        assert_eq!(find_cookie(curl, "d"), None);
        assert_eq!(find_cookie(curl, "d-s").unwrap(), "only");
    }

    #[test]
    fn reads_a_urlencoded_body_too() {
        let curl = "curl 'https://slack.com/api/blocks.actions' --data-raw 'token=xoxc-9&channel=C1'";
        let fields = find_fields(curl);
        assert_eq!(fields.get("token").unwrap(), "xoxc-9");
        assert_eq!(fields.get("channel").unwrap(), "C1");
    }
}
