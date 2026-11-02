//! Auto-acknowledge alerts posted to specific Slack channels.
//!
//! Listens over Socket Mode. For every new (or edited) message in an allowlisted
//! channel it looks for an "Acknowledge" button and tries, in order:
//!
//!   1. url_click  -- the button is a link button; fire the HTTP request a browser
//!                    would make when the user clicks it. This is a real ack.
//!   2. click      -- the button is interactive (action_id, no url), which the
//!                    public Slack API cannot press. Press it the way the Slack
//!                    client itself does, using browser-session credentials, then
//!                    verify the button actually went away. See click.rs.
//!   3. provider   -- scrape the incident id out of the message and ack through the
//!                    source tool's own API (PagerDuty, Opsgenie).
//!
//! If none of those apply the message is left exactly as it was and logged as NOT
//! ACKED. There is no reaction/thread-reply consolation prize: it would look like an
//! ack in the channel without being one.
//!
//! Two ways to see the messages:
//!
//!   bot mode (default)  -- Socket Mode events, needs the app invited to every
//!                          channel. Fastest, but impossible for private channels an
//!                          admin will not add the app to.
//!   user mode (--as-me) -- polls conversations.history with your own user token.
//!                          Works for any private channel *you* are already in,
//!                          because it is your access, not the app's. No invite, no
//!                          app-level token, and acks post as you.
//!
//! Credentials and config live in ~/.slack/ (env, config.yaml, ack_state.sqlite3),
//! so the binary can be run from any directory:
//!
//!     ack-bot                       # what config.yaml says (user mode here)
//!     ack-bot --dry-run --verbose
//!     ack-bot --list-channels
//!     ack-bot --as-me
//!     ack-bot --click C0123ABCDEF 1786288069.567779   # press one known message
//!     ack-bot --test --dry-run                        # what --test would press
//!     ack-bot --test                                  # press it (asks first)
//!     ack-bot paths                                   # where all of that resolved to
//!     ack-bot update                                  # self-update from the release branch
//!     ack-bot uninstall                               # delete the binary, keep ~/.slack

mod activity;
mod bot;
mod buttons;
mod click;
mod config;
mod envstore;
mod importsession;
mod logging;
mod manual;
mod oauth;
mod providers;
mod socket;
mod state;
mod tokens;
mod tui;
mod update;
mod util;
mod webapi;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use regex::Regex;

use crate::bot::AckBot;
use crate::config::Config;
use crate::util::{Stop, die};
use crate::webapi::WebClient;

#[derive(Parser, Debug)]
#[command(
    name = "ack-bot",
    version,
    about = "Auto-acknowledge Slack alerts in specific channels.",
    subcommand_negates_reqs = true
)]
struct Cli {
    /// config.yaml to read (default: ~/.slack/config.yaml, then ./config.yaml)
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// log what would be acked, change nothing
    #[arg(long = "dry-run")]
    dry_run: bool,

    /// print channel ids and exit
    #[arg(long = "list-channels")]
    list_channels: bool,

    /// read as your own account (SLACK_USER_TOKEN) by polling history; no app invite
    /// and no app-level token needed
    #[arg(long = "as-me")]
    as_me: bool,

    /// press the Ack button on one specific message and exit (use this to check the
    /// browser session works)
    #[arg(long, num_args = 2, value_names = ["CHANNEL", "TS"])]
    click: Option<Vec<String>>,

    /// press one button on the newest message that has one, to prove the click path
    /// works. Prefers an outstanding Ack button (a real ack), falling back to Resolve
    /// when there is none. Asks first; combine with --dry-run to only see the target
    #[arg(long)]
    test: bool,

    /// force which button --test presses, instead of Ack-then-Resolve
    #[arg(long = "test-button", value_name = "REGEX")]
    test_button: Option<String>,

    /// skip the --test / uninstall confirmation prompt
    #[arg(long, global = true)]
    yes: bool,

    #[arg(long, short)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Obtain a Slack user token (xoxp-) via OAuth + PKCE and store it
    GetUserToken {
        /// print the authorize URL and exit (the browser step can happen elsewhere)
        #[arg(long)]
        url: bool,
        /// exchange a code (or a pasted redirect URL) for a token
        #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "CODE_OR_URL")]
        exchange: Option<String>,
    },
    /// Extract xoxc/xoxd credentials from a DevTools "Copy as cURL" capture
    ImportSession {
        /// file holding the copied cURL command, or - for stdin
        capture: String,
        /// update the credentials file in place (no value = the file this binary reads)
        #[arg(long, num_args = 0..=1, value_name = "ENVFILE")]
        write: Option<Option<PathBuf>>,
    },
    /// Store the browser session (xoxc token + d cookie) pasted in directly; values
    /// not passed as flags are asked for with hidden input
    SetSession {
        /// xoxc-... token (omit to be prompted, keeps it out of shell history)
        #[arg(long, value_name = "XOXC")]
        token: Option<String>,
        /// value of the `d` cookie, xoxd-... (omit to be prompted)
        #[arg(long, value_name = "XOXD")]
        cookie: Option<String>,
        /// value of the `d-s` cookie, only some SSO workspaces need it
        #[arg(long = "ds-cookie", value_name = "VALUE")]
        ds_cookie: Option<String>,
    },
    /// Print where config, credentials and state resolved to
    Paths,
    /// Update ack-bot to the latest release (needs git access to the release repo)
    Update,
    /// Delete the ack-bot binary (~/.slack is kept); --yes skips the prompt
    Uninstall,
}

fn main() {
    let args = Cli::parse();

    logging::set_level(if args.verbose { logging::DEBUG } else { logging::INFO });

    // The Python's run.sh sourced .env before exec'ing the bot; there is no wrapper
    // here, so the binary loads the credentials file itself. Anything already
    // exported in the environment wins.
    tokens::load_env(tokens::env_path(), false);

    match args.command {
        Some(Command::GetUserToken { url, exchange }) => {
            std::process::exit(oauth::main(url, exchange))
        }
        Some(Command::ImportSession { capture, write }) => {
            std::process::exit(importsession::main(&capture, write))
        }
        Some(Command::SetSession {
            token,
            cookie,
            ds_cookie,
        }) => std::process::exit(importsession::set_session(token, cookie, ds_cookie)),
        Some(Command::Paths) => return print_paths(&args),
        Some(Command::Update) => {
            if let Err(error) = update::self_update() {
                die(error);
            }
            return;
        }
        Some(Command::Uninstall) => {
            if let Err(error) = update::uninstall(args.yes) {
                die(error);
            }
            return;
        }
        None => {}
    }

    let bot_token = envstore::get_ne("SLACK_BOT_TOKEN");
    // Rotating user tokens die after 12h, so renew before reading the value --
    // restarting the bot is then all it takes to recover from an expired one.
    let mut user_token = tokens::ensure_fresh_default();

    if args.list_channels {
        // Prefer the user token here: it is the only one that can see private
        // channels you belong to but the app has not been invited to.
        let token = user_token.clone().or_else(|| bot_token.clone());
        let Some(token) = token else {
            die("set SLACK_USER_TOKEN (preferred) or SLACK_BOT_TOKEN");
        };
        if user_token.is_none() {
            log_warn!(
                "listing with the bot token -- private channels the app is not in cannot appear \
                 at all. Set SLACK_USER_TOKEN to list yours."
            );
        }
        manual::list_channels(&WebClient::new(&token));
        return;
    }

    let config_path = args.config.clone().unwrap_or_else(config::default_config_path);
    let mut cfg = Config::load(&config_path);
    log_debug!("config: {}", cfg.source.display());
    log_debug!("credentials: {}", tokens::env_path().display());
    log_debug!("state: {}", cfg.state_db.display());

    if args.as_me {
        cfg.user_mode = true;
    }
    if args.dry_run {
        cfg.dry_run = true;
    }
    if cfg.dry_run {
        log_warn!("DRY RUN -- no acks will be sent");
    }
    if cfg.url_click_enabled && cfg.url_allow_hosts.is_empty() {
        die("url_click.allow_hosts is empty -- refusing to fire arbitrary URLs");
    }

    let clicker = manual::build_clicker(&cfg);

    if args.click.is_some() || args.test {
        // Both are one-shot: read the message, press one button, print, exit.
        let token = user_token.clone().or_else(|| bot_token.clone());
        let Some(token) = token else {
            die("set SLACK_USER_TOKEN or SLACK_BOT_TOKEN so the message can be read back");
        };
        let one = AckBot::new(cfg, WebClient::new(&token), clicker);
        match args.click {
            Some(pair) => manual::click_once(&one, &pair[0], &pair[1]),
            None => {
                let pattern = args.test_button.as_deref().map(|p| match Regex::new(p) {
                    Ok(re) => re,
                    Err(e) => die(format!("--test-button is not a valid regex: {e}")),
                });
                manual::test_press(&one, pattern, args.yes);
            }
        }
        return;
    }

    let stop = Arc::new(Stop::new());
    install_signal_handlers(stop.clone());

    if cfg.user_mode {
        // Start every long run on a token with its full lifetime ahead of it, rather
        // than on whatever is left of the one the credentials file happens to hold.
        // The poll loop renews on its own, so this only decides where the clock
        // starts -- but it means a restart is always a clean 12h, and a token spent
        // by another process is replaced here instead of at the first poll.
        // Non-rotating and no-refresh-token setups no-op inside ensure_fresh(), so
        // this costs nothing when it cannot help.
        if cfg.refresh_on_start {
            match tokens::ensure_fresh(tokens::MARGIN_SECONDS, tokens::env_path(), true, true) {
                Ok(Some(token)) => user_token = Some(token),
                Ok(None) => {}
                // A refresh that fails at startup is not fatal while the token in
                // hand is still good: fall through to the validity check below and
                // let the poll loop retry on its own schedule.
                Err(exc) => {
                    log_warn!("startup refresh failed ({exc}) -- carrying on with the stored token")
                }
            }
        }

        // Rotation-enabled workspaces issue "xoxe.xoxp-..." rather than plain
        // "xoxp-..."; both are user tokens. Rejecting the former would be wrong.
        let valid = user_token
            .as_deref()
            .is_some_and(|t| t.starts_with("xoxp-") || t.starts_with("xoxe.xoxp-"));
        if !valid {
            // A bot token here authenticates fine, so the failure would otherwise
            // surface much later as "channel not found" -- private channels are
            // simply invisible to an app that was never invited.
            let what = match user_token.as_deref() {
                None => "is not set".to_string(),
                Some(token) => format!(
                    "is a {}- token, not a user token",
                    token.split('-').next().unwrap_or(token)
                ),
            };
            die(format!(
                "user mode needs SLACK_USER_TOKEN to be an xoxp-... token; it {what}.\n\
                 Get one: ack-bot get-user-token\n\
                 or api.slack.com/apps -> your app -> OAuth & Permissions ->\n  \
                 User Token Scopes: groups:history, groups:read, channels:history,\n  \
                 channels:read, reactions:write, chat:write\n\
                 then Reinstall to Workspace and copy the 'User OAuth Token'."
            ));
        }
        // One identity for everything: reads and clicks are all you.
        let me = WebClient::new(user_token.as_deref().unwrap_or_default());
        let mut bot = AckBot::new(cfg, me, clicker);

        // A terminal gets the live dashboard; a pipe / service keeps the plain log.
        let activity = tui_enabled().then(|| {
            let act = activity::Activity::new("user", bot.cfg.dry_run);
            bot.set_activity(act.clone());
            logging::silence();
            act
        });

        let bot = Arc::new(bot);
        bot.resolve();
        bot.publish_channels();
        log_debug!("user mode -- reading as you, no app membership involved (ctrl-c to stop)");

        match activity {
            Some(activity) => {
                // Poll on a background thread; the dashboard owns the main thread.
                let worker = {
                    let bot = bot.clone();
                    let stop = stop.clone();
                    std::thread::spawn(move || bot.poll_forever(&stop))
                };
                run_dashboard(&activity, &stop);
                let _ = worker.join();
            }
            None => bot.poll_forever(&stop),
        }
        log_debug!("shutting down");
        return;
    }

    let Some(bot_token) = bot_token else {
        die("SLACK_BOT_TOKEN is not set");
    };
    let web = WebClient::new(&bot_token);

    let Some(app_token) = envstore::get_ne("SLACK_APP_TOKEN") else {
        die("SLACK_APP_TOKEN is not set (needs an xapp- app-level token with connections:write)");
    };

    let mut bot = AckBot::new(cfg, web, clicker);
    let activity = tui_enabled().then(|| {
        let act = activity::Activity::new("bot", bot.cfg.dry_run);
        bot.set_activity(act.clone());
        logging::silence();
        act
    });
    let bot = Arc::new(bot);
    bot.resolve();
    bot.publish_channels();

    // The socket lives in its own thread so ctrl-c is answered promptly, which is
    // what slack_sdk's SocketModeClient did with its own background threads.
    let listener = {
        let bot = bot.clone();
        let stop = stop.clone();
        std::thread::spawn(move || socket::run(app_token, bot, stop))
    };

    // In a terminal the dashboard owns the main thread until the user quits (which
    // sets the stop flag); otherwise just wait for the signal as before.
    match activity {
        Some(activity) => run_dashboard(&activity, &stop),
        None => stop.wait_forever(),
    }
    log_debug!("shutting down");
    // The reader may be parked in a 45s socket read; the process is on its way out,
    // so don't hold shutdown up waiting for it.
    drop(listener);
}

/// True when stdout is a real terminal, so the full-screen dashboard is safe to
/// draw. A pipe, a redirect to a file, or a service manager's captured stdout all
/// return false, and the bot keeps its plain stderr log.
fn tui_enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Draw the live dashboard until the user quits or the bot thread stops (the
/// shared `stop` flag). Quitting sets the flag so the worker winds down. Any TUI
/// error is swallowed after the terminal is restored — a dashboard failure must
/// not take the bot's own exit path down with it.
fn run_dashboard(activity: &activity::Activity, stop: &Arc<Stop>) {
    let done = {
        let stop = stop.clone();
        move || stop.is_set()
    };
    let quit = {
        let stop = stop.clone();
        move || stop.set()
    };
    if let Err(e) = tui::run(activity, done, quit) {
        // The terminal is already restored by tui::run; now that logging may be
        // visible again, say why the dashboard stopped.
        eprintln!("dashboard error: {e}");
    }
}

fn install_signal_handlers(stop: Arc<Stop>) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    // A handler thread rather than a bare flag: setting the flag has to notify the
    // condvar the waits are parked on, and locking a mutex inside a real signal
    // handler is not allowed.
    match Signals::new([SIGINT, SIGTERM]) {
        Ok(mut signals) => {
            std::thread::spawn(move || {
                for _ in signals.forever() {
                    stop.set();
                }
            });
        }
        Err(e) => log_warn!("could not install signal handlers ({e}) -- ctrl-c may be abrupt"),
    }
}

fn print_paths(args: &Cli) {
    let config_path = args.config.clone().unwrap_or_else(config::default_config_path);
    let exists = |p: &std::path::Path| if p.exists() { "" } else { "   (missing)" };
    println!(
        "credentials : {}{}",
        tokens::env_path().display(),
        exists(tokens::env_path())
    );
    println!("config      : {}{}", config_path.display(), exists(&config_path));
    if config_path.exists() {
        let cfg = Config::load(&config_path);
        println!("state db    : {}{}", cfg.state_db.display(), exists(&cfg.state_db));
        println!(
            "mode        : {}",
            if cfg.user_mode {
                "user (polls history as you)"
            } else {
                "bot (socket mode)"
            }
        );
        println!("channels    : {}", cfg.channels.join(", "));
    }
    println!("\noverrides   : SLACK_ACK_ENV, SLACK_ACK_CONFIG, --config PATH");
    let have = |key: &str| match envstore::get_ne(key) {
        Some(_) => "set",
        None => "-",
    };
    println!(
        "\ncredentials present:\n  \
         SLACK_USER_TOKEN   {}\n  \
         SLACK_BOT_TOKEN    {}\n  \
         SLACK_APP_TOKEN    {}\n  \
         SLACK_CLIENT_TOKEN {}\n  \
         SLACK_D_COOKIE     {}",
        have("SLACK_USER_TOKEN"),
        have("SLACK_BOT_TOKEN"),
        have("SLACK_APP_TOKEN"),
        have("SLACK_CLIENT_TOKEN"),
        have("SLACK_D_COOKIE"),
    );
}
