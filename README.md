# slack-alert-ack (Rust)

Auto-acknowledge alerts posted to specific Slack channels. A port of the Python
`ack_bot.py` (plus `slack_click.py`, `slack_tokens.py`, `get_user_token.py`,
`import_session.py`) into one static binary, `ack-bot`, installed on `PATH` and
runnable from any directory.

    ack-bot                  # run it (user mode, per ~/.slack/config.yaml)
    ack-bot --dry-run -v     # see what it would ack, change nothing
    ack-bot --list-channels
    ack-bot paths            # where config/credentials/state resolved to

Run it in a terminal and it opens a live dashboard; pipe or redirect it (a
service, `… >> ack.log`) and it keeps the plain stderr log instead. See
[The live dashboard](#the-live-dashboard).

## What it does

For every new (or edited) message in an allowlisted channel it looks for an
"Acknowledge" button and tries, in order:

1. **url_click** — the button is a link button; fire the HTTP request a browser
   would make when the user clicks it. A real ack.
2. **click** — the button is interactive (`action_id`, no `url`), which the public
   Slack API cannot press. Press it the way the Slack client itself does
   (`blocks.actions` + browser-session credentials), then verify the button
   actually went away. See `src/click.rs`.
3. **provider** — scrape the incident id out of the message and ack through the
   source tool's own API (PagerDuty, Opsgenie).

If none apply, the message is left exactly as it was and logged `NOT ACKED`. There
is deliberately no reaction/thread-reply fallback: it looks like an ack in the
channel without being one.

Two ways to see the messages:

| mode | how | needs |
| --- | --- | --- |
| bot (default) | Socket Mode events | app invited to every channel, `SLACK_APP_TOKEN` |
| user (`--as-me`, or `user_mode.enabled`) | polls `conversations.history` as you | only your own membership, `SLACK_USER_TOKEN` |

## Layout

Everything the binary needs lives in `~/.slack/` (mode 700), next to `~/.cargo`:

    ~/.slack/env                 credentials, shell-sourceable, mode 600
    ~/.slack/config.yaml         channels, strategies, timings
    ~/.slack/ack_state.sqlite3   dedupe + per-channel read cursors

Resolution order, first hit wins:

* credentials: `$SLACK_ACK_ENV` → `~/.slack/env` → `~/.slack/.env` → `~/.slack`
  (if it is a file) → `./.env`
* config: `--config PATH` → `$SLACK_ACK_CONFIG` → `~/.slack/config.yaml` →
  `./config.yaml`
* state: `state_db` from config; a relative path resolves **against the config's
  directory**, not the working directory — a PATH-installed binary run from
  anywhere must not scatter empty dedupe databases (an empty one means re-acking
  whatever is still inside the recheck window).

`~/.slack/env` keys:

    SLACK_USER_TOKEN              xoxp-... or xoxe.xoxp-...  (user mode)
    SLACK_USER_REFRESH_TOKEN      only source of renewals on rotating apps
    SLACK_USER_TOKEN_EXPIRES_AT   unix seconds; written by each refresh
    SLACK_BOT_TOKEN               xoxb-...   (bot mode)
    SLACK_APP_TOKEN               xapp-...   (bot mode, connections:write)
    SLACK_CLIENT_TOKEN            xoxc-...   browser session, for pressing buttons
    SLACK_D_COOKIE                xoxd-...   browser session
    SLACK_DS_COOKIE               only some SSO workspaces need it
    SLACK_CLIENT_SECRET           only if the app refreshes as a confidential client

Anything already exported in the environment wins over the file, so
`SLACK_USER_TOKEN=… ack-bot` still works as a one-off override.

## Keeping credentials alive

Rotating user tokens (`xoxe.xoxp-…`) expire after 12h. The binary renews from the
stored refresh token — before the deadline while polling, at startup when
`refresh_on_start` is set, and again if Slack answers `token_expired` anyway. Each
refresh spends the refresh token it used, so the new pair is written back to
`~/.slack/env` atomically (temp file + rename: that file is the only copy).

When the refresh token itself is spent, or the browser session expires:

    ack-bot get-user-token                    # OAuth + PKCE, writes the new triple
    ack-bot get-user-token --url              # just print the authorize URL
    ack-bot get-user-token --exchange '<url>' # finish a flow started elsewhere

    # Slack in a browser → DevTools → Network → click any message button →
    # right-click the blocks.actions request → Copy → Copy as cURL
    ack-bot import-session capture.txt --write

`import-session` also diffs the capture's form fields against the ones
`src/click.rs` sends — `blocks.actions` is undocumented, so a capture from your own
workspace is the only authority on its shape.

## Checking the click path

The mechanism can only be tested against a button that is actually on a message:

    ack-bot --test --dry-run              # show the message it would press
    ack-bot --test                        # press it (asks first)
    ack-bot --click C0123ABCDEF 1786288069.567779

`--test` prefers an outstanding **Acknowledge** button, which is a real ack, and
only falls back to **Resolve** when nothing is left unacknowledged — that closes a
live incident, so it says so and asks. The running bot never presses Resolve: it
presses only what `button_match` matches.

## The live dashboard

When stdout is a real terminal, the running bot (`ack-bot` or `ack-bot --as-me`)
draws a full-screen dashboard instead of scrolling log lines:

* a title bar with the mode (`user` / `bot`), a `dry-run` flag when set, uptime
  and a ticking clock;
* a **Watching** panel listing the channels the allowlist resolved to;
* an **Activity** feed of the most recent alerts, each with a status glyph —
  `✓ acked` (green), `● not acked` (yellow), `✗ failed` (red) — the ack method or
  the reason, and a relative age;
* a footer with running `acked / not acked / failed` counts.

Press `q`, `Esc`, or `Ctrl-C` to stop; that sets the same stop flag a signal
would, so the bot winds down cleanly. While the dashboard owns the screen the
stderr log is muted so it cannot scribble over it.

The dashboard is **terminal-only**. A pipe, a redirect to a file, or a service
manager's captured stdout all fall back to the ordinary stderr log, unchanged —
so `systemd`, `nohup … >> ~/.slack/ack.log`, and log greps keep working exactly
as before. One-shot commands (`paths`, `--list-channels`, `--test`, `--click`,
`get-user-token`, `import-session`) always stay plain.

The look is modeled on the companion `sts` tool's live views (rounded cyan
panels, status glyphs, keybinding footer).

## Install (teammates)

Needs git access to `pankajyadav-blitz/slack-alert-ack` (SSH key on GitHub). Linux,
macOS and WSL, x86_64 or arm64; one static binary, no Rust needed.

    rm -rf /tmp/ack-bot-install && git clone -q --depth 1 -b release git@github.com:pankajyadav-blitz/slack-alert-ack.git /tmp/ack-bot-install && sh /tmp/ack-bot-install/install.sh; rm -rf /tmp/ack-bot-install

That puts `ack-bot` in `~/.local/bin` (`ACK_BOT_INSTALL_DIR` to change) and, if
you have none, a starter `~/.slack/config.yaml`. Then:

    ack-bot get-user-token          # browser OAuth -> writes SLACK_USER_TOKEN to ~/.slack/env
    ack-bot import-session capture.txt --write   # browser session, for pressing buttons
    ack-bot paths                   # check what is set
    ack-bot --dry-run -v            # watch without acking
    ack-bot                         # run it

    ack-bot update                  # later: update to the newest release
    ack-bot uninstall               # remove the binary (keeps ~/.slack)

## Releasing

Bump `version` in `Cargo.toml`, commit, then tag and push:

    git tag v1.1.0 && git push origin main v1.1.0

The `release` workflow builds the four targets, attaches them to a GitHub
release and force-pushes them with `install.sh`, `config.example.yaml` and
`VERSION` to the `release` branch, which `install.sh` and `ack-bot update` read.

## Build and install

    cargo install --path .        # → ~/.cargo/bin/ack-bot (already on PATH)
    cargo test                    # 35 unit tests, no network

Run it as a service if it should survive logout:

    systemd --user unit, ExecStart=%h/.cargo/bin/ack-bot
    # or: nohup ack-bot >> ~/.slack/ack.log 2>&1 &

## Differences from the Python

Behaviour is a port, not a redesign: same config keys and defaults, same SQLite
schema and file, same log format (`%(asctime)s %(levelname)-7s %(message)s` on
stderr), same strategy order, same refusals, same `blocks.actions` payload. What
changed:

* **Credentials moved** to `~/.slack/env` from `.env` beside the script, and config
  to `~/.slack/config.yaml`, because a binary on `PATH` has no script directory.
  `./.env` and `./config.yaml` are still honoured when present.
* **No `run.sh`, no `deps/`, no `PYTHONPATH`** — one binary, and it loads the
  credentials file itself.
* `get_user_token.py` and `import_session.py` became the `get-user-token` and
  `import-session` subcommands; `REAUTH_HINT` points at the former.
* **`ack-bot paths`** is new: prints where config, credentials and state resolved
  to, and which credentials are present.
* **A live terminal dashboard** is new: when stdout is a terminal the running bot
  draws a full-screen view (see [The live dashboard](#the-live-dashboard)) instead
  of the scrolling log. The log itself is unchanged in piped / service use, so
  this is purely additive and off by default anywhere stdout is not a TTY.
* Internals only: `os.environ` became a process-local store (mutating the real
  environment from threads is unsound in Rust), Socket Mode is a hand-rolled
  `apps.connections.open` + websocket loop with reconnect, and slack_sdk's retry
  handlers became the retry loop in `src/webapi.rs` (3 tries on rate limits, 3 on
  connection/read timeouts, backoff with jitter — every call it makes is a read).

Only one of the two implementations should run at a time: each refresh invalidates
the refresh token the other holds on disk.

## Source map

| file | was |
| --- | --- |
| `src/main.rs` | `ack_bot.py` `main()`, argparse |
| `src/bot.rs` | `class AckBot` — resolve, poll, on_event, the three strategies |
| `src/buttons.rs` | `extract_buttons`, `message_text`, `message_urls`, `_clip` |
| `src/config.rs` | `class Config` |
| `src/state.rs` | `class State` (same schema) |
| `src/webapi.rs` | `make_web_client` + slack_sdk retry handlers |
| `src/click.rs` | `slack_click.py` |
| `src/socket.rs` | `SocketModeClient` |
| `src/tokens.rs` | `slack_tokens.py` |
| `src/oauth.rs` | `get_user_token.py` |
| `src/importsession.rs` | `import_session.py` |
| `src/manual.rs` | `--list-channels`, `--click`, `--test`, `describe` |
| `src/tui.rs` | the live terminal dashboard (new; modeled on the `sts` tool) |
| `src/activity.rs` | the thread-safe store the dashboard draws from (new) |
