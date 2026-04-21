//! # Inventory Module
//!
//! Tracks balances across venues and manages rebalancing:
//! - **Tracker**: Multi-venue balance tracking with skew detection ([`InventoryTracker`])
//! - **Rebalancer**: Plans transfers to correct inventory imbalance ([`RebalancePlanner`])
//! - **Executor**: Executes rebalance steps via IOC limit orders ([`RebalanceExecutor`])
//! - **PnL**: Arbitrage trade recording and PnL computation ([`PnLEngine`])
//! - **Wallet**: On-chain ERC-20 balance fetching ([`WalletBalanceFetcher`])

pub mod chart;
pub mod errors;
pub mod pnl;
pub mod rebalance_executor;
pub mod rebalancer;
pub mod tracker;
pub mod types;
pub mod wallet;

pub use chart::PnLChartExporter;
pub use errors::{InventoryError, InventoryResult};
pub use pnl::{ArbRecord, PnLEngine, PnLSummary, TradeLeg, TradeSummary};
pub use rebalance_executor::RebalanceExecutor;
pub use rebalancer::RebalancePlanner;
pub use tracker::InventoryTracker;
pub use types::{
    Balance, CostEstimate, ExecutorConfig, RebalanceResult, RebalanceStatus, RebalanceStep,
    TradeStep, TransferFeeInfo, TransferPlan, Venue, WithdrawStep, min_operating_balance,
    transfer_fees,
};
pub use wallet::WalletBalanceFetcher;
