//! The data the live dashboard draws.
//!
//! The bot runs its poll / socket loop on one thread and the TUI redraws on
//! another, so everything the screen shows lives behind a single `Mutex` and is
//! shared as an `Arc`. The bot only ever *appends* outcomes (one per handled
//! alert) and the TUI only ever *reads* a cheap snapshot — there is no
//! back-pressure and no blocking call held across the lock.
//!
//! This exists only for the TUI. In plain (piped / service) mode nothing records
//! into it and the stderr log is still the whole story, exactly as before.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// How many recent alerts the activity feed keeps. Older ones scroll off; the
/// counters below still carry the full totals.
const MAX_EVENTS: usize = 200;

/// What happened to one alert, which decides its glyph and colour on screen.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A real ack went through (url_click / click / provider / dry-run).
    Acked,
    /// Nothing could ack it — left visibly waiting for a human.
    NotAcked,
    /// An error while trying (expired session, 5xx) — will retry next event.
    Failed,
}

/// One line in the activity feed: a single alert and what became of it.
#[derive(Clone)]
pub struct Event {
    pub outcome: Outcome,
    /// Channel label (e.g. `#alerts`) or the raw id if it never resolved.
    pub channel: String,
    /// Slack message ts, shown so a line maps back to a specific message.
    pub ts: String,
    /// The ack method on success (e.g. `url_click:host:200`) or the reason it
    /// did not / could not ack.
    pub detail: String,
    /// Seconds since the store started, so the feed can show a relative age that
    /// ticks up without storing a wall clock per row.
    pub at_secs: u64,
}

struct Inner {
    started: Instant,
    /// What the title bar says the bot is doing: `"user"` or `"bot"`.
    mode: &'static str,
    dry_run: bool,
    /// cid/label pairs being watched, filled in once `resolve()` has run.
    channels: Vec<String>,
    events: VecDeque<Event>,
    acked: u64,
    not_acked: u64,
    failed: u64,
}

/// A cheap, owned copy of the store for one redraw. Collected under the lock and
/// then rendered without holding it.
pub struct Snapshot {
    pub uptime_secs: u64,
    pub mode: &'static str,
    pub dry_run: bool,
    pub channels: Vec<String>,
    pub events: Vec<Event>,
    pub acked: u64,
    pub not_acked: u64,
    pub failed: u64,
}

/// The shared activity store. Clone is cheap (an `Arc`); every clone points at
/// the same data.
#[derive(Clone)]
pub struct Activity(Arc<Mutex<Inner>>);

impl Activity {
    pub fn new(mode: &'static str, dry_run: bool) -> Self {
        Activity(Arc::new(Mutex::new(Inner {
            started: Instant::now(),
            mode,
            dry_run,
            channels: Vec::new(),
            events: VecDeque::new(),
            acked: 0,
            not_acked: 0,
            failed: 0,
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record the channels the bot settled on, shown in the Watching panel.
    pub fn set_channels(&self, channels: Vec<String>) {
        self.lock().channels = channels;
    }

    /// Append one handled alert and bump its counter. `detail` is the ack method
    /// on success or the reason otherwise.
    pub fn record(&self, outcome: Outcome, channel: &str, ts: &str, detail: &str) {
        let mut inner = self.lock();
        let at_secs = inner.started.elapsed().as_secs();
        match outcome {
            Outcome::Acked => inner.acked += 1,
            Outcome::NotAcked => inner.not_acked += 1,
            Outcome::Failed => inner.failed += 1,
        }
        inner.events.push_front(Event {
            outcome,
            channel: channel.to_string(),
            ts: ts.to_string(),
            detail: detail.to_string(),
            at_secs,
        });
        while inner.events.len() > MAX_EVENTS {
            inner.events.pop_back();
        }
    }

    /// A point-in-time copy for the renderer. `newest-first` events, bounded.
    pub fn snapshot(&self) -> Snapshot {
        let inner = self.lock();
        Snapshot {
            uptime_secs: inner.started.elapsed().as_secs(),
            mode: inner.mode,
            dry_run: inner.dry_run,
            channels: inner.channels.clone(),
            events: inner.events.iter().cloned().collect(),
            acked: inner.acked,
            not_acked: inner.not_acked,
            failed: inner.failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_newest_first_and_counts() {
        let act = Activity::new("user", false);
        act.record(Outcome::Acked, "#alerts", "1.1", "url_click:host:200");
        act.record(Outcome::NotAcked, "#alerts", "2.2", "no ack path");
        act.record(Outcome::Failed, "#ops", "3.3", "session expired");

        let snap = act.snapshot();
        assert_eq!(snap.acked, 1);
        assert_eq!(snap.not_acked, 1);
        assert_eq!(snap.failed, 1);
        // newest first
        assert_eq!(snap.events[0].ts, "3.3");
        assert_eq!(snap.events[2].ts, "1.1");
    }

    #[test]
    fn feed_is_bounded() {
        let act = Activity::new("bot", true);
        for i in 0..(MAX_EVENTS + 50) {
            act.record(Outcome::Acked, "#c", &i.to_string(), "ok");
        }
        let snap = act.snapshot();
        assert_eq!(snap.events.len(), MAX_EVENTS);
        // the counter still saw them all
        assert_eq!(snap.acked as usize, MAX_EVENTS + 50);
        assert!(snap.dry_run);
    }
}
