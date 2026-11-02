//! Keep SLACK_USER_TOKEN alive on rotation-enabled Slack apps. Port of slack_tokens.py.
//!
//! An app with "Advanced token security via token rotation" switched on issues
//! user tokens that look like `xoxe.xoxp-...` and stop working 12 hours later
//! with `token_expired`. The only renewal path is the refresh token Slack returns
//! *beside* the access token at OAuth time -- it is never shown anywhere in the
//! app dashboard, so if it is not captured then, the only recovery is redoing the
//! browser consent flow by hand.
//!
//! So we persist all three values to the credentials file (access token, refresh
//! token, expiry) and trade the refresh token for a fresh pair whenever the
//! current one is close to expiring. Each refresh invalidates the refresh token
//! it used and returns a new one, so every renewal has to be written back to disk
//! or the next one fails.
//!
//! Plain `xoxp-` tokens (rotation off) never expire; everything here no-ops for
//! them, so it is safe to call unconditionally.
//!
//! The one deliberate difference from the Python: the file is not `.env` beside
//! the script, because there is no script -- the binary lives on PATH and is run
//! from anywhere. See `env_path()` for where it looks instead.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::envstore;
use crate::util::now;
use crate::{log_debug, log_warn};

pub const TOKEN_KEY: &str = "SLACK_USER_TOKEN";
pub const REFRESH_KEY: &str = "SLACK_USER_REFRESH_TOKEN";
pub const EXPIRES_KEY: &str = "SLACK_USER_TOKEN_EXPIRES_AT";

/// Refresh this far ahead of the deadline rather than at it: a 12-hour token
/// handed out mid-poll should not expire between the check and the next call.
pub const MARGIN_SECONDS: u64 = 15 * 60;

pub const REAUTH_HINT: &str = "re-run:  ack-bot get-user-token";

/// Same app as `get-user-token`. Overridable so a second workspace can reuse
/// this without a rebuild.
pub fn client_id() -> String {
    envstore::get_ne("SLACK_CLIENT_ID").unwrap_or_else(|| "514487189412.11783092574247".to_string())
}

// --------------------------------------------------------------------------- //
// where the credentials live
// --------------------------------------------------------------------------- //

pub fn home_dir() -> PathBuf {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => crate::util::die("HOME is not set, so ~/.slack cannot be located"),
    }
}

/// `~/.slack` -- the credentials directory, alongside `~/.cargo`.
pub fn slack_dir() -> PathBuf {
    home_dir().join(".slack")
}

/// The file holding the token triple and the browser-session credentials.
///
/// Resolution order, first hit wins:
///   1. `$SLACK_ACK_ENV`            -- an explicit override
///   2. `~/.slack/env`              -- the normal home for it
///   3. `~/.slack/.env`             -- same directory, dot-name, if that is what exists
///   4. `~/.slack`                  -- if it is a plain file rather than a directory
///   5. `./.env`                    -- the layout the Python used, for a checkout
///   6. `~/.slack/env`              -- nothing exists yet: this is what gets created
pub fn env_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        if let Some(explicit) = envstore::get_ne("SLACK_ACK_ENV") {
            return PathBuf::from(explicit);
        }
        let dir = slack_dir();
        if dir.is_dir() {
            let plain = dir.join("env");
            if plain.is_file() {
                return plain;
            }
            let dotted = dir.join(".env");
            if dotted.is_file() {
                return dotted;
            }
            return plain;
        }
        if dir.is_file() {
            return dir;
        }
        let local = PathBuf::from(".env");
        if local.is_file() {
            return local;
        }
        dir.join("env")
    })
}

// --------------------------------------------------------------------------- //
// the credentials file
// --------------------------------------------------------------------------- //

/// The key a line assigns to, tolerating `export KEY=` -- the file is sourced by
/// shells too, so both forms are valid and a missed match here would append a
/// duplicate that shadows the update.
fn line_key(line: &str) -> String {
    if !line.contains('=') {
        return String::new();
    }
    let head = line.split('=').next().unwrap_or("").trim();
    head.strip_prefix("export ").unwrap_or(head).trim().to_string()
}

/// Set keys in the credentials file, leaving every other line (and its comments)
/// alone.
///
/// Rewritten in place rather than appended to, because a stale `SLACK_USER_TOKEN=`
/// line further down the file would win when a shell sources it -- `set -a; . ./env`
/// takes the last assignment.
pub fn update_env(updates: &[(String, String)], path: &Path) -> io::Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = if existing.is_empty() {
        Vec::new()
    } else {
        existing.lines().collect()
    };

    let mut remaining: Vec<(String, String)> = updates.to_vec();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + remaining.len());

    for line in lines {
        let key = line_key(line);
        match remaining.iter().position(|(k, _)| *k == key && !key.is_empty()) {
            Some(idx) => {
                let (k, v) = remaining.remove(idx);
                out.push(format!("{k}={v}"));
            }
            None => out.push(line.to_string()),
        }
    }
    out.extend(remaining.iter().map(|(k, v)| format!("{k}={v}")));

    // Write a sibling and rename over the target rather than truncating it. This
    // file is the *only* copy of the refresh token, and a plain write is a
    // truncate followed by a write: a crash, a full disk or a kill between the
    // two leaves an empty file and no way back to Slack short of re-consenting.
    // Rename is atomic within a filesystem, so a reader sees the old file or the
    // new one and never a half-written one.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
            set_mode(parent, 0o700);
        }
    }
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.tmp"));
    fs::write(&tmp, out.join("\n") + "\n")?;
    set_mode(&tmp, 0o600);
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

/// Parse a sourced-style credentials file into a map. Comments and blank lines skipped.
pub fn read_env(path: &Path) -> HashMap<String, String> {
    let mut values = HashMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return values;
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let (key, val) = line.split_once('=').unwrap();
        let key = key.trim();
        let key = key.strip_prefix("export ").unwrap_or(key).trim();
        let mut val = val.trim().to_string();
        // The file is sourced by shells, so bash would strip these quotes; do the
        // same here or the token would carry them into the Authorization header.
        if val.len() >= 2 {
            let bytes = val.as_bytes();
            let first = bytes[0] as char;
            let last = bytes[val.len() - 1] as char;
            if first == last && (first == '"' || first == '\'') {
                val = val[1..val.len() - 1].to_string();
            }
        }
        if !key.is_empty() {
            values.insert(key.to_string(), val);
        }
    }
    values
}

/// Copy the credentials file into the process store so the binary works with no
/// shell wrapper. Anything already exported wins, which keeps
/// `SLACK_USER_TOKEN=... ack-bot` working as an override.
pub fn load_env(path: &Path, override_existing: bool) {
    for (key, val) in read_env(path) {
        if override_existing || !envstore::contains(&key) {
            envstore::set(&key, &val);
        }
    }
}

/// True for tokens that expire. Only rotation-enabled apps issue these.
pub fn rotating(token: Option<&str>) -> bool {
    token.is_some_and(|t| t.starts_with("xoxe."))
}

/// Stored expiry as a timestamp. 0.0 when absent, blank or hand-mangled -- a
/// truncated value should trigger a refresh, not a crash.
fn deadline_of(source: &HashMap<String, String>) -> f64 {
    source
        .get(EXPIRES_KEY)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Take the file's token triple when it outlives the one this process holds.
///
/// Two things write tokens: this process, and a `get-user-token` run (or a second
/// bot) alongside it. Whoever wrote last also invalidated the refresh token we are
/// still holding in memory, so refreshing from it would fail with
/// invalid_refresh_token while a perfectly good token sat on disk. Re-reading
/// first means an out-of-band re-auth is picked up without a restart.
pub fn adopt_newer_from_disk(path: &Path) -> bool {
    let disk = read_env(path);
    let Some(token) = disk.get(TOKEN_KEY).filter(|t| !t.is_empty()) else {
        return false;
    };
    if Some(token.clone()) == envstore::get(TOKEN_KEY) {
        return false;
    }
    if deadline_of(&disk) <= deadline_of(&envstore::snapshot()) {
        // Older or undated: ours is the one worth keeping.
        return false;
    }

    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    log_debug!("adopting the newer {} found in {}", TOKEN_KEY, name);
    envstore::set(TOKEN_KEY, token);
    for key in [REFRESH_KEY, EXPIRES_KEY] {
        if let Some(v) = disk.get(key).filter(|v| !v.is_empty()) {
            envstore::set(key, v);
        }
    }
    true
}

/// Persist a token triple to the credentials file and to this process's store.
pub fn save(
    access_token: &str,
    refresh_token: Option<&str>,
    expires_in: Option<i64>,
    path: &Path,
) -> io::Result<()> {
    let mut updates = vec![(TOKEN_KEY.to_string(), access_token.to_string())];
    if let Some(rt) = refresh_token.filter(|r| !r.is_empty()) {
        updates.push((REFRESH_KEY.to_string(), rt.to_string()));
    }
    if let Some(exp) = expires_in.filter(|e| *e != 0) {
        updates.push((EXPIRES_KEY.to_string(), format!("{}", now() as i64 + exp)));
    }

    update_env(&updates, path)?;
    for (k, v) in &updates {
        envstore::set(k, v);
    }
    Ok(())
}

// --------------------------------------------------------------------------- //
// refresh
// --------------------------------------------------------------------------- //

/// Slack refused to renew the token.
///
/// `terminal` marks the refusals retrying cannot fix -- the refresh token is
/// spent, or the app needs a client secret it does not have. Everything else (a
/// timeout, a 5xx, an unrecognised error) may well work on the next attempt, so a
/// long-running caller can keep going instead of giving up.
#[derive(Debug, Clone)]
pub struct RefreshError {
    pub message: String,
    /// Carried for the same reason the Python carried it: the distinction is part of
    /// the contract even though today's callers all back off and retry regardless.
    #[allow(dead_code)]
    pub terminal: bool,
}

impl RefreshError {
    fn new(message: impl Into<String>, terminal: bool) -> Self {
        Self { message: message.into(), terminal }
    }
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RefreshError {}

pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
}

fn http() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client")
    })
}

fn post(params: &[(&str, &str)]) -> Result<Value, String> {
    let resp = http()
        .post("https://slack.com/api/oauth.v2.access")
        .form(params)
        .send()
        .map_err(|e| e.to_string())?;
    let text = resp.text().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("{e}: {}", &text[..text.len().min(200)]))
}

/// Pull the token triple out of a refresh response.
///
/// Slack nests a *user* token refresh under `authed_user` but returns a bot token
/// refresh at the top level, and the shape has moved before. Prefer the nested
/// object, fall back to the root, so either keeps working.
fn tokens_from(data: &Value) -> Tokens {
    let root = data;
    let nested = data.get("authed_user");
    let inner = match nested {
        Some(v) if v.get("access_token").and_then(Value::as_str).is_some() => v,
        _ => root,
    };
    Tokens {
        access_token: inner
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        refresh_token: inner
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        expires_in: inner.get("expires_in").and_then(Value::as_i64),
    }
}

/// Trade a refresh token for a new pair. Errors if Slack says no.
pub fn refresh(
    refresh_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<Tokens, RefreshError> {
    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    // PKCE public clients refresh without a secret; confidential ones require it
    // and answer invalid_client without. Send it only when we have one.
    if let Some(secret) = client_secret.filter(|s| !s.is_empty()) {
        params.push(("client_secret", secret));
    }

    // Not terminal: Slack being unreachable for a moment says nothing about
    // whether the refresh token is still good.
    let data = post(&params)
        .map_err(|e| RefreshError::new(format!("could not reach Slack: {e}"), false))?;

    if data.get("ok").and_then(Value::as_bool) != Some(true) {
        let err = data.get("error").and_then(Value::as_str).unwrap_or("unknown");
        if matches!(err, "invalid_client" | "bad_client_secret" | "client_secret_missing") {
            return Err(RefreshError::new(
                format!(
                    "Slack says {err} -- this app refreshes as a confidential client.\n\
                     Add its client secret to {} as SLACK_CLIENT_SECRET \
                     (api.slack.com/apps -> your app -> Basic Information ->\n  \
                     App Credentials -> Client Secret -> Show)",
                    env_path().display()
                ),
                true,
            ));
        }
        if matches!(err, "invalid_refresh_token" | "invalid_grant" | "token_revoked") {
            return Err(RefreshError::new(
                format!(
                    "Slack says {err} -- the refresh token is spent or revoked, so \
                     consent has to be granted again.\n{REAUTH_HINT}"
                ),
                true,
            ));
        }
        return Err(RefreshError::new(format!("refresh failed: {err}"), false));
    }

    let got = tokens_from(&data);
    if got.access_token.is_empty() {
        let mut keys: Vec<&str> = data.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
        keys.sort_unstable();
        return Err(RefreshError::new(
            format!("refresh returned no access token (keys: {keys:?})"),
            false,
        ));
    }
    Ok(got)
}

/// Renew `SLACK_USER_TOKEN` if it is rotating and near expiry. Returns the token.
///
/// `force` skips the expiry check, for when Slack has already answered
/// token_expired -- a stored deadline can still look comfortably in the future if
/// the clock is off or the token was replaced out of band.
///
/// `raise_on_failure` returns the `RefreshError` instead of logging it, so a
/// long-running caller can tell a transient failure (retry) from a terminal one
/// (needs consent again). Off by default: a failure then still leaves the caller
/// its current token, which is the right outcome for `--list-channels` on a bot
/// token and for a non-rotating workspace with no refresh token.
///
/// Safe to call on every poll tick -- it costs nothing until the token is actually
/// inside `margin` of its deadline.
pub fn ensure_fresh(
    margin: u64,
    path: &Path,
    force: bool,
    raise_on_failure: bool,
) -> Result<Option<String>, RefreshError> {
    // Do this first: another process may have replaced the token, which also spent
    // the refresh token we hold. Cheap, and it saves a doomed refresh.
    let adopted = adopt_newer_from_disk(path);

    let token = envstore::get(TOKEN_KEY);
    if !rotating(token.as_deref()) {
        return Ok(token);
    }

    let deadline = deadline_of(&envstore::snapshot());
    if deadline > 0.0 && now() < deadline - margin as f64 && (adopted || !force) {
        // `adopted` outranks `force`: the caller's token_expired was about the
        // token we just replaced, not the one now in hand.
        log_debug!("user token good for another {}s", (deadline - now()) as i64);
        return Ok(token);
    }

    let refresh_token = envstore::get_ne(REFRESH_KEY);
    let Some(refresh_token) = refresh_token else {
        // The common case for a token obtained before rotation was handled.
        let msg = format!(
            "{TOKEN_KEY} expires (rotating token) and no {REFRESH_KEY} is stored, \
             so it cannot be renewed -- {REAUTH_HINT}"
        );
        if raise_on_failure {
            return Err(RefreshError::new(msg, true));
        }
        log_warn!("{}", msg);
        return Ok(token);
    };

    log_debug!("user token expired or expiring -- refreshing");
    let got = match refresh(
        &refresh_token,
        &client_id(),
        envstore::get_ne("SLACK_CLIENT_SECRET").as_deref(),
    ) {
        Ok(got) => got,
        Err(exc) => {
            if raise_on_failure {
                return Err(exc);
            }
            crate::log_error!("{}", exc);
            return Ok(token);
        }
    };

    if let Err(e) = save(
        &got.access_token,
        got.refresh_token.as_deref(),
        got.expires_in,
        path,
    ) {
        // The token in hand is good; failing to persist it is worth shouting about
        // because the *next* renewal needs the new refresh token from disk.
        crate::log_error!("could not write {}: {} -- the renewed refresh token is only in memory", path.display(), e);
    }
    log_debug!(
        "refreshed user token (valid ~{}h)",
        got.expires_in.unwrap_or(0) / 3600
    );
    Ok(Some(got.access_token))
}

/// `ensure_fresh` with the defaults every caller but the poll loop wants.
pub fn ensure_fresh_default() -> Option<String> {
    ensure_fresh(MARGIN_SECONDS, env_path(), false, false).unwrap_or(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_env_replaces_in_place_and_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("ack-env-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("env");
        fs::write(&path, "# comment\nexport SLACK_USER_TOKEN=old\nOTHER=keep\n").unwrap();

        update_env(&[("SLACK_USER_TOKEN".into(), "new".into()), ("ADDED".into(), "1".into())], &path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, "# comment\nSLACK_USER_TOKEN=new\nOTHER=keep\nADDED=1\n");
        let values = read_env(&path);
        assert_eq!(values["SLACK_USER_TOKEN"], "new");
        assert_eq!(values["OTHER"], "keep");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_env_strips_quotes_like_a_shell_would() {
        let dir = std::env::temp_dir().join(format!("ack-env-q-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("env");
        fs::write(&path, "A=\"quoted\"\nB='single'\nC=bare\n# D=skipped\n").unwrap();
        let values = read_env(&path);
        assert_eq!(values["A"], "quoted");
        assert_eq!(values["B"], "single");
        assert_eq!(values["C"], "bare");
        assert!(!values.contains_key("D"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotating_only_matches_xoxe() {
        assert!(rotating(Some("xoxe.xoxp-1-abc")));
        assert!(!rotating(Some("xoxp-1-abc")));
        assert!(!rotating(None));
    }
}
