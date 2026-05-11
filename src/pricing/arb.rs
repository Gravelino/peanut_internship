use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::amm::UniswapV2Pair;
use super::mempool::ParsedSwap;
use super::router::{PoolRef, RouteFinder};
use super::v3::pool::UniswapV3Pool;
use crate::core::types::{
    Address, DECIMAL_BASE, DEFAULT_ARB_GAS_UNITS, MIN_CROSS_DEX_AMOUNT_WEI,
    MIN_TRIANGULAR_ARB_HOPS, Token, WEI_PER_GWEI,
};

/// Classification of arbitrage opportunity type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArbKind {
    /// Cross-DEX arb: same pair on different pools with a price spread.
    CrossDex,
    /// Triangular arb: cyclic route through multiple tokens returning a profit.
    Triangular,
    /// Mempool front-run: pending swap accepts less than fair value.
    MempoolFrontRun,
}

/// A detected arbitrage opportunity with profit and gas estimates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbOpportunity {
    /// Type of arbitrage detected.
    pub kind: ArbKind,
    /// Input token address.
    pub token_in: Address,
    /// Output token address.
    pub token_out: Address,
    /// Input amount in raw token units.
    pub amount_in: u128,
    /// Expected gross profit in ETH wei.
    pub expected_profit_wei: u128,
    /// Estimated gas cost in ETH wei.
    pub gas_cost_wei: u128,
    /// Net profit after gas in ETH wei.
    pub net_profit_wei: u128,
    /// Triggering transaction hash, if any.
    pub trigger_tx: Option<String>,
    /// Pool addresses involved in the route.
    pub route_pools: Vec<Address>,
}

impl ArbOpportunity {
    /// Returns `true` if the opportunity has positive net profit after gas.
    pub fn is_profitable(&self) -> bool {
        self.net_profit_wei > 0
    }
}

/// Detects arbitrage opportunities across V2 and V3 pools.
#[derive(Debug, Clone)]
pub struct ArbDetector {
    pools: Vec<PoolRef>,
    finder: RouteFinder,
    gas_price_gwei: u128,
}

impl ArbDetector {
    /// Creates a new detector from V2 and V3 pools and the current gas price.
    pub fn new(
        v2_pools: Vec<UniswapV2Pair>,
        v3_pools: Vec<UniswapV3Pool>,
        gas_price_gwei: u128,
    ) -> Self {
        let mut pools: Vec<PoolRef> = v2_pools.into_iter().map(PoolRef::V2).collect();
        pools.extend(v3_pools.into_iter().map(PoolRef::V3));
        let finder = RouteFinder::new(pools.clone());
        Self {
            pools,
            finder,
            gas_price_gwei,
        }
    }

    /// Scans for all arbitrage opportunities triggered by a pending swap.
    pub fn detect_from_swap(&self, swap: &ParsedSwap) -> Vec<ArbOpportunity> {
        let Some(token_in) = &swap.token_in else {
            return vec![];
        };
        let Some(token_out) = &swap.token_out else {
            return vec![];
        };

        let mut opportunities = Vec::new();

        opportunities.extend(self.detect_cross_dex(swap, token_in, token_out));
        opportunities.extend(self.detect_mempool_arb(swap, token_in, token_out));
        opportunities.extend(self.detect_triangular(token_in, token_out));

        opportunities
    }

    fn detect_cross_dex(
        &self,
        swap: &ParsedSwap,
        token_in: &Address,
        token_out: &Address,
    ) -> Vec<ArbOpportunity> {
        let matching: Vec<&PoolRef> = self
            .pools
            .iter()
            .filter(|p| {
                (p.token0().address == *token_in && p.token1().address == *token_out)
                    || (p.token1().address == *token_in && p.token0().address == *token_out)
            })
            .collect();

        if matching.len() < 2 {
            return vec![];
        }

        let amount_in = swap.amount_in.as_u128().max(MIN_CROSS_DEX_AMOUNT_WEI);
        let mut best: Option<ArbOpportunity> = None;

        for i in 0..matching.len() {
            for j in (i + 1)..matching.len() {
                let p1 = matching[i];
                let p2 = matching[j];

                let token_in_tok = if p1.token0().address == *token_in {
                    p1.token0()
                } else {
                    p1.token1()
                };
                let token_out_tok = if p1.token0().address == *token_out {
                    p1.token0()
                } else {
                    p1.token1()
                };

                let out1 = match p1.get_amount_out(amount_in, token_in_tok) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(
                            pool = %p1.address(),
                            token_in = %token_in,
                            amount_in,
                            error = %e,
                            "cross-DEX arb detection skipped pool after amount_out error"
                        );
                        continue;
                    }
                };
                let out2 = match p2.get_amount_out(amount_in, token_in_tok) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(
                            pool = %p2.address(),
                            token_in = %token_in,
                            amount_in,
                            error = %e,
                            "cross-DEX arb detection skipped pool after amount_out error"
                        );
                        continue;
                    }
                };

                let (buy_pool, sell_pool, buy_out, sell_out) = if out1 > out2 {
                    (p2, p1, out2, out1)
                } else {
                    (p1, p2, out1, out2)
                };

                let spread = sell_out.saturating_sub(buy_out);
                if spread == 0 {
                    continue;
                }

                let gas_cost =
                    u128::from(DEFAULT_ARB_GAS_UNITS) * self.gas_price_gwei * WEI_PER_GWEI;

                let scale_out = DECIMAL_BASE.pow(token_out_tok.decimals as u32);
                let scale_eth = DECIMAL_BASE.pow(18u32);
                let gas_in_output = (ethers::types::U256::from(gas_cost)
                    * ethers::types::U256::from(scale_out)
                    / ethers::types::U256::from(scale_eth))
                .as_u128();

                let net = spread.saturating_sub(gas_in_output);

                if net > 0 {
                    let opp = ArbOpportunity {
                        kind: ArbKind::CrossDex,
                        token_in: token_in.clone(),
                        token_out: token_out.clone(),
                        amount_in,
                        expected_profit_wei: spread,
                        gas_cost_wei: gas_cost,
                        net_profit_wei: net,
                        trigger_tx: Some(swap.tx_hash.clone()),
                        route_pools: vec![buy_pool.address().clone(), sell_pool.address().clone()],
                    };
                    if best
                        .as_ref()
                        .is_none_or(|b| opp.net_profit_wei > b.net_profit_wei)
                    {
                        best = Some(opp);
                    }
                }
            }
        }

        if let Some(opp) = best
            && opp.is_profitable()
        {
            info!(
                kind=?ArbKind::CrossDex,
                profit=?opp.net_profit_wei,
                token_in=?opp.token_in,
                token_out=?opp.token_out,
                "Cross-DEX arb detected"
            );
            return vec![opp];
        }

        vec![]
    }

    fn detect_mempool_arb(
        &self,
        swap: &ParsedSwap,
        token_in: &Address,
        token_out: &Address,
    ) -> Vec<ArbOpportunity> {
        let swap_min_out = swap.min_amount_out.as_u128();
        if swap_min_out == 0 || swap.amount_in.is_zero() {
            return vec![];
        }

        let amount_in = swap.amount_in.as_u128();

        let pools_for_pair: Vec<&PoolRef> = self
            .pools
            .iter()
            .filter(|p| {
                (p.token0().address == *token_in && p.token1().address == *token_out)
                    || (p.token1().address == *token_in && p.token0().address == *token_out)
            })
            .collect();

        let mut best: Option<ArbOpportunity> = None;

        for pool in &pools_for_pair {
            let token_in_tok = if pool.token0().address == *token_in {
                pool.token0()
            } else {
                pool.token1()
            };

            let fair_out = match pool.get_amount_out(amount_in, token_in_tok) {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        pool = %pool.address(),
                        token_in = %token_in,
                        amount_in,
                        error = %e,
                        "mempool arb detection skipped pool after amount_out error"
                    );
                    continue;
                }
            };

            if fair_out <= swap_min_out {
                continue;
            }

            let capturable = fair_out - swap_min_out;

            let gas_cost = u128::from(DEFAULT_ARB_GAS_UNITS) * self.gas_price_gwei * WEI_PER_GWEI;

            let token_out_tok = if pool.token0().address == *token_out {
                pool.token0()
            } else {
                pool.token1()
            };
            let scale_out = DECIMAL_BASE.pow(token_out_tok.decimals as u32);
            let scale_eth = DECIMAL_BASE.pow(18u32);
            let gas_in_output = (ethers::types::U256::from(gas_cost)
                * ethers::types::U256::from(scale_out)
                / ethers::types::U256::from(scale_eth))
            .as_u128();

            let net = capturable.saturating_sub(gas_in_output);

            if net > 0 {
                let opp = ArbOpportunity {
                    kind: ArbKind::MempoolFrontRun,
                    token_in: token_in.clone(),
                    token_out: token_out.clone(),
                    amount_in,
                    expected_profit_wei: capturable,
                    gas_cost_wei: gas_cost,
                    net_profit_wei: net,
                    trigger_tx: Some(swap.tx_hash.clone()),
                    route_pools: vec![pool.address().clone()],
                };
                if best
                    .as_ref()
                    .is_none_or(|b| opp.net_profit_wei > b.net_profit_wei)
                {
                    best = Some(opp);
                }
            }
        }

        if let Some(opp) = best
            && opp.is_profitable()
        {
            debug!(
                profit=?opp.net_profit_wei,
                tx=?swap.tx_hash,
                "Mempool arb: fair_out > min_amount_out"
            );
            return vec![opp];
        }

        vec![]
    }

    fn detect_triangular(&self, token_in: &Address, token_out: &Address) -> Vec<ArbOpportunity> {
        let start_tok = match self.find_token(token_in) {
            Some(t) => t,
            None => return vec![],
        };
        let end_tok = match self.find_token(token_out) {
            Some(t) => t,
            None => return vec![],
        };

        let amount_in: u128 = MIN_CROSS_DEX_AMOUNT_WEI;
        let routes = self.finder.find_all_routes(&start_tok, &end_tok, 3);

        let mut best: Option<ArbOpportunity> = None;

        for route in &routes {
            let Ok(forward_out) = route.get_output(amount_in) else {
                continue;
            };

            let reverse_routes = self.finder.find_all_routes(&end_tok, &start_tok, 3);
            for rev_route in &reverse_routes {
                if route.num_hops() + rev_route.num_hops() < MIN_TRIANGULAR_ARB_HOPS {
                    continue;
                }

                let Ok(back_out) = rev_route.get_output(forward_out) else {
                    continue;
                };

                if back_out <= amount_in {
                    continue;
                }

                let gross_profit = back_out - amount_in;

                let total_gas = route.estimate_gas() + rev_route.estimate_gas();
                let gas_cost = total_gas * self.gas_price_gwei * WEI_PER_GWEI;

                let scale_start = DECIMAL_BASE.pow(start_tok.decimals as u32);
                let scale_eth = DECIMAL_BASE.pow(18u32);
                let gas_in_start = (ethers::types::U256::from(gas_cost)
                    * ethers::types::U256::from(scale_start)
                    / ethers::types::U256::from(scale_eth))
                .as_u128();

                let net = gross_profit.saturating_sub(gas_in_start);

                if net > 0 {
                    let mut route_addrs: Vec<Address> =
                        route.pools.iter().map(|p| p.address().clone()).collect();
                    route_addrs.extend(rev_route.pools.iter().map(|p| p.address().clone()));

                    let opp = ArbOpportunity {
                        kind: ArbKind::Triangular,
                        token_in: token_in.clone(),
                        token_out: token_out.clone(),
                        amount_in,
                        expected_profit_wei: gross_profit,
                        gas_cost_wei: gas_cost,
                        net_profit_wei: net,
                        trigger_tx: None,
                        route_pools: route_addrs,
                    };
                    if best
                        .as_ref()
                        .is_none_or(|b| opp.net_profit_wei > b.net_profit_wei)
                    {
                        best = Some(opp);
                    }
                }
            }
        }

        if let Some(opp) = best
            && opp.is_profitable()
        {
            info!(
                profit=?opp.net_profit_wei,
                "Triangular arb detected"
            );
            return vec![opp];
        }

        vec![]
    }

    fn find_token(&self, addr: &Address) -> Option<Token> {
        for pool in &self.pools {
            if pool.token0().address == *addr {
                return Some(pool.token0().clone());
            }
            if pool.token1().address == *addr {
                return Some(pool.token1().clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Address;
    use ethers::types::U256;

    fn mock_token(symbol: &str, addr_hex: &str, decimals: u8) -> Token {
        Token {
            address: Address::new(addr_hex).unwrap(),
            symbol: symbol.to_string(),
            decimals,
        }
    }

    fn setup_detector() -> (ArbDetector, Token, Token, Token) {
        let shib = mock_token("SHIB", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let eth = mock_token("ETH", "0x0000000000000000000000000000000000000003", 18);

        let p1 = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            usdc.clone(),
            100_000_000_000_000_000_000,
            100_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let p2 = UniswapV2Pair::new(
            Address::new("0x2000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            eth.clone(),
            10_000_000_000_000_000_000_000,
            10_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let p3 = UniswapV2Pair::new(
            Address::new("0x3000000000000000000000000000000000000000").unwrap(),
            eth.clone(),
            usdc.clone(),
            10_000_000_000_000_000_000_000,
            10_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let detector = ArbDetector::new(vec![p1, p2, p3], vec![], 0);
        (detector, shib, usdc, eth)
    }

    fn make_swap(
        token_in: &Address,
        token_out: &Address,
        amount_in: u128,
        min_out: u128,
    ) -> ParsedSwap {
        ParsedSwap {
            tx_hash: "0xabc".to_string(),
            router: Address::new("0x0000000000000000000000000000000000000009").unwrap(),
            dex: "UniswapV2".to_string(),
            method: "swapExactTokensForTokens".to_string(),
            token_in: Some(token_in.clone()),
            token_out: Some(token_out.clone()),
            amount_in: U256::from(amount_in),
            min_amount_out: U256::from(min_out),
            deadline: U256::from(u64::MAX),
            sender: Address::new("0x0000000000000000000000000000000000000010").unwrap(),
            gas_price: U256::from(20 * WEI_PER_GWEI),
        }
    }

    #[test]
    fn test_arb_mempool_swap_no_opportunity() {
        let (detector, shib, usdc, _eth) = setup_detector();
        let fair_out = 99_700_000_000_000_000_000u128;
        let swap = make_swap(
            &shib.address,
            &usdc.address,
            100_000_000_000_000_000_000,
            fair_out,
        );
        let opps = detector.detect_from_swap(&swap);
        assert!(opps.is_empty() || opps.iter().all(|o| !o.is_profitable()));
    }

    #[test]
    fn test_cross_dex_needs_multiple_pools_for_pair() {
        let shib = mock_token("SHIB", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);

        let p1 = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            usdc.clone(),
            100_000_000_000_000_000_000,
            200_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let p2 = UniswapV2Pair::new(
            Address::new("0x2000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            usdc.clone(),
            200_000_000_000_000_000_000,
            200_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let detector = ArbDetector::new(vec![p1, p2], vec![], 0);
        let swap = make_swap(&shib.address, &usdc.address, 1_000_000_000_000_000_000, 0);
        let opps = detector.detect_from_swap(&swap);
        assert!(
            !opps.is_empty(),
            "Two pools with different reserves should yield cross-DEX arb"
        );
        assert_eq!(opps[0].kind, ArbKind::CrossDex);
    }

    #[test]
    fn test_triangular_arb_unbalanced_pools() {
        let shib = mock_token("SHIB", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);
        let eth = mock_token("ETH", "0x0000000000000000000000000000000000000003", 18);

        let p_shib_usdc = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            usdc.clone(),
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let p_shib_eth = UniswapV2Pair::new(
            Address::new("0x2000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            eth.clone(),
            1_000_000_000_000_000_000_000,
            1_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let p_eth_usdc = UniswapV2Pair::new(
            Address::new("0x3000000000000000000000000000000000000000").unwrap(),
            eth.clone(),
            usdc.clone(),
            1_000_000_000_000_000_000_000,
            1_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let detector = ArbDetector::new(vec![p_shib_usdc, p_shib_eth, p_eth_usdc], vec![], 0);
        let swap = make_swap(&shib.address, &usdc.address, 1e18 as u128, 0);
        let opps = detector.detect_from_swap(&swap);
        let tri_opps: Vec<&ArbOpportunity> = opps
            .iter()
            .filter(|o| o.kind == ArbKind::Triangular)
            .collect();
        assert!(
            !tri_opps.is_empty(),
            "Unbalanced pools should yield triangular arb"
        );
    }

    #[test]
    fn test_arb_gas_eats_profit() {
        let shib = mock_token("SHIB", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);

        let p1 = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            usdc.clone(),
            100_000_000_000_000_000_000,
            200_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let p2 = UniswapV2Pair::new(
            Address::new("0x2000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            usdc.clone(),
            200_000_000_000_000_000_000,
            200_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let detector = ArbDetector::new(vec![p1, p2], vec![], 100_000);
        let swap = make_swap(&shib.address, &usdc.address, 1_000, 0);
        let opps = detector.detect_from_swap(&swap);
        assert!(
            opps.iter().all(|o| !o.is_profitable()),
            "High gas should eat small arb"
        );
    }

    #[test]
    fn test_mempool_arb_capturable_spread() {
        let shib = mock_token("SHIB", "0x0000000000000000000000000000000000000001", 18);
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002", 18);

        let pool = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            usdc.clone(),
            10_000_000_000_000_000_000_000,
            20_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let detector = ArbDetector::new(vec![pool.clone()], vec![], 0);
        let fair_out = pool
            .get_amount_out(1_000_000_000_000_000_000, &shib)
            .unwrap();
        let low_min = fair_out / 2;
        let swap = make_swap(
            &shib.address,
            &usdc.address,
            1_000_000_000_000_000_000,
            low_min,
        );
        let opps = detector.detect_from_swap(&swap);
        let fr_opps: Vec<&ArbOpportunity> = opps
            .iter()
            .filter(|o| o.kind == ArbKind::MempoolFrontRun)
            .collect();
        assert!(
            !fr_opps.is_empty(),
            "Swap accepting far below fair value should be capturable"
        );
    }

    #[test]
    fn test_arb_v2_v3_cross_dex() {
        use ethers::types::U256;

        let weth = mock_token("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18);
        let usdc = mock_token("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 18);

        let v2_pool = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000000").unwrap(),
            weth.clone(),
            usdc.clone(),
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let v3_pool = UniswapV3Pool::new(
            Address::new("0x2000000000000000000000000000000000000000").unwrap(),
            weth.clone(),
            usdc.clone(),
            3000,
            U256::from(79228162514264337593543950336u128),
            1_000_000_000_000_000_000,
            0,
        )
        .unwrap();

        let detector = ArbDetector::new(vec![v2_pool], vec![v3_pool], 0);
        let swap = make_swap(&weth.address, &usdc.address, 1_000_000_000_000_000_000, 0);
        let opps = detector.detect_from_swap(&swap);
        let cross: Vec<&ArbOpportunity> = opps
            .iter()
            .filter(|o| o.kind == ArbKind::CrossDex)
            .collect();
        assert!(
            !cross.is_empty(),
            "V2+V3 for same pair should detect cross-DEX arb"
        );
    }
}
