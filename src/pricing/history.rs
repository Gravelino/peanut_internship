use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::amm::{DEFAULT_FEE_BPS, PriceImpactAnalyzer};
use super::errors::{PricingError, PricingResult};
use super::feed::PriceTick;
use crate::core::types::{Address, BPS_SCALE, Token};

const DEFAULT_TRADE_SIZES_BPS: &[u128] = &[1, 5, 10, 25, 50, 100];

/// A single historical observation containing price and impact measurements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalImpactPoint {
    /// Block number of this observation.
    pub block_number: u64,
    /// Unix timestamp (milliseconds) of this observation.
    pub timestamp: u64,
    /// Spot price at the time of observation.
    pub price: Decimal,
    /// Price impact at each configured trade size.
    pub impacts: Vec<SizeImpact>,
}

/// Price impact measurement for a specific trade size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeImpact {
    /// Trade size in basis points of the input reserve.
    pub size_bps: u128,
    /// Raw input amount corresponding to this BPS size.
    pub amount_in_raw: u128,
    /// Price impact expressed as a percentage.
    pub price_impact_pct: Decimal,
}

/// Aggregate summary of historical price impact observations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactSummary {
    /// Pool contract address.
    pub pool_address: Address,
    /// Input token address.
    pub token_in: Address,
    /// Output token address.
    pub token_out: Address,
    /// Total number of observations recorded.
    pub num_observations: usize,
    /// Average impact statistics per configured trade size.
    pub avg_impact_by_size: Vec<SizeImpactAvg>,
    /// Maximum price impact observed across all sizes and observations.
    pub max_impact_observed: Decimal,
    /// Price change from first to last observation as a percentage.
    pub price_change_pct: Decimal,
}

/// Average, max, and min impact for a single trade size across observations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeImpactAvg {
    /// Trade size in basis points of the input reserve.
    pub size_bps: u128,
    /// Average price impact percentage across observations.
    pub avg_impact_pct: Decimal,
    /// Maximum price impact percentage observed.
    pub max_impact_pct: Decimal,
    /// Minimum price impact percentage observed.
    pub min_impact_pct: Decimal,
}

/// Accumulates price impact observations over time and computes aggregate statistics.
#[derive(Debug, Clone)]
pub struct HistoricalImpactAnalyzer {
    token_in: Token,
    token_out: Token,
    observations: Vec<HistoricalImpactPoint>,
    trade_sizes_bps: Vec<u128>,
}

impl HistoricalImpactAnalyzer {
    /// Creates a new analyzer with default trade sizes (1, 5, 10, 25, 50, 100 BPS).
    pub fn new(token_in: Token, token_out: Token) -> Self {
        Self {
            token_in,
            token_out,
            observations: Vec::new(),
            trade_sizes_bps: DEFAULT_TRADE_SIZES_BPS.to_vec(),
        }
    }

    /// Creates a new analyzer with custom trade sizes specified in basis points.
    pub fn with_trade_sizes(token_in: Token, token_out: Token, sizes_bps: Vec<u128>) -> Self {
        Self {
            token_in,
            token_out,
            observations: Vec::new(),
            trade_sizes_bps: sizes_bps,
        }
    }

    /// Returns the input token being analyzed.
    pub fn token_in(&self) -> &Token {
        &self.token_in
    }

    /// Returns the output token being analyzed.
    pub fn token_out(&self) -> &Token {
        &self.token_out
    }

    /// Returns the recorded observations.
    pub fn observations(&self) -> &[HistoricalImpactPoint] {
        &self.observations
    }

    /// Returns the number of recorded observations.
    pub fn observation_count(&self) -> usize {
        self.observations.len()
    }

    /// Records a price tick and computes impact at the current reserves.
    pub fn record_tick(&mut self, tick: &PriceTick) -> PricingResult<()> {
        if tick.token_in != self.token_in.address || tick.token_out != self.token_out.address {
            return Err(PricingError::UnknownToken(format!(
                "tick direction mismatch: expected {}→{}, got {}→{}",
                self.token_in.address, self.token_out.address, tick.token_in, tick.token_out,
            )));
        }

        let impacts = self.compute_impacts_at_reserves(tick.reserve_in, tick.reserve_out)?;

        self.observations.push(HistoricalImpactPoint {
            block_number: tick.block_number,
            timestamp: tick.timestamp,
            price: tick.price,
            impacts,
        });

        Ok(())
    }

    /// Records an observation manually from raw reserve data.
    pub fn record_manual(
        &mut self,
        pool_address: Address,
        reserve_in: u128,
        reserve_out: u128,
        block_number: u64,
        timestamp: u64,
    ) -> PricingResult<()> {
        let pair = super::amm::UniswapV2Pair::new(
            pool_address,
            self.token_in.clone(),
            self.token_out.clone(),
            reserve_in,
            reserve_out,
            DEFAULT_FEE_BPS,
        )?;

        let price = pair.get_spot_price(&self.token_in)?;
        let impacts = self.compute_impacts_at_reserves(reserve_in, reserve_out)?;

        self.observations.push(HistoricalImpactPoint {
            block_number,
            timestamp,
            price,
            impacts,
        });

        Ok(())
    }

    fn compute_impacts_at_reserves(
        &self,
        reserve_in: u128,
        reserve_out: u128,
    ) -> PricingResult<Vec<SizeImpact>> {
        if reserve_in == 0 || reserve_out == 0 {
            return Ok(self
                .trade_sizes_bps
                .iter()
                .map(|&bps| SizeImpact {
                    size_bps: bps,
                    amount_in_raw: 0,
                    price_impact_pct: Decimal::ZERO,
                })
                .collect());
        }

        let pair = super::amm::UniswapV2Pair::new(
            Address::new("0x0000000000000000000000000000000000000000").unwrap(),
            self.token_in.clone(),
            self.token_out.clone(),
            reserve_in,
            reserve_out,
            DEFAULT_FEE_BPS,
        )?;

        let analyzer = PriceImpactAnalyzer::new(pair);

        self.trade_sizes_bps
            .iter()
            .map(|&bps| {
                let amount_in = reserve_in * bps / BPS_SCALE as u128;
                let impact = if amount_in == 0 {
                    Decimal::ZERO
                } else {
                    analyzer
                        .pair
                        .get_price_impact(amount_in, &self.token_in)
                        .map_err(|e| PricingError::ChainCall(format!("price impact: {e}")))?
                        * Decimal::ONE_HUNDRED
                };
                Ok(SizeImpact {
                    size_bps: bps,
                    amount_in_raw: amount_in,
                    price_impact_pct: impact,
                })
            })
            .collect()
    }

    /// Computes aggregate statistics over all recorded observations.
    pub fn summarize(&self) -> ImpactSummary {
        let n = self.observations.len();
        if n == 0 {
            return ImpactSummary {
                pool_address: Address::new("0x0000000000000000000000000000000000000000")
                    .unwrap_or_else(|_| self.token_in.address.clone()),
                token_in: self.token_in.address.clone(),
                token_out: self.token_out.address.clone(),
                num_observations: 0,
                avg_impact_by_size: self
                    .trade_sizes_bps
                    .iter()
                    .map(|&bps| SizeImpactAvg {
                        size_bps: bps,
                        avg_impact_pct: Decimal::ZERO,
                        max_impact_pct: Decimal::ZERO,
                        min_impact_pct: Decimal::ZERO,
                    })
                    .collect(),
                max_impact_observed: Decimal::ZERO,
                price_change_pct: Decimal::ZERO,
            };
        }

        let mut max_impact = Decimal::ZERO;
        let mut size_stats: Vec<(u128, Vec<Decimal>)> = self
            .trade_sizes_bps
            .iter()
            .map(|&bps| (bps, Vec::new()))
            .collect();

        for obs in &self.observations {
            for (i, si) in obs.impacts.iter().enumerate() {
                if si.price_impact_pct > max_impact {
                    max_impact = si.price_impact_pct;
                }
                if i < size_stats.len() {
                    size_stats[i].1.push(si.price_impact_pct);
                }
            }
        }

        let avg_impact_by_size: Vec<SizeImpactAvg> = size_stats
            .iter()
            .map(|(bps, impacts)| {
                if impacts.is_empty() {
                    return SizeImpactAvg {
                        size_bps: *bps,
                        avg_impact_pct: Decimal::ZERO,
                        max_impact_pct: Decimal::ZERO,
                        min_impact_pct: Decimal::ZERO,
                    };
                }
                let sum: Decimal = impacts.iter().sum();
                let avg = sum / Decimal::from(impacts.len());
                let max_val = impacts.iter().max().copied().unwrap_or(Decimal::ZERO);
                let min_val = impacts.iter().min().copied().unwrap_or(Decimal::ZERO);
                SizeImpactAvg {
                    size_bps: *bps,
                    avg_impact_pct: avg,
                    max_impact_pct: max_val,
                    min_impact_pct: min_val,
                }
            })
            .collect();

        let first_price = self
            .observations
            .first()
            .map(|o| o.price)
            .unwrap_or(Decimal::ZERO);
        let last_price = self
            .observations
            .last()
            .map(|o| o.price)
            .unwrap_or(Decimal::ZERO);
        let price_change_pct = if first_price.is_zero() {
            Decimal::ZERO
        } else {
            ((last_price - first_price) / first_price) * Decimal::ONE_HUNDRED
        };

        let pool_addr = self
            .observations
            .first()
            .map(|_| self.token_in.address.clone())
            .unwrap_or_else(|| self.token_in.address.clone());

        ImpactSummary {
            pool_address: pool_addr,
            token_in: self.token_in.address.clone(),
            token_out: self.token_out.address.clone(),
            num_observations: n,
            avg_impact_by_size,
            max_impact_observed: max_impact,
            price_change_pct,
        }
    }

    /// Returns the price series as `(timestamp, price)` pairs.
    pub fn price_series(&self) -> Vec<(u64, Decimal)> {
        self.observations
            .iter()
            .map(|o| (o.timestamp, o.price))
            .collect()
    }

    /// Returns the impact series for a specific trade size as `(timestamp, impact_pct)` pairs.
    pub fn impact_series_for_size(&self, size_bps: u128) -> Vec<(u64, Decimal)> {
        let idx = self
            .trade_sizes_bps
            .iter()
            .position(|&s| s == size_bps)
            .unwrap_or_else(|| {
                tracing::warn!(
                    size_bps,
                    "Requested impact size not in trade_sizes_bps, using index 0 as fallback"
                );
                0
            });

        self.observations
            .iter()
            .map(|o| {
                let impact = o
                    .impacts
                    .get(idx)
                    .map(|si| si.price_impact_pct)
                    .unwrap_or(Decimal::ZERO);
                (o.timestamp, impact)
            })
            .collect()
    }

    /// Clears all recorded observations.
    pub fn clear(&mut self) {
        self.observations.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Address;

    fn mock_token(symbol: &str, addr_hex: &str, decimals: u8) -> Token {
        Token {
            address: Address::new(addr_hex).unwrap(),
            symbol: symbol.to_string(),
            decimals,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_tick(
        pool: &str,
        t_in: &Address,
        t_out: &Address,
        price: Decimal,
        r_in: u128,
        r_out: u128,
        blk: u64,
        ts: u64,
    ) -> PriceTick {
        PriceTick {
            pool_address: Address::new(pool).unwrap(),
            token_in: t_in.clone(),
            token_out: t_out.clone(),
            price,
            reserve_in: r_in,
            reserve_out: r_out,
            block_number: blk,
            timestamp: ts,
        }
    }

    #[test]
    fn test_record_tick_increments_observations() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        let tick = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2000),
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            100,
            1000,
        );

        assert_eq!(analyzer.observation_count(), 0);
        analyzer.record_tick(&tick).unwrap();
        assert_eq!(analyzer.observation_count(), 1);
    }

    #[test]
    fn test_record_tick_rejects_wrong_direction() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        let tick = make_tick(
            "0x1000000000000000000000000000000000000000",
            &usdc.address,
            &weth.address,
            Decimal::from(2000),
            2_000_000_000_000_000_000_000,
            1_000_000_000_000_000_000_000,
            100,
            1000,
        );

        assert!(analyzer.record_tick(&tick).is_err());
    }

    #[test]
    fn test_impact_increases_with_size() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        let tick = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2000),
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            100,
            1000,
        );

        analyzer.record_tick(&tick).unwrap();
        let obs = &analyzer.observations()[0];

        for window in obs.impacts.windows(2) {
            assert!(
                window[1].price_impact_pct >= window[0].price_impact_pct,
                "Impact should be non-decreasing with size"
            );
        }
    }

    #[test]
    fn test_impact_increases_as_liquidity_decreases() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        let tick_high_liq = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2000),
            10_000_000_000_000_000_000_000,
            20_000_000_000_000_000_000_000,
            100,
            1000,
        );
        let tick_low_liq = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2000),
            100_000_000_000_000_000_000,
            200_000_000_000_000_000_000,
            101,
            2000,
        );

        analyzer.record_tick(&tick_high_liq).unwrap();
        analyzer.record_tick(&tick_low_liq).unwrap();

        let impact_high = analyzer.observations()[0].impacts[3].price_impact_pct;
        let impact_low = analyzer.observations()[1].impacts[3].price_impact_pct;
        assert!(
            impact_low > impact_high,
            "Lower liquidity should yield higher impact"
        );
    }

    #[test]
    fn test_summarize_empty() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let analyzer = HistoricalImpactAnalyzer::new(weth, usdc);

        let summary = analyzer.summarize();
        assert_eq!(summary.num_observations, 0);
        assert_eq!(summary.max_impact_observed, Decimal::ZERO);
        assert_eq!(summary.price_change_pct, Decimal::ZERO);
    }

    #[test]
    fn test_summarize_computes_stats() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        let t1 = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2000),
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            100,
            1000,
        );
        let t2 = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2100),
            1_000_000_000_000_000_000_000,
            2_100_000_000_000_000_000_000,
            101,
            2000,
        );

        analyzer.record_tick(&t1).unwrap();
        analyzer.record_tick(&t2).unwrap();

        let summary = analyzer.summarize();
        assert_eq!(summary.num_observations, 2);
        assert!(summary.max_impact_observed > Decimal::ZERO);
        assert!(summary.price_change_pct > Decimal::ZERO);
        assert!(!summary.avg_impact_by_size.is_empty());

        for stat in &summary.avg_impact_by_size {
            assert!(stat.avg_impact_pct >= stat.min_impact_pct);
            assert!(stat.max_impact_pct >= stat.avg_impact_pct);
        }
    }

    #[test]
    fn test_price_series_returns_timestamps_and_prices() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        let t1 = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2000),
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            100,
            1000,
        );
        let t2 = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2050),
            1_000_000_000_000_000_000_000,
            2_050_000_000_000_000_000_000,
            101,
            2000,
        );

        analyzer.record_tick(&t1).unwrap();
        analyzer.record_tick(&t2).unwrap();

        let series = analyzer.price_series();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].0, 1000);
        assert_eq!(series[1].0, 2000);
        assert!(series[1].1 > series[0].1);
    }

    #[test]
    fn test_impact_series_for_specific_size() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        let tick = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2000),
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            100,
            1000,
        );

        analyzer.record_tick(&tick).unwrap();

        let series = analyzer.impact_series_for_size(10);
        assert_eq!(series.len(), 1);
        assert!(series[0].1 > Decimal::ZERO);
    }

    #[test]
    fn test_record_manual() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        analyzer
            .record_manual(
                Address::new("0x1000000000000000000000000000000000000000").unwrap(),
                1_000_000_000_000_000_000_000,
                2_000_000_000_000_000_000_000,
                100,
                1000,
            )
            .unwrap();

        assert_eq!(analyzer.observation_count(), 1);
        assert!(analyzer.observations()[0].price > Decimal::ZERO);
    }

    #[test]
    fn test_clear_resets_observations() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        let tick = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2000),
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            100,
            1000,
        );
        analyzer.record_tick(&tick).unwrap();
        assert_eq!(analyzer.observation_count(), 1);

        analyzer.clear();
        assert_eq!(analyzer.observation_count(), 0);
    }

    #[test]
    fn test_custom_trade_sizes() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let analyzer =
            HistoricalImpactAnalyzer::with_trade_sizes(weth.clone(), usdc.clone(), vec![1, 50]);

        let tick = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2000),
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            100,
            1000,
        );

        let mut analyzer = analyzer;
        analyzer.record_tick(&tick).unwrap();

        assert_eq!(analyzer.observations()[0].impacts.len(), 2);
        assert_eq!(analyzer.observations()[0].impacts[0].size_bps, 1);
        assert_eq!(analyzer.observations()[0].impacts[1].size_bps, 50);
    }

    #[test]
    fn test_price_change_pct_negative() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        let t1 = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(2000),
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            100,
            1000,
        );
        let t2 = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::from(1800),
            1_000_000_000_000_000_000_000,
            1_800_000_000_000_000_000_000,
            101,
            2000,
        );

        analyzer.record_tick(&t1).unwrap();
        analyzer.record_tick(&t2).unwrap();

        let summary = analyzer.summarize();
        assert!(summary.price_change_pct < Decimal::ZERO);
    }

    #[test]
    fn test_zero_reserves_produce_zero_impact() {
        let weth = mock_token("WETH", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

        let tick = make_tick(
            "0x1000000000000000000000000000000000000000",
            &weth.address,
            &usdc.address,
            Decimal::ZERO,
            0,
            0,
            100,
            1000,
        );

        analyzer.record_tick(&tick).unwrap();
        for si in &analyzer.observations()[0].impacts {
            assert_eq!(si.price_impact_pct, Decimal::ZERO);
            assert_eq!(si.amount_in_raw, 0);
        }
    }
}
