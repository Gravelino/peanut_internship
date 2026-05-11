use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Snapshot of an order book at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshot {
    /// Trading pair symbol (e.g. "BTC/USDT").
    pub symbol: String,
    /// Unix-millisecond timestamp of the snapshot.
    pub timestamp: u64,
    /// Bid levels as (price, quantity) pairs, sorted descending.
    pub bids: Vec<(Decimal, Decimal)>,
    /// Ask levels as (price, quantity) pairs, sorted ascending.
    pub asks: Vec<(Decimal, Decimal)>,
    /// Best bid (price, quantity), if any.
    pub best_bid: Option<(Decimal, Decimal)>,
    /// Best ask (price, quantity), if any.
    pub best_ask: Option<(Decimal, Decimal)>,
    /// Mid-point price between best bid and best ask.
    pub mid_price: Option<Decimal>,
    /// Bid-ask spread expressed in basis points.
    pub spread_bps: Option<Decimal>,
}

/// Asset balance normalised across venues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedBalance {
    /// Amount available for trading.
    pub free: Decimal,
    /// Amount locked in open orders.
    pub locked: Decimal,
    /// Sum of free and locked.
    pub total: Decimal,
}

/// Result returned after placing an order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResult {
    /// Exchange-assigned order identifier.
    pub id: String,
    /// Trading pair symbol.
    pub symbol: String,
    /// Order side ("buy" or "sell").
    pub side: String,
    /// Order type (e.g. "limit", "market").
    pub order_type: String,
    /// Time-in-force policy (e.g. "GTC", "IOC").
    pub time_in_force: String,
    /// Quantity requested in the original order.
    pub amount_requested: Decimal,
    /// Quantity actually filled so far.
    pub amount_filled: Decimal,
    /// Volume-weighted average fill price.
    pub avg_fill_price: Decimal,
    /// Total fee charged.
    pub fee: Decimal,
    /// Asset in which the fee was charged.
    pub fee_asset: String,
    /// Current order status (e.g. "open", "filled", "cancelled").
    pub status: String,
    /// Unix-millisecond timestamp when the order was created.
    pub timestamp: u64,
}

/// Maker/taker fee rates for a venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeStructure {
    /// Maker fee rate.
    pub maker: Decimal,
    /// Taker fee rate.
    pub taker: Decimal,
}

/// Outcome of walking the order book to estimate fill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkResult {
    /// Volume-weighted average price across consumed levels.
    pub avg_price: Decimal,
    /// Total notional cost (or proceeds) of the walk.
    pub total_cost: Decimal,
    /// Price slippage relative to the best level, in basis points.
    pub slippage_bps: Decimal,
    /// Number of book levels consumed.
    pub levels_consumed: usize,
    /// Whether the requested quantity was fully filled.
    pub fully_filled: bool,
    /// Per-level fill breakdown.
    pub fills: Vec<FillLevel>,
}

/// A single price level consumed during a book walk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillLevel {
    /// Price at this level.
    pub price: Decimal,
    /// Quantity filled at this level.
    pub qty: Decimal,
    /// Notional value (price × qty) at this level.
    pub cost: Decimal,
}

/// Point-in-time portfolio snapshot across all venues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    /// UTC timestamp of the snapshot.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Balances keyed by venue then asset.
    pub venues: HashMap<String, HashMap<String, NormalizedBalance>>,
    /// Aggregate quantity per asset across venues.
    pub totals: HashMap<String, Decimal>,
    /// Estimated total portfolio value in USD.
    pub total_usd: Decimal,
}

/// Asset-skew analysis result across venues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkewResult {
    /// Asset symbol being analysed.
    pub asset: String,
    /// Net exposure (positive = long, negative = short).
    pub total: Decimal,
    /// Per-venue skew details.
    pub venues: HashMap<String, VenueSkew>,
    /// Largest percentage deviation from the target allocation.
    pub max_deviation_pct: f64,
    /// Whether a rebalance is recommended.
    pub needs_rebalance: bool,
}

/// Skew metrics for a single venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueSkew {
    /// Net position amount at this venue.
    pub amount: Decimal,
    /// Share of total exposure as a percentage.
    pub pct: f64,
    /// Deviation from the target allocation percentage.
    pub deviation_pct: f64,
}

/// Pre-flight check result for cross-venue execution feasibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanExecuteResult {
    /// Whether the trade can proceed.
    pub can_execute: bool,
    /// Available balance on the buy-side venue.
    pub buy_venue_available: Decimal,
    /// Balance required on the buy-side venue.
    pub buy_venue_needed: Decimal,
    /// Available balance on the sell-side venue.
    pub sell_venue_available: Decimal,
    /// Balance required on the sell-side venue.
    pub sell_venue_needed: Decimal,
    /// Human-readable reason when execution is not possible.
    pub reason: Option<String>,
}

/// A single fill (trade) returned by the exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyTrade {
    /// Exchange-assigned trade identifier.
    pub id: String,
    /// Parent order identifier.
    pub order_id: String,
    /// Trading pair symbol.
    pub symbol: String,
    /// Trade side ("buy" or "sell").
    pub side: String,
    /// Execution price.
    pub price: Decimal,
    /// Execution quantity.
    pub qty: Decimal,
    /// Fee charged for this fill.
    pub fee: Decimal,
    /// Asset in which the fee was charged.
    pub fee_asset: String,
    /// Unix-millisecond timestamp of the fill.
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapitalNetworkConfig {
    pub network: String,
    pub name: Option<String>,
    pub withdraw_enable: bool,
    pub deposit_enable: bool,
    pub withdraw_fee: Decimal,
    pub withdraw_min: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapitalCoinConfig {
    pub coin: String,
    pub name: Option<String>,
    pub networks: Vec<CapitalNetworkConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WithdrawalRecord {
    pub id: String,
    pub coin: String,
    pub amount: Decimal,
    pub network: Option<String>,
    pub address: Option<String>,
    pub tx_id: Option<String>,
    pub status: i64,
}

impl WithdrawalRecord {
    pub fn completed(&self) -> bool {
        self.status == 6
    }

    pub fn failed(&self) -> bool {
        matches!(self.status, 1 | 3 | 5)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DepositRecord {
    pub id: String,
    pub coin: String,
    pub amount: Decimal,
    pub network: Option<String>,
    pub address: Option<String>,
    pub tx_id: Option<String>,
    pub status: i64,
}

impl DepositRecord {
    pub fn credited(&self) -> bool {
        matches!(self.status, 1 | 6)
    }

    pub fn failed(&self) -> bool {
        self.status == 2
    }
}
