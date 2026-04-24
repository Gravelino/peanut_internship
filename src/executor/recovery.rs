//! Circuit breaker + replay protection.
//!
//! Both types accept an explicit `Instant` in their `*_at` APIs to keep tests
//! deterministic without needing a mock clock.
//!
//! # Replay persistence
//! [`ReplayProtection`] defaults to an in-memory `HashMap` (lost on restart).
//! Call [`ReplayProtection::with_journal`] to back it with a SQLite journal
//! so the TTL window survives process restarts. The journal stores
//! `(signal_id, expires_at_unix_seconds)` and is cleaned on open + on each
//! cleanup pass.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::executor::errors::{ExecutorError, ExecutorResult};
use crate::strategy::signal::Signal;

/// Tunables for the [`CircuitBreaker`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Number of failures within `window` that trip the breaker.
    pub failure_threshold: usize,
    /// Rolling window used to count failures.
    pub window: Duration,
    /// Duration the breaker stays open after tripping.
    pub cooldown: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            window: Duration::from_secs(300),
            cooldown: Duration::from_secs(600),
        }
    }
}

/// Short-circuits execution after too many recent failures.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    failures: VecDeque<Instant>,
    tripped_at: Option<Instant>,
}

impl CircuitBreaker {
    /// Creates a circuit breaker with the given config.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            failures: VecDeque::new(),
            tripped_at: None,
        }
    }

    /// Records a failure at time `now`; trips the breaker if threshold is reached.
    pub fn record_failure_at(&mut self, now: Instant) {
        self.failures.push_back(now);
        let cutoff = now.checked_sub(self.config.window);
        if let Some(cutoff) = cutoff {
            while let Some(front) = self.failures.front() {
                if *front <= cutoff {
                    self.failures.pop_front();
                } else {
                    break;
                }
            }
        }
        if self.failures.len() >= self.config.failure_threshold && self.tripped_at.is_none() {
            self.tripped_at = Some(now);
            warn!(
                failures = self.failures.len(),
                cooldown_s = self.config.cooldown.as_secs(),
                "CIRCUIT BREAKER TRIPPED"
            );
        }
    }

    /// Convenience wrapper using `Instant::now`.
    pub fn record_failure(&mut self) {
        self.record_failure_at(Instant::now());
    }

    /// No-op today; reserved for a future reset-on-success policy.
    pub fn record_success(&mut self) {}

    /// Number of failures currently counted in the window.
    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }

    /// Returns `true` if the breaker is open at time `now`. Auto-resets once
    /// the cooldown has elapsed.
    pub fn is_open_at(&mut self, now: Instant) -> bool {
        match self.tripped_at {
            None => false,
            Some(t) if now.duration_since(t) > self.config.cooldown => {
                self.tripped_at = None;
                self.failures.clear();
                false
            }
            Some(_) => true,
        }
    }

    /// Convenience wrapper using `Instant::now`.
    pub fn is_open(&mut self) -> bool {
        self.is_open_at(Instant::now())
    }

    /// Remaining cooldown duration, or zero if closed.
    pub fn time_until_reset(&self) -> Duration {
        match self.tripped_at {
            None => Duration::ZERO,
            Some(t) => self
                .config
                .cooldown
                .checked_sub(t.elapsed())
                .unwrap_or(Duration::ZERO),
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(CircuitBreakerConfig::default())
    }
}

/// Optional SQLite journal for [`ReplayProtection`].
///
/// Wrapped in `Arc<Mutex<>>` so the parent `ReplayProtection` stays `Clone`
/// (existing callers clone it for cross-task sharing). The mutex is only held
/// during the quick SQL upsert / prune — never across an await.
#[derive(Debug, Clone)]
struct ReplayJournal {
    /// Path of the SQLite database; retained for diagnostics / Debug output
    /// even though it's not read at runtime.
    #[allow(dead_code)]
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl ReplayJournal {
    fn open(path: &Path, ttl: Duration) -> ExecutorResult<Self> {
        let conn = Connection::open(path)
            .map_err(|e| ExecutorError::Persistence(format!("open {}: {e}", path.display())))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS replay_seen (
                signal_id  TEXT PRIMARY KEY,
                expires_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_replay_expires
                ON replay_seen(expires_at);",
        )
        .map_err(|e| ExecutorError::Persistence(format!("schema: {e}")))?;

        let journal = Self {
            path: path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
        };
        // Prune on open so stale entries from a crashed run don't linger.
        if let Err(e) = journal.prune_expired_at(now_unix()) {
            warn!(path = %path.display(), error = %e, "replay journal: prune on open failed");
        }
        debug!(path = %path.display(), ttl_s = ttl.as_secs(), "replay journal opened");
        Ok(journal)
    }

    fn upsert(&self, signal_id: &str, expires_at_unix: i64) -> ExecutorResult<()> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| ExecutorError::Persistence(format!("lock poisoned: {e}")))?;
        guard
            .execute(
                "INSERT INTO replay_seen(signal_id, expires_at) VALUES(?1, ?2)
                 ON CONFLICT(signal_id) DO UPDATE SET expires_at = excluded.expires_at",
                params![signal_id, expires_at_unix],
            )
            .map_err(|e| ExecutorError::Persistence(format!("upsert: {e}")))?;
        Ok(())
    }

    #[cfg(test)]
    fn contains(&self, signal_id: &str, now_unix: i64) -> ExecutorResult<bool> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| ExecutorError::Persistence(format!("lock poisoned: {e}")))?;
        let count: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM replay_seen WHERE signal_id = ?1 AND expires_at > ?2",
                params![signal_id, now_unix],
                |row| row.get(0),
            )
            .map_err(|e| ExecutorError::Persistence(format!("query: {e}")))?;
        Ok(count > 0)
    }

    fn prune_expired_at(&self, now_unix: i64) -> ExecutorResult<usize> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| ExecutorError::Persistence(format!("lock poisoned: {e}")))?;
        let n = guard
            .execute(
                "DELETE FROM replay_seen WHERE expires_at <= ?1",
                params![now_unix],
            )
            .map_err(|e| ExecutorError::Persistence(format!("prune: {e}")))?;
        Ok(n)
    }
}

/// Prevents the same signal ID from executing twice within a TTL window.
///
/// Defaults to in-memory only. Use [`ReplayProtection::with_journal`] to
/// persist seen IDs to SQLite so the TTL window survives restarts.
#[derive(Debug, Clone)]
pub struct ReplayProtection {
    executed: HashMap<String, Instant>,
    ttl: Duration,
    journal: Option<ReplayJournal>,
}

impl ReplayProtection {
    /// Creates a new in-memory replay guard with the given entry TTL.
    pub fn new(ttl: Duration) -> Self {
        Self {
            executed: HashMap::new(),
            ttl,
            journal: None,
        }
    }

    /// Creates a replay guard backed by a SQLite journal at `path`. Existing
    /// non-expired entries are loaded into memory so restarts don't release
    /// recently-seen signal IDs.
    pub fn with_journal<P: AsRef<Path>>(path: P, ttl: Duration) -> ExecutorResult<Self> {
        let journal = ReplayJournal::open(path.as_ref(), ttl)?;

        // Warm the in-memory cache from the journal for fast `is_duplicate`
        // lookups. Stored `expires_at` is absolute unix seconds; we translate
        // back into `Instant` relative to *now* so the existing in-memory
        // cleanup logic keeps working unchanged.
        let mut executed = HashMap::new();
        let now_u = now_unix();
        let now_i = Instant::now();
        {
            let guard = journal
                .conn
                .lock()
                .map_err(|e| ExecutorError::Persistence(format!("lock poisoned: {e}")))?;
            let mut stmt = guard
                .prepare("SELECT signal_id, expires_at FROM replay_seen WHERE expires_at > ?1")
                .map_err(|e| ExecutorError::Persistence(format!("prepare: {e}")))?;
            let rows = stmt
                .query_map(params![now_u], |row| {
                    let id: String = row.get(0)?;
                    let exp: i64 = row.get(1)?;
                    Ok((id, exp))
                })
                .map_err(|e| ExecutorError::Persistence(format!("query: {e}")))?;
            for row in rows {
                let (id, exp) = row.map_err(|e| ExecutorError::Persistence(format!("row: {e}")))?;
                // Reconstruct an Instant corresponding to the journal's
                // expiry. `seen_at = expires_at - ttl`, then `seen_at_instant
                // = now_instant - (now_unix - seen_at)`.
                let seen_at_unix = exp - ttl.as_secs() as i64;
                let age_secs = (now_u - seen_at_unix).max(0) as u64;
                let seen_at_instant = now_i
                    .checked_sub(Duration::from_secs(age_secs))
                    .unwrap_or(now_i);
                executed.insert(id, seen_at_instant);
            }
        }

        Ok(Self {
            executed,
            ttl,
            journal: Some(journal),
        })
    }

    /// Returns `true` if `signal.signal_id` was recorded in the last `ttl`.
    pub fn is_duplicate(&mut self, signal: &Signal) -> bool {
        self.cleanup(Instant::now());
        self.executed.contains_key(&signal.signal_id)
    }

    /// Marks `signal` as executed so subsequent calls flag it as duplicate.
    pub fn mark_executed(&mut self, signal: &Signal) {
        let now_i = Instant::now();
        self.executed.insert(signal.signal_id.clone(), now_i);

        if let Some(ref journal) = self.journal {
            let expires_at = now_unix().saturating_add(self.ttl.as_secs() as i64);
            if let Err(e) = journal.upsert(&signal.signal_id, expires_at) {
                // Journal write failure should NOT release the in-memory
                // block. Log loudly so ops can notice drift; the in-memory
                // state still protects this process.
                warn!(
                    signal_id = %signal.signal_id,
                    error = %e,
                    "replay journal: upsert failed; in-memory state still enforced"
                );
            }
        }
    }

    fn cleanup(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.executed.retain(|_, t| now.duration_since(*t) <= ttl);
        if let Some(ref journal) = self.journal
            && let Err(e) = journal.prune_expired_at(now_unix())
        {
            warn!(error = %e, "replay journal: prune failed");
        }
    }
}

/// Current unix timestamp in seconds. Panics only if the system clock is
/// before the epoch, which we treat as unrecoverable.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl Default for ReplayProtection {
    /// 60-second TTL matches the Python spec default.
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::signal::{Direction, SignalParams};

    fn mk_signal(id_suffix: &str) -> Signal {
        let mut s = Signal::new(SignalParams {
            pair: "ETH/USDT".into(),
            direction: Direction::BuyCexSellDex,
            cex_price: rust_decimal::Decimal::from(2000),
            dex_price: rust_decimal::Decimal::from(2020),
            spread_bps: rust_decimal::Decimal::from(100),
            size: rust_decimal::Decimal::ONE,
            expected_gross_pnl: rust_decimal::Decimal::from(20),
            expected_fees: rust_decimal::Decimal::from(5),
            expected_net_pnl: rust_decimal::Decimal::from(15),
            ttl: chrono::Duration::seconds(5),
            inventory_ok: true,
            within_limits: true,
        });
        s.signal_id = format!("test_{id_suffix}");
        s
    }

    #[test]
    fn breaker_trips_after_threshold() {
        let mut cb = CircuitBreaker::default();
        let t0 = Instant::now();
        assert!(!cb.is_open_at(t0));
        cb.record_failure_at(t0);
        cb.record_failure_at(t0);
        assert!(!cb.is_open_at(t0));
        cb.record_failure_at(t0);
        assert!(cb.is_open_at(t0));
    }

    #[test]
    fn breaker_ignores_failures_outside_window() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 3,
            window: Duration::from_secs(10),
            cooldown: Duration::from_secs(60),
        });
        let t0 = Instant::now();
        cb.record_failure_at(t0);
        cb.record_failure_at(t0 + Duration::from_secs(20));
        cb.record_failure_at(t0 + Duration::from_secs(21));
        // First failure is out of window -> only 2 fresh -> still closed.
        assert!(!cb.is_open_at(t0 + Duration::from_secs(22)));
        assert_eq!(cb.failure_count(), 2);
    }

    #[test]
    fn breaker_resets_after_cooldown() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 2,
            window: Duration::from_secs(60),
            cooldown: Duration::from_secs(5),
        });
        let t0 = Instant::now();
        cb.record_failure_at(t0);
        cb.record_failure_at(t0);
        assert!(cb.is_open_at(t0));
        assert!(cb.is_open_at(t0 + Duration::from_secs(4)));
        assert!(!cb.is_open_at(t0 + Duration::from_secs(6)));
    }

    #[test]
    fn replay_blocks_duplicate() {
        let mut rp = ReplayProtection::default();
        let s = mk_signal("a");
        assert!(!rp.is_duplicate(&s));
        rp.mark_executed(&s);
        assert!(rp.is_duplicate(&s));
    }

    #[test]
    fn replay_allows_new_id() {
        let mut rp = ReplayProtection::default();
        let a = mk_signal("a");
        let b = mk_signal("b");
        rp.mark_executed(&a);
        assert!(!rp.is_duplicate(&b));
    }

    // ---- S8: SQLite-backed persistent journal -------------------------

    #[test]
    fn journal_persists_seen_ids_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replay.db");
        let ttl = Duration::from_secs(60);

        {
            let mut rp = ReplayProtection::with_journal(&path, ttl).unwrap();
            rp.mark_executed(&mk_signal("persistent"));
            assert!(rp.is_duplicate(&mk_signal("persistent")));
        }
        // Simulate process restart: open a fresh ReplayProtection against
        // the same DB. The previously-marked signal must still be blocked.
        let mut rp2 = ReplayProtection::with_journal(&path, ttl).unwrap();
        assert!(rp2.is_duplicate(&mk_signal("persistent")));
        assert!(!rp2.is_duplicate(&mk_signal("fresh")));
    }

    #[test]
    fn journal_prunes_expired_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replay.db");

        // Write an entry that expires in the past.
        {
            let rp = ReplayProtection::with_journal(&path, Duration::from_secs(60)).unwrap();
            let journal = rp.journal.clone().unwrap();
            journal.upsert("stale", now_unix() - 10).unwrap();
            assert!(journal.contains("stale", 0).unwrap());
        }

        // Reopen — prune on open should have removed the stale row.
        let mut rp2 = ReplayProtection::with_journal(&path, Duration::from_secs(60)).unwrap();
        let mut sig = mk_signal("stale");
        sig.signal_id = "stale".into();
        assert!(!rp2.is_duplicate(&sig));
    }

    #[test]
    fn journal_upsert_overwrites_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replay.db");
        let mut rp = ReplayProtection::with_journal(&path, Duration::from_secs(60)).unwrap();

        let sig = mk_signal("once");
        rp.mark_executed(&sig);
        // Re-marking the same ID must not error (upsert, not insert).
        rp.mark_executed(&sig);
        assert!(rp.is_duplicate(&sig));
    }

    #[test]
    fn in_memory_default_has_no_journal() {
        let rp = ReplayProtection::default();
        assert!(rp.journal.is_none());
    }
}
