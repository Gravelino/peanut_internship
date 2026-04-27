//! Fee model used by the signal generator to decide profitability.
//!
//! All values are in basis points (1 bp = 0.01%) except `gas_cost_usd`.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::core::types::BPS_SCALE as BPS_U64;

fn bps_scale() -> Decimal {
    Decimal::from(BPS_U64)
}

/// Per-trade fee components.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeStructure {
    /// CEX taker fee, in basis points.
    pub cex_taker_bps: Decimal,
    /// DEX swap fee, in basis points (e.g. Uniswap V2 = 30 bp).
    pub dex_swap_bps: Decimal,
    /// Flat on-chain gas cost, in USD.
    pub gas_cost_usd: Decimal,
}

impl Default for FeeStructure {
    /// Defaults: 10 / 30 bp and $5 gas.
    fn default() -> Self {
        Self {
            cex_taker_bps: Decimal::from(10),
            dex_swap_bps: Decimal::from(30),
            gas_cost_usd: Decimal::from(5),
        }
    }
}

impl FeeStructure {
    /// Total fee burden for a trade, expressed in basis points of notional.
    ///
    /// Returns `Decimal::MAX` for zero-value trades.
    pub fn total_fee_bps(&self, trade_value_usd: Decimal) -> Decimal {
        if trade_value_usd <= Decimal::ZERO {
            return Decimal::MAX;
        }
        let gas_bps = self.gas_cost_usd / trade_value_usd * bps_scale();
        self.cex_taker_bps + self.dex_swap_bps + gas_bps
    }

    /// Minimum spread that covers all costs (alias for [`Self::total_fee_bps`]).
    pub fn breakeven_spread_bps(&self, trade_value_usd: Decimal) -> Decimal {
        self.total_fee_bps(trade_value_usd)
    }

    /// Net profit in USD for a given spread and notional.
    pub fn net_profit_usd(&self, spread_bps: Decimal, trade_value_usd: Decimal) -> Decimal {
        if trade_value_usd <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let bps = bps_scale();
        let gross = spread_bps / bps * trade_value_usd;
        let fees = self.total_fee_bps(trade_value_usd) / bps * trade_value_usd;
        gross - fees
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_fee_scales_gas_with_size() {
        let f = FeeStructure::default();
        // Small trade -> gas dominates.
        let small = f.total_fee_bps(Decimal::from(100));
        // Large trade -> gas is negligible.
        let large = f.total_fee_bps(Decimal::from(100_000));
        assert!(small > large);
        // At $100 notional, $5 gas = 500 bps -> total ~540 bps.
        assert_eq!(
            small,
            Decimal::from(10) + Decimal::from(30) + Decimal::from(500)
        );
    }

    #[test]
    fn breakeven_matches_total_fee() {
        let f = FeeStructure::default();
        let v = Decimal::from(5_000);
        assert_eq!(f.breakeven_spread_bps(v), f.total_fee_bps(v));
    }

    #[test]
    fn net_profit_positive_when_spread_above_breakeven() {
        let f = FeeStructure::default();
        // $10k trade, 100 bps spread
        let net = f.net_profit_usd(Decimal::from(100), Decimal::from(10_000));
        // gross = 100 bps * 10k = $100
        // fees = 10 + 30 + (5/10000 * 10000 bps = 5 bps) = 45 bps * 10k / 10k bps = $45
        assert_eq!(net, Decimal::from(55));
    }

    #[test]
    fn zero_trade_value_sentinel() {
        let f = FeeStructure::default();
        assert_eq!(f.total_fee_bps(Decimal::ZERO), Decimal::MAX);
        assert_eq!(
            f.net_profit_usd(Decimal::from(100), Decimal::ZERO),
            Decimal::ZERO
        );
    }
}
