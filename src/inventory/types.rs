use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::core::types::{DEFAULT_TRANSFER_TIME_MIN, ETH_CONFIRMATIONS};

/// A trading venue that holds asset balances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Venue {
    /// Binance centralized exchange.
    Binance,
    /// Bybit centralized exchange.
    Bybit,
    /// On-chain wallet.
    Wallet,
}

impl Venue {
    /// Returns true if this venue is a CEX with a trading API.
    pub fn is_cex(&self) -> bool {
        matches!(self, Venue::Binance | Venue::Bybit)
    }
}

impl std::fmt::Display for Venue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Venue::Binance => write!(f, "binance"),
            Venue::Bybit => write!(f, "bybit"),
            Venue::Wallet => write!(f, "wallet"),
        }
    }
}

/// Asset balance at a single venue, split into free and locked portions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    /// Venue where this balance is held.
    pub venue: Venue,
    /// Asset ticker (e.g. "ETH", "USDT").
    pub asset: String,
    /// Amount available for trading.
    pub free: Decimal,
    /// Amount locked in open orders.
    pub locked: Decimal,
}

impl Balance {
    /// Returns free + locked, the total holdings of this asset at the venue.
    pub fn total(&self) -> Decimal {
        self.free + self.locked
    }
}

/// Fee and timing parameters for withdrawing an asset from a venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferFeeInfo {
    /// Fee charged per withdrawal.
    pub withdrawal_fee: Decimal,
    /// Minimum amount that can be withdrawn.
    pub min_withdrawal: Decimal,
    /// Number of blockchain confirmations required.
    pub confirmations: u32,
    /// Estimated time for the withdrawal to complete, in minutes.
    pub estimated_time_min: u32,
}

/// A planned asset transfer between two venues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPlan {
    /// Source venue for the transfer.
    pub from_venue: Venue,
    /// Destination venue for the transfer.
    pub to_venue: Venue,
    /// Asset ticker to transfer.
    pub asset: String,
    /// Gross amount to transfer before fees.
    pub amount: Decimal,
    /// Estimated withdrawal fee for this transfer.
    pub estimated_fee: Decimal,
    /// Estimated time for the transfer to complete, in minutes.
    pub estimated_time_min: u32,
}

impl TransferPlan {
    /// Amount the destination venue will receive after fees.
    pub fn net_amount(&self) -> Decimal {
        self.amount - self.estimated_fee
    }
}

/// Aggregate cost estimate for a set of transfer plans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostEstimate {
    /// Number of transfers in the plan.
    pub total_transfers: usize,
    /// Total fees across all transfers, valued in USD.
    pub total_fees_usd: Decimal,
    /// Worst-case time for all transfers to complete, in minutes.
    pub total_time_min: u32,
    /// List of asset tickers affected by the transfers.
    pub assets_affected: Vec<String>,
}

/// A concrete step the rebalance executor can perform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RebalanceStep {
    /// Trade on a CEX venue to adjust inventory (fast, ~1s).
    Trade(TradeStep),
    /// Withdraw from a CEX to an on-chain wallet (slow, on-chain).
    Withdraw(WithdrawStep),
}

/// Parameters for a single trade on a CEX venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeStep {
    /// Venue to execute the trade on.
    pub venue: Venue,
    /// Trading pair symbol (e.g. "ETHUSDT").
    pub symbol: String,
    /// Order side: "BUY" or "SELL".
    pub side: String,
    /// Base asset being bought or sold (e.g. "ETH").
    pub base_asset: String,
    /// Quote asset used for pricing (e.g. "USDT").
    pub quote_asset: String,
    /// Quantity of the base asset to trade.
    pub amount: Decimal,
    /// Maximum acceptable slippage in basis points.
    pub max_slippage_bps: Decimal,
}

/// Parameters for an on-chain withdrawal from a CEX to a wallet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WithdrawStep {
    /// Source CEX venue.
    pub from_venue: Venue,
    /// Destination (always Wallet for now).
    pub to_venue: Venue,
    /// Asset to withdraw.
    pub asset: String,
    /// Amount to withdraw before fees.
    pub amount: Decimal,
    /// Estimated withdrawal fee.
    pub fee: Decimal,
}

/// Outcome of executing a single rebalance step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebalanceResult {
    /// The step that was (attempted to be) executed.
    pub step: RebalanceStep,
    /// Exchange-assigned order ID (if an order was placed).
    pub order_id: Option<String>,
    /// Quantity of base asset actually filled.
    pub amount_filled: Decimal,
    /// Volume-weighted average fill price.
    pub avg_price: Decimal,
    /// Fee charged by the exchange.
    pub fee: Decimal,
    /// Asset in which the fee was charged.
    pub fee_asset: String,
    /// Final status of this step.
    pub status: RebalanceStatus,
}

/// Final status of a rebalance step execution attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RebalanceStatus {
    /// Fully filled.
    Executed,
    /// Partially filled; IOC remainder cancelled.
    PartiallyFilled,
    /// Book walk showed slippage exceeding the limit; no order placed.
    SlippageExceeded,
    /// Pre-flight balance check failed; no order placed.
    InsufficientBalance,
    /// Exchange rejected the order.
    OrderRejected,
    /// Hit rate limit; backed off.
    RateLimited,
    /// Dry-run mode: logged but not executed.
    DryRun,
    /// A previous step failed; this step was skipped.
    Aborted,
    /// Withdrawals not yet implemented.
    NotSupported,
}

/// Configuration for the rebalance executor's safety limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorConfig {
    /// Maximum acceptable slippage per trade in basis points (default: 50 = 0.5%).
    pub max_slippage_bps: Decimal,
    /// Maximum notional value of a single trade in USD (default: 1000).
    pub max_single_trade_usd: Decimal,
    /// Minimum fill percentage to accept a partial fill (default: 0.8 = 80%).
    pub min_fill_pct: f64,
    /// Milliseconds between order-status polls (default: 500).
    pub order_poll_interval_ms: u64,
    /// Maximum number of poll attempts before giving up (default: 10).
    pub order_poll_max_attempts: u32,
    /// If true, log steps but do not place real orders.
    pub dry_run: bool,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            max_slippage_bps: Decimal::from(50),
            max_single_trade_usd: Decimal::from(1000),
            min_fill_pct: 0.8,
            order_poll_interval_ms: 500,
            order_poll_max_attempts: 10,
            dry_run: false,
        }
    }
}

/// Binance withdrawal fees and parameters.
/// Sources: https://www.binance.com/en/fee/cryptoFee
pub fn transfer_fees() -> HashMap<String, TransferFeeInfo> {
    let mut fees = HashMap::new();
    fees.insert(
        "ETH".into(),
        TransferFeeInfo {
            withdrawal_fee: Decimal::from_str_exact("0.005").expect("valid withdrawal_fee literal"),
            min_withdrawal: Decimal::from_str_exact("0.01").expect("valid min_withdrawal literal"),
            confirmations: ETH_CONFIRMATIONS,
            estimated_time_min: DEFAULT_TRANSFER_TIME_MIN,
        },
    );
    fees.insert(
        "USDT".into(),
        TransferFeeInfo {
            withdrawal_fee: Decimal::from_str_exact("1.0").expect("valid withdrawal_fee literal"),
            min_withdrawal: Decimal::from_str_exact("10.0").expect("valid min_withdrawal literal"),
            confirmations: ETH_CONFIRMATIONS,
            estimated_time_min: DEFAULT_TRANSFER_TIME_MIN,
        },
    );
    fees.insert(
        "USDC".into(),
        TransferFeeInfo {
            withdrawal_fee: Decimal::from_str_exact("1.0").expect("valid withdrawal_fee literal"),
            min_withdrawal: Decimal::from_str_exact("10.0").expect("valid min_withdrawal literal"),
            confirmations: ETH_CONFIRMATIONS,
            estimated_time_min: DEFAULT_TRANSFER_TIME_MIN,
        },
    );
    fees
}

/// Minimum balances needed to execute at least one typical arb trade per venue.
/// These are operational thresholds, not exchange minimums.
pub fn min_operating_balance() -> HashMap<String, Decimal> {
    let mut balances = HashMap::new();
    balances.insert(
        "ETH".into(),
        Decimal::from_str_exact("0.5").expect("valid min_balance literal"), // enough for 1 arb leg
    );
    balances.insert(
        "USDT".into(),
        Decimal::from_str_exact("500").expect("valid min_balance literal"), // ~0.25 ETH worth
    );
    balances.insert(
        "USDC".into(),
        Decimal::from_str_exact("500").expect("valid min_balance literal"),
    );
    balances
}
