//! Prometheus metrics holder.
//!
//! A single [`Metrics`] instance is constructed at startup via [`init_metrics`]
//! and stored in a process-wide `OnceLock`. Hot code paths call
//! [`metrics_handle`] to record observations; if metrics were never initialised
//! (e.g. in unit tests that don't care), the handle returns a no-op facade so
//! instrumentation never panics.
//!
//! # Cardinality guardrails
//! - `state` label is bounded by [`ExecutorState`] variants (~13).
//! - `pair` label grows with configured trading pairs (≤ dozens). Do NOT add
//!   labels that can be unbounded (signal IDs, tx hashes, error strings).

use std::sync::{Arc, OnceLock};

use prometheus::{
    Encoder, GaugeVec, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder, histogram_opts,
};

use crate::executor::ExecutorState;

/// Bucket layout in seconds. Tuned for arb executions: sub-second happy path,
/// long tail up to CEX/DEX leg2 timeout (~60s) + reconcile window.
const EXEC_DURATION_BUCKETS: &[f64] =
    &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

/// PnL histogram buckets (USD, signed). Symmetric around zero so we can see
/// loss distribution too.
const PNL_BUCKETS: &[f64] = &[
    -500.0, -100.0, -25.0, -5.0, -1.0, 0.0, 1.0, 5.0, 25.0, 100.0, 500.0,
];

/// Fill ratio: 0..=1 with tight resolution near 1 (we care about partials).
const FILL_RATIO_BUCKETS: &[f64] = &[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99, 1.0];

/// Process-wide metrics holder. Cheap to clone (all fields are `Arc`-wrapped
/// internally by the prometheus crate).
#[derive(Debug, Clone)]
pub struct Metrics {
    pub registry: Registry,

    // Counters
    pub signals_generated_total: IntCounterVec,
    pub executions_total: IntCounterVec,
    pub breaker_trips_total: IntCounter,
    pub unwind_failures_total: IntCounter,
    pub signal_queue_drops_total: IntCounterVec,
    pub flashbots_simulations_total: IntCounterVec,
    pub flashbots_bundles_submitted_total: IntCounterVec,
    pub flashbots_bundles_included_total: IntCounterVec,
    pub flashbots_bundles_not_included_total: IntCounterVec,
    pub flashbots_relay_errors_total: IntCounterVec,

    // Gauges
    pub breaker_open: IntGauge,
    pub pnl_breaker_halted: IntGauge,
    pub signal_queue_depth: IntGauge,
    pub inventory_skew_bps: GaugeVec,

    // Histograms
    pub execution_duration_seconds: HistogramVec,
    pub leg1_fill_ratio: HistogramVec,
    pub realized_pnl_usd: HistogramVec,
    pub flashbots_bundle_simulation_seconds: HistogramVec,
    pub flashbots_bundle_inclusion_blocks: HistogramVec,

    // Internal: counter for test-only assertions.
    #[doc(hidden)]
    pub enabled: bool,
}

impl Metrics {
    /// Builds a fresh registry with all metrics registered. Use this in tests
    /// when you want an isolated registry; production should use
    /// [`init_metrics`] instead.
    pub fn new() -> Self {
        let registry = Registry::new();

        let signals_generated_total = IntCounterVec::new(
            Opts::new(
                "peanut_signals_generated_total",
                "Signals emitted by the generator, labelled by pair/direction.",
            ),
            &["pair", "direction"],
        )
        .expect("valid counter opts");
        registry
            .register(Box::new(signals_generated_total.clone()))
            .expect("unique metric");

        let executions_total = IntCounterVec::new(
            Opts::new(
                "peanut_executions_total",
                "Executions grouped by terminal state.",
            ),
            &["state"],
        )
        .expect("valid counter opts");
        registry
            .register(Box::new(executions_total.clone()))
            .expect("unique metric");

        let breaker_trips_total = IntCounter::new(
            "peanut_breaker_trips_total",
            "Number of times the circuit breaker transitioned to Open.",
        )
        .expect("valid counter opts");
        registry
            .register(Box::new(breaker_trips_total.clone()))
            .expect("unique metric");

        let unwind_failures_total = IntCounter::new(
            "peanut_unwind_failures_total",
            "Unwind attempts that returned Err (leg1 still exposed).",
        )
        .expect("valid counter opts");
        registry
            .register(Box::new(unwind_failures_total.clone()))
            .expect("unique metric");

        let signal_queue_drops_total = IntCounterVec::new(
            Opts::new(
                "peanut_signal_queue_drops_total",
                "Signals dropped by the priority queue, labelled by reason.",
            ),
            &["reason"],
        )
        .expect("valid counter opts");
        registry
            .register(Box::new(signal_queue_drops_total.clone()))
            .expect("unique metric");

        let flashbots_simulations_total = IntCounterVec::new(
            Opts::new(
                "peanut_flashbots_simulations_total",
                "Flashbots bundle simulations grouped by status.",
            ),
            &["status"],
        )
        .expect("valid counter opts");
        registry
            .register(Box::new(flashbots_simulations_total.clone()))
            .expect("unique metric");

        let flashbots_bundles_submitted_total = IntCounterVec::new(
            Opts::new(
                "peanut_flashbots_bundles_submitted_total",
                "Flashbots bundles accepted by relay submission.",
            ),
            &["relay"],
        )
        .expect("valid counter opts");
        registry
            .register(Box::new(flashbots_bundles_submitted_total.clone()))
            .expect("unique metric");

        let flashbots_bundles_included_total = IntCounterVec::new(
            Opts::new(
                "peanut_flashbots_bundles_included_total",
                "Flashbots bundle transactions found in mined receipts.",
            ),
            &["relay"],
        )
        .expect("valid counter opts");
        registry
            .register(Box::new(flashbots_bundles_included_total.clone()))
            .expect("unique metric");

        let flashbots_bundles_not_included_total = IntCounterVec::new(
            Opts::new(
                "peanut_flashbots_bundles_not_included_total",
                "Flashbots bundles that expired without an on-chain receipt.",
            ),
            &["relay"],
        )
        .expect("valid counter opts");
        registry
            .register(Box::new(flashbots_bundles_not_included_total.clone()))
            .expect("unique metric");

        let flashbots_relay_errors_total = IntCounterVec::new(
            Opts::new(
                "peanut_flashbots_relay_errors_total",
                "Flashbots relay/client errors grouped by bounded error class.",
            ),
            &["relay", "error"],
        )
        .expect("valid counter opts");
        registry
            .register(Box::new(flashbots_relay_errors_total.clone()))
            .expect("unique metric");

        let breaker_open = IntGauge::new(
            "peanut_breaker_open",
            "1 when the circuit breaker is currently open, 0 otherwise.",
        )
        .expect("valid gauge opts");
        registry
            .register(Box::new(breaker_open.clone()))
            .expect("unique metric");

        let pnl_breaker_halted = IntGauge::new(
            "peanut_pnl_breaker_halted",
            "1 when daily PnL loss threshold has been exceeded, 0 otherwise.",
        )
        .expect("valid gauge opts");
        registry
            .register(Box::new(pnl_breaker_halted.clone()))
            .expect("unique metric");

        let signal_queue_depth = IntGauge::new(
            "peanut_signal_queue_depth",
            "Current depth of the signal priority queue.",
        )
        .expect("valid gauge opts");
        registry
            .register(Box::new(signal_queue_depth.clone()))
            .expect("unique metric");

        let inventory_skew_bps = GaugeVec::new(
            Opts::new(
                "peanut_inventory_skew_bps",
                "Per-venue inventory skew expressed in basis points.",
            ),
            &["asset", "venue"],
        )
        .expect("valid gauge opts");
        registry
            .register(Box::new(inventory_skew_bps.clone()))
            .expect("unique metric");

        let execution_duration_seconds = HistogramVec::new(
            histogram_opts!(
                "peanut_execution_duration_seconds",
                "Executor::execute wall-clock duration.",
                EXEC_DURATION_BUCKETS.to_vec()
            ),
            &["state"],
        )
        .expect("valid histogram opts");
        registry
            .register(Box::new(execution_duration_seconds.clone()))
            .expect("unique metric");

        let leg1_fill_ratio = HistogramVec::new(
            histogram_opts!(
                "peanut_leg1_fill_ratio",
                "Fill ratio of leg1 (filled / requested).",
                FILL_RATIO_BUCKETS.to_vec()
            ),
            &["venue"],
        )
        .expect("valid histogram opts");
        registry
            .register(Box::new(leg1_fill_ratio.clone()))
            .expect("unique metric");

        let realized_pnl_usd = HistogramVec::new(
            histogram_opts!(
                "peanut_realized_pnl_usd",
                "Realised net PnL per execution, in USD equivalent.",
                PNL_BUCKETS.to_vec()
            ),
            &["state"],
        )
        .expect("valid histogram opts");
        registry
            .register(Box::new(realized_pnl_usd.clone()))
            .expect("unique metric");

        let flashbots_bundle_simulation_seconds = HistogramVec::new(
            histogram_opts!(
                "peanut_flashbots_bundle_simulation_seconds",
                "Flashbots eth_callBundle latency.",
                EXEC_DURATION_BUCKETS.to_vec()
            ),
            &["relay", "status"],
        )
        .expect("valid histogram opts");
        registry
            .register(Box::new(flashbots_bundle_simulation_seconds.clone()))
            .expect("unique metric");

        let flashbots_bundle_inclusion_blocks = HistogramVec::new(
            histogram_opts!(
                "peanut_flashbots_bundle_inclusion_blocks",
                "Blocks between target bundle submission and observed inclusion.",
                vec![0.0, 1.0, 2.0, 3.0, 5.0, 10.0]
            ),
            &["relay"],
        )
        .expect("valid histogram opts");
        registry
            .register(Box::new(flashbots_bundle_inclusion_blocks.clone()))
            .expect("unique metric");

        Self {
            registry,
            signals_generated_total,
            executions_total,
            breaker_trips_total,
            unwind_failures_total,
            signal_queue_drops_total,
            flashbots_simulations_total,
            flashbots_bundles_submitted_total,
            flashbots_bundles_included_total,
            flashbots_bundles_not_included_total,
            flashbots_relay_errors_total,
            breaker_open,
            pnl_breaker_halted,
            signal_queue_depth,
            inventory_skew_bps,
            execution_duration_seconds,
            leg1_fill_ratio,
            realized_pnl_usd,
            flashbots_bundle_simulation_seconds,
            flashbots_bundle_inclusion_blocks,
            enabled: true,
        }
    }

    /// Returns the Prometheus text-format snapshot.
    pub fn render(&self) -> Result<Vec<u8>, prometheus::Error> {
        let mut buf = Vec::with_capacity(4096);
        TextEncoder::new().encode(&self.registry.gather(), &mut buf)?;
        Ok(buf)
    }

    /// Records a completed execution: duration, terminal state counter,
    /// optional PnL. `leg1_fill_ratio` is recorded separately since not every
    /// execution reaches leg1 (e.g. Rejected at pre-flight).
    pub fn record_execution(&self, state: ExecutorState, duration_secs: f64, pnl_usd: Option<f64>) {
        let label = state.to_string();
        self.executions_total.with_label_values(&[&label]).inc();
        self.execution_duration_seconds
            .with_label_values(&[&label])
            .observe(duration_secs);
        if let Some(pnl) = pnl_usd
            && state.is_filled()
        {
            self.realized_pnl_usd
                .with_label_values(&[&label])
                .observe(pnl);
        }
    }

    /// Records leg1 fill ratio after leg1 completes. `venue` is "cex" or "dex".
    pub fn record_leg1_fill(&self, venue: &str, ratio: f64) {
        self.leg1_fill_ratio
            .with_label_values(&[venue])
            .observe(ratio);
    }

    /// Increments generator counter for a newly-emitted signal.
    pub fn record_signal_generated(&self, pair: &str, direction: &str) {
        self.signals_generated_total
            .with_label_values(&[pair, direction])
            .inc();
    }

    /// Updates the circuit-breaker open gauge. Also increments trip counter
    /// on 0 → 1 transition.
    pub fn set_breaker_open(&self, open: bool) {
        let new = if open { 1 } else { 0 };
        let prev = self.breaker_open.get();
        self.breaker_open.set(new);
        if prev == 0 && new == 1 {
            self.breaker_trips_total.inc();
        }
    }

    /// Updates the PnL breaker halted gauge.
    pub fn set_pnl_breaker_halted(&self, halted: bool) {
        self.pnl_breaker_halted.set(if halted { 1 } else { 0 });
    }

    pub fn record_unwind_failure(&self) {
        self.unwind_failures_total.inc();
    }

    pub fn set_queue_depth(&self, depth: i64) {
        self.signal_queue_depth.set(depth);
    }

    pub fn record_queue_drop(&self, reason: &str) {
        self.signal_queue_drops_total
            .with_label_values(&[reason])
            .inc();
    }

    pub fn record_flashbots_simulation(&self, relay: &str, status: &str, duration_secs: f64) {
        self.flashbots_simulations_total
            .with_label_values(&[status])
            .inc();
        self.flashbots_bundle_simulation_seconds
            .with_label_values(&[relay, status])
            .observe(duration_secs);
    }

    pub fn record_flashbots_bundle_submitted(&self, relay: &str) {
        self.flashbots_bundles_submitted_total
            .with_label_values(&[relay])
            .inc();
    }

    pub fn record_flashbots_bundle_included(&self, relay: &str, inclusion_blocks: u64) {
        self.flashbots_bundles_included_total
            .with_label_values(&[relay])
            .inc();
        self.flashbots_bundle_inclusion_blocks
            .with_label_values(&[relay])
            .observe(inclusion_blocks as f64);
    }

    pub fn record_flashbots_bundle_not_included(&self, relay: &str) {
        self.flashbots_bundles_not_included_total
            .with_label_values(&[relay])
            .inc();
    }

    pub fn record_flashbots_relay_error(&self, relay: &str, error: &str) {
        self.flashbots_relay_errors_total
            .with_label_values(&[relay, error])
            .inc();
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Global access
// ---------------------------------------------------------------------------

static GLOBAL: OnceLock<Arc<Metrics>> = OnceLock::new();

/// Initialises the process-wide metrics registry. Idempotent: subsequent calls
/// return the originally-installed handle, ignoring the new instance.
pub fn init_metrics(metrics: Metrics) -> Arc<Metrics> {
    let arc = Arc::new(metrics);
    match GLOBAL.set(arc.clone()) {
        Ok(()) => arc,
        Err(_) => GLOBAL.get().expect("already set").clone(),
    }
}

/// Returns the global metrics handle, or a fresh no-op facade if none was
/// initialised. Cheap on hot paths (Arc clone).
pub fn metrics_handle() -> Arc<Metrics> {
    if let Some(m) = GLOBAL.get() {
        return m.clone();
    }
    // Fallback: install a disabled stub on first miss so we don't keep
    // allocating. The stub still has working counters — they just live in a
    // throwaway registry that nobody scrapes.
    static FALLBACK: OnceLock<Arc<Metrics>> = OnceLock::new();
    FALLBACK
        .get_or_init(|| {
            let mut m = Metrics::new();
            m.enabled = false;
            Arc::new(m)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_expected_names() {
        let m = Metrics::new();
        m.record_execution(ExecutorState::Done, 0.15, Some(12.5));
        m.record_execution(ExecutorState::Rejected, 0.001, None);
        m.record_signal_generated("ETH/USDT", "BuyCexSellDex");
        m.set_breaker_open(true);
        m.set_queue_depth(3);
        m.record_flashbots_simulation("relay", "ok", 0.01);
        m.record_flashbots_bundle_submitted("relay");
        m.record_flashbots_bundle_included("relay", 1);
        m.record_flashbots_bundle_not_included("relay");
        m.record_flashbots_relay_error("relay", "send");

        let body = String::from_utf8(m.render().unwrap()).unwrap();
        for name in [
            "peanut_executions_total",
            "peanut_signals_generated_total",
            "peanut_breaker_open",
            "peanut_breaker_trips_total",
            "peanut_signal_queue_depth",
            "peanut_execution_duration_seconds",
            "peanut_realized_pnl_usd",
            "peanut_flashbots_simulations_total",
            "peanut_flashbots_bundles_submitted_total",
            "peanut_flashbots_bundles_included_total",
            "peanut_flashbots_bundles_not_included_total",
            "peanut_flashbots_bundle_simulation_seconds",
            "peanut_flashbots_bundle_inclusion_blocks",
            "peanut_flashbots_relay_errors_total",
        ] {
            assert!(body.contains(name), "missing metric: {name}\n{body}");
        }
        // Breaker trip counter must have incremented on the 0->1 transition.
        assert!(body.contains("peanut_breaker_trips_total 1"));
    }

    #[test]
    fn breaker_trip_counter_only_on_rising_edge() {
        let m = Metrics::new();
        m.set_breaker_open(true);
        m.set_breaker_open(true); // idempotent
        m.set_breaker_open(false);
        m.set_breaker_open(true); // second rising edge
        assert_eq!(m.breaker_trips_total.get(), 2);
    }

    #[test]
    fn pnl_only_recorded_for_filled_states() {
        let m = Metrics::new();
        m.record_execution(ExecutorState::Failed, 1.0, Some(-5.0));
        m.record_execution(ExecutorState::Done, 1.0, Some(-2.5));

        let body = String::from_utf8(m.render().unwrap()).unwrap();
        // Done observation present:
        assert!(body.contains("peanut_realized_pnl_usd_count{state=\"DONE\"} 1"));
        // Failed observation absent (no line with state=FAILED on pnl):
        assert!(!body.contains("peanut_realized_pnl_usd_count{state=\"FAILED\"}"));
    }

    #[test]
    fn fallback_handle_does_not_panic() {
        // Do NOT init global; confirm metrics_handle() still works for code
        // that fires observations in test environments.
        let h = metrics_handle();
        h.record_execution(ExecutorState::Done, 0.1, Some(1.0));
        h.set_queue_depth(0);
        // The fallback handle must be observable — if `enabled` is flipped
        // to `true` accidentally we want the test to notice.
        assert!(!h.enabled, "fallback handle should be disabled");
    }
}
