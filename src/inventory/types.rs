use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::core::types::{DEFAULT_TRANSFER_TIME_MIN, ETH_CONFIRMATIONS};

/// A trading venue that holds asset balances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Venue {
    /// Binance centralized exchange.
    Binance,
    /// On-chain wallet.
    Wallet,
}

impl std::fmt::Display for Venue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Venue::Binance => write!(f, "binance"),
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
