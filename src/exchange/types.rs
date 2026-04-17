use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshot {
    pub symbol: String,
    pub timestamp: u64,
    pub bids: Vec<(Decimal, Decimal)>,
    pub asks: Vec<(Decimal, Decimal)>,
    pub best_bid: Option<(Decimal, Decimal)>,
    pub best_ask: Option<(Decimal, Decimal)>,
    pub mid_price: Decimal,
    pub spread_bps: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedBalance {
    pub free: Decimal,
    pub locked: Decimal,
    pub total: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResult {
    pub id: String,
    pub symbol: String,
    pub side: String,
    pub order_type: String,
    pub time_in_force: String,
    pub amount_requested: Decimal,
    pub amount_filled: Decimal,
    pub avg_fill_price: Decimal,
    pub fee: Decimal,
    pub fee_asset: String,
    pub status: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeStructure {
    pub maker: Decimal,
    pub taker: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkResult {
    pub avg_price: Decimal,
    pub total_cost: Decimal,
    pub slippage_bps: Decimal,
    pub levels_consumed: usize,
    pub fully_filled: bool,
    pub fills: Vec<FillLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillLevel {
    pub price: Decimal,
    pub qty: Decimal,
    pub cost: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub venues: HashMap<String, HashMap<String, NormalizedBalance>>,
    pub totals: HashMap<String, Decimal>,
    pub total_usd: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkewResult {
    pub asset: String,
    pub total: Decimal,
    pub venues: HashMap<String, VenueSkew>,
    pub max_deviation_pct: f64,
    pub needs_rebalance: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueSkew {
    pub amount: Decimal,
    pub pct: f64,
    pub deviation_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanExecuteResult {
    pub can_execute: bool,
    pub buy_venue_available: Decimal,
    pub buy_venue_needed: Decimal,
    pub sell_venue_available: Decimal,
    pub sell_venue_needed: Decimal,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyTrade {
    pub id: String,
    pub order_id: String,
    pub symbol: String,
    pub side: String,
    pub price: Decimal,
    pub qty: Decimal,
    pub fee: Decimal,
    pub fee_asset: String,
    pub timestamp: u64,
}
