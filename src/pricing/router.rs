use ethers::types::U256;
use std::collections::{HashMap, HashSet};

use super::amm::UniswapV2Pair;
use super::errors::{PricingError, PricingResult};
use super::v3::pool::{UniswapV3Pool, v3_base_gas, v3_gas_per_hop};
use crate::core::types::{Address, DECIMAL_BASE, ETH_DECIMALS, Token, WEI_PER_GWEI};

/// Base gas cost for a V2 swap execution (transfer + approval overhead).
/// Source: empirical measurement on Ethereum mainnet.
const BASE_GAS_COST: u128 = 150_000;
/// Additional gas per intermediate hop in a multi-hop route.
const GAS_PER_HOP: u128 = 100_000;

/// Enum holding either a V2 or V3 pool reference for route building.
#[derive(Debug, Clone)]
pub enum PoolRef {
    /// Uniswap V2 pair.
    V2(UniswapV2Pair),
    /// Uniswap V3 pool.
    V3(UniswapV3Pool),
}

impl PoolRef {
    /// Returns the on-chain address of the pool.
    pub fn address(&self) -> &Address {
        match self {
            PoolRef::V2(p) => &p.address,
            PoolRef::V3(p) => &p.address,
        }
    }

    /// Returns the lower-address token in the pool.
    pub fn token0(&self) -> &Token {
        match self {
            PoolRef::V2(p) => &p.token0,
            PoolRef::V3(p) => &p.token0,
        }
    }

    /// Returns the higher-address token in the pool.
    pub fn token1(&self) -> &Token {
        match self {
            PoolRef::V2(p) => &p.token1,
            PoolRef::V3(p) => &p.token1,
        }
    }

    /// Computes the output amount for a given input using the pool's AMM math.
    pub fn get_amount_out(&self, amount_in: u128, token_in: &Token) -> PricingResult<u128> {
        match self {
            PoolRef::V2(p) => p.get_amount_out(amount_in, token_in),
            PoolRef::V3(p) => p.quote_swap(amount_in, token_in).map(|q| q.amount_out),
        }
    }

    /// Returns the estimated gas cost per hop for this pool type.
    pub fn gas_per_hop(&self) -> u128 {
        match self {
            PoolRef::V2(_) => GAS_PER_HOP,
            PoolRef::V3(_) => v3_gas_per_hop(),
        }
    }

    /// Returns `true` if this pool is a Uniswap V3 pool.
    pub fn is_v3(&self) -> bool {
        matches!(self, PoolRef::V3(_))
    }
}

/// A multi-hop swap route through one or more pools.
#[derive(Debug, Clone)]
pub struct Route {
    /// Pools traversed by the route, in execution order.
    pub pools: Vec<PoolRef>,
    /// Token path from input to output (length = pools + 1).
    pub path: Vec<Token>,
}

impl Route {
    /// Creates a new route from the given pools and token path.
    pub fn new(pools: Vec<PoolRef>, path: Vec<Token>) -> Self {
        Self { pools, path }
    }

    /// Returns the number of hops (pools) in this route.
    pub fn num_hops(&self) -> usize {
        self.pools.len()
    }

    /// Simulates the route end-to-end and returns the final output amount.
    pub fn get_output(&self, amount_in: u128) -> PricingResult<u128> {
        let mut current_amount = amount_in;
        for (i, pool) in self.pools.iter().enumerate() {
            let token_in = &self.path[i];
            current_amount = pool.get_amount_out(current_amount, token_in)?;
        }
        Ok(current_amount)
    }

    /// Returns intermediate amounts at each hop, including input and final output.
    pub fn get_intermediate_amounts(&self, amount_in: u128) -> PricingResult<Vec<u128>> {
        let mut amounts = Vec::with_capacity(self.num_hops() + 1);
        amounts.push(amount_in);

        let mut current_amount = amount_in;
        for (i, pool) in self.pools.iter().enumerate() {
            let token_in = &self.path[i];
            current_amount = pool.get_amount_out(current_amount, token_in)?;
            amounts.push(current_amount);
        }
        Ok(amounts)
    }

    /// Estimates total gas units required to execute this route.
    pub fn estimate_gas(&self) -> u128 {
        let base = BASE_GAS_COST.min(v3_base_gas());
        let hop_gas: u128 = self.pools.iter().map(|p| p.gas_per_hop()).sum();
        base + hop_gas
    }

    /// Returns `true` if the route includes at least one V3 pool.
    pub fn is_v3_route(&self) -> bool {
        self.pools.iter().any(|p| p.is_v3())
    }
}

/// Gas-adjusted comparison of a single route's output.
#[derive(Debug, Clone)]
pub struct RouteComparison {
    /// The evaluated route.
    pub route: Route,
    /// Output before gas deduction.
    pub gross_output: u128,
    /// Estimated gas units for execution.
    pub gas_estimate: u128,
    /// Gas cost in ETH wei.
    pub gas_cost_eth: u128,
    /// Output after deducting gas cost (converted to output token units).
    pub net_output: u128,
}

struct DfsContext<'a> {
    target_token: &'a Token,
    max_hops: usize,
    current_path: &'a mut Vec<Token>,
    current_pools: &'a mut Vec<PoolRef>,
    visited: &'a mut HashSet<Token>,
    all_routes: &'a mut Vec<Route>,
}

/// Finds routes between tokens using a graph of V2 and V3 pools.
#[derive(Debug, Clone)]
pub struct RouteFinder {
    pools: Vec<PoolRef>,
    graph: HashMap<Token, Vec<(PoolRef, Token)>>,
}

impl RouteFinder {
    /// Creates a new route finder from the given pool references.
    pub fn new(pools: Vec<PoolRef>) -> Self {
        let graph = Self::build_graph(&pools);
        Self { pools, graph }
    }

    /// Returns the pool references used by this finder.
    pub fn pools(&self) -> &[PoolRef] {
        &self.pools
    }

    fn build_graph(pools: &[PoolRef]) -> HashMap<Token, Vec<(PoolRef, Token)>> {
        let mut graph: HashMap<Token, Vec<(PoolRef, Token)>> = HashMap::new();

        for pool in pools {
            graph
                .entry(pool.token0().clone())
                .or_default()
                .push((pool.clone(), pool.token1().clone()));
            graph
                .entry(pool.token1().clone())
                .or_default()
                .push((pool.clone(), pool.token0().clone()));
        }

        graph
    }

    /// Finds all routes from `token_in` to `token_out` with at most `max_hops` hops.
    pub fn find_all_routes(
        &self,
        token_in: &Token,
        token_out: &Token,
        max_hops: usize,
    ) -> Vec<Route> {
        let mut all_routes = Vec::new();
        let mut current_path = vec![token_in.clone()];
        let mut current_pools = Vec::new();
        let mut visited = HashSet::new();
        visited.insert(token_in.clone());

        let mut ctx = DfsContext {
            target_token: token_out,
            max_hops,
            current_path: &mut current_path,
            current_pools: &mut current_pools,
            visited: &mut visited,
            all_routes: &mut all_routes,
        };

        self.dfs(token_in, &mut ctx);

        all_routes
    }

    fn dfs(&self, current_token: &Token, ctx: &mut DfsContext<'_>) {
        if current_token == ctx.target_token {
            if !ctx.current_pools.is_empty() {
                ctx.all_routes.push(Route::new(
                    ctx.current_pools.clone(),
                    ctx.current_path.clone(),
                ));
            }
            return;
        }

        if ctx.current_pools.len() >= ctx.max_hops {
            return;
        }

        if let Some(edges) = self.graph.get(current_token) {
            for (pool, next_token) in edges {
                if !ctx.visited.contains(next_token) {
                    ctx.visited.insert(next_token.clone());
                    ctx.current_pools.push(pool.clone());
                    ctx.current_path.push(next_token.clone());

                    self.dfs(next_token, ctx);

                    ctx.current_path.pop();
                    ctx.current_pools.pop();
                    ctx.visited.remove(next_token);
                }
            }
        }
    }

    /// Compares all routes by gas-adjusted net output.
    pub fn compare_routes(
        &self,
        token_in: &Token,
        token_out: &Token,
        amount_in: u128,
        gas_price_gwei: u128,
        max_hops: usize,
    ) -> Vec<RouteComparison> {
        let routes = self.find_all_routes(token_in, token_out, max_hops);
        let mut comparisons = Vec::new();

        let gas_price_wei = gas_price_gwei * WEI_PER_GWEI;
        let scale_out = DECIMAL_BASE.pow(token_out.decimals as u32);
        let scale_eth: u128 = DECIMAL_BASE.pow(ETH_DECIMALS as u32);

        for route in routes {
            let gross_output_res = route.get_output(amount_in);
            if let Ok(gross_output) = gross_output_res {
                let gas_estimate = route.estimate_gas();
                let gas_cost_eth = gas_estimate * gas_price_wei;

                let gc_eth = U256::from(gas_cost_eth);
                let so = U256::from(scale_out);
                let se = U256::from(scale_eth);

                let gas_cost_in_output_token = (gc_eth * so / se).as_u128();

                let net_output = gross_output.saturating_sub(gas_cost_in_output_token);

                comparisons.push(RouteComparison {
                    route,
                    gross_output,
                    gas_estimate,
                    gas_cost_eth,
                    net_output,
                });
            }
        }

        comparisons
    }

    /// Returns the route with the highest gas-adjusted net output.
    pub fn find_best_route(
        &self,
        token_in: &Token,
        token_out: &Token,
        amount_in: u128,
        gas_price_gwei: u128,
        max_hops: usize,
    ) -> PricingResult<(Route, u128)> {
        let comparisons =
            self.compare_routes(token_in, token_out, amount_in, gas_price_gwei, max_hops);

        let best = comparisons.into_iter().max_by_key(|c| c.net_output);

        match best {
            Some(c) => Ok((c.route, c.net_output)),
            None => Err(PricingError::NoRouteExists),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Address;
    use proptest::prelude::*;

    fn mock_token(symbol: &str, addr_hex: &str) -> Token {
        Token {
            address: Address::new(addr_hex).unwrap(),
            symbol: symbol.to_string(),
            decimals: 18,
        }
    }

    fn setup_pools() -> (Token, Token, Token, Vec<PoolRef>) {
        let shib = mock_token("SHIB", "0x0000000000000000000000000000000000000001");
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002");
        let eth = mock_token("ETH", "0x0000000000000000000000000000000000000003");

        let pool_shib_usdc = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            usdc.clone(),
            100_000_000_000_000_000_000,
            100_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let pool_shib_eth = UniswapV2Pair::new(
            Address::new("0x2000000000000000000000000000000000000000").unwrap(),
            shib.clone(),
            eth.clone(),
            10_000_000_000_000_000_000_000,
            10_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let pool_eth_usdc = UniswapV2Pair::new(
            Address::new("0x3000000000000000000000000000000000000000").unwrap(),
            eth.clone(),
            usdc.clone(),
            10_000_000_000_000_000_000_000,
            10_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        (
            shib,
            usdc,
            eth,
            vec![
                PoolRef::V2(pool_shib_usdc),
                PoolRef::V2(pool_shib_eth),
                PoolRef::V2(pool_eth_usdc),
            ],
        )
    }

    #[test]
    fn test_direct_vs_multihop() {
        let (shib, usdc, _eth, pools) = setup_pools();
        let finder = RouteFinder::new(pools);
        let amount_in = 10_000_000_000_000_000_000;
        let (best_route, _net_out) = finder
            .find_best_route(&shib, &usdc, amount_in, 0, 3)
            .unwrap();
        assert_eq!(best_route.num_hops(), 2);
        assert_eq!(best_route.path[0].symbol, "SHIB");
        assert_eq!(best_route.path[1].symbol, "ETH");
        assert_eq!(best_route.path[2].symbol, "USDC");
    }

    #[test]
    fn test_gas_makes_direct_better() {
        let (shib, usdc, _eth, pools) = setup_pools();
        let finder = RouteFinder::new(pools);
        let amount_in = 10_000_000_000_000_000_000;
        let gas_price_gwei = 10_000;
        let (best_route, _net_out) = finder
            .find_best_route(&shib, &usdc, amount_in, gas_price_gwei, 3)
            .unwrap();
        assert_eq!(best_route.num_hops(), 1);
        assert_eq!(best_route.path.len(), 2);
        assert_eq!(best_route.path[0].symbol, "SHIB");
        assert_eq!(best_route.path[1].symbol, "USDC");
    }

    #[test]
    fn test_no_route_exists() {
        let shib = mock_token("SHIB", "0x0000000000000000000000000000000000000001");
        let usdc = mock_token("USDC", "0x0000000000000000000000000000000000000002");
        let finder = RouteFinder::new(vec![]);
        let res = finder.find_best_route(&shib, &usdc, 1000, 10, 3);
        assert!(matches!(res, Err(PricingError::NoRouteExists)));
    }

    #[test]
    fn test_route_output_matches_sequential_swaps() {
        let (shib, usdc, eth, pools) = setup_pools();
        let multi_route = Route::new(
            vec![pools[1].clone(), pools[2].clone()],
            vec![shib.clone(), eth.clone(), usdc.clone()],
        );
        let amount_in = 5_000_000_000_000_000_000;
        let out1 = pools[1].get_amount_out(amount_in, &shib).unwrap();
        let out2 = pools[2].get_amount_out(out1, &eth).unwrap();
        let route_out = multi_route.get_output(amount_in).unwrap();
        assert_eq!(route_out, out2);
        let intermediate = multi_route.get_intermediate_amounts(amount_in).unwrap();
        assert_eq!(intermediate.len(), 3);
        assert_eq!(intermediate[0], amount_in);
        assert_eq!(intermediate[1], out1);
        assert_eq!(intermediate[2], out2);
    }

    #[test]
    fn test_gas_flip_changes_best_route_by_net_output() {
        let (shib, usdc, _eth, pools) = setup_pools();
        let finder = RouteFinder::new(pools);
        let amount_in = 10_000_000_000_000_000_000u128;

        let low_gas = finder.compare_routes(&shib, &usdc, amount_in, 0, 3);
        let low_best = low_gas.iter().max_by_key(|c| c.net_output).unwrap();
        let low_direct = low_gas.iter().find(|c| c.route.num_hops() == 1).unwrap();
        assert_eq!(low_best.route.num_hops(), 2);
        assert!(low_best.gross_output > low_direct.gross_output);

        let high_gas = finder.compare_routes(&shib, &usdc, amount_in, 10_000, 3);
        let high_best = high_gas.iter().max_by_key(|c| c.net_output).unwrap();
        let high_direct = high_gas.iter().find(|c| c.route.num_hops() == 1).unwrap();
        let high_multihop = high_gas.iter().find(|c| c.route.num_hops() == 2).unwrap();

        assert_eq!(high_best.route.num_hops(), 1);
        assert!(high_direct.net_output >= high_multihop.net_output);
    }

    proptest! {
        #[test]
        fn prop_multihop_output_matches_sequential_for_any_amount(
            amount_in in 1u128..1_000_000_000_000_000_000_000u128,
        ) {
            let (shib, usdc, eth, pools) = setup_pools();
            let route = Route::new(
                vec![pools[1].clone(), pools[2].clone()],
                vec![shib.clone(), eth.clone(), usdc.clone()],
            );

            let out1 = pools[1].get_amount_out(amount_in, &shib).unwrap();
            let out2 = pools[2].get_amount_out(out1, &eth).unwrap();
            let route_out = route.get_output(amount_in).unwrap();

            prop_assert_eq!(route_out, out2);
        }

        #[test]
        fn prop_route_comparison_has_consistent_net_math(
            amount_in in 1u128..1_000_000_000_000_000_000_000u128,
            gas_price_gwei in 0u128..50_000u128,
        ) {
            let (shib, usdc, _eth, pools) = setup_pools();
            let finder = RouteFinder::new(pools);

            let comparisons = finder.compare_routes(&shib, &usdc, amount_in, gas_price_gwei, 3);
            prop_assert!(!comparisons.is_empty());

            for cmp in comparisons {
                prop_assert!(cmp.net_output <= cmp.gross_output);
                prop_assert!(cmp.gas_estimate > 0);
            }
        }
    }
}
