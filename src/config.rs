//! config.yaml, read exactly as the Python read it -- same keys, same defaults,
//! same refusals. Values are pulled out of the parsed YAML one at a time rather
//! than deserialised into a struct, so a stray extra key or an int where a string
//! was expected behaves the way `dict.get()` did instead of failing the whole file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_yaml::Value;

use crate::tokens;
use crate::util::die;

#[derive(Debug, Clone)]
pub struct ProviderCfg {
    pub enabled: bool,
    values: HashMap<String, String>,
}

impl ProviderCfg {
    pub fn get(&self, key: &str, default: &str) -> String {
        self.values
            .get(key)
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| default.to_string())
    }

    pub fn get_opt(&self, key: &str) -> Option<String> {
        self.values.get(key).filter(|v| !v.is_empty()).cloned()
    }
}

#[derive(Debug)]
pub struct Config {
    pub channels: Vec<String>,
    pub button_match: Regex,
    pub dry_run: bool,
    pub url_click_enabled: bool,
    pub url_click_method: String,
    pub url_click_timeout: u64,
    pub url_allow_hosts: Vec<String>,
    pub url_headers: HashMap<String, String>,
    pub click_enabled: bool,
    pub click_token_env: String,
    pub click_cookie_env: String,
    pub click_cookie_ds_env: String,
    pub click_verify_attempts: u32,
    pub click_verify_delay: f64,
    pub ack_bots_only: bool,
    /// Insertion order preserved: the Python tried providers in the order the file
    /// listed them.
    pub providers: Vec<(String, ProviderCfg)>,
    pub state_db: PathBuf,
    pub state_retention_days: i64,
    pub user_mode: bool,
    pub poll_interval: u64,
    pub recheck_minutes: u64,
    pub refresh_on_start: bool,
    /// Where this came from, so `--verbose` can say which file is in force.
    pub source: PathBuf,
}

fn as_bool(value: Option<&Value>, default: bool) -> bool {
    match value {
        None | Some(Value::Null) => default,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty() && !s.eq_ignore_ascii_case("false"),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => default,
    }
}

fn as_str(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

fn as_i64(value: Option<&Value>, default: i64) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(default),
        _ => default,
    }
}

fn as_f64(value: Option<&Value>, default: f64) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(default),
        _ => default,
    }
}

fn section<'a>(raw: &'a Value, key: &str) -> Option<&'a serde_yaml::Mapping> {
    raw.get(key).and_then(|v| v.as_mapping())
}

fn get<'a>(map: Option<&'a serde_yaml::Mapping>, key: &str) -> Option<&'a Value> {
    map?.get(Value::String(key.to_string()))
}

impl Config {
    pub fn load(path: &Path) -> Config {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => die(format!(
                "{}: cannot be read ({e}).\n\
                 Put the config there, or pass --config PATH. A starting point is in the \
                 project's config.yaml.",
                path.display()
            )),
        };
        let raw: Value = match serde_yaml::from_str(&text) {
            Ok(Value::Null) => Value::Mapping(Default::default()),
            Ok(v) => v,
            Err(e) => die(format!("{}: is not valid YAML: {e}", path.display())),
        };

        let channels: Vec<String> = raw
            .get("channels")
            .and_then(|v| v.as_sequence())
            .map(|s| s.iter().filter_map(|c| as_str(Some(c))).map(|c| c.trim().to_string()).collect())
            .unwrap_or_default();
        if channels.is_empty() {
            die(format!(
                "{}: 'channels' is empty -- refusing to run without a channel allowlist",
                path.display()
            ));
        }

        let url = section(&raw, "url_click");
        let click = section(&raw, "slack_click");
        let um = section(&raw, "user_mode");

        let pattern = as_str(raw.get("button_match"))
            .unwrap_or_else(|| r"(?i)\back(nowledged?)?\b".to_string());
        let button_match = match Regex::new(&pattern) {
            Ok(re) => re,
            Err(e) => die(format!("{}: button_match is not a valid regex: {e}", path.display())),
        };

        let mut providers: Vec<(String, ProviderCfg)> = Vec::new();
        if let Some(map) = section(&raw, "providers") {
            for (name, cfg) in map {
                let Some(name) = as_str(Some(name)) else { continue };
                let mut values = HashMap::new();
                let mut enabled = false;
                if let Some(inner) = cfg.as_mapping() {
                    for (k, v) in inner {
                        let Some(k) = as_str(Some(k)) else { continue };
                        if k == "enabled" {
                            enabled = as_bool(Some(v), false);
                            continue;
                        }
                        if let Some(v) = as_str(Some(v)) {
                            values.insert(k, v);
                        }
                    }
                }
                providers.push((name, ProviderCfg { enabled, values }));
            }
        }

        let mut url_headers = HashMap::new();
        if let Some(headers) = get(url, "headers").and_then(|v| v.as_mapping()) {
            for (k, v) in headers {
                if let (Some(k), Some(v)) = (as_str(Some(k)), as_str(Some(v))) {
                    url_headers.insert(k, v);
                }
            }
        }

        let state_db = as_str(raw.get("state_db")).unwrap_or_else(|| "ack_state.sqlite3".to_string());

        Config {
            channels,
            button_match,
            dry_run: as_bool(raw.get("dry_run"), false),
            url_click_enabled: as_bool(get(url, "enabled"), true),
            url_click_method: as_str(get(url, "method"))
                .unwrap_or_else(|| "GET".to_string())
                .to_uppercase(),
            url_click_timeout: as_i64(get(url, "timeout"), 10).max(1) as u64,
            url_allow_hosts: get(url, "allow_hosts")
                .and_then(|v| v.as_sequence())
                .map(|s| {
                    s.iter()
                        .filter_map(|h| as_str(Some(h)))
                        .map(|h| h.to_lowercase().trim_start_matches('.').to_string())
                        .collect()
                })
                .unwrap_or_default(),
            url_headers,
            click_enabled: as_bool(get(click, "enabled"), true),
            click_token_env: as_str(get(click, "token_env"))
                .unwrap_or_else(|| "SLACK_CLIENT_TOKEN".to_string()),
            click_cookie_env: as_str(get(click, "cookie_env"))
                .unwrap_or_else(|| "SLACK_D_COOKIE".to_string()),
            click_cookie_ds_env: as_str(get(click, "cookie_ds_env"))
                .unwrap_or_else(|| "SLACK_DS_COOKIE".to_string()),
            click_verify_attempts: as_i64(get(click, "verify_attempts"), 5).max(1) as u32,
            click_verify_delay: as_f64(get(click, "verify_delay"), 1.5).max(0.1),
            ack_bots_only: as_bool(raw.get("ack_bots_only"), true),
            providers,
            state_db: resolve_state_db(&state_db, path),
            state_retention_days: as_i64(raw.get("state_retention_days"), 30),
            user_mode: as_bool(get(um, "enabled"), false),
            poll_interval: as_i64(get(um, "poll_interval"), 30).max(5) as u64,
            recheck_minutes: as_i64(get(um, "recheck_minutes"), 15).max(0) as u64,
            refresh_on_start: as_bool(get(um, "refresh_on_start"), true),
            source: path.to_path_buf(),
        }
    }

    pub fn host_allowed(&self, url: &str) -> bool {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
            .unwrap_or_default();
        if host.is_empty() {
            return false;
        }
        self.url_allow_hosts
            .iter()
            .any(|h| host == *h || host.ends_with(&format!(".{h}")))
    }
}

/// A relative `state_db` is resolved against the config's own directory, not the
/// working directory. The binary lives on PATH and is run from anywhere, so
/// cwd-relative would scatter a fresh (empty) dedupe database per directory --
/// and an empty dedupe table means re-acking whatever is still in the window.
fn resolve_state_db(value: &str, config_path: &Path) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return path;
    }
    let base = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(tokens::slack_dir);
    base.join(path)
}

/// Where config.yaml comes from when `--config` was not given:
///   1. `$SLACK_ACK_CONFIG`
///   2. `~/.slack/config.yaml`   -- the normal home for it
///   3. `./config.yaml`          -- the layout the Python used, for a checkout
pub fn default_config_path() -> PathBuf {
    if let Some(explicit) = crate::envstore::get_ne("SLACK_ACK_CONFIG") {
        return PathBuf::from(explicit);
    }
    let home = tokens::slack_dir().join("config.yaml");
    if home.is_file() {
        return home;
    }
    let local = PathBuf::from("config.yaml");
    if local.is_file() {
        return local;
    }
    home
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ack-cfg-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn reads_the_shipped_shape() {
        let path = write(
            "user_mode:\n  enabled: true\n  poll_interval: 10\nchannels:\n  - \"alerts\"\n\
             button_match: '(?i)\\back(nowledged?)?\\b'\nurl_click:\n  enabled: true\n  \
             allow_hosts:\n    - api.opsgenie.com\nslack_click:\n  enabled: true\n  verify_attempts: 5\n\
             providers:\n  pagerduty:\n    enabled: false\n    from_email: a@b.c\n\
             state_db: ack_state.sqlite3\n",
        );
        let cfg = Config::load(&path);
        assert_eq!(cfg.channels, vec!["alerts"]);
        assert!(cfg.user_mode);
        assert_eq!(cfg.poll_interval, 10);
        assert_eq!(cfg.recheck_minutes, 15); // default
        assert!(cfg.button_match.is_match("Acknowledge"));
        assert!(cfg.button_match.is_match("Ack"));
        assert!(!cfg.button_match.is_match("Resolve"));
        assert_eq!(cfg.providers[0].0, "pagerduty");
        assert!(!cfg.providers[0].1.enabled);
        assert_eq!(cfg.providers[0].1.get("from_email", ""), "a@b.c");
        // relative state_db lands beside the config, not in the cwd
        assert_eq!(cfg.state_db, path.parent().unwrap().join("ack_state.sqlite3"));
    }

    #[test]
    fn host_allowed_matches_subdomains_only_under_an_allowed_host() {
        let path = write("channels: [a]\nurl_click:\n  allow_hosts:\n    - example.com\n");
        let cfg = Config::load(&path);
        assert!(cfg.host_allowed("https://example.com/ack"));
        assert!(cfg.host_allowed("https://alerts.example.com/ack"));
        assert!(!cfg.host_allowed("https://notexample.com/ack"));
        assert!(!cfg.host_allowed("https://example.com.evil.net/ack"));
        assert!(!cfg.host_allowed("not a url"));
    }

    #[test]
    fn poll_interval_has_a_floor() {
        let path = write("channels: [a]\nuser_mode:\n  enabled: true\n  poll_interval: 1\n");
        assert_eq!(Config::load(&path).poll_interval, 5);
    }
}
