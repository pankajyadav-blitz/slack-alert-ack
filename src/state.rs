//! Dedupe state -- survives restarts so an edited/replayed message isn't re-acked.
//! Same SQLite file and same schema as the Python, so an existing
//! ack_state.sqlite3 can be carried straight over.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, ErrorCode, params};

use crate::log_debug;
use crate::util::die;

pub struct State {
    db: Mutex<Connection>,
}

impl State {
    pub fn open(path: &Path, retention_days: i64) -> State {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    die(format!("cannot create {}: {e}", parent.display()));
                }
            }
        }
        let db = match Connection::open(path) {
            Ok(db) => db,
            Err(e) => die(format!(
                "cannot open the state database {}: {e}",
                path.display()
            )),
        };
        let setup = || -> rusqlite::Result<()> {
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS acked (
                   channel TEXT NOT NULL,
                   ts      TEXT NOT NULL,
                   method  TEXT,
                   at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                   PRIMARY KEY (channel, ts));
                 -- user mode: how far each channel has been read, so a restart neither
                 -- rescans old alerts nor skips ones posted while we were down.
                 CREATE TABLE IF NOT EXISTS cursors (
                   channel TEXT PRIMARY KEY,
                   last_ts TEXT NOT NULL);",
            )?;
            // Slack only replays recent events, so rows older than the retention
            // window can never match again -- prune so the file stays small.
            if retention_days > 0 {
                let pruned = db.execute(
                    "DELETE FROM acked WHERE at < datetime('now', ?1)",
                    params![format!("-{retention_days} days")],
                )?;
                if pruned > 0 {
                    log_debug!("pruned {pruned} ack record(s) older than {retention_days}d");
                }
            }
            Ok(())
        };
        if let Err(e) = setup() {
            die(format!(
                "cannot prepare the state database {}: {e}",
                path.display()
            ));
        }
        State { db: Mutex::new(db) }
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mark (channel, ts) as in-flight. False if already claimed.
    pub fn claim(&self, channel: &str, ts: &str) -> bool {
        let db = self.conn();
        match db.execute(
            "INSERT INTO acked (channel, ts) VALUES (?1, ?2)",
            params![channel, ts],
        ) {
            Ok(_) => true,
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == ErrorCode::ConstraintViolation =>
            {
                false
            }
            Err(e) => {
                crate::log_error!("state: claim {channel}/{ts} failed: {e}");
                false
            }
        }
    }

    pub fn record(&self, channel: &str, ts: &str, method: &str) {
        let db = self.conn();
        if let Err(e) = db.execute(
            "UPDATE acked SET method = ?1 WHERE channel = ?2 AND ts = ?3",
            params![method, channel, ts],
        ) {
            crate::log_error!("state: record {channel}/{ts} failed: {e}");
        }
    }

    /// Drop rows whose only "ack" was the old reaction/thread-reply fallback.
    ///
    /// Those alerts were never really acknowledged, so they must not stay claimed
    /// -- otherwise the dedupe table permanently hides the very messages that
    /// still need a click.
    pub fn forget_fallbacks(&self) -> usize {
        let db = self.conn();
        match db.execute("DELETE FROM acked WHERE method LIKE 'fallback%'", []) {
            Ok(n) => n,
            Err(e) => {
                crate::log_error!("state: pruning fallback rows failed: {e}");
                0
            }
        }
    }

    /// Un-claim so a later retry can try again (used when ack errored).
    pub fn release(&self, channel: &str, ts: &str) {
        let db = self.conn();
        if let Err(e) = db.execute(
            "DELETE FROM acked WHERE channel = ?1 AND ts = ?2",
            params![channel, ts],
        ) {
            crate::log_error!("state: release {channel}/{ts} failed: {e}");
        }
    }

    pub fn get_cursor(&self, channel: &str) -> Option<String> {
        let db = self.conn();
        db.query_row(
            "SELECT last_ts FROM cursors WHERE channel = ?1",
            params![channel],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    pub fn set_cursor(&self, channel: &str, ts: &str) {
        let db = self.conn();
        // Never move a cursor backwards: a late-arriving older message must not
        // rewind us into re-reading everything after it.
        if let Err(e) = db.execute(
            "INSERT INTO cursors (channel, last_ts) VALUES (?1, ?2) \
             ON CONFLICT(channel) DO UPDATE SET last_ts = ?3 \
             WHERE CAST(last_ts AS REAL) < CAST(?4 AS REAL)",
            params![channel, ts, ts, ts],
        ) {
            crate::log_error!("state: set_cursor {channel} failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ack-state-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("ack_state.sqlite3")
    }

    #[test]
    fn claim_is_once_only_and_release_undoes_it() {
        let path = temp_db("claim");
        std::fs::remove_file(&path).ok();
        let state = State::open(&path, 30);
        assert!(state.claim("C1", "1.0"));
        assert!(!state.claim("C1", "1.0"));
        state.release("C1", "1.0");
        assert!(state.claim("C1", "1.0"));
    }

    #[test]
    fn fallback_rows_are_forgotten_but_real_acks_are_kept() {
        let path = temp_db("fallback");
        std::fs::remove_file(&path).ok();
        let state = State::open(&path, 0);
        state.claim("C1", "1.0");
        state.record("C1", "1.0", "fallback:reaction");
        state.claim("C1", "2.0");
        state.record("C1", "2.0", "click:ack:INC-1");
        assert_eq!(state.forget_fallbacks(), 1);
        assert!(state.claim("C1", "1.0"));
        assert!(!state.claim("C1", "2.0"));
    }

    #[test]
    fn cursor_never_moves_backwards() {
        let path = temp_db("cursor");
        std::fs::remove_file(&path).ok();
        let state = State::open(&path, 0);
        assert_eq!(state.get_cursor("C1"), None);
        state.set_cursor("C1", "100.5");
        state.set_cursor("C1", "99.0");
        assert_eq!(state.get_cursor("C1").as_deref(), Some("100.5"));
        state.set_cursor("C1", "101.0");
        assert_eq!(state.get_cursor("C1").as_deref(), Some("101.0"));
    }
}
