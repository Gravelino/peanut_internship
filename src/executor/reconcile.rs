//! Post-timeout reconciliation for `LEG2_TIMEOUT` executions.
//!
//! When the DEX leg2 tx doesn't produce a receipt before [`leg2_timeout`],
//! the executor marks the `ExecutionContext` as
//! [`ExecutorState::Leg2Timeout`] and **does not** auto-unwind — the tx may
//! still land and flattening now would create double exposure. That leaves
//! an open leg-1 position that an operator must close manually.
//!
//! This module replaces the manual step with a background reconciler:
//! - [`ReconcileStore`] is an SQLite-backed log of (signal_id, tx_hash)
//!   entries awaiting a receipt.
//! - [`ReconcileWorker::tick_once`] drains one pending entry, polls the
//!   chain for a receipt, and either resolves (tx landed cleanly), unwinds
//!   (tx reverted), or leaves it pending.
//!
//! Everything is gated behind a [`ReceiptProvider`] trait so tests can
//! inject deterministic stubs without a live RPC endpoint.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, instrument, warn};

use crate::chain::client::ChainClient;
use crate::core::types::TransactionReceipt;
use crate::executor::errors::ExecutorError;
use crate::executor::migrations::{SqlMigration, migrate_executor_db};
use crate::strategy::signal::Direction;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Status of a [`PendingReconcile`] entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileStatus {
    /// Waiting for the tx to land (or expire).
    Pending,
    /// Tx landed with `status=1`. The corresponding execution was marked
    /// terminal (`Done`) by the worker.
    Resolved,
    /// Tx reverted or was treated as such; unwind was attempted.
    Reverted,
    /// Exceeded the age cap without a receipt — requires human review.
    Expired,
    /// Reconciliation itself errored (chain unreachable, decoding issue);
    /// retained for later retry / manual audit.
    Errored,
}

impl ReconcileStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolved => "resolved",
            Self::Reverted => "reverted",
            Self::Expired => "expired",
            Self::Errored => "errored",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "resolved" => Self::Resolved,
            "reverted" => Self::Reverted,
            "expired" => Self::Expired,
            "errored" => Self::Errored,
            other => {
                warn!(
                    status = other,
                    "unknown reconcile status in DB; defaulting to Pending"
                );
                Self::Pending
            }
        }
    }
}

/// A LEG2_TIMEOUT entry that the reconciler needs to track.
///
/// Serialised as JSON in the store's `payload` column so that
/// [`PendingReconcile`] fields can evolve without schema migrations for
/// minor additions — the small set of fields queried directly (tx_hash,
/// started_at, status) is promoted to dedicated columns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingReconcile {
    /// Signal id — primary key; prevents double-reconciliation if the
    /// executor accidentally enqueues the same timeout twice.
    pub signal_id: String,
    /// Hash of the leg-2 tx that timed out.
    pub tx_hash: String,
    /// Trading pair symbol (for alerts / debug output).
    pub pair: String,
    /// Direction of the originating signal.
    pub direction: Direction,
    /// Venue of the leg-1 position that is still open. Currently always
    /// `"cex"` in production (DEX-first uses Flashbots bundles which never
    /// produce partial exposure), but kept generic for future expansion.
    pub leg1_venue: String,
    /// Serialised decimal (e.g. `"0.5"`) of the leg-1 fill size that is
    /// still on-book. Kept as a string to avoid pulling `rust_decimal`
    /// serialization features here.
    pub leg1_fill_size: String,
    /// Unix timestamp (seconds) when the timeout occurred.
    pub started_at_unix: i64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors raised by [`ReconcileStore`] and [`ReconcileWorker`].
#[derive(Debug, Error)]
pub enum ReconcileError {
    /// SQLite open / I/O / migration failure.
    #[error("persistence: {0}")]
    Persistence(String),
    /// Payload JSON serialisation / deserialisation failure.
    #[error("serde: {0}")]
    Serde(String),
    /// Underlying RPC call failed.
    #[error("chain: {0}")]
    Chain(String),
}

/// Convenience alias.
pub type ReconcileResult<T> = Result<T, ReconcileError>;

fn reconcile_migration_error(error: ExecutorError) -> ReconcileError {
    ReconcileError::Persistence(error.to_string())
}

// ---------------------------------------------------------------------------
// Receipt provider (decoupled from ChainClient for testability)
// ---------------------------------------------------------------------------

/// Minimal abstraction over `ChainClient::get_receipt`. Extracted into a
/// trait so [`ReconcileWorker`] can be exercised with deterministic stubs.
#[async_trait]
pub trait ReceiptProvider: Send + Sync {
    /// Returns the receipt for `tx_hash` if the tx has been mined, else
    /// `None`. A network error surfaces as `Err`.
    async fn get_receipt(&self, tx_hash: &str) -> ReconcileResult<Option<TransactionReceipt>>;
}

/// Production [`ReceiptProvider`] backed by [`ChainClient`].
#[derive(Clone)]
pub struct ChainReceiptProvider {
    client: ChainClient,
}

impl ChainReceiptProvider {
    /// Wraps `client` as a receipt provider.
    pub fn new(client: ChainClient) -> Self {
        Self { client }
    }
}

impl std::fmt::Debug for ChainReceiptProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainReceiptProvider").finish()
    }
}

#[async_trait]
impl ReceiptProvider for ChainReceiptProvider {
    async fn get_receipt(&self, tx_hash: &str) -> ReconcileResult<Option<TransactionReceipt>> {
        self.client
            .get_receipt(tx_hash)
            .await
            .map_err(|e| ReconcileError::Chain(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// ReconcileStore
// ---------------------------------------------------------------------------

/// SQLite-backed persistence for pending reconciliations.
///
/// The `Arc<Mutex<Connection>>` is only locked for the duration of a single
/// SQL statement — never across an await — so holding it across threads is
/// safe. Sharing a single store across async tasks is therefore fine.
#[derive(Clone)]
pub struct ReconcileStore {
    #[allow(dead_code)]
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for ReconcileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReconcileStore")
            .field("path", &self.path)
            .finish()
    }
}

impl ReconcileStore {
    /// Opens (creating if absent) the reconcile database at `path`.
    pub fn open(path: impl AsRef<Path>) -> ReconcileResult<Self> {
        let path = path.as_ref().to_path_buf();
        let mut conn = Connection::open(&path)
            .map_err(|e| ReconcileError::Persistence(format!("open {}: {e}", path.display())))?;
        migrate_executor_db(
            &mut conn,
            &[SqlMigration {
                version: 1,
                sql: "CREATE TABLE IF NOT EXISTS pending_reconcile (
                signal_id   TEXT PRIMARY KEY,
                tx_hash     TEXT NOT NULL,
                payload     TEXT NOT NULL,
                started_at  INTEGER NOT NULL,
                status      TEXT NOT NULL DEFAULT 'pending',
                resolution  TEXT,
                resolved_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_reconcile_status
                ON pending_reconcile(status);",
            }],
        )
        .map_err(reconcile_migration_error)?;
        Ok(Self {
            path,
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Adds a new entry. Idempotent on `signal_id` — a second `add` for the
    /// same id refreshes `tx_hash`/`payload`/`status=pending` (handy when
    /// the operator manually requeues a stuck reconciliation).
    pub fn add(&self, entry: &PendingReconcile) -> ReconcileResult<()> {
        let payload = serde_json::to_string(entry)
            .map_err(|e| ReconcileError::Serde(format!("serialize: {e}")))?;
        let guard = self
            .conn
            .lock()
            .map_err(|e| ReconcileError::Persistence(format!("lock: {e}")))?;
        guard
            .execute(
                "INSERT INTO pending_reconcile
                    (signal_id, tx_hash, payload, started_at, status)
                 VALUES(?1, ?2, ?3, ?4, 'pending')
                 ON CONFLICT(signal_id) DO UPDATE SET
                    tx_hash = excluded.tx_hash,
                    payload = excluded.payload,
                    started_at = excluded.started_at,
                    status = 'pending',
                    resolution = NULL,
                    resolved_at = NULL",
                params![
                    entry.signal_id,
                    entry.tx_hash,
                    payload,
                    entry.started_at_unix,
                ],
            )
            .map_err(|e| ReconcileError::Persistence(format!("insert: {e}")))?;
        Ok(())
    }

    /// Returns all entries whose status is `pending`, oldest first.
    pub fn list_pending(&self) -> ReconcileResult<Vec<PendingReconcile>> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| ReconcileError::Persistence(format!("lock: {e}")))?;
        let mut stmt = guard
            .prepare(
                "SELECT payload FROM pending_reconcile
                 WHERE status = 'pending'
                 ORDER BY started_at ASC",
            )
            .map_err(|e| ReconcileError::Persistence(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| ReconcileError::Persistence(format!("query: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            let raw = row.map_err(|e| ReconcileError::Persistence(format!("row: {e}")))?;
            let entry: PendingReconcile = serde_json::from_str(&raw)
                .map_err(|e| ReconcileError::Serde(format!("deserialize: {e}")))?;
            out.push(entry);
        }
        Ok(out)
    }

    /// Transitions an entry to a terminal status, stamping `resolved_at`
    /// with the current unix time.
    pub fn mark(
        &self,
        signal_id: &str,
        status: ReconcileStatus,
        resolution: Option<&str>,
    ) -> ReconcileResult<()> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| ReconcileError::Persistence(format!("lock: {e}")))?;
        guard
            .execute(
                "UPDATE pending_reconcile
                 SET status = ?2, resolution = ?3, resolved_at = ?4
                 WHERE signal_id = ?1",
                params![signal_id, status.as_str(), resolution, now_unix()],
            )
            .map_err(|e| ReconcileError::Persistence(format!("update: {e}")))?;
        Ok(())
    }

    /// Async wrapper around [`ReconcileStore::add`] that runs the SQLite
    /// write on a `tokio::task::spawn_blocking` thread so the runtime
    /// workers are never blocked on disk I/O (A4).
    pub async fn add_async(&self, entry: PendingReconcile) -> ReconcileResult<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.add(&entry))
            .await
            .map_err(|e| ReconcileError::Persistence(format!("spawn_blocking: {e}")))?
    }

    /// Async wrapper around [`ReconcileStore::list_pending`] (A4).
    pub async fn list_pending_async(&self) -> ReconcileResult<Vec<PendingReconcile>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.list_pending())
            .await
            .map_err(|e| ReconcileError::Persistence(format!("spawn_blocking: {e}")))?
    }

    /// Async wrapper around [`ReconcileStore::mark`] (A4).
    pub async fn mark_async(
        &self,
        signal_id: String,
        status: ReconcileStatus,
        resolution: Option<String>,
    ) -> ReconcileResult<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.mark(&signal_id, status, resolution.as_deref()))
            .await
            .map_err(|e| ReconcileError::Persistence(format!("spawn_blocking: {e}")))?
    }

    /// Looks up the status of a single entry (for testing / dashboards).
    pub fn status_of(&self, signal_id: &str) -> ReconcileResult<Option<ReconcileStatus>> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| ReconcileError::Persistence(format!("lock: {e}")))?;
        let mut stmt = guard
            .prepare("SELECT status FROM pending_reconcile WHERE signal_id = ?1")
            .map_err(|e| ReconcileError::Persistence(format!("prepare: {e}")))?;
        let mut rows = stmt
            .query(params![signal_id])
            .map_err(|e| ReconcileError::Persistence(format!("query: {e}")))?;
        match rows
            .next()
            .map_err(|e| ReconcileError::Persistence(format!("next: {e}")))?
        {
            Some(row) => {
                let s: String = row
                    .get(0)
                    .map_err(|e| ReconcileError::Persistence(format!("get: {e}")))?;
                Ok(Some(ReconcileStatus::from_str(&s)))
            }
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// Tunables for [`ReconcileWorker`].
#[derive(Debug, Clone)]
pub struct ReconcileConfig {
    /// Polling interval for the worker's main loop.
    pub poll_interval: Duration,
    /// An entry older than this with no receipt is marked `Expired` and
    /// surfaced for manual intervention.
    pub max_age: Duration,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            // 1 hour — generously longer than typical mempool residency.
            max_age: Duration::from_secs(60 * 60),
        }
    }
}

/// Outcome of a single reconcile tick (one entry inspected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    /// No receipt yet; age within `max_age`.
    StillPending,
    /// Receipt landed with `status=1` — leg-2 succeeded. The caller (the
    /// worker itself, in production) marks the entry `Resolved`.
    ReceiptSuccess,
    /// Receipt landed with `status=0` — leg-2 reverted. The worker will
    /// trigger an unwind and mark `Reverted`.
    ReceiptReverted,
    /// Age exceeded `max_age` without a receipt. Marked `Expired`.
    Expired,
}

/// Worker that polls each [`PendingReconcile`] in the store for its
/// receipt and transitions it to a terminal status.
///
/// `unwind_fn` is invoked on revert to flatten leg-1 exposure. It is typed
/// as an `Arc<dyn Fn + Send + Sync>` so callers can close over the concrete
/// `LegExecutor` in `arb_bot` without this module depending on it directly.
#[derive(Clone)]
pub struct ReconcileWorker {
    /// Shared store handle. `ReconcileStore` is already internally
    /// `Arc<Mutex<Connection>>`, so this outer `Arc` is nominal — we take
    /// it so call sites (notably `arb_bot`) can keep their own
    /// `Arc<ReconcileStore>` and pass it via `Arc::clone`, instead of
    /// deref-clone dance.
    store: Arc<ReconcileStore>,
    provider: Arc<dyn ReceiptProvider>,
    config: ReconcileConfig,
}

impl std::fmt::Debug for ReconcileWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReconcileWorker")
            .field("store", &*self.store)
            .field("config", &self.config)
            .finish()
    }
}

impl ReconcileWorker {
    /// Creates a worker bound to `store` and `provider`. Accepts an
    /// `Arc<ReconcileStore>` so callers can share the store with other
    /// consumers (e.g. the `Executor` itself) without cloning.
    pub fn new(
        store: Arc<ReconcileStore>,
        provider: Arc<dyn ReceiptProvider>,
        config: ReconcileConfig,
    ) -> Self {
        Self {
            store,
            provider,
            config,
        }
    }

    /// Evaluates a single pending entry and returns the transition that
    /// *should* happen. Does NOT mutate the store — the caller chooses
    /// whether to act on the outcome (so the unwind callback stays
    /// separate from polling for cleaner tests).
    #[instrument(level = "debug", skip(self), fields(signal = %entry.signal_id, tx = %entry.tx_hash))]
    pub async fn inspect(&self, entry: &PendingReconcile) -> ReconcileResult<TickOutcome> {
        match self.provider.get_receipt(&entry.tx_hash).await? {
            Some(receipt) => {
                if receipt.status {
                    Ok(TickOutcome::ReceiptSuccess)
                } else {
                    Ok(TickOutcome::ReceiptReverted)
                }
            }
            None => {
                let age = now_unix().saturating_sub(entry.started_at_unix);
                if age >= self.config.max_age.as_secs() as i64 {
                    Ok(TickOutcome::Expired)
                } else {
                    Ok(TickOutcome::StillPending)
                }
            }
        }
    }

    /// Inspects every pending entry in the store, applying the terminal
    /// transition locally. Returns the list of (signal_id, outcome) pairs
    /// so the caller can fan out side-effects (e.g. unwind, alerts) in one
    /// place.
    pub async fn tick_once(&self) -> ReconcileResult<Vec<(PendingReconcile, TickOutcome)>> {
        let entries = self.store.list_pending_async().await?;
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            match self.inspect(&entry).await {
                Ok(TickOutcome::ReceiptSuccess) => {
                    self.store
                        .mark_async(entry.signal_id.clone(), ReconcileStatus::Resolved, None)
                        .await?;
                    info!(signal = %entry.signal_id, tx = %entry.tx_hash, "reconcile: resolved");
                    out.push((entry, TickOutcome::ReceiptSuccess));
                }
                Ok(TickOutcome::ReceiptReverted) => {
                    self.store
                        .mark_async(
                            entry.signal_id.clone(),
                            ReconcileStatus::Reverted,
                            Some("leg2 reverted on-chain".to_string()),
                        )
                        .await?;
                    warn!(
                        signal = %entry.signal_id,
                        tx = %entry.tx_hash,
                        "reconcile: leg2 reverted, unwind required"
                    );
                    out.push((entry, TickOutcome::ReceiptReverted));
                }
                Ok(TickOutcome::Expired) => {
                    self.store
                        .mark_async(
                            entry.signal_id.clone(),
                            ReconcileStatus::Expired,
                            Some("max_age exceeded without receipt".to_string()),
                        )
                        .await?;
                    warn!(
                        signal = %entry.signal_id,
                        tx = %entry.tx_hash,
                        "reconcile: expired — manual review required"
                    );
                    out.push((entry, TickOutcome::Expired));
                }
                Ok(TickOutcome::StillPending) => {
                    out.push((entry, TickOutcome::StillPending));
                }
                Err(e) => {
                    warn!(
                        signal = %entry.signal_id,
                        tx = %entry.tx_hash,
                        error = %e,
                        "reconcile: inspect failed, will retry"
                    );
                    out.push((entry, TickOutcome::StillPending));
                }
            }
        }
        Ok(out)
    }

    /// Polling interval configured for this worker.
    pub fn poll_interval(&self) -> Duration {
        self.config.poll_interval
    }

    /// Access the underlying store (read-only handle for dashboards).
    pub fn store(&self) -> &ReconcileStore {
        &self.store
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn mk_entry(id: &str, age_secs: i64) -> PendingReconcile {
        PendingReconcile {
            signal_id: id.into(),
            tx_hash: format!("0x{:064x}", id.len() as u64),
            pair: "ETH/USDC".into(),
            direction: Direction::BuyCexSellDex,
            leg1_venue: "cex".into(),
            leg1_fill_size: "0.5".into(),
            started_at_unix: now_unix() - age_secs,
        }
    }

    fn empty_receipt(tx: &str, status: bool) -> TransactionReceipt {
        TransactionReceipt {
            tx_hash: tx.into(),
            block_number: 1,
            status,
            gas_used: Default::default(),
            effective_gas_price: Default::default(),
            logs: vec![json!({})],
        }
    }

    #[derive(Debug, Default)]
    struct MockProvider {
        /// Response for the next get_receipt call.
        response: Mutex<Option<Option<TransactionReceipt>>>,
        /// If true, next call errors.
        error: Mutex<bool>,
    }

    impl MockProvider {
        fn set_receipt(&self, r: Option<TransactionReceipt>) {
            *self.response.lock().unwrap() = Some(r);
        }
        fn set_error(&self) {
            *self.error.lock().unwrap() = true;
        }
    }

    #[async_trait]
    impl ReceiptProvider for MockProvider {
        async fn get_receipt(&self, _tx_hash: &str) -> ReconcileResult<Option<TransactionReceipt>> {
            if std::mem::replace(&mut *self.error.lock().unwrap(), false) {
                return Err(ReconcileError::Chain("mocked failure".into()));
            }
            Ok(self.response.lock().unwrap().clone().unwrap_or(None))
        }
    }

    // ---- Store CRUD ----------------------------------------------------

    #[test]
    fn store_add_then_list_pending() {
        let dir = tempdir().unwrap();
        let store = ReconcileStore::open(dir.path().join("r.db")).unwrap();
        let a = mk_entry("sig-a", 5);
        let b = mk_entry("sig-b", 1);
        store.add(&a).unwrap();
        store.add(&b).unwrap();

        let pending = store.list_pending().unwrap();
        // Oldest first.
        assert_eq!(pending[0].signal_id, "sig-a");
        assert_eq!(pending[1].signal_id, "sig-b");
        assert_eq!(
            store.status_of("sig-a").unwrap(),
            Some(ReconcileStatus::Pending)
        );
    }

    #[test]
    fn store_mark_removes_from_pending_list() {
        let dir = tempdir().unwrap();
        let store = ReconcileStore::open(dir.path().join("r.db")).unwrap();
        store.add(&mk_entry("sig-a", 5)).unwrap();
        store.add(&mk_entry("sig-b", 1)).unwrap();

        store
            .mark("sig-a", ReconcileStatus::Resolved, Some("ok"))
            .unwrap();
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].signal_id, "sig-b");
        assert_eq!(
            store.status_of("sig-a").unwrap(),
            Some(ReconcileStatus::Resolved)
        );
    }

    #[test]
    fn store_add_is_idempotent_on_signal_id() {
        let dir = tempdir().unwrap();
        let store = ReconcileStore::open(dir.path().join("r.db")).unwrap();
        let mut e = mk_entry("sig-x", 10);
        store.add(&e).unwrap();
        e.tx_hash = "0xnew".into();
        // Second insert for same signal_id refreshes tx_hash + resets
        // status (handy for operator requeues).
        store
            .mark("sig-x", ReconcileStatus::Errored, Some("oops"))
            .unwrap();
        store.add(&e).unwrap();
        assert_eq!(
            store.status_of("sig-x").unwrap(),
            Some(ReconcileStatus::Pending)
        );
        let pending = store.list_pending().unwrap();
        assert_eq!(pending[0].tx_hash, "0xnew");
    }

    #[test]
    fn store_persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("r.db");
        {
            let s = ReconcileStore::open(&path).unwrap();
            s.add(&mk_entry("sig-persist", 3)).unwrap();
        }
        let s2 = ReconcileStore::open(&path).unwrap();
        let p = s2.list_pending().unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].signal_id, "sig-persist");
    }

    #[test]
    fn store_schema_sets_user_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("r.db");
        let _store = ReconcileStore::open(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    // ---- Worker inspect ------------------------------------------------

    fn worker_with(
        mock: Arc<MockProvider>,
        max_age_secs: u64,
    ) -> (tempfile::TempDir, ReconcileStore, ReconcileWorker) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("r.db");
        let store = ReconcileStore::open(&path).unwrap();
        let worker = ReconcileWorker::new(
            Arc::new(store.clone()),
            mock,
            ReconcileConfig {
                poll_interval: Duration::from_millis(100),
                max_age: Duration::from_secs(max_age_secs),
            },
        );
        (dir, store, worker)
    }

    #[tokio::test]
    async fn inspect_success_receipt_returns_success() {
        let mock = Arc::new(MockProvider::default());
        mock.set_receipt(Some(empty_receipt("0xabc", true)));
        let (_dir, _store, worker) = worker_with(Arc::clone(&mock), 3600);
        let outcome = worker.inspect(&mk_entry("sig-a", 5)).await.unwrap();
        assert_eq!(outcome, TickOutcome::ReceiptSuccess);
    }

    #[tokio::test]
    async fn inspect_reverted_receipt_returns_reverted() {
        let mock = Arc::new(MockProvider::default());
        mock.set_receipt(Some(empty_receipt("0xabc", false)));
        let (_dir, _store, worker) = worker_with(Arc::clone(&mock), 3600);
        let outcome = worker.inspect(&mk_entry("sig-a", 5)).await.unwrap();
        assert_eq!(outcome, TickOutcome::ReceiptReverted);
    }

    #[tokio::test]
    async fn inspect_none_within_age_returns_pending() {
        let mock = Arc::new(MockProvider::default());
        mock.set_receipt(None);
        let (_dir, _store, worker) = worker_with(Arc::clone(&mock), 3600);
        let outcome = worker.inspect(&mk_entry("sig-a", 5)).await.unwrap();
        assert_eq!(outcome, TickOutcome::StillPending);
    }

    #[tokio::test]
    async fn inspect_none_over_max_age_returns_expired() {
        let mock = Arc::new(MockProvider::default());
        mock.set_receipt(None);
        // max_age = 10s, entry age = 100s → expired.
        let (_dir, _store, worker) = worker_with(Arc::clone(&mock), 10);
        let outcome = worker.inspect(&mk_entry("sig-old", 100)).await.unwrap();
        assert_eq!(outcome, TickOutcome::Expired);
    }

    // ---- tick_once aggregate behaviour --------------------------------

    #[tokio::test]
    async fn tick_once_marks_resolved_on_success_receipt() {
        let mock = Arc::new(MockProvider::default());
        mock.set_receipt(Some(empty_receipt("0xabc", true)));
        let (_dir, store, worker) = worker_with(Arc::clone(&mock), 3600);
        store.add(&mk_entry("sig-a", 1)).unwrap();

        let outcomes = worker.tick_once().await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, TickOutcome::ReceiptSuccess);
        assert_eq!(
            store.status_of("sig-a").unwrap(),
            Some(ReconcileStatus::Resolved)
        );
        assert!(store.list_pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tick_once_marks_reverted_on_failed_receipt() {
        let mock = Arc::new(MockProvider::default());
        mock.set_receipt(Some(empty_receipt("0xabc", false)));
        let (_dir, store, worker) = worker_with(Arc::clone(&mock), 3600);
        store.add(&mk_entry("sig-a", 1)).unwrap();

        worker.tick_once().await.unwrap();
        assert_eq!(
            store.status_of("sig-a").unwrap(),
            Some(ReconcileStatus::Reverted)
        );
    }

    #[tokio::test]
    async fn tick_once_keeps_entry_pending_on_provider_error() {
        let mock = Arc::new(MockProvider::default());
        mock.set_error();
        let (_dir, store, worker) = worker_with(Arc::clone(&mock), 3600);
        store.add(&mk_entry("sig-a", 1)).unwrap();

        let outcomes = worker.tick_once().await.unwrap();
        assert_eq!(outcomes[0].1, TickOutcome::StillPending);
        assert_eq!(
            store.status_of("sig-a").unwrap(),
            Some(ReconcileStatus::Pending)
        );
    }

    #[tokio::test]
    async fn tick_once_expires_entries_past_max_age() {
        let mock = Arc::new(MockProvider::default());
        mock.set_receipt(None);
        let (_dir, store, worker) = worker_with(Arc::clone(&mock), 5);
        store.add(&mk_entry("sig-stale", 100)).unwrap();

        worker.tick_once().await.unwrap();
        assert_eq!(
            store.status_of("sig-stale").unwrap(),
            Some(ReconcileStatus::Expired)
        );
    }
}
