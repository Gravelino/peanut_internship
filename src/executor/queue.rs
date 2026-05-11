//! Priority queue for signals awaiting execution.
//!
//! The queue decouples signal *generation* from *execution* so multiple
//! simultaneous signals can be ranked and processed in score order. A
//! [`QueueWorker`] pulls from the heap and invokes [`Executor::execute`]
//! respecting a concurrency cap.
//!
//! ## Ordering
//! Higher `priority` first. On ties, the older `enqueued_at` wins (fairness:
//! signals don't starve under steady load).
//!
//! ## Backpressure
//! - **Bounded size** (`max_size`): when full, `push` drops the worst-scored
//!   entry in the heap (or rejects the new one if it would be the worst).
//! - **TTL**: on pop, expired signals (`!signal.is_valid()`) are skipped and
//!   accounted in metrics.
//!
//! ## Concurrency safety
//! The heap is protected by a `tokio::Mutex` so push/pop are async-safe.
//! Executions themselves run in task-level parallelism bounded by a
//! `Semaphore`.
//!
//! # TODO
//! Safe `max_concurrent_executions > 1` additionally requires inventory
//! locking (S6) so two signals don't both commit the same balance.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rust_decimal::Decimal;
use tokio::sync::{Mutex, Semaphore, mpsc, watch};
use tracing::{debug, info, warn};

use crate::core::types::{DEFAULT_QUEUE_MAX_AGE_SECS, DEFAULT_QUEUE_MAX_SIZE, split_pair_symbols};
use crate::executor::engine::{ExecutionContext, Executor};
use crate::inventory::types::Venue;
use crate::observability::metrics_handle;
use crate::strategy::signal::Direction;
use crate::strategy::signal::Signal;

/// Receives completed executions from a [`QueueWorker`]. Downstream consumers
/// typically update the scorer history + PnL ledger from this channel.
pub type ExecutionSink = mpsc::UnboundedSender<ExecutionContext>;

#[derive(Debug, Clone)]
struct InventoryReservation {
    venue: Venue,
    asset: String,
    amount: Decimal,
}

/// A signal annotated with ordering keys. Produced by the signal generator,
/// consumed by [`QueueWorker`].
#[derive(Debug, Clone)]
pub struct PrioritizedSignal {
    pub signal: Signal,
    pub priority: Decimal,
    pub enqueued_at: Instant,
}

impl PrioritizedSignal {
    pub fn new(signal: Signal) -> Self {
        let priority = signal.score;
        Self {
            signal,
            priority,
            enqueued_at: Instant::now(),
        }
    }
}

impl PartialEq for PrioritizedSignal {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.enqueued_at == other.enqueued_at
    }
}

impl Eq for PrioritizedSignal {}

impl Ord for PrioritizedSignal {
    /// Higher priority first; ties broken by older `enqueued_at` (fairness).
    fn cmp(&self, other: &Self) -> Ordering {
        match self.priority.cmp(&other.priority) {
            Ordering::Equal => other.enqueued_at.cmp(&self.enqueued_at),
            non_eq => non_eq,
        }
    }
}

impl PartialOrd for PrioritizedSignal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Configuration knobs for [`SignalQueue`].
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Maximum in-flight signals. When reached, worst-score entries are
    /// evicted on push.
    pub max_size: usize,
    /// Maximum age of an enqueued signal before it's considered stale and
    /// dropped on pop. This is a belt-and-braces check on top of
    /// [`Signal::is_valid`] which uses the signal's own TTL.
    pub max_age: Duration,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_QUEUE_MAX_SIZE,
            max_age: Duration::from_secs(DEFAULT_QUEUE_MAX_AGE_SECS),
        }
    }
}

/// Reasons why a signal can be dropped from the queue. Used as a metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Queue full at push time — incoming signal had a lower score than the
    /// weakest occupant, or the weakest occupant was evicted to make room.
    Backpressure,
    /// Signal expired (TTL) before it reached the worker.
    Expired,
}

impl DropReason {
    fn as_label(self) -> &'static str {
        match self {
            DropReason::Backpressure => "backpressure",
            DropReason::Expired => "expired",
        }
    }
}

/// Shared, async-safe priority queue.
#[derive(Debug)]
pub struct SignalQueue {
    inner: Mutex<BinaryHeap<PrioritizedSignal>>,
    config: QueueConfig,
}

impl SignalQueue {
    pub fn new(config: QueueConfig) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BinaryHeap::new()),
            config,
        })
    }

    /// Current depth (snapshot; may be stale the moment it returns).
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// `true` when the queue has no pending signals (snapshot).
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }

    /// Pushes a signal onto the queue. When full, drops either the incoming
    /// signal or the heap's current minimum, whichever has the lower score.
    /// Returns `true` if the incoming signal was enqueued.
    pub async fn push(self: &Arc<Self>, signal: Signal) -> bool {
        let incoming = PrioritizedSignal::new(signal);
        let mut heap = self.inner.lock().await;

        if heap.len() < self.config.max_size {
            heap.push(incoming);
            metrics_handle().set_queue_depth(heap.len() as i64);
            return true;
        }

        // Full: find weakest entry. BinaryHeap is a max-heap so min lives in
        // one of the leaves; scan is O(n) but n is bounded by `max_size`.
        let weakest_idx = heap
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(i, _)| i);
        let Some(idx) = weakest_idx else {
            heap.push(incoming);
            return true;
        };

        let weakest_score = heap.iter().nth(idx).map(|s| s.priority).unwrap_or_default();
        if incoming.priority <= weakest_score {
            // Incoming is worst — drop it.
            drop(heap);
            metrics_handle().record_queue_drop(DropReason::Backpressure.as_label());
            debug!(
                score = %incoming.priority,
                "queue full; incoming signal dropped (backpressure)"
            );
            return false;
        }

        // Rebuild the heap without the weakest; push incoming.
        let mut tmp: Vec<PrioritizedSignal> = heap.drain().collect();
        // The `idx` above was into iter() order, which is not heap order after
        // drain. Re-find the weakest in the drained Vec.
        if let Some((i, _)) = tmp.iter().enumerate().min_by(|(_, a), (_, b)| a.cmp(b)) {
            tmp.swap_remove(i);
        }
        tmp.push(incoming);
        *heap = BinaryHeap::from(tmp);
        let depth = heap.len() as i64;
        drop(heap);
        metrics_handle().record_queue_drop(DropReason::Backpressure.as_label());
        metrics_handle().set_queue_depth(depth);
        true
    }

    /// Pops the highest-priority signal that is still valid. Drops expired
    /// entries along the way (recording metrics). Returns `None` when empty.
    pub async fn pop_valid(self: &Arc<Self>) -> Option<PrioritizedSignal> {
        let mut heap = self.inner.lock().await;
        loop {
            let top = heap.pop()?;
            let aged_out = top.enqueued_at.elapsed() > self.config.max_age;
            if aged_out || !top.signal.is_valid() {
                metrics_handle().record_queue_drop(DropReason::Expired.as_label());
                continue;
            }
            metrics_handle().set_queue_depth(heap.len() as i64);
            return Some(top);
        }
    }
}

fn send_rejected_to_sink(sink: &Option<ExecutionSink>, signal: Signal, reason: String) {
    if let Some(tx) = sink
        && let Err(e) = tx.send(ExecutionContext::rejected(signal, reason))
    {
        warn!(error = %e, "completion sink closed; rejected execution context dropped");
    }
}

/// Pulls from a [`SignalQueue`] and executes signals through [`Executor`],
/// bounded by a concurrency cap.
pub struct QueueWorker {
    queue: Arc<SignalQueue>,
    executor: Arc<Executor>,
    semaphore: Arc<Semaphore>,
    max_concurrent_executions: usize,
    poll_interval: Duration,
    /// Optional sink that receives completed [`ExecutionContext`]s. `None`
    /// discards them (useful in tests and smoke-run setups).
    completion_sink: Option<ExecutionSink>,
    inventory: Option<Arc<tokio::sync::RwLock<crate::inventory::tracker::InventoryTracker>>>,
}

impl QueueWorker {
    /// Constructs a worker. `max_concurrent_executions = 1` preserves the
    /// pre-queue behaviour; safely raising above 1 requires inventory locks
    /// (see S6 in `docs/STRETCH_GOALS.md`).
    pub fn new(
        queue: Arc<SignalQueue>,
        executor: Arc<Executor>,
        max_concurrent_executions: usize,
        poll_interval: Duration,
    ) -> Self {
        Self {
            queue,
            executor,
            semaphore: Arc::new(Semaphore::new(max_concurrent_executions.max(1))),
            max_concurrent_executions: max_concurrent_executions.max(1),
            poll_interval,
            completion_sink: None,
            inventory: None,
        }
    }

    /// Returns a new worker with the given completion sink. Completed
    /// executions are forwarded to `sink`; if the receiver has been dropped
    /// the error is logged and the execution is discarded.
    pub fn with_sink(mut self, sink: ExecutionSink) -> Self {
        self.completion_sink = Some(sink);
        self
    }

    /// Sets the inventory tracker for inventory locking (S6).
    pub fn with_inventory(
        mut self,
        inv: Arc<tokio::sync::RwLock<crate::inventory::tracker::InventoryTracker>>,
    ) -> Self {
        self.inventory = Some(inv);
        self
    }

    /// Runs the worker loop until the outer task is cancelled. The caller is
    /// responsible for feeding `queue` via [`SignalQueue::push`].
    pub async fn run(&self) {
        self.run_loop(None).await;
    }

    /// Runs the worker until `shutdown` is signalled, then waits for all
    /// in-flight executions to finish before returning.
    pub async fn run_until_shutdown(&self, shutdown: watch::Receiver<bool>) {
        self.run_loop(Some(shutdown)).await;
    }

    async fn run_loop(&self, mut shutdown: Option<watch::Receiver<bool>>) {
        info!("queue worker started");
        loop {
            if shutdown.as_ref().is_some_and(|rx| *rx.borrow()) {
                break;
            }

            // If the queue is empty, back off briefly before polling again.
            let Some(pending) = self.queue.pop_valid().await else {
                if let Some(rx) = shutdown.as_mut() {
                    tokio::select! {
                        _ = rx.changed() => {
                            if *rx.borrow() {
                                break;
                            }
                        }
                        _ = tokio::time::sleep(self.poll_interval) => {}
                    }
                } else {
                    tokio::time::sleep(self.poll_interval).await;
                }
                continue;
            };

            let mut reservations: Vec<InventoryReservation> = Vec::new();
            // Attempt to lock inventory.
            if let Some(inv) = &self.inventory {
                let mut tracker = inv.write().await;
                let (base, quote) = match split_pair_symbols(&pending.signal.pair) {
                    Ok(parts) => parts,
                    Err(error) => {
                        warn!(
                            pair = %pending.signal.pair,
                            error = %error,
                            "invalid signal pair; dropping queued signal"
                        );
                        send_rejected_to_sink(
                            &self.completion_sink,
                            pending.signal.clone(),
                            format!("invalid signal pair: {error}"),
                        );
                        continue;
                    }
                };
                let (buy_venue, buy_asset, buy_amount, sell_venue, sell_asset, sell_amount) =
                    match pending.signal.direction {
                        Direction::BuyCexSellDex => (
                            Venue::Binance,
                            quote,
                            pending.signal.size * pending.signal.cex_price,
                            Venue::Wallet,
                            base,
                            pending.signal.size,
                        ),
                        Direction::BuyDexSellCex => (
                            Venue::Wallet,
                            quote,
                            pending.signal.size * pending.signal.dex_price,
                            Venue::Binance,
                            base,
                            pending.signal.size,
                        ),
                    };

                let check = tracker.can_execute(
                    buy_venue,
                    buy_asset,
                    buy_amount,
                    sell_venue,
                    sell_asset,
                    sell_amount,
                );
                if !check.can_execute {
                    warn!(
                        pair = %pending.signal.pair,
                        reason = ?check.reason,
                        "Dropping popped signal due to insufficient inventory (locked by another execution)"
                    );
                    send_rejected_to_sink(
                        &self.completion_sink,
                        pending.signal.clone(),
                        format!(
                            "insufficient inventory before execution: {}",
                            check.reason.unwrap_or_else(|| "unknown".to_string())
                        ),
                    );
                    continue; // Skip executing this signal, lock failed
                }

                if let Err(e) = tracker.reserve(buy_venue, buy_asset, buy_amount) {
                    warn!(pair = %pending.signal.pair, error = %e, "Failed to reserve buy amount");
                    send_rejected_to_sink(
                        &self.completion_sink,
                        pending.signal.clone(),
                        format!("failed to reserve buy amount before execution: {e}"),
                    );
                    continue;
                }
                reservations.push(InventoryReservation {
                    venue: buy_venue,
                    asset: buy_asset.to_string(),
                    amount: buy_amount,
                });
                if let Err(e) = tracker.reserve(sell_venue, sell_asset, sell_amount) {
                    warn!(pair = %pending.signal.pair, error = %e, "Failed to reserve sell amount");
                    let _ = tracker.release(buy_venue, buy_asset, buy_amount);
                    send_rejected_to_sink(
                        &self.completion_sink,
                        pending.signal.clone(),
                        format!("failed to reserve sell amount before execution: {e}"),
                    );
                    continue;
                }
                reservations.push(InventoryReservation {
                    venue: sell_venue,
                    asset: sell_asset.to_string(),
                    amount: sell_amount,
                });
            }

            // Acquire a concurrency slot (blocks when cap reached). Once a
            // signal has been popped, we still execute it even if shutdown is
            // signalled while waiting for capacity; shutdown only prevents
            // popping additional signals.
            let permit = match self.semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    if let Some(inv) = &self.inventory {
                        let mut tracker = inv.write().await;
                        for reservation in &reservations {
                            let _ = tracker.release(
                                reservation.venue,
                                &reservation.asset,
                                reservation.amount,
                            );
                        }
                    }
                    warn!("semaphore closed; stopping worker");
                    return;
                }
            };

            let executor = Arc::clone(&self.executor);
            let sink = self.completion_sink.clone();
            let inventory = self.inventory.clone();
            tokio::spawn(async move {
                let _permit = permit; // held for duration of the execution
                let ctx: ExecutionContext = executor.execute(pending.signal).await;
                if let Some(inv) = inventory {
                    let mut tracker = inv.write().await;
                    for reservation in reservations {
                        let _ = tracker.release(
                            reservation.venue,
                            &reservation.asset,
                            reservation.amount,
                        );
                    }
                }
                if let Some(tx) = sink
                    && let Err(e) = tx.send(ctx)
                {
                    warn!(error = %e, "completion sink closed; execution context dropped");
                }
            });
        }

        info!("queue worker draining in-flight executions");
        match self
            .semaphore
            .clone()
            .acquire_many_owned(self.max_concurrent_executions as u32)
            .await
        {
            Ok(permits) => {
                drop(permits);
                info!("queue worker drained");
            }
            Err(_) => warn!("semaphore closed while draining queue worker"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::MAINNET_CHAIN_ID;
    use crate::exchange::types::NormalizedBalance;
    use crate::executor::engine::{ExecutorConfig, ExecutorState, SimulatedLegs};
    use crate::inventory::tracker::InventoryTracker;
    use crate::strategy::signal::{Direction, SignalParams};

    fn mk_signal(score: Decimal, pair: &str) -> Signal {
        let mut s = Signal::new(SignalParams {
            pair: pair.into(),
            direction: Direction::BuyCexSellDex,
            cex_price: Decimal::from(2000),
            dex_price: Decimal::from(2020),
            spread_bps: Decimal::from(100),
            size: Decimal::ONE,
            notional_usd: Decimal::from(2000),
            expected_gross_pnl: Decimal::from(20),
            expected_fees: Decimal::from(5),
            expected_net_pnl: Decimal::from(15),
            ttl: chrono::Duration::seconds(5),
            inventory_ok: true,
            within_limits: true,
        });
        s.score = score;
        s
    }

    fn expired_signal(score: Decimal) -> Signal {
        let mut s = Signal::new(SignalParams {
            pair: "ETH/USDT".into(),
            direction: Direction::BuyCexSellDex,
            cex_price: Decimal::from(2000),
            dex_price: Decimal::from(2020),
            spread_bps: Decimal::from(100),
            size: Decimal::ONE,
            notional_usd: Decimal::from(2000),
            expected_gross_pnl: Decimal::from(20),
            expected_fees: Decimal::from(5),
            expected_net_pnl: Decimal::from(15),
            // Negative TTL — already expired.
            ttl: chrono::Duration::seconds(-10),
            inventory_ok: true,
            within_limits: true,
        });
        s.score = score;
        s
    }

    #[tokio::test]
    async fn pop_returns_highest_score_first() {
        let q = SignalQueue::new(QueueConfig::default());
        assert!(q.push(mk_signal(Decimal::from(50), "A")).await);
        assert!(q.push(mk_signal(Decimal::from(90), "B")).await);
        assert!(q.push(mk_signal(Decimal::from(70), "C")).await);

        let first = q.pop_valid().await.unwrap();
        assert_eq!(first.signal.pair, "B");
        let second = q.pop_valid().await.unwrap();
        assert_eq!(second.signal.pair, "C");
        let third = q.pop_valid().await.unwrap();
        assert_eq!(third.signal.pair, "A");
        assert!(q.pop_valid().await.is_none());
    }

    #[tokio::test]
    async fn tie_resolves_by_older_first() {
        let q = SignalQueue::new(QueueConfig::default());
        q.push(mk_signal(Decimal::from(80), "OLD")).await;
        // Sleep so enqueued_at differs measurably.
        tokio::time::sleep(Duration::from_millis(5)).await;
        q.push(mk_signal(Decimal::from(80), "NEW")).await;
        let first = q.pop_valid().await.unwrap();
        assert_eq!(first.signal.pair, "OLD");
    }

    #[tokio::test]
    async fn expired_signals_are_dropped_on_pop() {
        let q = SignalQueue::new(QueueConfig::default());
        q.push(expired_signal(Decimal::from(95))).await;
        q.push(mk_signal(Decimal::from(60), "GOOD")).await;
        // Expired has higher score but should be dropped.
        let popped = q.pop_valid().await.unwrap();
        assert_eq!(popped.signal.pair, "GOOD");
    }

    #[tokio::test]
    async fn backpressure_drops_weakest() {
        let q = SignalQueue::new(QueueConfig {
            max_size: 2,
            max_age: Duration::from_secs(60),
        });
        assert!(q.push(mk_signal(Decimal::from(40), "LOW")).await);
        assert!(q.push(mk_signal(Decimal::from(80), "HIGH")).await);
        // Full; new signal with better score should evict LOW.
        assert!(q.push(mk_signal(Decimal::from(70), "MID")).await);
        assert_eq!(q.len().await, 2);
        let first = q.pop_valid().await.unwrap();
        assert_eq!(first.signal.pair, "HIGH");
        let second = q.pop_valid().await.unwrap();
        assert_eq!(second.signal.pair, "MID");
    }

    #[tokio::test]
    async fn backpressure_rejects_when_incoming_is_worst() {
        let q = SignalQueue::new(QueueConfig {
            max_size: 2,
            max_age: Duration::from_secs(60),
        });
        q.push(mk_signal(Decimal::from(60), "A")).await;
        q.push(mk_signal(Decimal::from(90), "B")).await;
        // Full; incoming score lower than current min — rejected.
        assert!(!q.push(mk_signal(Decimal::from(50), "REJECT")).await);
        assert_eq!(q.len().await, 2);
    }

    #[tokio::test]
    async fn worker_executes_until_queue_empty() {
        let q = SignalQueue::new(QueueConfig::default());
        q.push(mk_signal(Decimal::from(80), "ETH/USDT")).await;

        let executor = Arc::new(Executor::new(
            Arc::new(SimulatedLegs::default()),
            ExecutorConfig {
                use_flashbots: false,
                leg1_timeout: Duration::from_millis(200),
                leg2_timeout: Duration::from_millis(200),
                ..ExecutorConfig::default()
            },
        ));
        let worker = QueueWorker::new(q.clone(), executor, 1, Duration::from_millis(10));

        let handle = tokio::spawn(async move { worker.run().await });
        // Give the worker time to pop + execute + return to idle poll.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(q.len().await, 0);
        handle.abort();
    }

    #[tokio::test]
    async fn worker_respects_concurrency_cap() {
        // Push 3 signals; cap=1. Expect them to process serially.
        let q = SignalQueue::new(QueueConfig::default());
        for i in 0..3 {
            q.push(mk_signal(Decimal::from(80 + i), "ETH/USDT")).await;
        }
        // Make the executor slow so concurrency matters.
        let executor = Arc::new(Executor::new(
            Arc::new(SimulatedLegs {
                cex_latency: Duration::from_millis(80),
                dex_latency: Duration::from_millis(80),
                ..Default::default()
            }),
            ExecutorConfig {
                use_flashbots: false,
                leg1_timeout: Duration::from_secs(2),
                leg2_timeout: Duration::from_secs(2),
                ..ExecutorConfig::default()
            },
        ));

        let worker = QueueWorker::new(q.clone(), executor, 1, Duration::from_millis(10));
        let start = Instant::now();
        let handle = tokio::spawn(async move { worker.run().await });
        // With cap=1, 3 signals × ~160ms each ≥ 480ms total.
        tokio::time::sleep(Duration::from_millis(600)).await;
        handle.abort();
        // Sanity: all three were drained.
        assert_eq!(q.len().await, 0);
        assert!(start.elapsed() >= Duration::from_millis(400));
    }

    #[tokio::test]
    async fn worker_shutdown_drains_in_flight_execution() {
        let q = SignalQueue::new(QueueConfig::default());
        q.push(mk_signal(Decimal::from(80), "ETH/USDT")).await;

        let executor = Arc::new(Executor::new(
            Arc::new(SimulatedLegs {
                cex_latency: Duration::from_millis(80),
                dex_latency: Duration::from_millis(80),
                ..Default::default()
            }),
            ExecutorConfig {
                use_flashbots: false,
                leg1_timeout: Duration::from_secs(2),
                leg2_timeout: Duration::from_secs(2),
                ..ExecutorConfig::default()
            },
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker =
            QueueWorker::new(q.clone(), executor, 1, Duration::from_millis(10)).with_sink(tx);
        let handle = tokio::spawn(async move { worker.run_until_shutdown(shutdown_rx).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown_tx.send(true).unwrap();

        let ctx = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(ctx.state.is_filled());
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(q.len().await, 0);
    }

    #[tokio::test]
    async fn worker_inventory_reservations_block_overlap_and_release_after_execution() {
        let q = SignalQueue::new(QueueConfig::default());
        q.push(mk_signal(Decimal::from(90), "ETH/USDT")).await;
        q.push(mk_signal(Decimal::from(80), "ETH/USDT")).await;

        let mut tracker =
            InventoryTracker::new(vec![Venue::Binance, Venue::Wallet], MAINNET_CHAIN_ID);
        tracker.update_from_cex(
            Venue::Binance,
            std::collections::HashMap::from([(
                "USDT".to_string(),
                NormalizedBalance {
                    free: Decimal::from(2000),
                    locked: Decimal::ZERO,
                    total: Decimal::from(2000),
                },
            )]),
        );
        tracker.update_from_wallet(
            Venue::Wallet,
            std::collections::HashMap::from([("ETH".to_string(), Decimal::ONE)]),
        );
        let inventory = Arc::new(tokio::sync::RwLock::new(tracker));

        let executor = Arc::new(Executor::new(
            Arc::new(SimulatedLegs {
                cex_latency: Duration::from_millis(80),
                dex_latency: Duration::from_millis(80),
                ..Default::default()
            }),
            ExecutorConfig {
                use_flashbots: false,
                leg1_timeout: Duration::from_secs(2),
                leg2_timeout: Duration::from_secs(2),
                ..ExecutorConfig::default()
            },
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let worker = QueueWorker::new(q.clone(), executor, 2, Duration::from_millis(10))
            .with_sink(tx)
            .with_inventory(Arc::clone(&inventory));
        let handle = tokio::spawn(async move { worker.run().await });

        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let states = [first.state, second.state];
        assert!(states.iter().any(|state| state.is_filled()));
        assert!(states.contains(&ExecutorState::Rejected));
        assert!(rx.try_recv().is_err());
        assert_eq!(q.len().await, 0);
        assert_eq!(
            inventory
                .read()
                .await
                .get_available(Venue::Binance, "USDT")
                .unwrap(),
            Decimal::from(2000)
        );
        assert_eq!(
            inventory
                .read()
                .await
                .get_available(Venue::Wallet, "ETH")
                .unwrap(),
            Decimal::ONE
        );
        handle.abort();
    }

    #[tokio::test]
    async fn worker_sends_rejected_completion_when_inventory_preflight_drops_signal() {
        let q = SignalQueue::new(QueueConfig::default());
        q.push(mk_signal(Decimal::from(90), "ETH/USDT")).await;

        let tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet], MAINNET_CHAIN_ID);
        let inventory = Arc::new(tokio::sync::RwLock::new(tracker));
        let executor = Arc::new(Executor::new(
            Arc::new(SimulatedLegs::default()),
            ExecutorConfig::default(),
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let worker = QueueWorker::new(q.clone(), executor, 1, Duration::from_millis(10))
            .with_sink(tx)
            .with_inventory(Arc::clone(&inventory));
        let handle = tokio::spawn(async move { worker.run().await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let ctx = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ctx.state, crate::executor::engine::ExecutorState::Rejected);
        assert!(
            ctx.error
                .as_deref()
                .unwrap_or_default()
                .contains("insufficient inventory")
        );
        assert_eq!(q.len().await, 0);
        handle.abort();
    }
}
