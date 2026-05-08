//! Executor state machine for coordinated CEX/DEX leg execution.
//!
//! Two leg orderings are supported:
//! - **CEX-first** (default): place CEX order, then DEX swap; on DEX failure we
//!   attempt to unwind the filled CEX leg back to flat.
//! - **DEX-first** (when `use_flashbots = true`): DEX bundle fails at zero
//!   cost, so we can try it first and only touch CEX after confirmation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{info, warn};

use ethers::types::U256;

use crate::core::types::{Address, BPS_SCALE};
use crate::exchange::client::ExchangeClient;
use crate::executor::dex_swapper::{self, DexSwapper, DexSwapperConfig, PairAddressBook};
use crate::executor::errors::ExecutorResult;
use crate::executor::reconcile::{PendingReconcile, ReconcileStore};
use crate::executor::recovery::{CircuitBreaker, ReplayProtection};
use crate::observability::{emit_event, metrics_handle};
use crate::strategy::fees::FeeStructure;
use crate::strategy::signal::{Direction, Signal};
use rust_decimal::prelude::ToPrimitive;

/// Lifecycle states of a single execution attempt.
///
/// State transitions are deterministic and enforced by [`Executor::execute`].
/// Terminal states are those for which [`is_terminal`](Self::is_terminal)
/// returns `true`; anything else is transient. See module docs for the full
/// graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutorState {
    /// Initial state before any work begins.
    Idle,
    /// Pre-flight checks (signal validity, replay, circuit breaker).
    Validating,
    /// Leg 1 order submitted, awaiting fill.
    Leg1Pending,
    /// Leg 1 confirmed filled.
    Leg1Filled,
    /// Leg 1 partially filled above the `partial_proceed_min_ratio` but
    /// below `min_fill_ratio`. Transient — the executor continues to leg 2
    /// at the *actual* filled size (rather than the requested size).
    Leg1Partial,
    /// Best-effort cancellation of a resting or partially-filled leg 1 order.
    Leg1Cancelling,
    /// Leg 2 order submitted, awaiting fill.
    Leg2Pending,
    /// Leg 2 DEX transaction mined on-chain but reverted (no fill).
    /// Terminal. Leg 1 was unwound (or attempted) before reaching this.
    Leg2Reverted,
    /// Leg 2 wait window expired without a definitive outcome.
    /// **Terminal without auto-unwind** — the transaction may still land,
    /// so operator must reconcile before flattening leg 1.
    Leg2Timeout,
    /// Both legs filled. Terminal. Profit/loss is determined by realised PnL.
    Done,
    /// Execution failed after a leg-2 problem; leg-1 unwind was attempted.
    /// Terminal. See [`ExecutionContext::error`].
    Failed,
    /// Unwinding a leg-1 position after a leg-2 reject/revert/error.
    Unwinding,
    /// Pre-flight or leg-1 rejection: no position was opened. Terminal.
    Rejected,
}

impl ExecutorState {
    /// Returns `true` when no further transitions are allowed.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Failed | Self::Rejected | Self::Leg2Reverted | Self::Leg2Timeout
        )
    }

    /// Returns `true` when both legs filled (regardless of PnL sign).
    pub fn is_filled(self) -> bool {
        matches!(self, Self::Done)
    }
}

impl std::fmt::Display for ExecutorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Idle => "IDLE",
            Self::Validating => "VALIDATING",
            Self::Leg1Pending => "LEG1_PENDING",
            Self::Leg1Filled => "LEG1_FILLED",
            Self::Leg1Partial => "LEG1_PARTIAL",
            Self::Leg1Cancelling => "LEG1_CANCELLING",
            Self::Leg2Pending => "LEG2_PENDING",
            Self::Leg2Reverted => "LEG2_REVERTED",
            Self::Leg2Timeout => "LEG2_TIMEOUT",
            Self::Done => "DONE",
            Self::Failed => "FAILED",
            Self::Unwinding => "UNWINDING",
            Self::Rejected => "REJECTED",
        };
        f.write_str(s)
    }
}

/// Definitive outcome of a single leg call, as reported by the backend.
///
/// Timeout is *not* a variant: timeout is an engine-level concept (the future
/// is dropped before the backend can return) and is handled separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegOutcome {
    /// Order accepted; `filled` carries the fill amount (may be partial).
    Accepted,
    /// Backend rejected the order before any fill occurred.
    Rejected,
    /// DEX transaction confirmed on-chain but reverted (no fill, gas spent).
    /// Not produced by CEX backends.
    Reverted,
}

/// Outcome returned by a single leg executor call.
#[derive(Debug, Clone)]
pub struct LegFill {
    /// Backend-reported outcome of the order.
    pub outcome: LegOutcome,
    /// Volume-weighted average fill price (zero when not filled).
    pub price: Decimal,
    /// Filled base-asset quantity (zero when not filled).
    pub filled: Decimal,
    /// Exchange order ID or transaction hash, when known.
    pub handle: Option<String>,
    /// Human-readable error description when `outcome != Accepted`.
    pub error: Option<String>,
}

/// Running state of one [`Executor::execute`] call.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    /// The signal being executed.
    pub signal: Signal,
    /// Current state in the lifecycle.
    pub state: ExecutorState,

    /// Venue label for leg 1 ("cex" or "dex").
    pub leg1_venue: &'static str,
    /// Exchange order ID or tx hash for leg 1.
    pub leg1_handle: Option<String>,
    /// Leg 1 fill price.
    pub leg1_fill_price: Option<Decimal>,
    /// Leg 1 filled quantity.
    pub leg1_fill_size: Option<Decimal>,

    /// Venue label for leg 2 ("cex" or "dex").
    pub leg2_venue: &'static str,
    /// Exchange order ID or tx hash for leg 2.
    pub leg2_handle: Option<String>,
    /// Leg 2 fill price.
    pub leg2_fill_price: Option<Decimal>,
    /// Leg 2 filled quantity.
    pub leg2_fill_size: Option<Decimal>,

    /// Monotonic start time.
    pub started_at: Instant,
    /// Monotonic finish time (set on terminal transition).
    pub finished_at: Option<Instant>,
    /// Realised net PnL after both legs (only for `Done`).
    pub actual_net_pnl: Option<Decimal>,
    /// Error description for `Failed` states.
    pub error: Option<String>,
}

impl ExecutionContext {
    fn new(signal: Signal) -> Self {
        Self {
            signal,
            state: ExecutorState::Idle,
            leg1_venue: "",
            leg1_handle: None,
            leg1_fill_price: None,
            leg1_fill_size: None,
            leg2_venue: "",
            leg2_handle: None,
            leg2_fill_price: None,
            leg2_fill_size: None,
            started_at: Instant::now(),
            finished_at: None,
            actual_net_pnl: None,
            error: None,
        }
    }

    /// Pre-flight / leg-1 rejection (no position opened).
    fn reject(mut self, msg: impl Into<String>) -> Self {
        self.state = ExecutorState::Rejected;
        self.error = Some(msg.into());
        self.finished_at = Some(Instant::now());
        self
    }

    /// Leg-2 failure after leg-1 was unwound (or unwind attempted).
    fn fail(mut self, msg: impl Into<String>) -> Self {
        self.state = ExecutorState::Failed;
        self.error = Some(msg.into());
        self.finished_at = Some(Instant::now());
        self
    }

    /// Leg-2 wait expired — tx may still land. Operator must reconcile.
    fn leg2_timeout(mut self, msg: impl Into<String>) -> Self {
        self.state = ExecutorState::Leg2Timeout;
        self.error = Some(msg.into());
        self.finished_at = Some(Instant::now());
        self
    }

    /// Leg-2 on-chain revert; leg-1 unwound (or unwind attempted).
    fn leg2_reverted(mut self, msg: impl Into<String>) -> Self {
        self.state = ExecutorState::Leg2Reverted;
        self.error = Some(msg.into());
        self.finished_at = Some(Instant::now());
        self
    }

    fn complete(mut self, pnl: Decimal) -> Self {
        self.actual_net_pnl = Some(pnl);
        self.state = ExecutorState::Done;
        self.finished_at = Some(Instant::now());
        self
    }

    pub fn is_profitable(&self) -> bool {
        self.actual_net_pnl
            .map(|pnl| pnl > Decimal::ZERO)
            .unwrap_or(false)
    }
}

/// Shared slot a [`LegExecutor`] writes the order handle into as soon as it
/// knows one. Passed into [`LegExecutor::execute_cex_tracked`] so the
/// Executor can still reach the handle if its outer `timeout(...)` wrapper
/// drops the submission future before it completes. The handle-to-sink
/// write MUST happen before any cancellable await inside the backend's
/// submission path.
pub type HandleSink = Arc<std::sync::OnceLock<String>>;

fn dex_pool_kind_label(pool_kind: dex_swapper::DexPoolKind) -> &'static str {
    match pool_kind {
        dex_swapper::DexPoolKind::V2 => "v2",
        dex_swapper::DexPoolKind::V3 => "v3",
    }
}

/// Outcome of [`LegExecutor::cancel_cex`]. Richer than a plain `Result`
/// because a cancel can race with a fill (the "race-on-cancel" case) —
/// callers need to branch on the outcome to decide whether to proceed to
/// leg 2, unwind, or escalate for manual review.
#[derive(Debug, Clone)]
pub enum CancelOutcome {
    /// Order cancelled cleanly; zero fills occurred.
    Cancelled,
    /// The exchange reported the order fully filled before the cancel
    /// landed. The returned [`LegFill`] is the actual fill; treat as
    /// `LEG1_FILLED` and continue to leg 2.
    RaceFilled(LegFill),
    /// Some filled before cancel, the rest was cancelled. Exposure
    /// remains — the Executor MUST unwind before returning.
    PartiallyFilled(LegFill),
    /// Cancel state is ambiguous (exchange timed out, RPC error, etc.).
    /// The Executor surfaces this as a manual-review terminal state so an
    /// operator can reconcile — we never assume a side.
    Unknown(String),
}

/// Abstraction over the two leg executions. Tests inject deterministic stubs;
/// production uses [`SimulatedLegs`] in `simulation_mode` or a future live impl.
#[async_trait]
pub trait LegExecutor: Send + Sync {
    /// Submits the CEX leg and returns a fill result.
    async fn execute_cex(&self, signal: &Signal, size: Decimal) -> ExecutorResult<LegFill>;

    /// Tracked variant of [`LegExecutor::execute_cex`]. Writes the order
    /// handle into `sink` as soon as it's known — typically after the
    /// `POST /order` roundtrip but before the fill-wait phase — so that a
    /// caller whose outer `timeout` fires can still issue
    /// [`LegExecutor::cancel_cex`] with a valid handle.
    ///
    /// Default impl is a no-op wrapper around `execute_cex` (the sink will
    /// only be written once the full fill is known, which defeats the
    /// purpose). Backends should override.
    async fn execute_cex_tracked(
        &self,
        signal: &Signal,
        size: Decimal,
        _sink: HandleSink,
    ) -> ExecutorResult<LegFill> {
        self.execute_cex(signal, size).await
    }

    /// Submits the DEX leg and returns a fill result.
    async fn execute_dex(&self, signal: &Signal, size: Decimal) -> ExecutorResult<LegFill>;

    async fn execute_dex_tracked(
        &self,
        signal: &Signal,
        size: Decimal,
        sink: HandleSink,
    ) -> ExecutorResult<LegFill> {
        let fill = self.execute_dex(signal, size).await?;
        if let Some(ref handle) = fill.handle {
            let _ = sink.set(handle.clone());
        }
        Ok(fill)
    }

    /// Best-effort cancellation of a resting / partially-filled CEX leg-1
    /// order. Used when leg-1 times out or partially fills below threshold.
    /// Default impl returns `Cancelled` (appropriate for IOC-only backends
    /// where auto-cancel happens server-side).
    async fn cancel_cex(&self, _handle: &str) -> ExecutorResult<CancelOutcome> {
        Ok(CancelOutcome::Cancelled)
    }

    async fn cancel_dex(
        &self,
        _handle: &str,
    ) -> ExecutorResult<dex_swapper::PendingSwapCancelOutcome> {
        Ok(dex_swapper::PendingSwapCancelOutcome::Unknown(
            "DEX cancellation is not supported by this executor".into(),
        ))
    }

    /// Market-flattens a stuck leg-1 position after a leg-2 failure.
    async fn unwind(&self, ctx: &ExecutionContext) -> ExecutorResult<()>;

    /// Flattens a specific position without a full [`ExecutionContext`].
    /// Used by the reconcile worker (S3) which only has
    /// `(pair, venue, direction, size)` recovered from persistence.
    /// Default impl synthesises a minimal context and delegates to
    /// [`LegExecutor::unwind`] so backends that don't care about the
    /// price hint work out-of-the-box.
    async fn unwind_position(
        &self,
        pair: &str,
        venue: &str,
        direction: Direction,
        size: Decimal,
    ) -> ExecutorResult<()> {
        use crate::strategy::signal::SignalParams;
        let params = SignalParams {
            pair: pair.to_string(),
            direction,
            cex_price: Decimal::ZERO, // caller's `unwind` impl may refresh.
            dex_price: Decimal::ZERO,
            spread_bps: Decimal::ZERO,
            size,
            expected_gross_pnl: Decimal::ZERO,
            expected_fees: Decimal::ZERO,
            expected_net_pnl: Decimal::ZERO,
            ttl: chrono::Duration::seconds(0),
            inventory_ok: true,
            within_limits: true,
        };
        let signal = Signal::new(params);
        let mut ctx = ExecutionContext::new(signal);
        ctx.leg1_venue = match venue {
            "cex" => "cex",
            "dex" => "dex",
            _ => "cex",
        };
        ctx.leg1_fill_size = Some(size);
        self.unwind(&ctx).await
    }
}

/// In-process, deterministic leg executor used for demos and tests.
#[derive(Debug, Default, Clone)]
pub struct SimulatedLegs {
    /// Artificial latency for the CEX leg.
    pub cex_latency: Duration,
    /// Artificial latency for the DEX leg.
    pub dex_latency: Duration,
    /// Behaviour of the CEX leg.
    pub cex_behaviour: LegBehaviour,
    /// Behaviour of the DEX leg.
    pub dex_behaviour: LegBehaviour,
    /// Outcome of a `cancel_cex` call. Lets tests cover all four race-
    /// on-cancel branches deterministically.
    pub cancel_behaviour: CancelBehaviour,
}

/// Outcome knob for [`SimulatedLegs::cancel_cex`]. Maps 1:1 to
/// [`CancelOutcome`] variants (with a size carried alongside the `Race*`
/// variants for the synthesised [`LegFill`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CancelBehaviour {
    /// Returns [`CancelOutcome::Cancelled`] (no fill happened).
    #[default]
    CleanCancel,
    /// Returns [`CancelOutcome::RaceFilled`] with a full-size synthetic
    /// fill at `signal.cex_price`.
    RaceFilled,
    /// Returns [`CancelOutcome::PartiallyFilled`] with a half-size fill.
    PartialRace,
    /// Returns [`CancelOutcome::Unknown`] — cancel state ambiguous.
    Unknown,
}

/// Failure-injection knob for [`SimulatedLegs`]. Tests use this to cover
/// every branch of the executor state machine deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LegBehaviour {
    /// Fill at (near-)market price.
    #[default]
    Fill,
    /// Return `LegOutcome::Rejected` immediately (no fill).
    Reject,
    /// Simulate on-chain revert: `LegOutcome::Reverted`, zero fill.
    /// Only meaningful for the DEX leg; CEX backends can't revert.
    Revert,
    /// Fill only 50% of the requested size (outcome = Accepted).
    PartialFill,
    /// Sleep beyond any reasonable timeout (5 minutes).
    Hang,
}

#[async_trait]
impl LegExecutor for SimulatedLegs {
    async fn execute_cex(&self, signal: &Signal, size: Decimal) -> ExecutorResult<LegFill> {
        run_simulated(self.cex_latency, self.cex_behaviour, signal.cex_price, size).await
    }

    async fn execute_cex_tracked(
        &self,
        signal: &Signal,
        size: Decimal,
        sink: HandleSink,
    ) -> ExecutorResult<LegFill> {
        // Populate the sink BEFORE the first cancellable await so a caller
        // whose outer timeout fires can still read the handle. We encode
        // price + size into the handle so `cancel_cex` can synthesise a
        // deterministic race-filled [`LegFill`] without extra shared state.
        let handle = format!("sim_cex:{}:{}", signal.cex_price, size);
        let _ = sink.set(handle);
        run_simulated(self.cex_latency, self.cex_behaviour, signal.cex_price, size).await
    }

    async fn execute_dex(&self, signal: &Signal, size: Decimal) -> ExecutorResult<LegFill> {
        run_simulated(self.dex_latency, self.dex_behaviour, signal.dex_price, size).await
    }

    async fn cancel_cex(&self, handle: &str) -> ExecutorResult<CancelOutcome> {
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Parse `sim_cex:{price}:{size}` to build realistic race-fills.
        let (price, size) = parse_sim_handle(handle).unwrap_or((Decimal::ZERO, Decimal::ZERO));
        Ok(match self.cancel_behaviour {
            CancelBehaviour::CleanCancel => CancelOutcome::Cancelled,
            CancelBehaviour::RaceFilled => CancelOutcome::RaceFilled(LegFill {
                outcome: LegOutcome::Accepted,
                price,
                filled: size,
                handle: Some(handle.to_string()),
                error: None,
            }),
            CancelBehaviour::PartialRace => CancelOutcome::PartiallyFilled(LegFill {
                outcome: LegOutcome::Accepted,
                price,
                filled: size / Decimal::TWO,
                handle: Some(handle.to_string()),
                error: None,
            }),
            CancelBehaviour::Unknown => CancelOutcome::Unknown("simulated ambiguous cancel".into()),
        })
    }

    async fn unwind(&self, _ctx: &ExecutionContext) -> ExecutorResult<()> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    }
}

/// Parses a `sim_cex:{price}:{size}` handle back into its components.
/// Used by [`SimulatedLegs::cancel_cex`] only.
fn parse_sim_handle(handle: &str) -> Option<(Decimal, Decimal)> {
    let rest = handle.strip_prefix("sim_cex:")?;
    let (p, s) = rest.split_once(':')?;
    Some((
        Decimal::from_str_exact(p).ok()?,
        Decimal::from_str_exact(s).ok()?,
    ))
}

async fn run_simulated(
    latency: Duration,
    behaviour: LegBehaviour,
    price: Decimal,
    size: Decimal,
) -> ExecutorResult<LegFill> {
    match behaviour {
        LegBehaviour::Hang => {
            tokio::time::sleep(Duration::from_secs(300)).await;
            unreachable!("test timeout should fire first")
        }
        LegBehaviour::Reject => Ok(LegFill {
            outcome: LegOutcome::Rejected,
            price: Decimal::ZERO,
            filled: Decimal::ZERO,
            handle: None,
            error: Some("simulated reject".into()),
        }),
        LegBehaviour::Revert => {
            tokio::time::sleep(latency).await;
            Ok(LegFill {
                outcome: LegOutcome::Reverted,
                price: Decimal::ZERO,
                filled: Decimal::ZERO,
                handle: Some("sim_tx_reverted".into()),
                error: Some("simulated on-chain revert".into()),
            })
        }
        LegBehaviour::PartialFill => {
            tokio::time::sleep(latency).await;
            Ok(LegFill {
                outcome: LegOutcome::Accepted,
                price,
                filled: size / Decimal::TWO,
                handle: Some("sim_partial".into()),
                error: None,
            })
        }
        LegBehaviour::Fill => {
            tokio::time::sleep(latency).await;
            Ok(LegFill {
                outcome: LegOutcome::Accepted,
                price,
                filled: size,
                handle: Some("sim_fill".into()),
                error: None,
            })
        }
    }
}

/// Live leg executor backed by [`ExchangeClient`] on the CEX side and,
/// optionally, a [`DexSwapper`] on the DEX side. When no swapper is
/// configured, `execute_dex` / `unwind` return
/// [`ExecutorError::NotImplemented`] — callers that intend to trade
/// on-chain must inject a [`UniswapV2Swapper`] via
/// [`LiveLegs::with_dex`].
#[derive(Debug)]
pub struct LiveLegs {
    exchange: Arc<ExchangeClient>,
    dex: Option<Arc<dyn DexSwapper>>,
    address_book: Option<Arc<PairAddressBook>>,
    dex_config: DexSwapperConfig,
    pending_dex_submissions: Arc<Mutex<HashMap<String, dex_swapper::SwapSubmission>>>,
    /// Address that receives DEX output tokens (typically the wallet owner).
    recipient: Option<Address>,
}

impl LiveLegs {
    /// Constructs a live leg executor with only the CEX side wired.
    pub fn new(exchange: Arc<ExchangeClient>) -> Self {
        Self {
            exchange,
            dex: None,
            address_book: None,
            dex_config: DexSwapperConfig::default(),
            pending_dex_submissions: Arc::new(Mutex::new(HashMap::new())),
            recipient: None,
        }
    }

    /// Attaches a live DEX backend. `recipient` is the address that receives
    /// the swap output (used for Transfer-log parsing).
    pub fn with_dex(
        mut self,
        dex: Arc<dyn DexSwapper>,
        address_book: Arc<PairAddressBook>,
        dex_config: DexSwapperConfig,
        recipient: Address,
    ) -> Self {
        self.dex = Some(dex);
        self.address_book = Some(address_book);
        self.dex_config = dex_config;
        self.recipient = Some(recipient);
        self
    }

    async fn unwind_dex(&self, ctx: &ExecutionContext) -> ExecutorResult<()> {
        let Some(filled) = ctx.leg1_fill_size else {
            return Ok(());
        };
        if filled <= Decimal::ZERO {
            return Ok(());
        }

        let mut reverse_signal = ctx.signal.clone();
        reverse_signal.direction = match ctx.signal.direction {
            Direction::BuyCexSellDex => Direction::BuyDexSellCex,
            Direction::BuyDexSellCex => Direction::BuyCexSellDex,
        };

        match self
            .execute_dex_inner(&reverse_signal, filled, None)
            .await?
        {
            LegFill {
                outcome: LegOutcome::Accepted,
                filled: reverse_filled,
                ..
            } if self.meets_unwind_fill(reverse_filled, filled) => Ok(()),
            LegFill {
                outcome,
                filled: reverse_filled,
                error,
                ..
            } => Err(crate::executor::errors::ExecutorError::InvalidSignal(
                format!(
                    "DEX unwind did not fully fill: outcome={outcome:?}, filled={reverse_filled}, requested={filled}, error={}",
                    error.unwrap_or_default()
                ),
            )),
        }
    }

    fn meets_unwind_fill(&self, filled: Decimal, requested: Decimal) -> bool {
        if requested <= Decimal::ZERO {
            return false;
        }
        let allowed_bps = BPS_SCALE.saturating_sub(self.dex_config.slippage_bps);
        filled >= requested * Decimal::from(allowed_bps) / Decimal::from(BPS_SCALE)
    }

    async fn execute_dex_inner(
        &self,
        signal: &Signal,
        size: Decimal,
        sink: Option<HandleSink>,
    ) -> ExecutorResult<LegFill> {
        let Some(ref dex) = self.dex else {
            return Err(crate::executor::errors::ExecutorError::NotImplemented(
                "LiveLegs: DEX swapper not configured (call LiveLegs::with_dex)",
            ));
        };
        let Some(ref book) = self.address_book else {
            return Err(crate::executor::errors::ExecutorError::NotImplemented(
                "LiveLegs: address book not configured",
            ));
        };
        let Some(ref recipient) = self.recipient else {
            return Err(crate::executor::errors::ExecutorError::NotImplemented(
                "LiveLegs: recipient address not configured",
            ));
        };

        let tokens = book.get(&signal.pair).ok_or_else(|| {
            crate::executor::errors::ExecutorError::InvalidSignal(format!(
                "no address-book entry for pair {}",
                signal.pair
            ))
        })?;

        let (token_in, token_out, decimals_in, decimals_out) = match signal.direction {
            Direction::BuyCexSellDex => (
                &tokens.base,
                &tokens.quote,
                tokens.base_decimals,
                tokens.quote_decimals,
            ),
            Direction::BuyDexSellCex => (
                &tokens.quote,
                &tokens.base,
                tokens.quote_decimals,
                tokens.base_decimals,
            ),
        };

        let amount_in_units: Decimal = match signal.direction {
            Direction::BuyCexSellDex => size,
            Direction::BuyDexSellCex => size * signal.dex_price,
        };
        let amount_in = decimal_to_u256_scaled(amount_in_units, decimals_in)
            .map_err(crate::executor::errors::ExecutorError::InvalidSignal)?;

        let expected_out_units: Decimal = match signal.direction {
            Direction::BuyCexSellDex => size * signal.dex_price,
            Direction::BuyDexSellCex => size,
        };
        let expected_out = decimal_to_u256_scaled(expected_out_units, decimals_out)
            .map_err(crate::executor::errors::ExecutorError::InvalidSignal)?;

        let min_out = dex_swapper::apply_slippage(expected_out, self.dex_config.slippage_bps);

        match dex
            .quote_exact_input_for_pair(tokens, token_in, token_out, amount_in)
            .await
        {
            Ok(Some(fresh_out)) if fresh_out < min_out => {
                return Ok(LegFill {
                    outcome: LegOutcome::Rejected,
                    price: Decimal::ZERO,
                    filled: Decimal::ZERO,
                    handle: None,
                    error: Some(format!(
                        "DEX preflight quote below min_out: fresh_out={fresh_out}, min_out={min_out}"
                    )),
                });
            }
            Ok(_) => {}
            Err(e) => {
                return Ok(LegFill {
                    outcome: LegOutcome::Rejected,
                    price: Decimal::ZERO,
                    filled: Decimal::ZERO,
                    handle: None,
                    error: Some(format!("DEX preflight quote failed: {e}")),
                });
            }
        }

        let submission = match dex
            .submit_swap_for_pair(tokens, token_in, token_out, amount_in, min_out, recipient)
            .await
        {
            Ok(submission) => submission,
            Err(dex_swapper::SwapperError::Reverted(tx)) => {
                return Ok(LegFill {
                    outcome: LegOutcome::Reverted,
                    price: Decimal::ZERO,
                    filled: Decimal::ZERO,
                    handle: Some(tx),
                    error: Some("on-chain revert".into()),
                });
            }
            Err(e) => {
                return Ok(LegFill {
                    outcome: LegOutcome::Rejected,
                    price: Decimal::ZERO,
                    filled: Decimal::ZERO,
                    handle: None,
                    error: Some(e.to_string()),
                });
            }
        };

        let submission_tx_hash = submission.tx_hash.clone();
        self.pending_dex_submissions
            .lock()
            .await
            .insert(submission_tx_hash.clone(), submission.clone());
        if let Some(sink) = sink {
            let _ = sink.set(submission_tx_hash.clone());
        }

        let wait_result = dex.wait_swap(submission).await;
        self.pending_dex_submissions
            .lock()
            .await
            .remove(&submission_tx_hash);

        match wait_result {
            Ok(result) => {
                let filled = u256_to_decimal_scaled(result.amount_out, decimals_out);
                let price = if matches!(signal.direction, Direction::BuyCexSellDex) {
                    if size > Decimal::ZERO {
                        filled / size
                    } else {
                        signal.dex_price
                    }
                } else if filled > Decimal::ZERO {
                    amount_in_units / filled
                } else {
                    signal.dex_price
                };
                let filled_base = match signal.direction {
                    Direction::BuyCexSellDex => size,
                    Direction::BuyDexSellCex => filled,
                };
                Ok(LegFill {
                    outcome: LegOutcome::Accepted,
                    price,
                    filled: filled_base,
                    handle: Some(result.tx_hash),
                    error: None,
                })
            }
            Err(dex_swapper::SwapperError::Reverted(tx)) => Ok(LegFill {
                outcome: LegOutcome::Reverted,
                price: Decimal::ZERO,
                filled: Decimal::ZERO,
                handle: Some(tx),
                error: Some("on-chain revert".into()),
            }),
            Err(e) => Ok(LegFill {
                outcome: LegOutcome::Rejected,
                price: Decimal::ZERO,
                filled: Decimal::ZERO,
                handle: None,
                error: Some(e.to_string()),
            }),
        }
    }
}

#[async_trait]
impl LegExecutor for LiveLegs {
    async fn execute_cex(&self, signal: &Signal, size: Decimal) -> ExecutorResult<LegFill> {
        use rust_decimal::prelude::ToPrimitive;
        let side = match signal.direction {
            Direction::BuyCexSellDex => "buy",
            Direction::BuyDexSellCex => "sell",
        };
        let price = cex_ioc_cross_price(side, signal.cex_price);
        // Convert to f64 for the exchange API. A None conversion would mean the
        // Decimal doesn't fit in f64 — never silently send a zero order; abort.
        let amount_f = size.to_f64().ok_or_else(|| {
            crate::executor::errors::ExecutorError::InvalidSignal(format!(
                "size {size} cannot be represented as f64"
            ))
        })?;
        let price_f = price.to_f64().ok_or_else(|| {
            crate::executor::errors::ExecutorError::InvalidSignal(format!(
                "price {price} cannot be represented as f64"
            ))
        })?;
        let result = self
            .exchange
            .create_limit_ioc_order(&signal.pair, side, amount_f, price_f)
            .await?;
        // Binance-flavoured status mapping. `FILLED` and `PARTIALLY_FILLED`
        // are both "Accepted" from our state machine's perspective; the
        // engine applies the min-fill threshold above this layer.
        let status_upper = result.status.to_uppercase();
        let outcome = match status_upper.as_str() {
            "FILLED" | "PARTIALLY_FILLED" => LegOutcome::Accepted,
            _ => LegOutcome::Rejected,
        };
        Ok(LegFill {
            outcome,
            price: result.avg_fill_price,
            filled: result.amount_filled,
            handle: Some(result.id),
            error: if outcome == LegOutcome::Accepted {
                None
            } else {
                Some(result.status)
            },
        })
    }

    async fn execute_dex(&self, signal: &Signal, size: Decimal) -> ExecutorResult<LegFill> {
        self.execute_dex_inner(signal, size, None).await
    }

    async fn execute_dex_tracked(
        &self,
        signal: &Signal,
        size: Decimal,
        sink: HandleSink,
    ) -> ExecutorResult<LegFill> {
        self.execute_dex_inner(signal, size, Some(sink)).await
    }

    async fn cancel_dex(
        &self,
        handle: &str,
    ) -> ExecutorResult<dex_swapper::PendingSwapCancelOutcome> {
        let Some(ref dex) = self.dex else {
            return Ok(dex_swapper::PendingSwapCancelOutcome::Unknown(
                "LiveLegs: DEX swapper not configured".into(),
            ));
        };
        let submission = self.pending_dex_submissions.lock().await.remove(handle);
        let Some(submission) = submission else {
            return Ok(dex_swapper::PendingSwapCancelOutcome::Unknown(
                "no pending DEX submission metadata for handle".into(),
            ));
        };
        let metrics = metrics_handle();
        metrics.record_dex_pending_timeout(
            dex_pool_kind_label(submission.pool_kind),
            submission.private_bundle.is_some(),
        );
        metrics.record_dex_cancel_attempt(if submission.private_bundle.is_some() {
            "private"
        } else {
            "public"
        });
        emit_event(
            "dex_pending_timeout",
            json!({
                "tx_hash": submission.tx_hash,
                "nonce": submission.nonce,
                "pool_kind": dex_pool_kind_label(submission.pool_kind),
                "private": submission.private_bundle.is_some(),
            }),
        );
        dex.cancel_pending_swap(&submission)
            .await
            .map_err(|e| crate::executor::errors::ExecutorError::InvalidSignal(e.to_string()))
    }

    async fn unwind(&self, ctx: &ExecutionContext) -> ExecutorResult<()> {
        if ctx.leg1_venue == "dex" {
            return self.unwind_dex(ctx).await;
        }
        if ctx.leg1_venue != "cex" {
            return Ok(());
        }
        let Some(filled) = ctx.leg1_fill_size else {
            return Ok(());
        };
        if filled <= Decimal::ZERO {
            return Ok(());
        }
        // Send the opposite side as a market (IOC) order to close out.
        use rust_decimal::prelude::ToPrimitive;
        let reverse_side = match ctx.signal.direction {
            Direction::BuyCexSellDex => "sell",
            Direction::BuyDexSellCex => "buy",
        };
        // Cross-the-book by 10 bps on the unwind side to maximise fill
        // probability; we are prioritising exposure closure over price.
        let buffer = match reverse_side {
            "sell" => Decimal::new(999, 3), // 0.999
            _ => Decimal::new(1001, 3),     // 1.001
        };
        let price = ctx.signal.cex_price * buffer;
        let amount_f = filled.to_f64().ok_or_else(|| {
            crate::executor::errors::ExecutorError::InvalidSignal(format!(
                "unwind size {filled} cannot be represented as f64"
            ))
        })?;
        let price_f = price.to_f64().ok_or_else(|| {
            crate::executor::errors::ExecutorError::InvalidSignal(format!(
                "unwind price {price} cannot be represented as f64"
            ))
        })?;
        self.exchange
            .create_limit_ioc_order(&ctx.signal.pair, reverse_side, amount_f, price_f)
            .await?;
        Ok(())
    }
}

/// Multiplies a `Decimal` by `10^decimals` and converts to [`U256`], rounding
/// truncation-style toward zero. Returns an error string on overflow or when
/// the value is negative.
fn decimal_to_u256_scaled(value: Decimal, decimals: u8) -> Result<U256, String> {
    if value < Decimal::ZERO {
        return Err(format!("negative amount: {value}"));
    }
    let scale = Decimal::from(10u64.pow(decimals as u32));
    let scaled = (value * scale).trunc();
    let as_str = scaled.to_string();
    U256::from_dec_str(&as_str).map_err(|e| format!("scale overflow ({as_str}): {e}"))
}

/// Inverse of [`decimal_to_u256_scaled`]. Lossy for values that exceed the
/// Decimal 28-digit precision window.
fn u256_to_decimal_scaled(value: U256, decimals: u8) -> Decimal {
    let s = value.to_string();
    let raw = Decimal::from_str_exact(&s).unwrap_or(Decimal::ZERO);
    let scale = Decimal::from(10u64.pow(decimals as u32));
    raw / scale
}

fn cex_ioc_cross_price(side: &str, reference_price: Decimal) -> Decimal {
    match side {
        "sell" => reference_price * Decimal::new(999, 3),
        _ => reference_price * Decimal::new(1001, 3),
    }
}

/// Tunables for the executor.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Timeout for the CEX leg.
    pub leg1_timeout: Duration,
    /// Timeout for the DEX leg.
    pub leg2_timeout: Duration,
    /// Minimum fraction of requested size that counts as a **full** fill.
    /// At or above this ratio the executor takes the normal path, sending
    /// leg 2 at the originally requested size.
    pub min_fill_ratio: Decimal,
    /// Ratio boundary below `min_fill_ratio` at or above which the
    /// executor **proceeds with leg 2 at the actually-filled size**
    /// (LEG1_PARTIAL decisional state). Between `partial_dust_ratio` and
    /// this value the executor aborts and unwinds.
    ///
    /// Default equals `min_fill_ratio`, which disables the proceed-reduced
    /// branch — operators must opt in by lowering it (e.g. to `0.5`).
    pub partial_proceed_min_ratio: Decimal,
    /// Below this ratio the partial fill is treated as dust: the remainder
    /// is cancelled and the execution is rejected without any unwind
    /// (the dust is left on-book for venue-side house-keeping). Default
    /// equals `min_fill_ratio`, which — combined with the default value
    /// of `partial_proceed_min_ratio` — means every partial below full
    /// fill falls through to the dust/reject path, preserving pre-S9
    /// behaviour. Operators enable the abort-and-unwind branch by
    /// lowering this below `partial_proceed_min_ratio`.
    pub partial_dust_ratio: Decimal,
    /// When `true`, DEX goes first (safe only with Flashbots bundles).
    pub use_flashbots: bool,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        let min_fill = Decimal::new(8, 1); // 0.8
        Self {
            leg1_timeout: Duration::from_secs(10),
            leg2_timeout: Duration::from_secs(60),
            min_fill_ratio: min_fill,
            partial_proceed_min_ratio: min_fill,
            partial_dust_ratio: min_fill,
            use_flashbots: true,
        }
    }
}

/// Classification of a partial-fill outcome — derived from the fill ratio
/// plus the three thresholds on [`ExecutorConfig`]. Drives the partial-fill
/// decision in the executor (S9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillClass {
    /// `ratio >= min_fill_ratio` — treat as a full fill.
    Full,
    /// `partial_proceed_min_ratio <= ratio < min_fill_ratio` — continue to
    /// leg 2 at the actual filled size (LEG1_PARTIAL decisional state).
    ProceedReduced,
    /// `partial_dust_ratio <= ratio < partial_proceed_min_ratio` — abort
    /// and unwind the partial exposure.
    AbortUnwind,
    /// `ratio < partial_dust_ratio` — treat as zero; cancel remainder and
    /// reject without unwind.
    Dust,
}

/// Coordinates leg execution with circuit-breaker and replay-protection guards.
pub struct Executor {
    legs: Arc<dyn LegExecutor>,
    config: ExecutorConfig,
    circuit_breaker: Arc<Mutex<CircuitBreaker>>,
    replay_protection: Arc<Mutex<ReplayProtection>>,
    /// Optional SQLite-backed store for `LEG2_TIMEOUT` entries awaiting
    /// reconciliation. When present, a timed-out leg-2 with a known
    /// [`ExecutionContext::leg2_handle`] is enqueued for the
    /// [`ReconcileWorker`](super::reconcile::ReconcileWorker) to poll.
    reconcile_store: Option<Arc<ReconcileStore>>,
    /// Fee model used by [`Self::calc_pnl`] to compute realised PnL net of
    /// CEX taker + DEX swap + gas costs. Defaults to [`FeeStructure::default`]
    /// (10/30 bps + $5 gas) and can be overridden via
    /// [`Self::with_fees`].
    fees: FeeStructure,
}

impl Executor {
    /// Creates an executor with the given leg backend + config. Replay
    /// protection runs in-memory only; use [`Executor::with_replay`] to back
    /// it with a persistent journal.
    pub fn new(legs: Arc<dyn LegExecutor>, config: ExecutorConfig) -> Self {
        Self::with_replay(legs, config, ReplayProtection::default())
    }

    /// Creates an executor with a caller-supplied [`ReplayProtection`] — use
    /// this to inject a SQLite-backed guard via
    /// [`ReplayProtection::with_journal`].
    pub fn with_replay(
        legs: Arc<dyn LegExecutor>,
        config: ExecutorConfig,
        replay: ReplayProtection,
    ) -> Self {
        Self {
            legs,
            config,
            circuit_breaker: Arc::new(Mutex::new(CircuitBreaker::default())),
            replay_protection: Arc::new(Mutex::new(replay)),
            reconcile_store: None,
            fees: FeeStructure::default(),
        }
    }

    /// Overrides the default fee model used for realised PnL accounting.
    /// Chainable with [`Self::with_replay`] / [`Self::with_reconcile_store`].
    pub fn with_fees(mut self, fees: FeeStructure) -> Self {
        self.fees = fees;
        self
    }

    /// Attaches a [`ReconcileStore`]. When set, `LEG2_TIMEOUT` events that
    /// carry a known `leg2_handle` (on-chain tx hash) are pushed into the
    /// store for a background [`ReconcileWorker`](super::reconcile::ReconcileWorker)
    /// to poll.
    ///
    /// Chainable with [`Executor::with_replay`].
    pub fn with_reconcile_store(mut self, store: Arc<ReconcileStore>) -> Self {
        self.reconcile_store = Some(store);
        self
    }

    /// Shared circuit breaker; main loop can inspect it outside execution.
    pub fn circuit_breaker(&self) -> Arc<Mutex<CircuitBreaker>> {
        Arc::clone(&self.circuit_breaker)
    }

    /// Handle to the reconcile store if one was configured.
    pub fn reconcile_store(&self) -> Option<Arc<ReconcileStore>> {
        self.reconcile_store.clone()
    }

    /// Pushes a [`PendingReconcile`] entry into the store, if one is
    /// configured. No-op otherwise. Errors are logged and swallowed — a
    /// failed reconcile push must never prevent the execution flow from
    /// completing.
    async fn maybe_push_reconcile(&self, ctx: &ExecutionContext) {
        let Some(store) = self.reconcile_store.as_ref() else {
            return;
        };
        let Some(tx_hash) = ctx.leg2_handle.as_ref() else {
            metrics_handle().record_reconcile_entry("missing_tx_hash");
            // No on-chain handle captured — nothing the reconciler can
            // poll. See the `leg2_timeout` docs for the follow-up on
            // two-phase swap submission needed to capture this reliably.
            warn!(
                signal = %ctx.signal.signal_id,
                "leg2_timeout without tx_hash; reconcile store not updated"
            );
            return;
        };
        let Some(leg1_size) = ctx.leg1_fill_size else {
            metrics_handle().record_reconcile_entry("missing_leg1_fill");
            return;
        };
        let entry = PendingReconcile {
            signal_id: ctx.signal.signal_id.clone(),
            tx_hash: tx_hash.clone(),
            pair: ctx.signal.pair.clone(),
            direction: ctx.signal.direction,
            leg1_venue: ctx.leg1_venue.to_string(),
            leg1_fill_size: leg1_size.to_string(),
            started_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        };
        // A4: use the async wrapper so the SQLite write is offloaded to
        // the blocking thread pool instead of stalling this runtime worker.
        let signal_id_log = entry.signal_id.clone();
        let tx_hash_log = entry.tx_hash.clone();
        if let Err(e) = store.add_async(entry).await {
            metrics_handle().record_reconcile_entry("push_failed");
            emit_event(
                "reconcile_enqueue_failed",
                json!({
                    "signal_id": signal_id_log,
                    "tx_hash": tx_hash_log,
                    "reason": "push_failed",
                    "error": e.to_string(),
                }),
            );
            tracing::error!(
                signal = %signal_id_log,
                error = %e,
                "reconcile store push failed"
            );
        } else {
            metrics_handle().record_reconcile_entry("leg2_timeout");
            emit_event(
                "reconcile_enqueued",
                json!({
                    "signal_id": signal_id_log,
                    "tx_hash": tx_hash_log,
                    "reason": "leg2_timeout",
                }),
            );
            info!(
                signal = %signal_id_log,
                tx = %tx_hash_log,
                "leg2_timeout: enqueued for reconciliation"
            );
        }
    }

    /// Executes a signal end-to-end, routing through the configured leg order.
    pub async fn execute(&self, signal: Signal) -> ExecutionContext {
        let mut ctx = ExecutionContext::new(signal);

        // Pre-flight: these are REJECTED (no position opened), not FAILED.
        if self.circuit_breaker.lock().await.is_open() {
            return ctx.reject("Circuit breaker open");
        }
        if self
            .replay_protection
            .lock()
            .await
            .is_duplicate(&ctx.signal)
        {
            return ctx.reject("Duplicate signal");
        }

        ctx.state = ExecutorState::Validating;
        if !ctx.signal.is_valid() {
            return ctx.reject("Signal invalid");
        }

        let ctx = if self.config.use_flashbots {
            self.execute_dex_first(ctx).await
        } else {
            self.execute_cex_first(ctx).await
        };

        // Mark replay + feed breaker. Rejections from pre-flight are neutral:
        // they don't count toward failure streak (the breaker already said
        // no, or a duplicate was filtered — no new failure occurred).
        self.replay_protection
            .lock()
            .await
            .mark_executed(&ctx.signal);
        let mut cb = self.circuit_breaker.lock().await;
        match ctx.state {
            ExecutorState::Done if ctx.actual_net_pnl.is_some_and(|pnl| pnl < Decimal::ZERO) => {
                cb.record_failure()
            }
            ExecutorState::Done => cb.record_success(),
            ExecutorState::Rejected => { /* neutral */ }
            _ => cb.record_failure(),
        }
        let breaker_open = cb.is_open();
        drop(cb);

        Self::emit_execution_metrics(&ctx, breaker_open);

        ctx
    }

    /// Pushes the terminal execution into Prometheus. Runs outside any lock
    /// (metrics crate is internally synchronised via atomics).
    fn emit_execution_metrics(ctx: &ExecutionContext, breaker_open: bool) {
        let metrics = metrics_handle();
        metrics.set_breaker_open(breaker_open);

        let duration = ctx
            .finished_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(ctx.started_at)
            .as_secs_f64();
        let pnl = ctx.actual_net_pnl.and_then(|d| d.to_f64());
        metrics.record_execution(ctx.state, duration, pnl);

        // Fill ratio (only meaningful when leg1 actually attempted + we know
        // requested size). Skip for pre-flight rejections where leg1 never ran.
        if let Some(filled) = ctx.leg1_fill_size
            && ctx.signal.size > Decimal::ZERO
            && !ctx.leg1_venue.is_empty()
        {
            let ratio = (filled / ctx.signal.size)
                .to_f64()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            metrics.record_leg1_fill(ctx.leg1_venue, ratio);
        }
    }

    async fn execute_cex_first(&self, mut ctx: ExecutionContext) -> ExecutionContext {
        ctx.state = ExecutorState::Leg1Pending;
        ctx.leg1_venue = "cex";

        // Shared sink the backend writes the order handle into as soon as
        // it's known. If our outer `timeout` fires, we read this to issue
        // `cancel_cex(handle)` and branch on the race-on-cancel outcome.
        let handle_sink: HandleSink = Arc::new(std::sync::OnceLock::new());
        let leg1 = match timeout(
            self.config.leg1_timeout,
            self.legs
                .execute_cex_tracked(&ctx.signal, ctx.signal.size, Arc::clone(&handle_sink)),
        )
        .await
        {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => return ctx.reject(format!("CEX error: {e}")),
            Err(_) => {
                // Timeout fired. If the backend populated the sink, we
                // can issue a proper cancel with a rich outcome; if not
                // (e.g. error before the first await), fall back to the
                // old IOC-auto-cancel assumption.
                let captured = handle_sink.get().cloned();
                let Some(handle) = captured else {
                    ctx.state = ExecutorState::Leg1Cancelling;
                    warn!(
                        signal = %ctx.signal.signal_id,
                        "CEX leg1 timeout; no handle captured, relying on IOC semantics"
                    );
                    return ctx.reject("CEX timeout before fill confirmation");
                };
                ctx.state = ExecutorState::Leg1Cancelling;
                ctx.leg1_handle = Some(handle.clone());
                match self.legs.cancel_cex(&handle).await {
                    Ok(CancelOutcome::Cancelled) => {
                        return ctx.reject("CEX timeout; order cancelled cleanly");
                    }
                    Ok(CancelOutcome::RaceFilled(fill)) => {
                        // Cancel raced with a fill — promote to the normal
                        // leg1-filled path and continue to leg 2.
                        warn!(
                            signal = %ctx.signal.signal_id,
                            handle = %handle,
                            filled = %fill.filled,
                            "race-on-cancel: leg1 filled despite cancel attempt"
                        );
                        fill
                    }
                    Ok(CancelOutcome::PartiallyFilled(fill)) => {
                        // Apply the S9 classifier so a "big enough"
                        // race-partial can proceed with leg 2 instead of
                        // being unconditionally unwound.
                        match self.classify_fill(fill.filled, ctx.signal.size) {
                            FillClass::Full | FillClass::ProceedReduced => {
                                warn!(
                                    signal = %ctx.signal.signal_id,
                                    handle = %handle,
                                    filled = %fill.filled,
                                    "race-on-cancel: leg1 partial above proceed threshold; continuing"
                                );
                                fill
                            }
                            FillClass::AbortUnwind | FillClass::Dust => {
                                warn!(
                                    signal = %ctx.signal.signal_id,
                                    handle = %handle,
                                    filled = %fill.filled,
                                    "race-on-cancel: leg1 partially filled; unwinding"
                                );
                                ctx.leg1_fill_price = Some(fill.price);
                                ctx.leg1_fill_size = Some(fill.filled);
                                ctx.state = ExecutorState::Unwinding;
                                let unwind_status = self.log_unwind(&ctx).await;
                                return ctx.fail(format!(
                                    "race-on-cancel partial fill - {unwind_status}"
                                ));
                            }
                        }
                    }
                    Ok(CancelOutcome::Unknown(msg)) => {
                        tracing::error!(
                            signal = %ctx.signal.signal_id,
                            handle = %handle,
                            reason = %msg,
                            "cancel state unknown — manual review required"
                        );
                        return ctx.fail(format!("cancel state unknown: {msg}"));
                    }
                    Err(e) => {
                        return ctx.fail(format!("cancel_cex error: {e}"));
                    }
                }
            }
        };

        ctx.leg1_handle = leg1.handle.clone();

        match leg1.outcome {
            LegOutcome::Rejected => {
                return ctx.reject(format!("CEX rejected: {}", leg1.error.unwrap_or_default()));
            }
            LegOutcome::Reverted => {
                // CEX shouldn't produce this — log and treat as reject.
                warn!(
                    signal = %ctx.signal.signal_id,
                    "CEX backend returned Reverted outcome (unexpected)"
                );
                return ctx.reject("CEX reported revert (unexpected)");
            }
            LegOutcome::Accepted => {}
        }

        // --- S9: classify the leg1 fill into Full / ProceedReduced /
        //         AbortUnwind / Dust and branch accordingly.
        let class = self.classify_fill(leg1.filled, ctx.signal.size);
        match class {
            FillClass::Full => {
                ctx.leg1_fill_price = Some(leg1.price);
                ctx.leg1_fill_size = Some(leg1.filled);
                ctx.state = ExecutorState::Leg1Filled;
            }
            FillClass::ProceedReduced => {
                // S9: proceed with leg 2 at the actually-filled size.
                info!(
                    signal = %ctx.signal.signal_id,
                    requested = %ctx.signal.size,
                    filled = %leg1.filled,
                    "LEG1_PARTIAL: proceeding with reduced size"
                );
                ctx.leg1_fill_price = Some(leg1.price);
                ctx.leg1_fill_size = Some(leg1.filled);
                ctx.state = ExecutorState::Leg1Partial;
                // Best-effort cancel of the unfilled remainder to free
                // exchange margin; ignore the outcome — we're proceeding
                // with what we've got regardless.
                if let Some(ref h) = leg1.handle {
                    let _ = self.legs.cancel_cex(h).await;
                }
            }
            FillClass::AbortUnwind => {
                // Partial exposure we're unwilling to trade through —
                // flatten the fill via the unwind path before returning.
                warn!(
                    signal = %ctx.signal.signal_id,
                    requested = %ctx.signal.size,
                    filled = %leg1.filled,
                    "LEG1_PARTIAL: aborting and unwinding partial exposure"
                );
                if let Some(ref h) = leg1.handle {
                    ctx.state = ExecutorState::Leg1Cancelling;
                    if let Err(e) = self.legs.cancel_cex(h).await {
                        tracing::error!(
                            signal = %ctx.signal.signal_id,
                            handle = %h,
                            error = %e,
                            "CANCEL FAILED after partial leg1 fill"
                        );
                    }
                }
                ctx.leg1_fill_price = Some(leg1.price);
                ctx.leg1_fill_size = Some(leg1.filled);
                ctx.state = ExecutorState::Unwinding;
                let requested = ctx.signal.size;
                let filled = leg1.filled;
                let unwind_status = self.log_unwind(&ctx).await;
                return ctx.fail(format!(
                    "leg1 partial {filled} / {requested} - {unwind_status}"
                ));
            }
            FillClass::Dust => {
                // Below dust threshold: cancel rest, reject without
                // unwind. Preserves pre-S9 behaviour when `dust_ratio=0`.
                if let Some(ref h) = leg1.handle {
                    ctx.state = ExecutorState::Leg1Cancelling;
                    if let Err(e) = self.legs.cancel_cex(h).await {
                        tracing::error!(
                            signal = %ctx.signal.signal_id,
                            handle = %h,
                            error = %e,
                            "CANCEL FAILED after partial leg1 fill"
                        );
                    }
                }
                let requested = ctx.signal.size;
                return ctx.reject(format!(
                    "CEX partial fill below threshold ({} / {})",
                    leg1.filled, requested
                ));
            }
        }

        // --- Leg 2: DEX ---
        ctx.state = ExecutorState::Leg2Pending;
        ctx.leg2_venue = "dex";

        let leg1_filled = leg1.filled;
        let leg2_handle_sink: HandleSink = Arc::new(std::sync::OnceLock::new());
        let leg2_res = timeout(
            self.config.leg2_timeout,
            self.legs
                .execute_dex_tracked(&ctx.signal, leg1_filled, Arc::clone(&leg2_handle_sink)),
        )
        .await;

        match leg2_res {
            Ok(Ok(leg2)) => self.finalize_cex_first_leg2(ctx, leg2).await,
            Ok(Err(e)) => {
                ctx.state = ExecutorState::Unwinding;
                let unwind_status = self.log_unwind(&ctx).await;
                ctx.fail(format!("DEX error - {unwind_status}: {e}"))
            }
            Err(_) => {
                ctx.leg2_handle = leg2_handle_sink.get().cloned();
                metrics_handle().record_leg_outcome("dex", "leg2", "timeout");
                if ctx.leg2_handle.is_none() {
                    metrics_handle().record_dex_pending_timeout("unknown", false);
                }
                if let Some(handle) = ctx.leg2_handle.clone() {
                    match self.legs.cancel_dex(&handle).await {
                        Ok(dex_swapper::PendingSwapCancelOutcome::Cancelled { cancel_tx_hash }) => {
                            metrics_handle().record_dex_cancel_outcome("cancelled");
                            emit_event(
                                "dex_cancel_outcome",
                                json!({
                                    "signal_id": ctx.signal.signal_id,
                                    "tx_hash": handle,
                                    "outcome": "cancelled",
                                    "cancel_tx_hash": cancel_tx_hash,
                                }),
                            );
                            tracing::warn!(
                                signal = %ctx.signal.signal_id,
                                tx = %handle,
                                cancel_tx = %cancel_tx_hash,
                                "DEX leg2 timeout cancelled by same-nonce replacement; unwinding leg1"
                            );
                            ctx.state = ExecutorState::Unwinding;
                            let unwind_status = self.log_unwind(&ctx).await;
                            return ctx.fail(format!(
                                "DEX leg2 timeout cancelled by replacement {cancel_tx_hash} - {unwind_status}"
                            ));
                        }
                        Ok(dex_swapper::PendingSwapCancelOutcome::OriginalMined {
                            tx_hash,
                            success,
                        }) => {
                            if success {
                                metrics_handle()
                                    .record_dex_cancel_outcome("original_mined_success");
                                emit_event(
                                    "dex_cancel_outcome",
                                    json!({
                                        "signal_id": ctx.signal.signal_id,
                                        "tx_hash": tx_hash,
                                        "outcome": "original_mined_success",
                                    }),
                                );
                                tracing::warn!(
                                    signal = %ctx.signal.signal_id,
                                    tx = %tx_hash,
                                    "DEX leg2 timeout resolved: original tx mined before cancellation"
                                );
                                let timed_out = ctx.leg2_timeout(
                                    "DEX leg2 timeout but original tx mined during cancel attempt; reconcile fill manually",
                                );
                                self.maybe_push_reconcile(&timed_out).await;
                                return timed_out;
                            }
                            metrics_handle().record_dex_cancel_outcome("original_mined_reverted");
                            emit_event(
                                "dex_cancel_outcome",
                                json!({
                                    "signal_id": ctx.signal.signal_id,
                                    "tx_hash": tx_hash,
                                    "outcome": "original_mined_reverted",
                                }),
                            );
                            ctx.state = ExecutorState::Unwinding;
                            let unwind_status = self.log_unwind(&ctx).await;
                            return ctx.leg2_reverted(format!(
                                "DEX tx reverted during timeout cancel check - {unwind_status}"
                            ));
                        }
                        Ok(dex_swapper::PendingSwapCancelOutcome::Unknown(reason)) => {
                            metrics_handle().record_dex_cancel_outcome("unknown");
                            emit_event(
                                "dex_cancel_outcome",
                                json!({
                                    "signal_id": ctx.signal.signal_id,
                                    "tx_hash": handle,
                                    "outcome": "unknown",
                                    "reason": reason,
                                }),
                            );
                            tracing::warn!(
                                signal = %ctx.signal.signal_id,
                                tx = %handle,
                                reason = %reason,
                                "DEX leg2 timeout cancellation inconclusive"
                            );
                        }
                        Err(e) => {
                            metrics_handle().record_dex_cancel_outcome("error");
                            emit_event(
                                "dex_cancel_outcome",
                                json!({
                                    "signal_id": ctx.signal.signal_id,
                                    "tx_hash": handle,
                                    "outcome": "error",
                                    "error": e.to_string(),
                                }),
                            );
                            tracing::error!(
                                signal = %ctx.signal.signal_id,
                                tx = %handle,
                                error = %e,
                                "DEX leg2 timeout cancellation failed"
                            );
                        }
                    }
                }
                tracing::error!(
                    signal = %ctx.signal.signal_id,
                    leg1_handle = ?ctx.leg1_handle,
                    leg1_filled = %leg1_filled,
                    "DEX leg2 timeout - tx state unknown, manual reconcile required"
                );
                let timed_out = ctx
                    .leg2_timeout("DEX leg2 timeout; tx may still land, manual reconcile required");
                self.maybe_push_reconcile(&timed_out).await;
                timed_out
            }
        }
    }

    async fn finalize_cex_first_leg2(
        &self,
        mut ctx: ExecutionContext,
        leg2: LegFill,
    ) -> ExecutionContext {
        ctx.leg2_handle = leg2.handle.clone();

        match leg2.outcome {
            LegOutcome::Accepted => {
                if !self.meets_min_fill(leg2.filled, ctx.leg1_fill_size.unwrap_or_default()) {
                    // Partial leg2 — unwind leg1 to flatten.
                    ctx.state = ExecutorState::Unwinding;
                    let unwind_status = self.log_unwind(&ctx).await;
                    return ctx.fail(format!("DEX partial below threshold - {unwind_status}"));
                }
                ctx.leg2_fill_price = Some(leg2.price);
                ctx.leg2_fill_size = Some(leg2.filled);
                let pnl = self.calc_pnl(&ctx);
                let done = ctx.complete(pnl);
                info!(
                    signal = %done.signal.signal_id,
                    state = %done.state,
                    pnl = %pnl,
                    "execution complete (cex-first)"
                );
                done
            }
            LegOutcome::Reverted => {
                // On-chain revert = deterministic no-fill on leg2. Leg1 still
                // exposed; unwind it.
                ctx.state = ExecutorState::Unwinding;
                let unwind_status = self.log_unwind(&ctx).await;
                ctx.leg2_reverted(format!(
                    "DEX tx reverted on-chain - {unwind_status}: {}",
                    leg2.error.unwrap_or_default()
                ))
            }
            LegOutcome::Rejected => {
                ctx.state = ExecutorState::Unwinding;
                let unwind_status = self.log_unwind(&ctx).await;
                ctx.fail(format!(
                    "DEX rejected - {unwind_status}: {}",
                    leg2.error.unwrap_or_default()
                ))
            }
        }
    }

    async fn execute_dex_first(&self, mut ctx: ExecutionContext) -> ExecutionContext {
        ctx.state = ExecutorState::Leg1Pending;
        ctx.leg1_venue = "dex";

        let leg1 = match timeout(
            self.config.leg2_timeout,
            self.legs.execute_dex(&ctx.signal, ctx.signal.size),
        )
        .await
        {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => return ctx.reject(format!("DEX error: {e}")),
            Err(_) => {
                // Flashbots bundle with no confirmation within window:
                // treat as not-landed (bundles fail atomically).
                return ctx.reject("DEX leg1 timeout (Flashbots bundle assumed not landed)");
            }
        };

        ctx.leg1_handle = leg1.handle.clone();

        match leg1.outcome {
            LegOutcome::Rejected | LegOutcome::Reverted => {
                // Flashbots: bundle either lands atomically or does nothing.
                // No exposure, no unwind needed.
                return ctx.reject(format!(
                    "DEX leg1 not executed (no cost via Flashbots): {}",
                    leg1.error.unwrap_or_default()
                ));
            }
            LegOutcome::Accepted => {}
        }

        if !self.meets_min_fill(leg1.filled, ctx.signal.size) {
            // Bundle landed but only partial — this is effectively a sandwich
            // issue. Nothing to cancel (tx already mined). Abort without
            // starting leg2; leg1 partial exposure remains on-chain.
            let requested = ctx.signal.size;
            return ctx.reject(format!(
                "DEX partial fill below threshold ({} / {})",
                leg1.filled, requested
            ));
        }

        ctx.leg1_fill_price = Some(leg1.price);
        ctx.leg1_fill_size = Some(leg1.filled);
        ctx.state = ExecutorState::Leg1Filled;

        // --- Leg 2: CEX ---
        ctx.state = ExecutorState::Leg2Pending;
        ctx.leg2_venue = "cex";

        let leg1_filled = leg1.filled;
        let leg2_res = timeout(
            self.config.leg1_timeout,
            self.legs.execute_cex(&ctx.signal, leg1_filled),
        )
        .await;

        match leg2_res {
            Ok(Ok(leg2)) => self.finalize_dex_first_leg2(ctx, leg2).await,
            Ok(Err(e)) => {
                ctx.state = ExecutorState::Unwinding;
                let unwind_status = self.log_unwind(&ctx).await;
                ctx.fail(format!("CEX error after DEX - {unwind_status}: {e}"))
            }
            Err(_) => {
                // CEX leg2 timeout: the order may have been placed but we
                // didn't see the response. Treat similarly to the DEX case:
                // don't auto-unwind (unwind would also place a CEX order,
                // potentially racing the first one).
                tracing::error!(
                    signal = %ctx.signal.signal_id,
                    leg1_handle = ?ctx.leg1_handle,
                    leg1_filled = %leg1_filled,
                    "CEX leg2 timeout - order state unknown, manual reconcile required"
                );
                ctx.leg2_timeout("CEX leg2 timeout; order state unknown, manual reconcile required")
            }
        }
    }

    async fn finalize_dex_first_leg2(
        &self,
        mut ctx: ExecutionContext,
        leg2: LegFill,
    ) -> ExecutionContext {
        ctx.leg2_handle = leg2.handle.clone();

        match leg2.outcome {
            LegOutcome::Accepted => {
                if !self.meets_min_fill(leg2.filled, ctx.leg1_fill_size.unwrap_or_default()) {
                    ctx.state = ExecutorState::Unwinding;
                    let unwind_status = self.log_unwind(&ctx).await;
                    return ctx.fail(format!("CEX partial below threshold - {unwind_status}"));
                }
                ctx.leg2_fill_price = Some(leg2.price);
                ctx.leg2_fill_size = Some(leg2.filled);
                let pnl = self.calc_pnl(&ctx);
                let done = ctx.complete(pnl);
                info!(
                    signal = %done.signal.signal_id,
                    state = %done.state,
                    pnl = %pnl,
                    "execution complete (dex-first)"
                );
                done
            }
            LegOutcome::Rejected | LegOutcome::Reverted => {
                ctx.state = ExecutorState::Unwinding;
                let unwind_status = self.log_unwind(&ctx).await;
                ctx.fail(format!(
                    "CEX {} after DEX - {unwind_status}: {}",
                    if leg2.outcome == LegOutcome::Reverted {
                        "reverted"
                    } else {
                        "rejected"
                    },
                    leg2.error.unwrap_or_default()
                ))
            }
        }
    }

    /// Runs the leg-executor's unwind routine and logs the outcome.
    ///
    /// Returns a short status string suitable for embedding in the outer
    /// failure message, so operators can tell at a glance whether the arb
    /// was flattened cleanly or the position is still open.
    async fn log_unwind(&self, ctx: &ExecutionContext) -> &'static str {
        match self.legs.unwind(ctx).await {
            Ok(()) => "unwound",
            Err(e) => {
                tracing::error!(
                    signal = %ctx.signal.signal_id,
                    leg1_venue = ctx.leg1_venue,
                    leg1_filled = ?ctx.leg1_fill_size,
                    error = %e,
                    "UNWIND FAILED - position left open"
                );
                metrics_handle().record_unwind_failure();
                "UNWIND_FAILED"
            }
        }
    }

    fn meets_min_fill(&self, filled: Decimal, requested: Decimal) -> bool {
        matches!(self.classify_fill(filled, requested), FillClass::Full)
    }

    /// Classifies a fill ratio against the three thresholds on
    /// [`ExecutorConfig`] (S9). Zero-requested always → `Dust`.
    fn classify_fill(&self, filled: Decimal, requested: Decimal) -> FillClass {
        if requested <= Decimal::ZERO {
            return FillClass::Dust;
        }
        let ratio = filled / requested;
        if ratio >= self.config.min_fill_ratio {
            FillClass::Full
        } else if ratio >= self.config.partial_proceed_min_ratio {
            FillClass::ProceedReduced
        } else if ratio >= self.config.partial_dust_ratio {
            FillClass::AbortUnwind
        } else {
            FillClass::Dust
        }
    }

    fn calc_pnl(&self, ctx: &ExecutionContext) -> Decimal {
        let (leg1_price, leg1_size, leg2_price) =
            match (ctx.leg1_fill_price, ctx.leg1_fill_size, ctx.leg2_fill_price) {
                (Some(p1), Some(s1), Some(p2)) => (p1, s1, p2),
                _ => {
                    warn!("calc_pnl called with missing leg data");
                    return Decimal::ZERO;
                }
            };
        // Map leg1/leg2 prices back to cex/dex, since leg order depends on
        // config (`use_flashbots`). This keeps the PnL math agnostic to the
        // execution ordering.
        let (cex_price, dex_price) = if ctx.leg1_venue == "cex" {
            (leg1_price, leg2_price)
        } else {
            (leg2_price, leg1_price)
        };
        let gross = match ctx.signal.direction {
            Direction::BuyCexSellDex => (dex_price - cex_price) * leg1_size,
            Direction::BuyDexSellCex => (cex_price - dex_price) * leg1_size,
        };
        let trade_value = leg1_size * cex_price;
        let bps = Decimal::from(BPS_SCALE);
        let fees = self.fees.total_fee_bps(trade_value) / bps * trade_value;
        gross - fees
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::signal::{Direction, SignalParams};

    fn mk_signal(score: Decimal) -> Signal {
        let mut s = Signal::new(SignalParams {
            pair: "ETH/USDT".into(),
            direction: Direction::BuyCexSellDex,
            cex_price: Decimal::from(2000),
            dex_price: Decimal::from(2020),
            spread_bps: Decimal::from(100),
            size: Decimal::ONE,
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

    fn cex_first() -> ExecutorConfig {
        ExecutorConfig {
            use_flashbots: false,
            leg1_timeout: Duration::from_millis(200),
            leg2_timeout: Duration::from_millis(200),
            ..ExecutorConfig::default()
        }
    }

    #[test]
    fn cex_ioc_cross_price_crosses_correct_side() {
        let reference = Decimal::from(100);
        assert_eq!(cex_ioc_cross_price("buy", reference), Decimal::new(1001, 1));
        assert_eq!(cex_ioc_cross_price("sell", reference), Decimal::new(999, 1));
    }

    #[tokio::test]
    async fn executes_successfully_done_with_profit() {
        let legs = Arc::new(SimulatedLegs::default());
        let ex = Executor::new(legs, cex_first());
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        // cex=2000, dex=2020, size=1 -> gross=20.
        // Default FeeStructure: 10 + 30 bps + $5 gas/$2000 = 65 bps
        // -> fees = 2000 * 0.0065 = $13 -> net = $7 > 0.
        assert_eq!(ctx.state, ExecutorState::Done);
        assert!(ctx.state.is_filled());
        assert!(ctx.state.is_terminal());
        assert!(ctx.actual_net_pnl.unwrap() > Decimal::ZERO);
        assert_eq!(ctx.leg1_venue, "cex");
        assert_eq!(ctx.leg2_venue, "dex");
    }

    #[tokio::test]
    async fn with_fees_override_shifts_pnl_into_loss() {
        // Same signal that yields +$7 under the default fee model should flip
        // to a loss when we inject a heavier bespoke FeeStructure (200 bps +
        // $50 gas on $2000 notional = 450 bps fees = $90 > $20 gross).
        let legs = Arc::new(SimulatedLegs::default());
        let heavy = FeeStructure {
            cex_taker_bps: Decimal::from(50),
            dex_swap_bps: Decimal::from(150),
            gas_cost_usd: Decimal::from(50),
        };
        let ex = Executor::new(legs, cex_first()).with_fees(heavy);
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Done);
        assert!(ctx.actual_net_pnl.unwrap() < Decimal::ZERO);
    }

    #[tokio::test]
    async fn done_when_spread_covered_by_fees() {
        // Build a signal whose gross = fees, so net PnL is zero -> Done.
        let mut signal = mk_signal(Decimal::from(80));
        signal.cex_price = Decimal::from(2000);
        signal.dex_price = Decimal::from(2000); // zero gross
        let legs = Arc::new(SimulatedLegs::default());
        let ex = Executor::new(legs, cex_first());
        let ctx = ex.execute(signal).await;
        assert_eq!(ctx.state, ExecutorState::Done);
        assert!(ctx.actual_net_pnl.unwrap() <= Decimal::ZERO);
    }

    #[tokio::test]
    async fn cex_leg1_timeout_rejects() {
        let legs = Arc::new(SimulatedLegs {
            cex_behaviour: LegBehaviour::Hang,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first());
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Rejected);
        // With S4 race-on-cancel, a clean cancel produces this message.
        // The default `CleanCancel` behaviour preserves REJECTED semantics.
        assert!(
            ctx.error.as_deref().unwrap_or("").contains("CEX timeout"),
            "expected CEX timeout reject, got: {:?}",
            ctx.error
        );
        // We DID capture the handle via the sink (proof: it's in ctx).
        assert!(ctx.leg1_handle.is_some());
    }

    // ---- S4 race-on-cancel branches -----------------------------------

    #[tokio::test]
    async fn leg1_timeout_race_filled_continues_to_leg2() {
        // Backend "hangs" but cancel races with a fill — result should
        // be Done via the normal path (leg2 then fills as usual).
        let legs = Arc::new(SimulatedLegs {
            cex_behaviour: LegBehaviour::Hang,
            cancel_behaviour: CancelBehaviour::RaceFilled,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first());
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert!(
            ctx.state.is_filled(),
            "expected race-filled to proceed to terminal filled state, got {}",
            ctx.state
        );
        assert!(ctx.leg1_fill_size.is_some());
    }

    #[tokio::test]
    async fn leg1_timeout_partially_filled_unwinds_and_fails() {
        let legs = Arc::new(SimulatedLegs {
            cex_behaviour: LegBehaviour::Hang,
            cancel_behaviour: CancelBehaviour::PartialRace,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first());
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Failed);
        assert!(
            ctx.error
                .as_deref()
                .unwrap_or("")
                .contains("race-on-cancel partial")
        );
        // Partial fill was recorded so the unwind ran over the right size.
        assert!(ctx.leg1_fill_size.is_some());
    }

    #[tokio::test]
    async fn leg1_timeout_unknown_cancel_fails_for_manual_review() {
        let legs = Arc::new(SimulatedLegs {
            cex_behaviour: LegBehaviour::Hang,
            cancel_behaviour: CancelBehaviour::Unknown,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first());
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Failed);
        assert!(
            ctx.error
                .as_deref()
                .unwrap_or("")
                .contains("cancel state unknown")
        );
    }

    #[tokio::test]
    async fn default_leg_executor_cancel_returns_cancelled() {
        // Regression: the default blanket impl of `cancel_cex` must return
        // `CancelOutcome::Cancelled` so IOC-only backends keep their
        // pre-S4 behaviour.
        struct MinimalLegs;
        #[async_trait::async_trait]
        impl LegExecutor for MinimalLegs {
            async fn execute_cex(
                &self,
                _signal: &Signal,
                _size: Decimal,
            ) -> ExecutorResult<LegFill> {
                unreachable!()
            }
            async fn execute_dex(
                &self,
                _signal: &Signal,
                _size: Decimal,
            ) -> ExecutorResult<LegFill> {
                unreachable!()
            }
            async fn unwind(&self, _ctx: &ExecutionContext) -> ExecutorResult<()> {
                Ok(())
            }
        }
        let legs = MinimalLegs;
        let outcome = legs.cancel_cex("anything").await.unwrap();
        assert!(matches!(outcome, CancelOutcome::Cancelled));
    }

    // ---- S9 LEG1_PARTIAL decisional state -----------------------------

    fn cex_first_with_proceed_min(proceed_min: &str, dust: &str) -> ExecutorConfig {
        ExecutorConfig {
            use_flashbots: false,
            leg1_timeout: Duration::from_millis(200),
            leg2_timeout: Duration::from_millis(200),
            partial_proceed_min_ratio: Decimal::from_str_exact(proceed_min).unwrap(),
            partial_dust_ratio: Decimal::from_str_exact(dust).unwrap(),
            ..ExecutorConfig::default()
        }
    }

    #[test]
    fn fill_class_default_config_is_pre_s9_compatible() {
        // With the default config (proceed=min_fill, dust=min_fill), any
        // fill below `min_fill_ratio` is classified as Dust → REJECTED
        // without unwind. That preserves pre-S9 behaviour.
        let legs = Arc::new(SimulatedLegs::default());
        let ex = Executor::new(legs, ExecutorConfig::default());
        assert_eq!(
            ex.classify_fill(Decimal::ONE, Decimal::ONE),
            FillClass::Full
        );
        assert_eq!(
            ex.classify_fill(Decimal::new(5, 1), Decimal::ONE),
            FillClass::Dust
        );
        assert_eq!(
            ex.classify_fill(Decimal::ZERO, Decimal::ONE),
            FillClass::Dust
        );
    }

    #[test]
    fn fill_class_opt_in_proceed_reduced_branch() {
        // min_fill=0.8, proceed=0.5, dust=0.1 → 0.5..0.8 is ProceedReduced.
        let legs = Arc::new(SimulatedLegs::default());
        let ex = Executor::new(legs, cex_first_with_proceed_min("0.5", "0.1"));
        // Just above proceed threshold.
        assert_eq!(
            ex.classify_fill(Decimal::new(5, 1), Decimal::ONE),
            FillClass::ProceedReduced
        );
        // Just below proceed but above dust.
        assert_eq!(
            ex.classify_fill(Decimal::new(3, 1), Decimal::ONE),
            FillClass::AbortUnwind
        );
        // Below dust.
        assert_eq!(
            ex.classify_fill(Decimal::new(5, 2), Decimal::ONE),
            FillClass::Dust
        );
    }

    #[tokio::test]
    async fn leg1_partial_proceed_reduced_runs_leg2_and_completes() {
        // PartialFill simulated leg1 = 50% of requested. With proceed_min=0.5,
        // the executor enters LEG1_PARTIAL and runs leg2 at the reduced
        // size → should still reach a terminal filled state.
        let legs = Arc::new(SimulatedLegs {
            cex_behaviour: LegBehaviour::PartialFill,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first_with_proceed_min("0.5", "0.1"));
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert!(
            ctx.state.is_filled(),
            "expected DONE after LEG1_PARTIAL proceed, got {}",
            ctx.state
        );
        // Leg 2 fill size must match the reduced leg 1 fill, not the
        // originally requested size.
        assert_eq!(ctx.leg1_fill_size, Some(Decimal::new(5, 1))); // 0.5
        assert_eq!(ctx.leg2_fill_size, Some(Decimal::new(5, 1)));
    }

    #[tokio::test]
    async fn leg1_partial_abort_unwind_fails_with_unwind() {
        // Set thresholds so 0.5 falls into AbortUnwind window:
        // min_fill=0.8, proceed=0.7, dust=0.2 → 0.2..0.7 = AbortUnwind.
        let legs = Arc::new(SimulatedLegs {
            cex_behaviour: LegBehaviour::PartialFill,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first_with_proceed_min("0.7", "0.2"));
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Failed);
        assert!(
            ctx.error.as_deref().unwrap_or("").contains("leg1 partial"),
            "got error: {:?}",
            ctx.error
        );
    }

    #[test]
    fn leg1_partial_display_is_uppercase_underscore() {
        assert_eq!(ExecutorState::Leg1Partial.to_string(), "LEG1_PARTIAL");
        // Non-terminal — execution continues from here.
        assert!(!ExecutorState::Leg1Partial.is_terminal());
    }

    #[tokio::test]
    async fn leg2_reject_triggers_unwind_and_failed() {
        let legs = Arc::new(SimulatedLegs {
            dex_behaviour: LegBehaviour::Reject,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first());
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Failed);
        assert!(ctx.error.as_deref().unwrap_or("").contains("unwound"));
    }

    #[tokio::test]
    async fn leg2_revert_transitions_to_leg2_reverted() {
        let legs = Arc::new(SimulatedLegs {
            dex_behaviour: LegBehaviour::Revert,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first());
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Leg2Reverted);
        assert!(ctx.state.is_terminal());
        assert!(ctx.error.as_deref().unwrap_or("").contains("reverted"));
    }

    #[tokio::test]
    async fn leg2_timeout_without_tx_hash_skips_reconcile_push() {
        // Legacy/non-tracked DEX executors may still fail to surface a tx_hash
        // before timeout; in that case `maybe_push_reconcile` must skip.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::executor::reconcile::ReconcileStore::open(dir.path().join("r.db")).unwrap(),
        );
        let legs = Arc::new(SimulatedLegs {
            dex_behaviour: LegBehaviour::Hang,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first()).with_reconcile_store(Arc::clone(&store));
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Leg2Timeout);
        // Store must be empty: no tx_hash -> no push.
        assert!(store.list_pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn leg2_timeout_with_tracked_tx_hash_pushes_reconcile() {
        #[derive(Debug)]
        struct TwoPhaseTimeoutLegs;

        #[async_trait::async_trait]
        impl LegExecutor for TwoPhaseTimeoutLegs {
            async fn execute_cex(&self, signal: &Signal, size: Decimal) -> ExecutorResult<LegFill> {
                Ok(LegFill {
                    outcome: LegOutcome::Accepted,
                    price: signal.cex_price,
                    filled: size,
                    handle: Some("cex-order-1".into()),
                    error: None,
                })
            }

            async fn execute_dex(
                &self,
                _signal: &Signal,
                _size: Decimal,
            ) -> ExecutorResult<LegFill> {
                unreachable!("executor should use execute_dex_tracked for cex-first leg2")
            }

            async fn execute_dex_tracked(
                &self,
                _signal: &Signal,
                _size: Decimal,
                sink: HandleSink,
            ) -> ExecutorResult<LegFill> {
                let _ = sink.set("0xtwophase".into());
                tokio::time::sleep(Duration::from_secs(300)).await;
                unreachable!("outer timeout should fire first")
            }

            async fn unwind(&self, _ctx: &ExecutionContext) -> ExecutorResult<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::executor::reconcile::ReconcileStore::open(dir.path().join("r.db")).unwrap(),
        );
        let cfg = ExecutorConfig {
            use_flashbots: false,
            leg1_timeout: Duration::from_millis(200),
            leg2_timeout: Duration::from_millis(20),
            ..ExecutorConfig::default()
        };
        let ex = Executor::new(Arc::new(TwoPhaseTimeoutLegs), cfg)
            .with_reconcile_store(Arc::clone(&store));

        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Leg2Timeout);
        assert_eq!(ctx.leg2_handle.as_deref(), Some("0xtwophase"));

        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tx_hash, "0xtwophase");
        assert_eq!(pending[0].leg1_fill_size, "1");
    }

    #[tokio::test]
    async fn leg2_timeout_cancelled_replacement_unwinds_leg1() {
        #[derive(Debug)]
        struct CancelledDexTimeoutLegs {
            unwinds: Arc<Mutex<u32>>,
        }

        #[async_trait::async_trait]
        impl LegExecutor for CancelledDexTimeoutLegs {
            async fn execute_cex(&self, signal: &Signal, size: Decimal) -> ExecutorResult<LegFill> {
                Ok(LegFill {
                    outcome: LegOutcome::Accepted,
                    price: signal.cex_price,
                    filled: size,
                    handle: Some("cex-order-1".into()),
                    error: None,
                })
            }

            async fn execute_dex(
                &self,
                _signal: &Signal,
                _size: Decimal,
            ) -> ExecutorResult<LegFill> {
                unreachable!("executor should use execute_dex_tracked for cex-first leg2")
            }

            async fn execute_dex_tracked(
                &self,
                _signal: &Signal,
                _size: Decimal,
                sink: HandleSink,
            ) -> ExecutorResult<LegFill> {
                let _ = sink.set("0xtimeout".into());
                tokio::time::sleep(Duration::from_secs(300)).await;
                unreachable!("outer timeout should fire first")
            }

            async fn cancel_dex(
                &self,
                handle: &str,
            ) -> ExecutorResult<dex_swapper::PendingSwapCancelOutcome> {
                assert_eq!(handle, "0xtimeout");
                Ok(dex_swapper::PendingSwapCancelOutcome::Cancelled {
                    cancel_tx_hash: "0xcancel".into(),
                })
            }

            async fn unwind(&self, _ctx: &ExecutionContext) -> ExecutorResult<()> {
                *self.unwinds.lock().await += 1;
                Ok(())
            }
        }

        let unwinds = Arc::new(Mutex::new(0));
        let cfg = ExecutorConfig {
            use_flashbots: false,
            leg1_timeout: Duration::from_millis(200),
            leg2_timeout: Duration::from_millis(20),
            ..ExecutorConfig::default()
        };
        let ex = Executor::new(
            Arc::new(CancelledDexTimeoutLegs {
                unwinds: Arc::clone(&unwinds),
            }),
            cfg,
        );

        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Failed);
        assert_eq!(ctx.leg2_handle.as_deref(), Some("0xtimeout"));
        assert!(ctx.error.as_deref().unwrap_or("").contains("0xcancel"));
        assert_eq!(*unwinds.lock().await, 1);
    }

    #[derive(Debug)]
    struct TimeoutCancelBranchLegs {
        outcome: dex_swapper::PendingSwapCancelOutcome,
        unwinds: Arc<Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl LegExecutor for TimeoutCancelBranchLegs {
        async fn execute_cex(&self, signal: &Signal, size: Decimal) -> ExecutorResult<LegFill> {
            Ok(LegFill {
                outcome: LegOutcome::Accepted,
                price: signal.cex_price,
                filled: size,
                handle: Some("cex-order-1".into()),
                error: None,
            })
        }

        async fn execute_dex(&self, _signal: &Signal, _size: Decimal) -> ExecutorResult<LegFill> {
            unreachable!("executor should use execute_dex_tracked for cex-first leg2")
        }

        async fn execute_dex_tracked(
            &self,
            _signal: &Signal,
            _size: Decimal,
            sink: HandleSink,
        ) -> ExecutorResult<LegFill> {
            let _ = sink.set("0xtimeout-branch".into());
            tokio::time::sleep(Duration::from_secs(300)).await;
            unreachable!("outer timeout should fire first")
        }

        async fn cancel_dex(
            &self,
            handle: &str,
        ) -> ExecutorResult<dex_swapper::PendingSwapCancelOutcome> {
            assert_eq!(handle, "0xtimeout-branch");
            Ok(self.outcome.clone())
        }

        async fn unwind(&self, _ctx: &ExecutionContext) -> ExecutorResult<()> {
            *self.unwinds.lock().await += 1;
            Ok(())
        }
    }

    fn timeout_cancel_branch_config() -> ExecutorConfig {
        ExecutorConfig {
            use_flashbots: false,
            leg1_timeout: Duration::from_millis(200),
            leg2_timeout: Duration::from_millis(20),
            ..ExecutorConfig::default()
        }
    }

    #[tokio::test]
    async fn leg2_timeout_original_mined_success_pushes_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::executor::reconcile::ReconcileStore::open(dir.path().join("r.db")).unwrap(),
        );
        let unwinds = Arc::new(Mutex::new(0));
        let ex = Executor::new(
            Arc::new(TimeoutCancelBranchLegs {
                outcome: dex_swapper::PendingSwapCancelOutcome::OriginalMined {
                    tx_hash: "0xoriginal".into(),
                    success: true,
                },
                unwinds: Arc::clone(&unwinds),
            }),
            timeout_cancel_branch_config(),
        )
        .with_reconcile_store(Arc::clone(&store));

        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Leg2Timeout);
        assert_eq!(ctx.leg2_handle.as_deref(), Some("0xtimeout-branch"));
        assert!(
            ctx.error
                .as_deref()
                .unwrap_or("")
                .contains("original tx mined")
        );
        assert_eq!(*unwinds.lock().await, 0);
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tx_hash, "0xtimeout-branch");
    }

    #[tokio::test]
    async fn leg2_timeout_original_mined_reverted_unwinds_leg1() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::executor::reconcile::ReconcileStore::open(dir.path().join("r.db")).unwrap(),
        );
        let unwinds = Arc::new(Mutex::new(0));
        let ex = Executor::new(
            Arc::new(TimeoutCancelBranchLegs {
                outcome: dex_swapper::PendingSwapCancelOutcome::OriginalMined {
                    tx_hash: "0xoriginal".into(),
                    success: false,
                },
                unwinds: Arc::clone(&unwinds),
            }),
            timeout_cancel_branch_config(),
        )
        .with_reconcile_store(Arc::clone(&store));

        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Leg2Reverted);
        assert_eq!(ctx.leg2_handle.as_deref(), Some("0xtimeout-branch"));
        assert!(ctx.error.as_deref().unwrap_or("").contains("reverted"));
        assert_eq!(*unwinds.lock().await, 1);
        assert!(store.list_pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn leg2_timeout_cancel_unknown_preserves_reconcile_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::executor::reconcile::ReconcileStore::open(dir.path().join("r.db")).unwrap(),
        );
        let unwinds = Arc::new(Mutex::new(0));
        let ex = Executor::new(
            Arc::new(TimeoutCancelBranchLegs {
                outcome: dex_swapper::PendingSwapCancelOutcome::Unknown("no nonce".into()),
                unwinds: Arc::clone(&unwinds),
            }),
            timeout_cancel_branch_config(),
        )
        .with_reconcile_store(Arc::clone(&store));

        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Leg2Timeout);
        assert_eq!(ctx.leg2_handle.as_deref(), Some("0xtimeout-branch"));
        assert!(
            ctx.error
                .as_deref()
                .unwrap_or("")
                .contains("manual reconcile")
        );
        assert_eq!(*unwinds.lock().await, 0);
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tx_hash, "0xtimeout-branch");
    }

    #[tokio::test]
    async fn reconcile_store_accessor_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::executor::reconcile::ReconcileStore::open(dir.path().join("r.db")).unwrap(),
        );
        let legs = Arc::new(SimulatedLegs::default());
        let ex = Executor::new(legs, cex_first()).with_reconcile_store(Arc::clone(&store));
        assert!(ex.reconcile_store().is_some());
        let other = Executor::new(Arc::new(SimulatedLegs::default()), cex_first());
        assert!(other.reconcile_store().is_none());
    }

    #[tokio::test]
    async fn leg2_timeout_does_not_unwind() {
        let legs = Arc::new(SimulatedLegs {
            dex_behaviour: LegBehaviour::Hang,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first());
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Leg2Timeout);
        assert!(ctx.state.is_terminal());
        // Leg 1 is still recorded as filled — proof we did NOT unwind.
        assert!(ctx.leg1_fill_size.is_some());
        assert!(
            ctx.error
                .as_deref()
                .unwrap_or("")
                .contains("manual reconcile")
        );
    }

    #[tokio::test]
    async fn leg1_partial_fill_rejected_after_cancel() {
        let legs = Arc::new(SimulatedLegs {
            cex_behaviour: LegBehaviour::PartialFill,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first());
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Rejected);
        assert!(ctx.error.as_deref().unwrap_or("").contains("partial fill"));
    }

    #[tokio::test]
    async fn circuit_breaker_rejects_pre_flight() {
        let legs = Arc::new(SimulatedLegs {
            dex_behaviour: LegBehaviour::Reject,
            ..Default::default()
        });
        let ex = Executor::new(legs, cex_first());
        // 3 genuine leg2 failures trip the breaker.
        for _ in 0..3 {
            let _ = ex.execute(mk_signal(Decimal::from(80))).await;
        }
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Rejected);
        assert_eq!(ctx.error.as_deref(), Some("Circuit breaker open"));
    }

    #[tokio::test]
    async fn circuit_breaker_counts_negative_done_pnl_as_failure() {
        let legs = Arc::new(SimulatedLegs::default());
        let heavy = FeeStructure {
            cex_taker_bps: Decimal::from(50),
            dex_swap_bps: Decimal::from(150),
            gas_cost_usd: Decimal::from(50),
        };
        let ex = Executor::new(legs, cex_first()).with_fees(heavy);

        for _ in 0..3 {
            let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
            assert_eq!(ctx.state, ExecutorState::Done);
            assert!(ctx.actual_net_pnl.unwrap() < Decimal::ZERO);
        }

        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Rejected);
        assert_eq!(ctx.error.as_deref(), Some("Circuit breaker open"));
    }

    #[tokio::test]
    async fn replay_rejects_duplicate() {
        let legs = Arc::new(SimulatedLegs::default());
        let ex = Executor::new(legs, cex_first());
        let sig = mk_signal(Decimal::from(80));
        let first = ex.execute(sig.clone()).await;
        assert!(first.state.is_filled());
        let second = ex.execute(sig).await;
        assert_eq!(second.state, ExecutorState::Rejected);
        assert_eq!(second.error.as_deref(), Some("Duplicate signal"));
    }

    #[tokio::test]
    async fn dex_first_path_succeeds() {
        let legs = Arc::new(SimulatedLegs::default());
        let cfg = ExecutorConfig {
            use_flashbots: true,
            leg1_timeout: Duration::from_millis(500),
            leg2_timeout: Duration::from_millis(500),
            ..ExecutorConfig::default()
        };
        let ex = Executor::new(legs, cfg);
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Done);
        assert_eq!(ctx.leg1_venue, "dex");
        assert_eq!(ctx.leg2_venue, "cex");
    }

    #[tokio::test]
    async fn dex_first_leg1_revert_rejects_cleanly() {
        // DEX-first: bundle revert = no exposure, no unwind needed.
        let legs = Arc::new(SimulatedLegs {
            dex_behaviour: LegBehaviour::Revert,
            ..Default::default()
        });
        let cfg = ExecutorConfig {
            use_flashbots: true,
            leg1_timeout: Duration::from_millis(500),
            leg2_timeout: Duration::from_millis(500),
            ..ExecutorConfig::default()
        };
        let ex = Executor::new(legs, cfg);
        let ctx = ex.execute(mk_signal(Decimal::from(80))).await;
        assert_eq!(ctx.state, ExecutorState::Rejected);
        assert!(ctx.leg1_fill_size.is_none());
    }

    #[test]
    fn state_terminal_helpers() {
        use ExecutorState::*;
        assert!(Done.is_terminal());
        assert!(Failed.is_terminal());
        assert!(Rejected.is_terminal());
        assert!(Leg2Reverted.is_terminal());
        assert!(Leg2Timeout.is_terminal());
        assert!(!Leg1Pending.is_terminal());
        assert!(!Unwinding.is_terminal());

        assert!(Done.is_filled());
        assert!(!Failed.is_filled());
    }

    #[test]
    fn context_profitability_comes_from_realized_pnl() {
        let mut positive = ExecutionContext::new(mk_signal(Decimal::from(80)));
        positive.actual_net_pnl = Some(Decimal::ONE);
        assert!(positive.is_profitable());

        let mut zero = ExecutionContext::new(mk_signal(Decimal::from(80)));
        zero.actual_net_pnl = Some(Decimal::ZERO);
        assert!(!zero.is_profitable());

        let mut negative = ExecutionContext::new(mk_signal(Decimal::from(80)));
        negative.actual_net_pnl = Some(-Decimal::ONE);
        assert!(!negative.is_profitable());
    }

    #[test]
    fn state_display_matches_spec() {
        assert_eq!(ExecutorState::Leg2Timeout.to_string(), "LEG2_TIMEOUT");
        assert_eq!(ExecutorState::Done.to_string(), "DONE");
        assert_eq!(ExecutorState::Leg1Cancelling.to_string(), "LEG1_CANCELLING");
    }

    // ---- S1 wiring: LiveLegs::execute_dex via a mock DexSwapper ------

    #[test]
    fn decimal_u256_scaled_roundtrip() {
        // 1.5 USDC (6 decimals) -> 1_500_000 -> 1.5 back.
        let d = Decimal::from_str_exact("1.5").unwrap();
        let u = decimal_to_u256_scaled(d, 6).unwrap();
        assert_eq!(u, U256::from(1_500_000u64));
        let back = u256_to_decimal_scaled(u, 6);
        assert_eq!(back, d);
    }

    #[test]
    fn decimal_u256_scaled_rejects_negative() {
        let d = Decimal::from_str_exact("-1").unwrap();
        assert!(decimal_to_u256_scaled(d, 6).is_err());
    }

    #[derive(Debug)]
    struct MockSwapper {
        /// What `swap` should return.
        behaviour: MockSwapBehaviour,
        /// Last recorded (amount_in, min_out).
        last: Arc<Mutex<Option<(U256, U256)>>>,
        preflight_quote: Option<U256>,
    }

    #[derive(Debug, Clone)]
    enum MockSwapBehaviour {
        /// Return `amount_out = amount_in * ratio / 1e6` (ratio in 1e6 units).
        FillWithRatio { ratio_micros: u64 },
        /// Return `SwapperError::Reverted`.
        Revert,
    }

    #[async_trait]
    impl DexSwapper for MockSwapper {
        async fn submit_swap(
            &self,
            _token_in: &Address,
            token_out: &Address,
            amount_in: U256,
            min_out: U256,
            recipient: &Address,
        ) -> dex_swapper::SwapperResult<dex_swapper::SwapSubmission> {
            *self.last.lock().await = Some((amount_in, min_out));
            let tx_hash = match self.behaviour {
                MockSwapBehaviour::FillWithRatio { .. } => "0xdeadbeef",
                MockSwapBehaviour::Revert => "0xdead",
            };
            Ok(dex_swapper::SwapSubmission {
                tx_hash: tx_hash.into(),
                nonce: None,
                amount_in,
                token_out: token_out.clone(),
                recipient: recipient.clone(),
                private_bundle: None,
                pool_kind: dex_swapper::DexPoolKind::V2,
            })
        }

        async fn quote_exact_input_for_pair(
            &self,
            _tokens: &dex_swapper::PairTokens,
            _token_in: &Address,
            _token_out: &Address,
            _amount_in: U256,
        ) -> dex_swapper::SwapperResult<Option<U256>> {
            Ok(self.preflight_quote)
        }

        async fn wait_swap(
            &self,
            submission: dex_swapper::SwapSubmission,
        ) -> dex_swapper::SwapperResult<dex_swapper::SwapResult> {
            match self.behaviour {
                MockSwapBehaviour::FillWithRatio { ratio_micros } => {
                    let amount_out =
                        submission.amount_in * U256::from(ratio_micros) / U256::from(1_000_000u64);
                    Ok(dex_swapper::SwapResult {
                        tx_hash: submission.tx_hash,
                        amount_in: submission.amount_in,
                        amount_out,
                        gas_used: U256::from(150_000u64),
                        success: true,
                    })
                }
                MockSwapBehaviour::Revert => {
                    Err(dex_swapper::SwapperError::Reverted("0xdead".into()))
                }
            }
        }
    }

    fn dex_test_signal(dex_price: &str) -> Signal {
        Signal::new(SignalParams {
            pair: "ETH/USDC".into(),
            direction: Direction::BuyCexSellDex,
            cex_price: Decimal::from(2000),
            dex_price: Decimal::from_str_exact(dex_price).unwrap(),
            spread_bps: Decimal::from(100),
            size: Decimal::ONE,
            expected_gross_pnl: Decimal::from(20),
            expected_fees: Decimal::from(5),
            expected_net_pnl: Decimal::from(15),
            ttl: chrono::Duration::seconds(5),
            inventory_ok: true,
            within_limits: true,
        })
    }

    fn dex_address_book() -> Arc<PairAddressBook> {
        let mut book = PairAddressBook::new();
        book.insert(
            "ETH/USDC",
            dex_swapper::PairTokens {
                base: Address::new("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
                base_decimals: 18,
                quote: Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                quote_decimals: 6,
                pool_kind: dex_swapper::DexPoolKind::V2,
                v3_fee: None,
                v3_path: None,
                v3_fees: None,
                v3_quoter: None,
            },
        );
        Arc::new(book)
    }

    fn dummy_recipient() -> Address {
        Address::new("0x0000000000000000000000000000000000000ABC").unwrap()
    }

    // Build a LiveLegs with a fake exchange (we only exercise execute_dex,
    // which never touches the exchange). The exchange Arc is constructed
    // from a real ExchangeClient pointed at a dummy URL; none of the CEX
    // methods are called in these tests.
    fn dummy_exchange() -> Arc<ExchangeClient> {
        let config = crate::exchange::config::BinanceConfig::with_custom_url(
            "test_key".into(),
            "test_secret".into(),
            "https://test.invalid".into(),
        );
        Arc::new(ExchangeClient::new(config).expect("dummy exchange must build"))
    }

    fn live_legs_with_mock(mock: Arc<MockSwapper>, cfg: DexSwapperConfig) -> LiveLegs {
        LiveLegs::new(dummy_exchange()).with_dex(mock, dex_address_book(), cfg, dummy_recipient())
    }

    #[tokio::test]
    async fn execute_dex_happy_path_sells_base_for_quote() {
        // BuyCexSellDex: sell 1 ETH on DEX -> receive quote (USDC).
        // Mock returns exactly the quoted amount (ratio 1.0 in scaled units).
        // Expected: amount_in = 1e18, amount_out = dex_price * 1e6.
        let mock = Arc::new(MockSwapper {
            // 1 ETH (1e18) * ratio_micros / 1e6 = amount_out in USDC raw (6 decimals).
            // We want 2010 USDC out for 1 ETH => 2010_000_000 raw.
            // amount_in raw = 1e18. 1e18 * r / 1e6 = 2.01e9 => r = 2.01e9 / 1e12 = 0.00201.
            // ratio_micros = 2010 (micros = 2010e-6 = 0.00201). Good.
            behaviour: MockSwapBehaviour::FillWithRatio { ratio_micros: 2010 },
            last: Arc::new(Mutex::new(None)),
            preflight_quote: None,
        });
        let cfg = DexSwapperConfig {
            slippage_bps: 50, // 0.5%
            ..DexSwapperConfig::default()
        };
        let legs = live_legs_with_mock(Arc::clone(&mock), cfg);
        let sig = dex_test_signal("2010");

        let fill = legs.execute_dex(&sig, Decimal::ONE).await.unwrap();
        assert_eq!(fill.outcome, LegOutcome::Accepted);
        assert_eq!(fill.handle.as_deref(), Some("0xdeadbeef"));
        assert_eq!(fill.filled, Decimal::ONE); // filled_base == requested size

        // Verify swapper saw the right amount_in (1e18) and min_out = 2010e6 * 0.995.
        let last = mock.last.lock().await.unwrap();
        assert_eq!(last.0, U256::from(10u64).pow(U256::from(18u64)));
        let expected_min =
            U256::from(2_010_000_000u64) * U256::from(9_950u64) / U256::from(10_000u64);
        assert_eq!(last.1, expected_min);
    }

    #[tokio::test]
    async fn execute_dex_revert_maps_to_reverted() {
        let mock = Arc::new(MockSwapper {
            behaviour: MockSwapBehaviour::Revert,
            last: Arc::new(Mutex::new(None)),
            preflight_quote: None,
        });
        let legs = live_legs_with_mock(Arc::clone(&mock), DexSwapperConfig::default());
        let sig = dex_test_signal("2010");

        let fill = legs.execute_dex(&sig, Decimal::ONE).await.unwrap();
        assert_eq!(fill.outcome, LegOutcome::Reverted);
        assert_eq!(fill.handle.as_deref(), Some("0xdead"));
        assert_eq!(fill.filled, Decimal::ZERO);
    }

    #[tokio::test]
    async fn execute_dex_rejects_when_preflight_quote_below_min_out() {
        let last = Arc::new(Mutex::new(None));
        let mock = Arc::new(MockSwapper {
            behaviour: MockSwapBehaviour::FillWithRatio { ratio_micros: 2010 },
            last: Arc::clone(&last),
            preflight_quote: Some(U256::from(1_000_000_000u64)),
        });
        let cfg = DexSwapperConfig {
            slippage_bps: 50,
            ..DexSwapperConfig::default()
        };
        let legs = live_legs_with_mock(mock, cfg);
        let sig = dex_test_signal("2010");

        let fill = legs.execute_dex(&sig, Decimal::ONE).await.unwrap();
        assert_eq!(fill.outcome, LegOutcome::Rejected);
        assert!(
            fill.error
                .unwrap()
                .contains("preflight quote below min_out")
        );
        assert!(last.lock().await.is_none());
    }

    #[tokio::test]
    async fn unwind_dex_leg_runs_reverse_dex_swap() {
        let last = Arc::new(Mutex::new(None));
        let mock = Arc::new(MockSwapper {
            behaviour: MockSwapBehaviour::FillWithRatio {
                ratio_micros: 497_512_437_810_945,
            },
            last: Arc::clone(&last),
            preflight_quote: None,
        });
        let legs = live_legs_with_mock(Arc::clone(&mock), DexSwapperConfig::default());
        let mut ctx = ExecutionContext::new(dex_test_signal("2010"));
        ctx.leg1_venue = "dex";
        ctx.leg1_fill_size = Some(Decimal::ONE);

        legs.unwind(&ctx).await.unwrap();

        let recorded = last.lock().await.unwrap();
        assert_eq!(recorded.0, U256::from(2_010_000_000u64));
        let expected_min =
            U256::from(10u64).pow(U256::from(18u64)) * U256::from(9_950u64) / U256::from(10_000u64);
        assert_eq!(recorded.1, expected_min);
    }

    #[tokio::test]
    async fn execute_dex_without_swapper_is_not_implemented() {
        let legs = LiveLegs::new(dummy_exchange()); // no with_dex
        let sig = dex_test_signal("2010");
        let err = legs.execute_dex(&sig, Decimal::ONE).await.unwrap_err();
        assert!(matches!(
            err,
            crate::executor::errors::ExecutorError::NotImplemented(_)
        ));
    }

    #[tokio::test]
    async fn execute_dex_unknown_pair_is_invalid_signal() {
        let mock = Arc::new(MockSwapper {
            behaviour: MockSwapBehaviour::FillWithRatio { ratio_micros: 2010 },
            last: Arc::new(Mutex::new(None)),
            preflight_quote: None,
        });
        let legs = live_legs_with_mock(Arc::clone(&mock), DexSwapperConfig::default());
        let mut sig = dex_test_signal("2010");
        sig.pair = "BTC/USDT".into(); // not in address book
        let err = legs.execute_dex(&sig, Decimal::ONE).await.unwrap_err();
        assert!(matches!(
            err,
            crate::executor::errors::ExecutorError::InvalidSignal(_)
        ));
    }
}
