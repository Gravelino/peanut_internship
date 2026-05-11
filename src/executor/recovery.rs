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

use crate::core::types::{
    DEFAULT_BREAKER_COOLDOWN_SECS, DEFAULT_BREAKER_FAILURE_THRESHOLD, DEFAULT_BREAKER_WINDOW_SECS,
    DEFAULT_MAX_DAILY_LOSS_USD, DEFAULT_REPLAY_TTL_SECS,
};
use crate::executor::errors::{ExecutorError, ExecutorResult};
use crate::executor::migrations::{SqlMigration, migrate_executor_db};
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
            failure_threshold: DEFAULT_BREAKER_FAILURE_THRESHOLD,
            window: Duration::from_secs(DEFAULT_BREAKER_WINDOW_SECS),
            cooldown: Duration::from_secs(DEFAULT_BREAKER_COOLDOWN_SECS),
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

// ---------------------------------------------------------------------------
// PnL-based breaker
// ---------------------------------------------------------------------------

/// Tunables for the [`PnlBreaker`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlBreakerConfig {
    /// Maximum cumulative loss (absolute, positive number) before the breaker
    /// trips. For example, `100` means the breaker fires when daily realised
    /// PnL drops below `-$100`.
    pub max_daily_loss_usd: rust_decimal::Decimal,
}

impl Default for PnlBreakerConfig {
    fn default() -> Self {
        Self {
            max_daily_loss_usd: rust_decimal::Decimal::from(DEFAULT_MAX_DAILY_LOSS_USD),
        }
    }
}

/// Halts execution when cumulative realised PnL for the current UTC day
/// exceeds the configured loss threshold.
///
/// Unlike [`CircuitBreaker`] (which counts execution *failures*), this
/// breaker fires on **economic drawdown** — the bot may be executing
/// successfully but still bleeding money.
///
/// Resets automatically when the UTC date rolls over. All times use
/// [`chrono::Utc`] so the reset is deterministic regardless of host TZ.
#[derive(Debug, Clone)]
pub struct PnlBreaker {
    config: PnlBreakerConfig,
    cumulative_pnl: rust_decimal::Decimal,
    /// UTC date of the current accumulation window.
    day: chrono::NaiveDate,
    halted: bool,
}

impl PnlBreaker {
    /// Creates a PnL breaker with the given config.
    pub fn new(config: PnlBreakerConfig) -> Self {
        Self {
            config,
            cumulative_pnl: rust_decimal::Decimal::ZERO,
            day: chrono::Utc::now().date_naive(),
            halted: false,
        }
    }

    /// Records a realised PnL observation. Trips the breaker when the
    /// cumulative daily PnL drops below `-max_daily_loss_usd`.
    pub fn record_pnl(&mut self, net_pnl: rust_decimal::Decimal) {
        self.maybe_reset_day();
        self.cumulative_pnl += net_pnl;
        if !self.halted && self.cumulative_pnl < -self.config.max_daily_loss_usd {
            self.halted = true;
            warn!(
                cumulative_pnl = %self.cumulative_pnl,
                max_daily_loss = %self.config.max_daily_loss_usd,
                "PNL BREAKER HALTED — daily loss threshold exceeded"
            );
        }
    }

    /// Deterministic variant for testing: caller supplies `today`.
    pub fn record_pnl_at(&mut self, net_pnl: rust_decimal::Decimal, today: chrono::NaiveDate) {
        self.maybe_reset_day_at(today);
        self.cumulative_pnl += net_pnl;
        if !self.halted && self.cumulative_pnl < -self.config.max_daily_loss_usd {
            self.halted = true;
            warn!(
                cumulative_pnl = %self.cumulative_pnl,
                max_daily_loss = %self.config.max_daily_loss_usd,
                "PNL BREAKER HALTED — daily loss threshold exceeded"
            );
        }
    }

    /// Returns `true` when the breaker is halted (daily loss exceeded).
    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// Cumulative PnL for the current day.
    pub fn cumulative_pnl(&self) -> rust_decimal::Decimal {
        self.cumulative_pnl
    }

    /// Max daily loss threshold.
    pub fn max_daily_loss(&self) -> rust_decimal::Decimal {
        self.config.max_daily_loss_usd
    }

    /// Resets to a fresh day if the UTC date has rolled over.
    fn maybe_reset_day(&mut self) {
        self.maybe_reset_day_at(chrono::Utc::now().date_naive());
    }

    fn maybe_reset_day_at(&mut self, today: chrono::NaiveDate) {
        if today != self.day {
            debug!(
                old_day = %self.day,
                new_day = %today,
                final_pnl = %self.cumulative_pnl,
                "PnlBreaker: day rollover — resetting"
            );
            self.day = today;
            self.cumulative_pnl = rust_decimal::Decimal::ZERO;
            self.halted = false;
        }
    }
}

impl Default for PnlBreaker {
    fn default() -> Self {
        Self::new(PnlBreakerConfig::default())
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
        let mut conn = Connection::open(path)
            .map_err(|e| ExecutorError::Persistence(format!("open {}: {e}", path.display())))?;
        migrate_executor_db(
            &mut conn,
            &[SqlMigration {
                version: 1,
                sql: "CREATE TABLE IF NOT EXISTS replay_seen (
                signal_id  TEXT PRIMARY KEY,
                expires_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_replay_expires
                ON replay_seen(expires_at);",
            }],
        )?;

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
    /// Default TTL matches the Python spec default.
    fn default() -> Self {
        Self::new(Duration::from_secs(DEFAULT_REPLAY_TTL_SECS))
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
            notional_usd: rust_decimal::Decimal::from(2000),
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
    fn journal_schema_sets_user_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replay.db");
        let _rp = ReplayProtection::with_journal(&path, Duration::from_secs(60)).unwrap();
        let conn = Connection::open(&path).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn in_memory_default_has_no_journal() {
        let rp = ReplayProtection::default();
        assert!(rp.journal.is_none());
    }

    // ---- PnlBreaker -------------------------------------------------------

    #[test]
    fn pnl_breaker_trips_on_loss_threshold() {
        let mut pb = PnlBreaker::new(PnlBreakerConfig {
            max_daily_loss_usd: rust_decimal::Decimal::from(50),
        });
        let today = chrono::Utc::now().date_naive();
        pb.record_pnl_at(rust_decimal::Decimal::from(-30), today);
        assert!(!pb.is_halted());
        assert_eq!(pb.cumulative_pnl(), rust_decimal::Decimal::from(-30));

        pb.record_pnl_at(rust_decimal::Decimal::from(-25), today);
        // -55 < -50 → halted
        assert!(pb.is_halted());
        assert_eq!(pb.cumulative_pnl(), rust_decimal::Decimal::from(-55));
    }

    #[test]
    fn pnl_breaker_profits_offset_losses() {
        let mut pb = PnlBreaker::new(PnlBreakerConfig {
            max_daily_loss_usd: rust_decimal::Decimal::from(100),
        });
        let today = chrono::Utc::now().date_naive();
        pb.record_pnl_at(rust_decimal::Decimal::from(-80), today);
        assert!(!pb.is_halted());
        // Profit pushes cumulative back up
        pb.record_pnl_at(rust_decimal::Decimal::from(50), today);
        assert!(!pb.is_halted());
        assert_eq!(pb.cumulative_pnl(), rust_decimal::Decimal::from(-30));
    }

    #[test]
    fn pnl_breaker_resets_on_day_rollover() {
        let mut pb = PnlBreaker::new(PnlBreakerConfig {
            max_daily_loss_usd: rust_decimal::Decimal::from(50),
        });
        let day1 = chrono::NaiveDate::from_ymd_opt(2025, 1, 15).unwrap();
        let day2 = chrono::NaiveDate::from_ymd_opt(2025, 1, 16).unwrap();

        pb.record_pnl_at(rust_decimal::Decimal::from(-60), day1);
        assert!(pb.is_halted());

        // New day → reset
        pb.record_pnl_at(rust_decimal::Decimal::from(-10), day2);
        assert!(!pb.is_halted());
        assert_eq!(pb.cumulative_pnl(), rust_decimal::Decimal::from(-10));
    }

    #[test]
    fn pnl_breaker_stays_halted_within_same_day() {
        let mut pb = PnlBreaker::new(PnlBreakerConfig {
            max_daily_loss_usd: rust_decimal::Decimal::from(20),
        });
        let today = chrono::Utc::now().date_naive();
        pb.record_pnl_at(rust_decimal::Decimal::from(-25), today);
        assert!(pb.is_halted());

        // Even a profit doesn't un-halt within the same day
        pb.record_pnl_at(rust_decimal::Decimal::from(100), today);
        assert!(pb.is_halted());
    }

    #[test]
    fn pnl_breaker_default_threshold_is_100() {
        let pb = PnlBreaker::default();
        assert_eq!(pb.max_daily_loss(), rust_decimal::Decimal::from(100));
        assert!(!pb.is_halted());
    }
}
