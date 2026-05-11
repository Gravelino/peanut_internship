//! Signal data model: a validated arbitrage opportunity ready for execution.

use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Direction of an arbitrage leg pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    /// Buy on CEX, sell on DEX.
    BuyCexSellDex,
    /// Buy on DEX, sell on CEX.
    BuyDexSellCex,
}

impl Direction {
    /// Short string name (matches Python spec values).
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::BuyCexSellDex => "buy_cex_sell_dex",
            Direction::BuyDexSellCex => "buy_dex_sell_cex",
        }
    }

    /// Returns the venue where the base asset is bought.
    pub fn buy_venue(&self) -> crate::inventory::types::Venue {
        match self {
            Direction::BuyCexSellDex => crate::inventory::types::Venue::Binance,
            Direction::BuyDexSellCex => crate::inventory::types::Venue::Wallet,
        }
    }

    /// Returns the venue where the base asset is sold.
    pub fn sell_venue(&self) -> crate::inventory::types::Venue {
        match self {
            Direction::BuyCexSellDex => crate::inventory::types::Venue::Wallet,
            Direction::BuyDexSellCex => crate::inventory::types::Venue::Binance,
        }
    }
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated arbitrage opportunity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    /// Unique identifier (pair + 8 random hex chars).
    pub signal_id: String,
    /// Trading pair (e.g. "ETH/USDT").
    pub pair: String,
    /// Direction of the arbitrage.
    pub direction: Direction,

    /// Price on the CEX side.
    pub cex_price: Decimal,
    /// Price on the DEX side.
    pub dex_price: Decimal,
    /// Spread in basis points.
    pub spread_bps: Decimal,
    /// Base-asset size of one leg.
    pub size: Decimal,
    /// Value of one leg in quote asset (size * cex_price).
    pub notional_quote: Decimal,
    /// Value of one leg in USD.
    pub notional_usd: Decimal,

    /// Gross PnL in USD before fees.
    pub expected_gross_pnl: Decimal,
    /// Total fees in USD (CEX + DEX + gas).
    pub expected_fees: Decimal,
    /// Net PnL in USD after fees.
    pub expected_net_pnl: Decimal,

    /// Score in the range [0, 100].
    pub score: Decimal,
    /// Creation timestamp.
    pub timestamp: DateTime<Utc>,
    /// Expiry timestamp; after this the signal is considered stale.
    pub expiry: DateTime<Utc>,

    /// Whether inventory is sufficient to execute the signal.
    pub inventory_ok: bool,
    /// Whether the trade fits within risk-limit caps.
    pub within_limits: bool,
}

/// Builder-style constructor arguments for [`Signal::new`].
///
/// Grouped into a struct so callers don't need to juggle a 10+ argument
/// function signature — which is an easy source of bugs.
#[derive(Debug, Clone)]
pub struct SignalParams {
    /// Trading pair (e.g. "ETH/USDT").
    pub pair: String,
    /// Arbitrage direction.
    pub direction: Direction,
    /// CEX-side price.
    pub cex_price: Decimal,
    /// DEX-side price.
    pub dex_price: Decimal,
    /// Spread in basis points.
    pub spread_bps: Decimal,
    /// Base-asset size.
    pub size: Decimal,
    /// USD value of one leg.
    pub notional_usd: Decimal,
    /// Gross PnL in USD.
    pub expected_gross_pnl: Decimal,
    /// Fees in USD.
    pub expected_fees: Decimal,
    /// Net PnL in USD.
    pub expected_net_pnl: Decimal,
    /// Time-to-live from creation.
    pub ttl: Duration,
    /// Whether inventory is sufficient.
    pub inventory_ok: bool,
    /// Whether the trade is within risk limits.
    pub within_limits: bool,
}

impl Signal {
    /// Creates a new signal with a freshly generated ID and `timestamp = now`.
    ///
    /// The `score` starts at zero — callers are expected to set it via
    /// [`SignalScorer`](crate::strategy::SignalScorer) before execution.
    pub fn new(params: SignalParams) -> Self {
        let now = Utc::now();
        Self {
            signal_id: Self::generate_id(&params.pair),
            pair: params.pair,
            direction: params.direction,
            cex_price: params.cex_price,
            dex_price: params.dex_price,
            spread_bps: params.spread_bps,
            size: params.size,
            notional_quote: params.size * params.cex_price,
            notional_usd: params.notional_usd,
            expected_gross_pnl: params.expected_gross_pnl,
            expected_fees: params.expected_fees,
            expected_net_pnl: params.expected_net_pnl,
            score: Decimal::ZERO,
            timestamp: now,
            expiry: now + params.ttl,
            inventory_ok: params.inventory_ok,
            within_limits: params.within_limits,
        }
    }

    /// Returns `true` when the signal is still executable right now.
    pub fn is_valid(&self) -> bool {
        Utc::now() < self.expiry
            && self.inventory_ok
            && self.within_limits
            && self.expected_net_pnl > Decimal::ZERO
            && self.score > Decimal::ZERO
    }

    /// Seconds since the signal was created.
    pub fn age_seconds(&self) -> f64 {
        let delta = Utc::now() - self.timestamp;
        delta.num_milliseconds() as f64 / 1000.0
    }

    fn generate_id(pair: &str) -> String {
        let prefix: String = pair.chars().filter(|c| *c != '/').collect();
        let mut bytes = [0u8; 4];
        rand::thread_rng().fill_bytes(&mut bytes);
        format!("{prefix}_{}", hex::encode(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(score: Decimal, net_pnl: Decimal, ttl_secs: i64) -> Signal {
        let mut s = Signal::new(SignalParams {
            pair: "ETH/USDT".into(),
            direction: Direction::BuyCexSellDex,
            cex_price: Decimal::from(2000),
            dex_price: Decimal::from(2020),
            spread_bps: Decimal::from(100),
            size: Decimal::ONE,
            notional_usd: Decimal::from(2000),
            expected_gross_pnl: Decimal::from(20),
            expected_fees: Decimal::from(5),
            expected_net_pnl: net_pnl,
            ttl: Duration::seconds(ttl_secs),
            inventory_ok: true,
            within_limits: true,
        });
        s.score = score;
        s
    }

    #[test]
    fn unique_ids_per_signal() {
        let a = make(Decimal::from(50), Decimal::from(15), 5);
        let b = make(Decimal::from(50), Decimal::from(15), 5);
        assert_ne!(a.signal_id, b.signal_id);
        assert!(a.signal_id.starts_with("ETHUSDT_"));
    }

    #[test]
    fn is_valid_happy_path() {
        let s = make(Decimal::from(50), Decimal::from(15), 5);
        assert!(s.is_valid());
    }

    #[test]
    fn invalid_when_expired() {
        let s = make(Decimal::from(50), Decimal::from(15), -1);
        assert!(!s.is_valid());
    }

    #[test]
    fn invalid_when_score_zero() {
        let s = make(Decimal::ZERO, Decimal::from(15), 5);
        assert!(!s.is_valid());
    }

    #[test]
    fn invalid_when_net_pnl_non_positive() {
        let s = make(Decimal::from(50), Decimal::ZERO, 5);
        assert!(!s.is_valid());
    }

    #[test]
    fn direction_string() {
        assert_eq!(Direction::BuyCexSellDex.as_str(), "buy_cex_sell_dex");
        assert_eq!(Direction::BuyDexSellCex.as_str(), "buy_dex_sell_cex");
    }
}
