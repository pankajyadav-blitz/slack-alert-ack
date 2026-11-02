//! What `os.environ` was doing for the Python: one process-wide bag of values,
//! seeded from the real environment and then written to when a token is renewed.
//!
//! It is a store of our own rather than `std::env::set_var` because mutating the
//! real environment from more than one thread is unsound in Rust (and unsafe from
//! the 2024 edition on). Every read in this program goes through here, so the
//! behaviour is the same: the shell's exported value wins, a renewal overrides it.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

static STORE: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn store() -> &'static RwLock<HashMap<String, String>> {
    STORE.get_or_init(|| RwLock::new(std::env::vars().collect()))
}

/// `os.environ.get(key)` -- present but empty is still present.
pub fn get(key: &str) -> Option<String> {
    store().read().ok()?.get(key).cloned()
}

/// The same, but empty counts as missing -- which is what every `if not token`
/// in the Python actually meant.
pub fn get_ne(key: &str) -> Option<String> {
    get(key).filter(|v| !v.is_empty())
}

pub fn set(key: &str, value: &str) {
    if let Ok(mut map) = store().write() {
        map.insert(key.to_string(), value.to_string());
    }
}

pub fn contains(key: &str) -> bool {
    store().read().map(|m| m.contains_key(key)).unwrap_or(false)
}

/// Snapshot, for the places that need to look at several keys at once.
pub fn snapshot() -> HashMap<String, String> {
    store().read().map(|m| m.clone()).unwrap_or_default()
}
