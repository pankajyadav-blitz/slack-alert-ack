//! The bot. Port of `class AckBot` -- resolve, poll, react to an event, and the
//! three ack strategies.
//!
//! Mutable fields live behind locks rather than `&mut self` because socket mode
//! hands events to another thread; the behaviour is identical, and the poll loop
//! still runs one channel at a time.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use regex::Regex;
use serde_json::{Map, Value};

use crate::activity::{Activity, Outcome};
use crate::buttons::{Button, clip_default, extract_buttons, message_text};
use crate::click::{self, ClickError, ClientSession};
use crate::config::Config;
use crate::providers::{self, ProviderError};
use crate::state::State;
use crate::tokens;
use crate::util::{Stop, die, local_midnight, now, now_ts, sleep, ts_f64};
use crate::webapi::{self, SlackError, WebClient};
use crate::{log_debug, log_error, log_info, log_warn};

/// Cap the backoff: long enough not to hammer Slack while a refresh keeps failing,
/// short enough that an out-of-band re-auth is picked up promptly.
const REFRESH_RETRY_CAP: u64 = 15 * 60;

/// Re-look-up a channel's id after this many consecutive not-found ticks.
const RERESOLVE_AFTER: u32 = 3;

/// What `--test` presses when no Acknowledge button is outstanding anywhere. Only
/// ever reached by hand, never by the running bot: it closes a live incident.
pub fn default_test_button() -> Regex {
    Regex::new(r"(?i)\bresolve\b").unwrap()
}

/// No strategy could really acknowledge this message -- leave it alone.
#[derive(Debug)]
pub enum AckError {
    NoAckPath(String),
    Click(ClickError),
    /// Anything else: the Python's bare `Exception`, logged with a traceback.
    Other(String),
}

impl fmt::Display for AckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AckError::NoAckPath(m) => write!(f, "{m}"),
            AckError::Click(e) => write!(f, "{e}"),
            AckError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl From<ClickError> for AckError {
    fn from(e: ClickError) -> Self {
        AckError::Click(e)
    }
}

impl From<SlackError> for AckError {
    fn from(e: SlackError) -> Self {
        AckError::Other(e.to_string())
    }
}

pub struct AckBot {
    pub cfg: Config,
    /// Reads history, resolves channels.
    pub web: WebClient,
    /// Browser session: presses interactive buttons.
    pub clicker: Option<ClientSession>,
    pub state: State,
    /// cid -> label, in the order config listed them (Python used a dict).
    channel_ids: RwLock<Vec<(String, String)>>,
    self_bot_id: RwLock<Option<String>>,
    self_user_id: RwLock<Option<String>>,
    /// Consecutive refresh failures; drives the retry backoff.
    refresh_failures: AtomicU32,
    /// cid -> consecutive not-founds.
    channel_failures: Mutex<HashMap<String, u32>>,
    /// 00:00 local on the day the bot started; user mode never reads before it.
    history_floor: f64,
    http: reqwest::blocking::Client,
    /// The live dashboard's data sink. `None` in plain / piped mode, where the
    /// stderr log is the whole story and nothing is recorded here.
    activity: Option<Activity>,
}

impl AckBot {
    pub fn new(cfg: Config, web: WebClient, clicker: Option<ClientSession>) -> AckBot {
        let state = State::open(&cfg.state_db, cfg.state_retention_days);
        let http = reqwest::blocking::Client::builder()
            .build()
            .expect("http client");
        AckBot {
            cfg,
            web,
            clicker,
            state,
            channel_ids: RwLock::new(Vec::new()),
            self_bot_id: RwLock::new(None),
            self_user_id: RwLock::new(None),
            refresh_failures: AtomicU32::new(0),
            channel_failures: Mutex::new(HashMap::new()),
            history_floor: local_midnight(),
            http,
            activity: None,
        }
    }

    /// Attach the live dashboard's data sink. Called only when stdout is a real
    /// terminal; left unset otherwise so plain mode records nothing.
    pub fn set_activity(&mut self, activity: Activity) {
        self.activity = Some(activity);
    }

    /// Mirror the watched channels into the dashboard, if one is attached. No-op
    /// in plain mode.
    pub fn publish_channels(&self) {
        if let Some(activity) = &self.activity {
            activity.set_channels(self.channels().into_iter().map(|(_, label)| label).collect());
        }
    }

    pub fn channels(&self) -> Vec<(String, String)> {
        self.channel_ids.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn label_for(&self, cid: &str) -> Option<String> {
        self.channels()
            .into_iter()
            .find(|(id, _)| id == cid)
            .map(|(_, label)| label)
    }

    /// `self.channel_ids.get(channel, channel)`.
    fn label_or_id(&self, cid: &str) -> String {
        self.label_for(cid).unwrap_or_else(|| cid.to_string())
    }

    // -- setup ------------------------------------------------------------- //

    /// name -> id and id -> channel, over every page of conversations_list.
    pub fn channel_directory(
        &self,
    ) -> Result<(HashMap<String, String>, HashMap<String, Value>), SlackError> {
        let mut by_name: HashMap<String, String> = HashMap::new();
        let mut by_id: HashMap<String, Value> = HashMap::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors: Vec<String> = Vec::new();

        loop {
            let resp =
                self.web
                    .conversations_list("public_channel,private_channel", 1000, cursor.as_deref())?;
            for ch in resp
                .get("channels")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                let (Some(name), Some(id)) = (
                    ch.get("name").and_then(Value::as_str),
                    ch.get("id").and_then(Value::as_str),
                ) else {
                    continue;
                };
                by_name.insert(name.to_string(), id.to_string());
                by_id.insert(id.to_string(), ch.clone());
            }
            cursor = webapi::next_cursor(&resp);
            let Some(next) = cursor.clone() else { break };
            // A cursor that repeats would page for ever and the bot would never
            // finish starting. Stop with what we have and let the caller report
            // whichever channel is genuinely missing.
            if seen_cursors.contains(&next) {
                log_warn!(
                    "conversations_list repeated a cursor -- stopping paging after {} channel(s)",
                    by_id.len()
                );
                break;
            }
            seen_cursors.push(next);
        }
        Ok((by_name, by_id))
    }

    pub fn resolve(&self) {
        let auth = match self.web.auth_test() {
            Ok(auth) => auth,
            Err(exc) => {
                // Slack's answer beats the stored expiry, which can be missing or
                // stale if the token was replaced out of band. Renew and retry once.
                if exc.api_error() != Some("token_expired") || !self.renew_token() {
                    die(format!("auth.test failed: {exc}"));
                }
                match self.web.auth_test() {
                    Ok(auth) => auth,
                    Err(exc) => die(format!("auth.test failed: {exc}")),
                }
            }
        };
        let bot_id = auth.get("bot_id").and_then(Value::as_str).map(str::to_string);
        *self.self_bot_id.write().unwrap_or_else(|e| e.into_inner()) = bot_id.clone();
        log_debug!(
            "authed as {} (bot_id={}) in {}",
            auth.get("user").and_then(Value::as_str).unwrap_or("None"),
            bot_id.as_deref().unwrap_or("None"),
            auth.get("team").and_then(Value::as_str).unwrap_or("None")
        );

        if self.cfg.user_mode {
            // Reading as ourselves: remember our own user id so anything we post is
            // not mistaken for a fresh alert on the next poll.
            *self.self_user_id.write().unwrap_or_else(|e| e.into_inner()) = auth
                .get("user_id")
                .and_then(Value::as_str)
                .map(str::to_string);
        }

        if let Some(clicker) = &self.clicker {
            // Check the browser session now rather than discovering it expired
            // halfway through an alert: an invalid xoxc token fails here loudly.
            let who = match clicker.whoami() {
                Ok(who) => who,
                Err(exc) => die(exc.to_string()),
            };
            clicker.set_team_id_if_unset(who.get("team_id").and_then(Value::as_str));
            log_debug!(
                "button clicks will be attributed to {} ({}) via blocks.actions",
                who.get("user").and_then(Value::as_str).unwrap_or("None"),
                who.get("user_id").and_then(Value::as_str).unwrap_or("None")
            );
        }

        let (by_name, by_id) = match self.channel_directory() {
            Ok(pair) => pair,
            Err(exc) => die(format!("could not list channels: {exc}")),
        };

        let id_re = Regex::new(r"^[CG][A-Z0-9]{6,}$").unwrap();
        let mut resolved: Vec<(String, String)> = Vec::new();
        let mut not_member: Vec<String> = Vec::new();

        for entry in &self.cfg.channels {
            let name = entry.trim_start_matches('#');
            if name.is_empty() {
                die(format!(
                    "channel entry {entry:?} is empty -- put a real channel name or id in {}",
                    self.cfg.source.display()
                ));
            }

            let (cid, label) = if id_re.is_match(entry) {
                let label = by_id
                    .get(entry)
                    .and_then(|ch| ch.get("name"))
                    .and_then(Value::as_str)
                    .map(|n| format!("#{n}"))
                    .unwrap_or_else(|| entry.clone());
                (entry.clone(), label)
            } else if let Some(cid) = by_name.get(name) {
                (cid.clone(), format!("#{name}"))
            } else {
                die(format!(
                    "channel {entry:?} not found. Run --list-channels to see the exact name/id."
                ));
            };

            resolved.push((cid.clone(), label.clone()));
            // conversations_list returns channels the app has NOT joined, but
            // message events are only delivered for channels it is a member of.
            // Without this check the bot starts cleanly and then sees nothing.
            if let Some(ch) = by_id.get(&cid) {
                if ch.get("is_member").and_then(Value::as_bool) != Some(true) {
                    not_member.push(label);
                }
            }
        }

        *self.channel_ids.write().unwrap_or_else(|e| e.into_inner()) = resolved;

        if !not_member.is_empty() && self.cfg.user_mode {
            die(format!(
                "your account is not a member of: {}\n\
                 In user mode the bot reads with your access, so you have to be in the \
                 channel yourself. Fix: open each channel in Slack and join it.",
                not_member.join(", ")
            ));
        }
        if !not_member.is_empty() {
            die(format!(
                "the app is not a member of: {}\n\
                 Message events are only delivered for channels it has joined.\n\
                 Fix: in Slack, run  /invite @<your-app-name>  in each of those channels.\n\
                 If an admin will not add the app to a private channel, run with --as-me \
                 instead (reads as you, no invite needed).",
                not_member.join(", ")
            ));
        }

        let stale = self.state.forget_fallbacks();
        if stale > 0 {
            log_debug!(
                "un-claimed {stale} alert(s) that only ever got the old reaction/reply fallback, \
                 so a real ack can be attempted"
            );
        }

        let channels = self.channels();
        log_debug!(
            "watching {} channel(s): {}",
            channels.len(),
            channels.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join(", ")
        );
    }

    // -- user mode: poll history instead of receiving events ---------------- //

    /// Read each channel's new messages on a timer, as the signed-in user.
    ///
    /// Socket Mode is not an option here: Events API messages are only delivered
    /// for channels the *app* belongs to, and the whole point of user mode is that
    /// the app cannot be added. conversations.history with a user token needs no
    /// invite -- it is simply what we can already read.
    pub fn poll_forever(&self, stop: &Stop) {
        // Only today's alerts: a channel with no cursor, or one left behind by an
        // earlier run, starts at 00:00 instead of catching up on old history.
        let floor = format!("{:.6}", self.history_floor);
        for (cid, _) in self.channels() {
            if self
                .state
                .get_cursor(&cid)
                .is_none_or(|c| ts_f64(&c) < self.history_floor)
            {
                self.state.set_cursor(&cid, &floor);
            }
        }

        log_debug!(
            "polling every {}s (messages since {})",
            self.cfg.poll_interval,
            chrono::DateTime::from_timestamp(self.history_floor as i64, 0)
                .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default()
        );
        while !stop.is_set() {
            // Renew ahead of the deadline rather than waiting for Slack to refuse a
            // call: a no-op until the token is inside the margin, so the steady
            // state costs nothing and no poll is ever wasted.
            self.refresh_token_if_needed(stop);

            // Iterate a snapshot: handle_missing_channel() may rebind the list
            // mid-tick, and this tick should finish on the copy it started with.
            for (cid, label) in self.channels() {
                if stop.is_set() {
                    break;
                }
                match self.poll_channel(&cid) {
                    Ok(()) => {
                        self.channel_failures
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&cid);
                    }
                    Err(exc) => match exc.api_error() {
                        Some(err @ ("not_in_channel" | "channel_not_found")) => {
                            // Losing access mid-run is worth a loud line, not a
                            // crash: the other channels should keep being watched.
                            self.handle_missing_channel(&cid, &label, err);
                        }
                        Some("token_expired" | "token_revoked") => {
                            // Belt to the proactive braces above: a clock skew, a
                            // suspended laptop or a token revoked early can still
                            // get here. The cursor is untouched, so the next tick
                            // re-reads this channel and nothing is missed.
                            if self.renew_token() {
                                log_debug!("token renewed -- retrying {label} next tick");
                            } else {
                                // Don't exit -- see wait_out_refresh_failure().
                                self.wait_out_refresh_failure(stop);
                                break; // re-check the token before more channels
                            }
                        }
                        Some("ratelimited") => {
                            // Honour Retry-After instead of spending the next ticks
                            // collecting the same 429.
                            let wait = exc.retry_after(self.cfg.poll_interval);
                            log_warn!("{label}: rate limited -- waiting {wait}s");
                            stop.wait(wait as f64);
                        }
                        Some("missing_scope") => die(format!(
                            "{label}: user token is missing a scope (needs channels:history for \
                             public, groups:history for private). Add it under User Token Scopes, \
                             reinstall, and use the new xoxp- token."
                        )),
                        Some(err) => log_error!("{label}: history failed: {err}"),
                        None => {
                            // The client already retried this several times with
                            // backoff. Slack or the link is having a bad minute; the
                            // cursor was never advanced, so the next tick re-reads
                            // the same window and no alert is missed. A stack trace
                            // would say nothing a reader could act on, so log the
                            // one line that matters.
                            let kind = match &exc {
                                SlackError::Network { kind, .. } => *kind,
                                _ => "NetworkError",
                            };
                            log_warn!("{label}: network error ({kind}) -- retrying next tick");
                        }
                    },
                }
            }
            stop.wait(self.cfg.poll_interval as f64);
        }
    }

    /// A channel stopped answering: re-resolve its id, don't log for ever.
    ///
    /// Ids are resolved once at startup. A channel converted to private, or replaced
    /// by a new one reusing the name, gets a *different* id -- and re-requesting the
    /// dead one every tick looks exactly like the bot being stuck on "not found". So
    /// after a few failures, look the name up again and move to whatever id it has now.
    fn handle_missing_channel(&self, cid: &str, label: &str, err: &str) {
        let seen = {
            let mut failures = self.channel_failures.lock().unwrap_or_else(|e| e.into_inner());
            let seen = failures.get(cid).copied().unwrap_or(0) + 1;
            failures.insert(cid.to_string(), seen);
            seen
        };

        if seen % RERESOLVE_AFTER != 0 {
            log_error!("{label}: {err} -- are you still a member?");
            return;
        }

        let by_name = match self.channel_directory() {
            Ok((by_name, _)) => by_name,
            Err(exc) => {
                log_error!("{label}: {err}, and re-resolving it failed: {exc}");
                return;
            }
        };

        let found = by_name.get(label.trim_start_matches('#'));
        let Some(found) = found.filter(|f| f.as_str() != cid) else {
            log_error!(
                "{label}: still {err} after {seen} tries, and the name resolves to the same id \
                 -- rejoin it in Slack, or drop it from {}",
                self.cfg.source.display()
            );
            return;
        };

        log_warn!("{label}: id changed ({cid} -> {found}) -- watching the new one");
        {
            let mut channels = self.channel_ids.write().unwrap_or_else(|e| e.into_inner());
            for (id, _) in channels.iter_mut() {
                if id == cid {
                    *id = found.clone();
                }
            }
        }
        // Carry the cursor over so the switch does not re-read (or skip) history.
        let carried = self.state.get_cursor(cid).unwrap_or_else(now_ts);
        self.state.set_cursor(found, &carried);
        self.channel_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(cid);
    }

    /// Refresh the user token and point the client at it. False if it stayed put.
    pub fn renew_token(&self) -> bool {
        let token = match tokens::ensure_fresh(tokens::MARGIN_SECONDS, tokens::env_path(), true, true)
        {
            Ok(token) => token,
            Err(exc) => {
                log_error!("{exc}");
                return false;
            }
        };
        match token {
            Some(token) if token != self.web.token() => {
                self.web.set_token(&token);
                self.refresh_failures.store(0, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    /// Renew before the deadline. No-op unless the token is near expiry.
    fn refresh_token_if_needed(&self, stop: &Stop) {
        let token =
            match tokens::ensure_fresh(tokens::MARGIN_SECONDS, tokens::env_path(), false, true) {
                Ok(token) => token,
                Err(_) => {
                    // The token is still valid for up to MARGIN_SECONDS, so polling
                    // can carry on; retry sooner than the next tick only if that
                    // runs out.
                    self.wait_out_refresh_failure(stop);
                    return;
                }
            };
        if let Some(token) = token {
            if token != self.web.token() {
                self.web.set_token(&token);
                self.refresh_failures.store(0, Ordering::SeqCst);
                log_debug!("swapped in the renewed user token");
            }
        }
    }

    /// Back off after a failed renewal, then let the loop try again.
    ///
    /// Deliberately not fatal. Every reason a refresh fails is either transient
    /// (Slack unreachable, a 5xx) or fixable in place without stopping the bot:
    /// re-running `ack-bot get-user-token` rewrites the credentials file, and
    /// ensure_fresh() re-reads it each time, so the next attempt picks the new token
    /// up on its own. Exiting here would instead take the bot down until someone
    /// noticed.
    fn wait_out_refresh_failure(&self, stop: &Stop) {
        let failures = self.refresh_failures.fetch_add(1, Ordering::SeqCst) + 1;
        let wait = self
            .cfg
            .poll_interval
            .saturating_mul(1u64 << (failures - 1).min(20))
            .min(REFRESH_RETRY_CAP);
        log_error!(
            "token refresh has failed {failures} time(s) in a row -- retrying in {wait}s. \
             If it does not clear on its own: {}",
            tokens::REAUTH_HINT
        );
        stop.wait(wait as f64);
    }

    fn poll_channel(&self, cid: &str) -> Result<(), SlackError> {
        let cursor = self.state.get_cursor(cid);
        let mut newest = cursor.clone();
        // Alert tools often post first and attach the Ack button in an edit, and
        // history has no "edited since" filter -- an edit does not change the ts, so
        // a cursor-only read would never see the button appear. Re-read a trailing
        // window every poll; the dedupe table makes that free.
        let mut oldest = cursor.clone();
        if self.cfg.recheck_minutes > 0 {
            // Never reach back past 00:00 of the start day, even just after midnight.
            let since = (now() - (self.cfg.recheck_minutes * 60) as f64).max(self.history_floor);
            let window = format!("{since:.6}");
            if oldest.as_ref().is_none_or(|o| ts_f64(&window) < ts_f64(o)) {
                oldest = Some(window);
            }
        }

        let mut pages = 0;
        let mut page_cursor: Option<String> = None;
        loop {
            let resp = self.web.conversations_history(
                cid,
                oldest.as_deref(),
                None,
                200,
                page_cursor.as_deref(),
                false,
            )?;
            let msgs = webapi::messages(&resp);
            // history returns newest-first; ack oldest-first so alerts are handled
            // in the order they were posted.
            for msg in msgs.iter().rev() {
                let Some(ts) = msg.get("ts").and_then(Value::as_str) else { continue };
                if newest.as_ref().is_none_or(|n| ts_f64(ts) > ts_f64(n)) {
                    newest = Some(ts.to_string());
                }
                let mut event = msg.as_object().cloned().unwrap_or_default();
                event.insert("type".to_string(), Value::String("message".to_string()));
                event.insert("channel".to_string(), Value::String(cid.to_string()));
                self.on_event(&Value::Object(event));
            }
            page_cursor = webapi::next_cursor(&resp);
            pages += 1;
            if page_cursor.is_none() || pages >= 10 {
                break;
            }
        }

        if let Some(newest) = newest {
            if Some(&newest) != cursor.as_ref() {
                // Only advance after the batch is handled: a crash mid-batch should
                // re-read it, and the dedupe table stops anything acking twice.
                self.state.set_cursor(cid, &newest);
            }
        }
        Ok(())
    }

    // -- event entry point ------------------------------------------------- //

    pub fn on_event(&self, event: &Value) {
        if event.get("type").and_then(Value::as_str) != Some("message") {
            return;
        }

        let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("");
        let empty = Value::Object(Map::new());
        let msg: &Value = if subtype == "message_changed" {
            // Alert tools often post first, then edit the message to attach the Ack
            // button -- so edits matter as much as new posts.
            event.get("message").filter(|m| m.is_object()).unwrap_or(&empty)
        } else if matches!(
            subtype,
            "message_deleted" | "channel_join" | "channel_leave" | "thread_broadcast"
        ) {
            return;
        } else {
            event
        };

        let channel = event
            .get("channel")
            .and_then(Value::as_str)
            .or_else(|| msg.get("channel").and_then(Value::as_str));
        let ts = msg.get("ts").and_then(Value::as_str);
        let (Some(channel), Some(ts)) = (channel, ts) else { return };
        let Some(label) = self.label_for(channel) else { return };

        let bot_id = msg.get("bot_id").and_then(Value::as_str);
        let user = msg.get("user").and_then(Value::as_str);
        if bot_id.is_some()
            && bot_id.map(str::to_string)
                == *self.self_bot_id.read().unwrap_or_else(|e| e.into_inner())
        {
            return; // anything we posted ourselves (bot identity)
        }
        if user.is_some()
            && user.map(str::to_string)
                == *self.self_user_id.read().unwrap_or_else(|e| e.into_inner())
        {
            return; // anything we posted ourselves (user identity)
        }
        if self.cfg.ack_bots_only && bot_id.is_none() && msg.get("app_id").is_none() {
            log_debug!("skip {channel}/{ts}: human message");
            return;
        }

        let buttons = extract_buttons(msg);
        let ack_buttons: Vec<&Button> = buttons
            .iter()
            .filter(|b| self.cfg.button_match.is_match(&b.text))
            .collect();

        // Log the alert itself, not just its ids -- without this the operator cannot
        // tell from the log which alarm was acked, and every line looks identical.
        // Full text under --verbose, one truncated line otherwise.
        let body = message_text(msg);
        if !buttons.is_empty() || !body.is_empty() {
            log_debug!(
                "{label}/{ts} message: {}",
                if body.is_empty() { "(no text)" } else { &body }
            );
            let described = buttons
                .iter()
                .map(|b| {
                    let arrow = b.url.as_deref().map(|u| format!("->{u}")).unwrap_or_default();
                    format!("{:?}{arrow}", b.text)
                })
                .collect::<Vec<_>>()
                .join(", ");
            log_debug!(
                "{label}/{ts} buttons: {}",
                if described.is_empty() { "(none)".to_string() } else { described }
            );
        }

        if ack_buttons.is_empty() && buttons.is_empty() {
            log_debug!("skip {channel}/{ts}: no buttons");
            return;
        }
        if ack_buttons.is_empty() {
            log_debug!(
                "skip {label}: {} button(s) ({}) match none of {} | {}",
                buttons.len(),
                buttons.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join(", "),
                self.cfg.button_match.as_str(),
                clip_default(&body)
            );
            return;
        }

        // Claim before logging, not after. A poll window overlaps the previous one,
        // so an alert we acked hours ago comes back in the history on every tick;
        // logging first meant one ALERT line per already-acked message per poll, and
        // the log filled with alerts that nothing was going to be done about. claim()
        // is a single atomic INSERT and is the only thing that distinguishes a new
        // alert from a repeat, so it has to run before we decide to say anything.
        if !self.state.claim(channel, ts) {
            log_debug!("skip {channel}/{ts}: already acked");
            return;
        }

        log_debug!("ALERT {label}/{ts} | {}", clip_default(&body));

        match self.acknowledge(channel, ts, msg, &ack_buttons) {
            Ok(method) => {
                self.state.record(channel, ts, &method);
                let label = self.label_or_id(channel);
                // The one line logged by default, so it carries the alert text too.
                log_info!("ACKED {label}/{ts} via {method} | {}", clip_default(&body));
                self.note(Outcome::Acked, &label, ts, &method);
            }
            // Not an error in the code, a missing capability: say so in one line
            // rather than a traceback, and leave the alert visibly unacked.
            Err(AckError::NoAckPath(why)) => {
                self.state.release(channel, ts);
                log_warn!("NOT ACKED {label}/{ts}: {why}");
                self.note(Outcome::NotAcked, &label, ts, &why);
            }
            // Usually the browser session expiring mid-run, which would otherwise
            // print a traceback per alert for as long as it stays expired.
            Err(AckError::Click(exc)) => {
                self.state.release(channel, ts);
                let detail = exc.to_string();
                log_error!("NOT ACKED {label}/{ts}: {detail}");
                self.note(Outcome::Failed, &label, ts, &detail);
            }
            Err(AckError::Other(detail)) => {
                self.state.release(channel, ts);
                log_error!("ack failed for {channel}/{ts} -- will retry on next event: {detail}");
                self.note(Outcome::Failed, &label, ts, &detail);
            }
        }
    }

    /// Push one outcome to the dashboard feed, if one is attached. No-op in plain
    /// mode, so it is cheap to call on every handled alert.
    fn note(&self, outcome: Outcome, channel: &str, ts: &str, detail: &str) {
        if let Some(activity) = &self.activity {
            activity.record(outcome, channel, ts, detail);
        }
    }

    // -- the three ack strategies ------------------------------------------ //

    pub fn acknowledge(
        &self,
        channel: &str,
        ts: &str,
        msg: &Value,
        buttons: &[&Button],
    ) -> Result<String, AckError> {
        let link_buttons: Vec<&&Button> = buttons.iter().filter(|b| b.clickable()).collect();
        let press_buttons: Vec<&&Button> = buttons.iter().filter(|b| b.pressable()).collect();

        if let Some(btn) = link_buttons.first().filter(|_| self.cfg.url_click_enabled) {
            let url = btn.url.clone().unwrap_or_default();
            if !self.cfg.host_allowed(&url) {
                log_warn!("ack button url host not in allow_hosts, not firing: {url}");
            } else {
                if self.cfg.dry_run {
                    return Ok(format!("dry-run:url_click {} {url}", self.cfg.url_click_method));
                }
                let method = reqwest::Method::from_bytes(self.cfg.url_click_method.as_bytes())
                    .map_err(|e| AckError::Other(format!("url_click method is not valid: {e}")))?;
                let mut request = self
                    .http
                    .request(method, &url)
                    .timeout(Duration::from_secs(self.cfg.url_click_timeout));
                for (key, value) in &self.cfg.url_headers {
                    request = request.header(key, value);
                }
                let resp = request
                    .send()
                    .map_err(|e| AckError::Other(format!("url_click failed: {e}")))?;
                let status = resp.status().as_u16();
                if status >= 400 {
                    let body = resp.text().unwrap_or_default();
                    let snippet: String = body.chars().take(200).collect();
                    return Err(AckError::Other(format!("url_click {status}: {snippet}")));
                }
                let host = url::Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_default();
                return Ok(format!("url_click:{host}:{status}"));
            }
        }

        if let Some(btn) = press_buttons.first() {
            if self.cfg.click_enabled && self.clicker.is_some() {
                return self.press(channel, ts, msg, btn);
            }
        }

        for (name, pcfg) in &self.cfg.providers {
            if !pcfg.enabled {
                continue;
            }
            let Some(func) = providers::provider_fn(name) else {
                log_warn!("unknown provider {name:?} in config");
                continue;
            };
            if self.cfg.dry_run {
                return Ok(format!("dry-run:provider:{name}"));
            }
            match func(pcfg, msg) {
                Ok(method) => return Ok(method),
                Err(ProviderError(why)) => log_debug!("provider {name} declined: {why}"),
            }
        }

        // Nothing else to try. Deliberately no reaction/thread-reply consolation
        // prize: it looks like an ack in the channel without being one, which is
        // worse than a message that is visibly still waiting for a human.
        let why = if let Some(btn) = press_buttons.first() {
            if self.clicker.is_none() {
                format!(
                    "button {:?} is interactive and no browser session is configured -- set {} and {}",
                    btn.text, self.cfg.click_token_env, self.cfg.click_cookie_env
                )
            } else if !self.cfg.click_enabled {
                "slack_click is disabled in config".to_string()
            } else {
                "no ack path".to_string()
            }
        } else if let Some(btn) = buttons.first() {
            format!(
                "button {:?} is neither a link nor an addressable block element",
                btn.text
            )
        } else {
            "no ack path".to_string()
        };
        Err(AckError::NoAckPath(why))
    }

    // -- pressing an interactive button ------------------------------------- //

    /// Click an interactive button, then confirm the app actually acted.
    pub fn press(
        &self,
        channel: &str,
        ts: &str,
        msg: &Value,
        btn: &Button,
    ) -> Result<String, AckError> {
        if self.cfg.dry_run {
            return Ok(format!(
                "dry-run:click {}={}",
                btn.action_id.as_deref().unwrap_or("None"),
                btn.value.as_deref().unwrap_or("None")
            ));
        }
        let Some(clicker) = &self.clicker else {
            return Err(AckError::NoAckPath(
                "no browser session is configured, so the button cannot be pressed".to_string(),
            ));
        };

        // The interaction is addressed to the app by the message's bot_id, and Slack
        // rejects the request outright without it -- so fail here with a readable
        // reason rather than sending something it will refuse.
        let Some(service_id) = msg.get("bot_id").and_then(Value::as_str) else {
            return Err(AckError::NoAckPath(
                "the message has no bot_id, so the click cannot be addressed to the app that \
                 owns the button"
                    .to_string(),
            ));
        };
        let bot = clicker.bot_info(service_id);
        click::click(
            clicker,
            click::ClickTarget {
                channel,
                message_ts: ts,
                block_id: btn.block_id.as_deref().unwrap_or(""),
                element: &btn.element,
                service_id,
                blocks: msg.get("blocks"),
                thread_ts: msg.get("thread_ts").and_then(Value::as_str),
                bot_user_id: bot.get("user_id").and_then(Value::as_str),
                is_ephemeral: false,
            },
        )?;

        // `ok: true` only means Slack accepted the interaction and forwarded it to
        // the app -- the ack itself happens in the app, asynchronously. The only
        // honest confirmation is the Ack button disappearing from the message, so
        // wait for that instead of reporting success on faith.
        let mut gone: Option<bool> = None;
        for attempt in 1..=self.cfg.click_verify_attempts {
            sleep(self.cfg.click_verify_delay);
            gone = self.button_gone(channel, ts, btn)?;
            if gone == Some(true) {
                return Ok(format!(
                    "click:{}:{}",
                    btn.action_id.as_deref().unwrap_or("None"),
                    btn.value.as_deref().unwrap_or("-")
                ));
            }
            log_debug!(
                "{channel}/{ts}: {} after click ({attempt}/{})",
                if gone.is_none() {
                    "cannot re-read the message"
                } else {
                    "button still there"
                },
                self.cfg.click_verify_attempts
            );
        }

        let detail = if gone.is_none() {
            "the message can no longer be read back, so the click cannot be confirmed".to_string()
        } else {
            format!(
                "the {:?} button is still on the message -- the app did not act on it",
                btn.text
            )
        };
        Err(AckError::Other(format!(
            "clicked {:?} (value={:?}) and Slack accepted it, but {detail}",
            btn.action_id.as_deref().unwrap_or("None"),
            btn.value.as_deref().unwrap_or("None")
        )))
    }

    pub fn fetch_message(&self, channel: &str, ts: &str) -> Result<Option<Value>, SlackError> {
        let resp = self
            .web
            .conversations_history(channel, Some(ts), Some(ts), 1, None, true)?;
        for msg in webapi::messages(&resp) {
            if msg.get("ts").and_then(Value::as_str) == Some(ts) {
                return Ok(Some(msg.clone()));
            }
        }
        Ok(None)
    }

    /// Has the button we pressed disappeared from the message?
    ///
    /// This is the proof that the click landed: these apps rewrite the message in
    /// response, dropping the button that no longer applies (Acknowledge vanishes
    /// once acknowledged). Keyed on action_id rather than the label, so it holds for
    /// any button, not just the Ack one. None means the message could not be re-read,
    /// which is inconclusive rather than a failure.
    pub fn button_gone(
        &self,
        channel: &str,
        ts: &str,
        btn: &Button,
    ) -> Result<Option<bool>, SlackError> {
        let Some(msg) = self.fetch_message(channel, ts)? else {
            return Ok(None);
        };
        Ok(Some(
            !extract_buttons(&msg)
                .iter()
                .any(|b| b.action_id == btn.action_id),
        ))
    }
}
