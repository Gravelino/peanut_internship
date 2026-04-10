use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::chain::client::ChainClient;
use crate::core::types::{Address, Token};
use crate::pricing::UniswapV2Pair;
use crate::pricing::errors::PricingError;
use crate::pricing::mempool::{MempoolMonitor, ParsedSwap};
use crate::pricing::router::{Route, RouteFinder};
use crate::pricing::simulator::ForkSimulator;

/// Default number of hops considered by route search.
const DEFAULT_MAX_HOPS: usize = 3;

/// Validation tolerance denominator for quote consistency checks.
///
/// `1 / 1000` corresponds to 0.1%.
const QUOTE_TOLERANCE_DENOMINATOR: u128 = 1000;

/// Errors returned by [PricingEngine].
#[derive(Debug, Error)]
pub enum QuoteError {
    #[error("pricing engine has no initialized router")]
    RouterNotInitialized,

    #[error("no route could be found")]
    NoRoute,

    #[error("simulation failed: {0}")]
    SimulationFailed(String),

    #[error("pool load failed for {address}: {reason}")]
    PoolLoadFailed { address: String, reason: String },

    #[error("mempool stream failed: {0}")]
    Mempool(String),
}

/// Result alias used by quote and engine operations.
pub type QuoteResult<T> = Result<T, QuoteError>;

/// Final quote produced by [PricingEngine::get_quote].
#[derive(Debug, Clone)]
pub struct Quote {
    /// Selected route used for execution.
    pub route: Route,
    /// Input amount in raw token units.
    pub amount_in: u128,
    /// Route output after gas-aware route scoring.
    pub expected_output: u128,
    /// Output produced by route simulation.
    pub simulated_output: u128,
    /// Estimated gas units for the simulated route.
    pub gas_estimate: u64,
    /// Unix timestamp when quote was produced.
    pub timestamp: f64,
}

impl Quote {
    /// Quote is valid when expected and simulated outputs differ by less than 0.1%.
    pub fn is_valid(&self) -> bool {
        if self.expected_output == 0 {
            return false;
        }

        let diff = self.expected_output.abs_diff(self.simulated_output);
        diff.saturating_mul(QUOTE_TOLERANCE_DENOMINATOR) < self.expected_output
    }
}

/// Main interface for the pricing module.
/// Integrates AMM math, routing, simulation, and mempool monitoring.
pub struct PricingEngine {
    /// On-chain RPC client.
    pub client: ChainClient,
    /// Local fork simulator for pre-trade verification.
    pub simulator: ForkSimulator,
    /// Mempool subscription helper.
    pub monitor: MempoolMonitor,
    /// In-memory cache of loaded pools.
    pub pools: HashMap<Address, UniswapV2Pair>,
    /// Route search graph built from `pools`.
    pub router: Option<RouteFinder>,
    /// Maximum route hops accepted during search.
    pub max_hops: usize,
}

impl PricingEngine {
    /// Creates a new engine with fork simulator and mempool monitor configured.
    pub fn new(
        chain_client: ChainClient,
        fork_url: impl AsRef<str>,
        ws_url: impl Into<String>,
    ) -> Result<Self, PricingError> {
        Ok(Self {
            client: chain_client,
            simulator: ForkSimulator::new(fork_url)?,
            monitor: MempoolMonitor::new(ws_url),
            pools: HashMap::new(),
            router: None,
            max_hops: DEFAULT_MAX_HOPS,
        })
    }

    /// Load pool data from chain and build a route graph.
    pub async fn load_pools(&mut self, pool_addresses: &[Address]) -> QuoteResult<()> {
        for address in pool_addresses {
            let pair = UniswapV2Pair::from_chain(address.clone(), &self.client)
                .await
                .map_err(|e| QuoteError::PoolLoadFailed {
                    address: address.to_string(),
                    reason: e.to_string(),
                })?;
            self.pools.insert(address.clone(), pair);
        }

        self.rebuild_router();
        Ok(())
    }

    /// Refresh a single pool reserve snapshot.
    pub async fn refresh_pool(&mut self, address: &Address) -> QuoteResult<()> {
        let pair = UniswapV2Pair::from_chain(address.clone(), &self.client)
            .await
            .map_err(|e| QuoteError::PoolLoadFailed {
                address: address.to_string(),
                reason: e.to_string(),
            })?;

        self.pools.insert(address.clone(), pair);
        self.rebuild_router();
        Ok(())
    }

    /// Get the best quote and verify the route with simulation.
    pub async fn get_quote(
        &self,
        token_in: &Token,
        token_out: &Token,
        amount_in: u128,
        gas_price_gwei: u128,
        sender: Address,
    ) -> QuoteResult<Quote> {
        let router = self
            .router
            .as_ref()
            .ok_or(QuoteError::RouterNotInitialized)?;

        let (route, net_output) = router
            .find_best_route(
                token_in,
                token_out,
                amount_in,
                gas_price_gwei,
                self.max_hops,
            )
            .map_err(|_| QuoteError::NoRoute)?;

        let simulation = self
            .simulator
            .simulate_route(&route, amount_in, sender)
            .await
            .map_err(|e| QuoteError::SimulationFailed(e.to_string()))?;

        if !simulation.success {
            return Err(QuoteError::SimulationFailed(
                simulation
                    .error
                    .unwrap_or_else(|| "unknown simulation error".to_string()),
            ));
        }

        Ok(Quote {
            route,
            amount_in,
            expected_output: net_output,
            simulated_output: simulation.amount_out,
            gas_estimate: simulation.gas_used,
            timestamp: now_unix_seconds(),
        })
    }

    /// Start mempool monitoring and return receiver with parsed swaps.
    pub async fn start_mempool(&self) -> QuoteResult<tokio::sync::mpsc::Receiver<ParsedSwap>> {
        self.monitor
            .start()
            .await
            .map_err(|e| QuoteError::Mempool(e.to_string()))
    }

    /// Returns true if a pending swap touches any known token in loaded pools.
    pub fn on_mempool_swap(&self, swap: &ParsedSwap) -> bool {
        let Some(token_in) = &swap.token_in else {
            return false;
        };
        let Some(token_out) = &swap.token_out else {
            return false;
        };

        self.pools.values().any(|pool| {
            let t0 = &pool.token0.address;
            let t1 = &pool.token1.address;
            token_in == t0 || token_in == t1 || token_out == t0 || token_out == t1
        })
    }

    fn rebuild_router(&mut self) {
        self.router = Some(RouteFinder::new(self.pools.values().cloned().collect()));
    }
}

fn now_unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::U256;

    fn token(symbol: &str, addr: &str) -> Token {
        Token {
            address: Address::new(addr).unwrap(),
            symbol: symbol.to_string(),
            decimals: 18,
        }
    }

    fn setup_engine() -> (PricingEngine, Token, Token, Token) {
        let shib = token("SHIB", "0x0000000000000000000000000000000000000001");
        let usdc = token("USDC", "0x0000000000000000000000000000000000000002");
        let eth = token("ETH", "0x0000000000000000000000000000000000000003");

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

        let client = ChainClient::new(vec!["http://127.0.0.1:8545".to_string()], 5, 0);
        let mut engine =
            PricingEngine::new(client, "http://127.0.0.1:8545", "ws://127.0.0.1:8545").unwrap();

        engine
            .pools
            .insert(pool_shib_usdc.address.clone(), pool_shib_usdc);
        engine
            .pools
            .insert(pool_shib_eth.address.clone(), pool_shib_eth);
        engine
            .pools
            .insert(pool_eth_usdc.address.clone(), pool_eth_usdc);
        engine.rebuild_router();

        (engine, shib, usdc, eth)
    }

    #[test]
    fn test_quote_is_valid_true_when_within_tolerance() {
        let quote = Quote {
            route: Route::new(vec![], vec![]),
            amount_in: 100,
            expected_output: 1000,
            simulated_output: 1000,
            gas_estimate: 21000,
            timestamp: 1.0,
        };
        assert!(quote.is_valid());
    }

    #[test]
    fn test_quote_is_valid_false_when_outside_tolerance() {
        let quote = Quote {
            route: Route::new(vec![], vec![]),
            amount_in: 100,
            expected_output: 1000,
            simulated_output: 998,
            gas_estimate: 21000,
            timestamp: 1.0,
        };
        assert!(!quote.is_valid());
    }

    #[tokio::test]
    async fn test_get_quote_success() {
        let (engine, shib, usdc, _eth) = setup_engine();
        let sender = Address::new("0x00000000000000000000000000000000000000aa").unwrap();

        let quote = engine
            .get_quote(&shib, &usdc, 10_000_000_000_000_000_000, 0, sender)
            .await
            .unwrap();

        assert!(quote.amount_in > 0);
        assert!(quote.expected_output > 0);
        assert!(quote.simulated_output > 0);
        assert!(quote.gas_estimate > 0);
        assert!(quote.timestamp > 0.0);
    }

    #[tokio::test]
    async fn test_get_quote_fails_without_router() {
        let client = ChainClient::new(vec!["http://127.0.0.1:8545".to_string()], 5, 0);
        let engine =
            PricingEngine::new(client, "http://127.0.0.1:8545", "ws://127.0.0.1:8545").unwrap();
        let t0 = token("A", "0x0000000000000000000000000000000000000011");
        let t1 = token("B", "0x0000000000000000000000000000000000000012");
        let sender = Address::new("0x00000000000000000000000000000000000000bb").unwrap();

        let err = engine
            .get_quote(&t0, &t1, 1000, 0, sender)
            .await
            .unwrap_err();
        assert!(matches!(err, QuoteError::RouterNotInitialized));
    }

    #[test]
    fn test_on_mempool_swap_detects_affected_pool() {
        let (engine, shib, usdc, _eth) = setup_engine();

        let swap = ParsedSwap {
            tx_hash: "0xabc".to_string(),
            router: Address::new("0x0000000000000000000000000000000000000009").unwrap(),
            dex: "UniswapV2".to_string(),
            method: "swapExactTokensForTokens".to_string(),
            token_in: Some(shib.address.clone()),
            token_out: Some(usdc.address.clone()),
            amount_in: U256::from(1u64),
            min_amount_out: U256::from(1u64),
            deadline: U256::from(1u64),
            sender: Address::new("0x0000000000000000000000000000000000000010").unwrap(),
            gas_price: U256::from(1u64),
        };

        assert!(engine.on_mempool_swap(&swap));
    }

    #[test]
    fn test_on_mempool_swap_ignores_unrelated_pool() {
        let (engine, _shib, _usdc, _eth) = setup_engine();

        let swap = ParsedSwap {
            tx_hash: "0xdef".to_string(),
            router: Address::new("0x0000000000000000000000000000000000000009").unwrap(),
            dex: "UniswapV2".to_string(),
            method: "swapExactTokensForTokens".to_string(),
            token_in: Some(Address::new("0x00000000000000000000000000000000000000f1").unwrap()),
            token_out: Some(Address::new("0x00000000000000000000000000000000000000f2").unwrap()),
            amount_in: U256::from(1u64),
            min_amount_out: U256::from(1u64),
            deadline: U256::from(1u64),
            sender: Address::new("0x0000000000000000000000000000000000000010").unwrap(),
            gas_price: U256::from(1u64),
        };

        assert!(!engine.on_mempool_swap(&swap));
    }
}
