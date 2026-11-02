//! Small pieces the rest of the port leans on: a stop flag with interruptible
//! waits (Python's `threading.Event`), wall-clock helpers, and `sys.exit(msg)`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds since the epoch as a float, i.e. Python's `time.time()`.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A Slack ts-shaped timestamp, matching Python's `f"{time.time():.6f}"`.
pub fn now_ts() -> String {
    format!("{:.6}", now())
}

/// Today's 00:00 in local time, as epoch seconds. Nothing older than this is
/// read: starting the bot means "handle today's alerts", not "catch up".
pub fn local_midnight() -> f64 {
    use chrono::{Local, TimeZone};
    let midnight = Local::now().date_naive().and_hms_opt(0, 0, 0).unwrap();
    // A DST jump at midnight can make 00:00 ambiguous or skipped; take the
    // earliest instant that exists, or fall back to now.
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|t| t.timestamp() as f64)
        .unwrap_or_else(now)
}

/// Slack timestamps are decimal strings; an unparsable one sorts oldest rather
/// than taking the process down.
pub fn ts_f64(ts: &str) -> f64 {
    ts.parse::<f64>().unwrap_or(0.0)
}

/// `sys.exit("message")`: one line on stderr, exit status 1.
pub fn die(message: impl AsRef<str>) -> ! {
    eprintln!("{}", message.as_ref());
    std::process::exit(1)
}

pub fn sleep(seconds: f64) {
    if seconds > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(seconds));
    }
}

/// `threading.Event`, near enough: a flag plus waits that a signal can cut short.
pub struct Stop {
    flag: AtomicBool,
    lock: Mutex<bool>,
    cv: Condvar,
}

impl Stop {
    pub fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            lock: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    pub fn set(&self) {
        let mut set = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        *set = true;
        self.flag.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }

    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Wait up to `seconds`, returning early (and true) once stopped.
    pub fn wait(&self, seconds: f64) -> bool {
        let guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if *guard {
            return true;
        }
        let (guard, _) = self
            .cv
            .wait_timeout(guard, Duration::from_secs_f64(seconds.max(0.0)))
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }

    /// Block until stopped -- what `stop.wait()` with no timeout does in Python.
    pub fn wait_forever(&self) {
        let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        while !*guard {
            guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Default for Stop {
    fn default() -> Self {
        Self::new()
    }
}
