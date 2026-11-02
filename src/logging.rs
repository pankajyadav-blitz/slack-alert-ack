//! The logging Python's `logging.basicConfig` gave us, and nothing more.
//!
//! Same format string -- `%(asctime)s %(levelname)-7s %(message)s` -- and the
//! same destination (stderr), so an operator watching the log after the port
//! sees lines that look exactly like the ones before it. `print()` output in the
//! Python stayed on stdout and still does here.

use std::sync::atomic::{AtomicU8, Ordering};

pub const DEBUG: u8 = 10;
pub const INFO: u8 = 20;
pub const WARNING: u8 = 30;
pub const ERROR: u8 = 40;
/// Above every real level: nothing emits. Used while the full-screen dashboard
/// owns the terminal, where a stray stderr line would scribble over the UI.
pub const SILENT: u8 = 255;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);

pub fn set_level(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

/// Mute all logging — the dashboard is drawing to this terminal.
pub fn silence() {
    LEVEL.store(SILENT, Ordering::Relaxed);
}

pub fn enabled(level: u8) -> bool {
    level >= LEVEL.load(Ordering::Relaxed)
}

pub fn emit(level: u8, message: &str) {
    let name = match level {
        DEBUG => "DEBUG",
        INFO => "INFO",
        WARNING => "WARNING",
        ERROR => "ERROR",
        _ => "CRITICAL",
    };
    // Python renders milliseconds after a comma; keep that so log greps still work.
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S,%3f");
    eprintln!("{now} {name:<7} {message}");
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::logging::enabled($crate::logging::DEBUG) {
            $crate::logging::emit($crate::logging::DEBUG, &format!($($arg)*))
        }
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        if $crate::logging::enabled($crate::logging::INFO) {
            $crate::logging::emit($crate::logging::INFO, &format!($($arg)*))
        }
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        if $crate::logging::enabled($crate::logging::WARNING) {
            $crate::logging::emit($crate::logging::WARNING, &format!($($arg)*))
        }
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        if $crate::logging::enabled($crate::logging::ERROR) {
            $crate::logging::emit($crate::logging::ERROR, &format!($($arg)*))
        }
    };
}
