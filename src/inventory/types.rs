use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Venue {
    Binance,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    pub venue: Venue,
    pub asset: String,
    pub free: Decimal,
    pub locked: Decimal,
}

impl Balance {
    pub fn total(&self) -> Decimal {
        self.free + self.locked
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferFeeInfo {
    pub withdrawal_fee: Decimal,
    pub min_withdrawal: Decimal,
    pub confirmations: u32,
    pub estimated_time_min: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPlan {
    pub from_venue: Venue,
    pub to_venue: Venue,
    pub asset: String,
    pub amount: Decimal,
    pub estimated_fee: Decimal,
    pub estimated_time_min: u32,
}

impl TransferPlan {
    pub fn net_amount(&self) -> Decimal {
        self.amount - self.estimated_fee
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostEstimate {
    pub total_transfers: usize,
    pub total_fees_usd: Decimal,
    pub total_time_min: u32,
    pub assets_affected: Vec<String>,
}

pub fn transfer_fees() -> HashMap<String, TransferFeeInfo> {
    let mut fees = HashMap::new();
    fees.insert(
        "ETH".into(),
        TransferFeeInfo {
            withdrawal_fee: Decimal::from_str_exact("0.005").unwrap_or(Decimal::ZERO),
            min_withdrawal: Decimal::from_str_exact("0.01").unwrap_or(Decimal::ZERO),
            confirmations: 12,
            estimated_time_min: 15,
        },
    );
    fees.insert(
        "USDT".into(),
        TransferFeeInfo {
            withdrawal_fee: Decimal::from_str_exact("1.0").unwrap_or(Decimal::ZERO),
            min_withdrawal: Decimal::from_str_exact("10.0").unwrap_or(Decimal::ZERO),
            confirmations: 12,
            estimated_time_min: 15,
        },
    );
    fees.insert(
        "USDC".into(),
        TransferFeeInfo {
            withdrawal_fee: Decimal::from_str_exact("1.0").unwrap_or(Decimal::ZERO),
            min_withdrawal: Decimal::from_str_exact("10.0").unwrap_or(Decimal::ZERO),
            confirmations: 12,
            estimated_time_min: 15,
        },
    );
    fees
}

pub fn min_operating_balance() -> HashMap<String, Decimal> {
    let mut balances = HashMap::new();
    balances.insert(
        "ETH".into(),
        Decimal::from_str_exact("0.5").unwrap_or(Decimal::ZERO),
    );
    balances.insert(
        "USDT".into(),
        Decimal::from_str_exact("500").unwrap_or(Decimal::ZERO),
    );
    balances.insert(
        "USDC".into(),
        Decimal::from_str_exact("500").unwrap_or(Decimal::ZERO),
    );
    balances
}
