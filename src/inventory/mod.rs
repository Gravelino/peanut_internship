//! # Inventory Module
//!
//! Tracks balances across venues and manages rebalancing:
//! - **Tracker**: Multi-venue balance tracking with skew detection ([`InventoryTracker`])
//! - **Rebalancer**: Plans transfers to correct inventory imbalance ([`RebalancePlanner`])
//! - **PnL**: Arbitrage trade recording and PnL computation ([`PnLEngine`])
//! - **Wallet**: On-chain ERC-20 balance fetching ([`WalletBalanceFetcher`])

pub mod errors;
pub mod pnl;
pub mod rebalancer;
pub mod tracker;
pub mod types;
pub mod wallet;

pub use errors::{InventoryError, InventoryResult};
pub use pnl::{ArbRecord, PnLEngine, PnLSummary, TradeLeg, TradeSummary};
pub use rebalancer::RebalancePlanner;
pub use tracker::InventoryTracker;
pub use types::{
    Balance, CostEstimate, TransferFeeInfo, TransferPlan, Venue, min_operating_balance,
    transfer_fees,
};
pub use wallet::WalletBalanceFetcher;
