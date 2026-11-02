//! The by-hand paths: `--list-channels`, `--click`, `--test`. Everything here
//! prints to stdout (the Python used `print`) and exits, rather than running a loop.

use std::io::{self, BufRead, Write};

use regex::Regex;
use serde_json::Value;

use crate::bot::{AckBot, AckError, default_test_button};
use crate::buttons::{Button, clip, extract_buttons, message_text};
use crate::config::Config;
use crate::util::die;
use crate::webapi::{self, WebClient};
use crate::{envstore, log_debug, log_info, log_warn};

pub fn list_channels(web: &WebClient) {
    let mut cursor: Option<String> = None;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    loop {
        let resp = match web.conversations_list("public_channel,private_channel", 1000, cursor.as_deref()) {
            Ok(resp) => resp,
            Err(exc) => {
                // ensure_fresh() already had its chance in main(), so there is nothing
                // left to retry -- say what to do instead of dumping a traceback.
                if exc.api_error() == Some("token_expired") {
                    die(format!(
                        "SLACK_USER_TOKEN has expired and could not be renewed.\n{}",
                        crate::tokens::REAUTH_HINT
                    ));
                }
                die(format!("conversations.list failed: {exc}"));
            }
        };
        for ch in resp
            .get("channels")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or(&[])
        {
            let (Some(id), Some(name)) = (
                ch.get("id").and_then(Value::as_str),
                ch.get("name").and_then(Value::as_str),
            ) else {
                continue;
            };
            let member = if ch.get("is_member").and_then(Value::as_bool) == Some(true) {
                "member"
            } else {
                "-"
            };
            rows.push((id.to_string(), format!("#{name}"), member.to_string()));
        }
        cursor = webapi::next_cursor(&resp);
        if cursor.is_none() {
            break;
        }
    }
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (cid, name, member) in rows {
        let _ = writeln!(out, "{cid}\t{member:7}\t{name}");
    }
}

/// The browser session used to press interactive buttons, if configured.
pub fn build_clicker(cfg: &Config) -> Option<crate::click::ClientSession> {
    if !cfg.click_enabled {
        log_debug!("slack_click disabled -- interactive Ack buttons will be left alone");
        return None;
    }

    let token = envstore::get_ne(&cfg.click_token_env);
    let cookie = envstore::get_ne(&cfg.click_cookie_env);
    let (Some(token), Some(cookie)) = (token.clone(), cookie.clone()) else {
        let missing: Vec<&str> = [
            (cfg.click_token_env.as_str(), token.is_some()),
            (cfg.click_cookie_env.as_str(), cookie.is_some()),
        ]
        .into_iter()
        .filter(|(_, present)| !present)
        .map(|(name, _)| name)
        .collect();
        log_warn!(
            "{} not set -- an interactive Ack button cannot be pressed without a browser \
             session, so those alerts will be reported as NOT ACKED. \
             `ack-bot import-session <capture> --write` copies both out of a DevTools capture.",
            missing.join(" and ")
        );
        return None;
    };
    match crate::click::ClientSession::new(
        &token,
        &cookie,
        envstore::get_ne(&cfg.click_cookie_ds_env).as_deref(),
    ) {
        Ok(session) => Some(session),
        Err(exc) => die(exc.to_string()),
    }
}

/// Print a message the way a human needs to see it before pressing anything.
pub fn describe(bot: &AckBot, channel: &str, ts: &str, msg: &Value) {
    let labels: Vec<String> = extract_buttons(msg)
        .iter()
        .map(|b| {
            format!(
                "{:?}[{}={}]",
                b.text,
                b.action_id.as_deref().unwrap_or("link"),
                b.value.as_deref().unwrap_or("None")
            )
        })
        .collect();
    let channel_label = bot.label_for(channel).unwrap_or_else(|| channel.to_string());
    println!("  channel : {channel_label} ({channel})");
    println!("  ts      : {ts}");
    println!("  message : {}", clip(&message_text(msg), 300));
    println!(
        "  buttons : {}",
        if labels.is_empty() { "(none)".to_string() } else { labels.join(", ") }
    );
    if let Ok(resp) = bot.web.chat_get_permalink(channel, ts) {
        if let Some(link) = resp.get("permalink").and_then(Value::as_str) {
            println!("  link    : {link}");
        }
    }
    // A permalink is nice to have; never worth failing the run over.
}

/// Newest message in the watched channels with a pressable button matching.
pub fn find_pressable(
    bot: &AckBot,
    pattern: &Regex,
    pages: u32,
) -> Option<(String, Value, Button)> {
    for (cid, label) in bot.channels() {
        let mut cursor: Option<String> = None;
        for _ in 0..pages {
            let resp = match bot
                .web
                .conversations_history(&cid, None, None, 200, cursor.as_deref(), false)
            {
                Ok(resp) => resp,
                Err(exc) => {
                    log_warn!("{label}: history failed: {exc}");
                    break;
                }
            };
            for msg in webapi::messages(&resp) {
                // newest first
                for btn in extract_buttons(msg) {
                    if btn.pressable() && pattern.is_match(&btn.text) {
                        return Some((cid.clone(), msg.clone(), btn));
                    }
                }
            }
            cursor = webapi::next_cursor(&resp);
            if cursor.is_none() {
                break;
            }
        }
        log_info!(
            "{label}: no pressable button matching {} in the last {pages} page(s)",
            pattern.as_str()
        );
    }
    None
}

fn confirm(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None, // EOF: no tty to confirm on
        Ok(_) => Some(line.trim().to_lowercase()),
    }
}

/// Press one button by hand to prove the click path works end to end.
///
/// Needed because the mechanism can only be tested against a button that is
/// actually on a message. An outstanding Acknowledge button is the ideal target:
/// pressing it exercises the whole path *and* is the thing the bot exists to do.
/// Only when there is none -- every alert already acknowledged, next one hours away
/// -- does this fall back to Resolve, which reaches the same `blocks.actions` code
/// with a different action_id but closes a live incident.
///
/// Note the asymmetry, which is deliberate: the running bot presses *only* buttons
/// matching `button_match` in config. Resolve is never pressed automatically -- it
/// is reachable exclusively through this flag, one message at a time, after you
/// confirm the specific incident.
pub fn test_press(bot: &AckBot, pattern: Option<Regex>, assume_yes: bool) {
    if bot.clicker.is_none() && !bot.cfg.dry_run {
        die(format!(
            "no browser session configured -- set {} and {} (see click.rs), or add --dry-run \
             to just see which message would be used",
            bot.cfg.click_token_env, bot.cfg.click_cookie_env
        ));
    }
    bot.resolve();

    let candidates: Vec<(String, Regex)> = match pattern {
        // --test-button given: obey it
        Some(pattern) => vec![(pattern.as_str().to_string(), pattern)],
        None => vec![
            (
                "the Ack button, which is a real ack".to_string(),
                bot.cfg.button_match.clone(),
            ),
            (
                "Resolve, since nothing is left unacknowledged".to_string(),
                default_test_button(),
            ),
        ],
    };

    let mut found: Option<(String, Value, Button)> = None;
    for (why, pattern) in &candidates {
        if let Some(hit) = find_pressable(bot, pattern, 2) {
            log_info!("testing with {why}");
            found = Some(hit);
            break;
        }
    }
    let Some((channel, msg, btn)) = found else {
        die(format!(
            "no message with a pressable button matching {} -- pass --test-button REGEX, \
             or wait for a new alert",
            candidates
                .iter()
                .map(|(_, p)| format!("{:?}", p.as_str()))
                .collect::<Vec<_>>()
                .join(" or ")
        ));
    };
    let ts = msg.get("ts").and_then(Value::as_str).unwrap_or("").to_string();

    println!("\nabout to press {:?} on:", btn.text);
    describe(bot, &channel, &ts, &msg);

    if bot.cfg.dry_run {
        println!("\nDRY RUN -- stopping here, nothing was pressed");
        return;
    }

    let real_ack = bot.cfg.button_match.is_match(&btn.text);
    println!(
        "\nThis is a real click: the incident app will treat it as you pressing {:?} on \
         incident {}, and act on it.{}",
        btn.text,
        btn.value.as_deref().unwrap_or("None"),
        if real_ack {
            ""
        } else {
            "\nThat CLOSES an open incident -- it is not an acknowledgement."
        }
    );
    if !assume_yes {
        match confirm("press it? [y/N] ") {
            None => die("no tty to confirm on -- re-run with --yes if you meant it"),
            Some(answer) if answer != "y" && answer != "yes" => die("cancelled -- nothing pressed"),
            Some(_) => {}
        }
    }

    match bot.press(&channel, &ts, &msg, &btn) {
        Ok(result) => println!("\nresult  : {result}"),
        Err(AckError::Click(exc)) => die(exc.to_string()),
        Err(AckError::NoAckPath(why)) => die(why),
        Err(AckError::Other(why)) => {
            // Verification failed. Show the message as it stands now: whether the app
            // changed anything is the useful signal, not the bot's own verdict.
            println!("\nunconfirmed: {why}");
            if let Ok(Some(after)) = bot.fetch_message(&channel, &ts) {
                println!("\nthe message now reads:");
                describe(bot, &channel, &ts, &after);
            }
            std::process::exit(1);
        }
    }

    if let Ok(Some(after)) = bot.fetch_message(&channel, &ts) {
        println!("\nthe message now reads:");
        describe(bot, &channel, &ts, &after);
    }
}

/// Press the Ack button on one message -- the smoke test for the session.
pub fn click_once(bot: &AckBot, channel: &str, ts: &str) {
    let Some(clicker) = &bot.clicker else {
        die("no browser session configured; nothing to test");
    };

    let who = match clicker.whoami() {
        Ok(who) => who,
        Err(exc) => die(exc.to_string()),
    };
    clicker.set_team_id(who.get("team_id").and_then(Value::as_str));
    log_info!(
        "session belongs to {} ({}) in {}",
        who.get("user").and_then(Value::as_str).unwrap_or("None"),
        who.get("user_id").and_then(Value::as_str).unwrap_or("None"),
        who.get("team").and_then(Value::as_str).unwrap_or("None")
    );

    let msg = match bot.fetch_message(channel, ts) {
        Ok(Some(msg)) => msg,
        Ok(None) => die(format!(
            "no message {ts} in {channel} (wrong channel id, or you cannot read it)"
        )),
        Err(exc) => die(format!("could not read {channel}/{ts}: {exc}")),
    };

    describe(bot, channel, ts, &msg);
    let acks: Vec<Button> = extract_buttons(&msg)
        .into_iter()
        .filter(|b| bot.cfg.button_match.is_match(&b.text) && b.pressable())
        .collect();
    let Some(btn) = acks.first() else {
        die("no pressable Ack button on that message -- already acknowledged, or a link button");
    };

    log_info!(
        "pressing {:?} (block_id={} action_id={} value={})",
        btn.text,
        btn.block_id.as_deref().unwrap_or("None"),
        btn.action_id.as_deref().unwrap_or("None"),
        btn.value.as_deref().unwrap_or("None")
    );
    match bot.press(channel, ts, &msg, btn) {
        Ok(result) => println!("{result}"),
        Err(AckError::Click(exc)) => die(exc.to_string()),
        Err(other) => die(other.to_string()),
    }
}
