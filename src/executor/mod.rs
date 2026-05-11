//! # Executor module
//!
//! Drives an arbitrage [`Signal`](crate::strategy::Signal) through a state
//! machine that coordinates the CEX and DEX legs, enforces per-leg timeouts,
//! and falls back to an unwind path when the second leg fails.

pub mod dex_swapper;
pub mod engine;
pub mod errors;
pub mod migrations;
pub mod queue;
pub mod reconcile;
pub mod recovery;

pub use dex_swapper::{
    CompositeDexSwapper, DexPoolKind, DexSwapper, DexSwapperConfig, FlashbotsSwapper,
    PairAddressBook, PairTokens, SwapResult, SwapperError, UniswapV2Swapper, UniswapV3Swapper,
    apply_slippage, build_allowance_calldata, build_approve_calldata, build_swap_calldata,
    v3_swap_calldata_for_pair,
};
pub use reconcile::{
    ChainReceiptProvider, PendingReconcile, ReceiptProvider, ReconcileConfig, ReconcileError,
    ReconcileStatus, ReconcileStore, ReconcileWorker, TickOutcome,
};

pub use engine::{
    ExecutionContext, Executor, ExecutorConfig, ExecutorState, LegBehaviour, LegExecutor, LegFill,
    LegOutcome, LiveLegs, SimulatedLegs,
};
pub use errors::{ExecutorError, ExecutorResult};
pub use queue::{
    DropReason, ExecutionSink, PrioritizedSignal, QueueConfig, QueueWorker, SignalQueue,
};
pub use recovery::{
    CircuitBreaker, CircuitBreakerConfig, PnlBreaker, PnlBreakerConfig, ReplayProtection,
};
