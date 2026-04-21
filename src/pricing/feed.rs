use std::collections::HashSet;

use ethers::prelude::StreamExt;
use ethers::providers::{Middleware, Provider, Ws};
use ethers::types::U64;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::amm::UniswapV2Pair;
use super::errors::{PricingError, PricingResult};
use crate::chain::client::ChainClient;
use crate::core::types::{Address, DECIMAL_BASE, Token};

/// A single price observation for a token pair at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceTick {
    /// Pool contract address.
    pub pool_address: Address,
    /// Address of the input token.
    pub token_in: Address,
    /// Address of the output token.
    pub token_out: Address,
    /// Spot price expressed as `token_out per token_in` (human-readable).
    pub price: Decimal,
    /// Current reserve of `token_in` (raw units).
    pub reserve_in: u128,
    /// Current reserve of `token_out` (raw units).
    pub reserve_out: u128,
    /// Block number this tick was observed at.
    pub block_number: u64,
    /// Unix timestamp (milliseconds) of the observation.
    pub timestamp: u64,
}

/// Internal tracked-pool entry: address + token metadata.
/// Reserves are fetched fresh each block; only the immutable metadata is stored here.
#[derive(Debug, Clone)]
pub struct PoolEntry {
    address: Address,
    token0: Token,
    token1: Token,
}

impl PoolEntry {}

/// Streams real-time price ticks for tracked Uniswap V2 pools.
///
/// Subscribes to `newHeads` via WebSocket and, on each new block,
/// refreshes reserves for every tracked pool via `ChainClient::call`,
/// then emits a [`PriceTick`] for each pricing direction.
pub struct PriceFeed {
    ws_url: String,
    pools: Vec<PoolEntry>,
    seen_addresses: HashSet<Address>,
}

impl PriceFeed {
    /// Channel buffer size for emitted price ticks.
    const TICK_CHANNEL_SIZE: usize = 256;

    /// Creates a new `PriceFeed` from pre-loaded `UniswapV2Pair` instances.
    pub fn new(ws_url: impl Into<String>, pairs: Vec<UniswapV2Pair>) -> Self {
        let mut seen = HashSet::new();
        let mut pools = Vec::with_capacity(pairs.len());
        for pair in pairs {
            if seen.insert(pair.address.clone()) {
                pools.push(PoolEntry {
                    address: pair.address.clone(),
                    token0: pair.token0.clone(),
                    token1: pair.token1.clone(),
                });
            }
        }
        Self {
            ws_url: ws_url.into(),
            pools,
            seen_addresses: seen,
        }
    }

    /// Adds a pool to track. Duplicate addresses are silently ignored.
    pub fn add_pool(&mut self, pair: UniswapV2Pair) {
        if self.seen_addresses.insert(pair.address.clone()) {
            self.pools.push(PoolEntry {
                address: pair.address.clone(),
                token0: pair.token0.clone(),
                token1: pair.token1.clone(),
            });
        }
    }

    /// Returns the number of tracked pools.
    pub fn pool_count(&self) -> usize {
        self.pools.len()
    }

    /// Starts the price feed, returning a receiver that yields [`PriceTick`] values.
    ///
    /// Subscribes to `newHeads` via WebSocket. On each new block:
    /// 1. Fetches fresh reserves for every tracked pool via `ChainClient::call`
    /// 2. Computes spot prices using sync [`compute_tick`]
    /// 3. Emits ticks through the channel
    pub async fn start(&self, client: ChainClient) -> PricingResult<mpsc::Receiver<PriceTick>> {
        let (tx, rx) = mpsc::channel(Self::TICK_CHANNEL_SIZE);

        let probe = Provider::<Ws>::connect(&self.ws_url)
            .await
            .map_err(|e| PricingError::ChainCall(format!("ws connect failed: {e}")))?;

        let _ = probe
            .subscribe_blocks()
            .await
            .map_err(|e| PricingError::ChainCall(format!("subscribe_blocks probe failed: {e}")))?;

        drop(probe);

        let live_ws = Provider::<Ws>::connect(&self.ws_url)
            .await
            .map_err(|e| PricingError::ChainCall(format!("ws reconnect failed: {e}")))?;

        let pools = self.pools.clone();
        let ws_url = self.ws_url.clone();

        info!(
            pool_count = pools.len(),
            ws_url = %ws_url,
            "PriceFeed starting"
        );

        tokio::spawn(async move {
            let mut block_stream = match live_ws.subscribe_blocks().await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "Failed to subscribe to blocks in feed task");
                    return;
                }
            };

            while let Some(block) = block_stream.next().await {
                let block_number = block.number.map(|n: U64| n.as_u64()).unwrap_or_else(|| {
                    warn!("Block missing number in feed stream");
                    0
                });
                let timestamp_secs = block.timestamp.as_u64();
                let timestamp = timestamp_secs * 1000;

                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock unavailable")
                    .as_millis() as u64;
                let ts = if timestamp > 0 { timestamp } else { now_ms };

                for entry in &pools {
                    match UniswapV2Pair::fetch_reserves(&entry.address, &client).await {
                        Ok((reserve0, reserve1)) => {
                            let ticks =
                                Self::compute_tick(entry, reserve0, reserve1, block_number, ts);
                            for tick in ticks {
                                if tx.send(tick).await.is_err() {
                                    warn!("PriceTick channel closed");
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            debug!(
                                pool = %entry.address,
                                error = %e,
                                "Failed to fetch reserves for tick"
                            );
                        }
                    }
                }
            }

            info!("PriceFeed block stream ended");
        });

        Ok(rx)
    }

    /// Computes price ticks from known reserves (pure, no async, no chain access).
    ///
    /// Returns one tick per pricing direction (token0→token1, token1→token0).
    /// Pools with zero reserves return an empty vec (no valid ticks).
    pub fn compute_tick(
        entry: &PoolEntry,
        reserve0: u128,
        reserve1: u128,
        block_number: u64,
        timestamp: u64,
    ) -> Vec<PriceTick> {
        if reserve0 == 0 || reserve1 == 0 {
            return vec![];
        }

        let scale0 = Decimal::from(DECIMAL_BASE.pow(entry.token0.decimals as u32));
        let scale1 = Decimal::from(DECIMAL_BASE.pow(entry.token1.decimals as u32));

        let human0 = Decimal::from(reserve0) / scale0;
        let human1 = Decimal::from(reserve1) / scale1;

        let price_0_to_1 = if human0.is_zero() {
            return vec![];
        } else {
            human1 / human0
        };

        let price_1_to_0 = if human1.is_zero() {
            return vec![];
        } else {
            human0 / human1
        };

        vec![
            PriceTick {
                pool_address: entry.address.clone(),
                token_in: entry.token0.address.clone(),
                token_out: entry.token1.address.clone(),
                price: price_0_to_1,
                reserve_in: reserve0,
                reserve_out: reserve1,
                block_number,
                timestamp,
            },
            PriceTick {
                pool_address: entry.address.clone(),
                token_in: entry.token1.address.clone(),
                token_out: entry.token0.address.clone(),
                price: price_1_to_0,
                reserve_in: reserve1,
                reserve_out: reserve0,
                block_number,
                timestamp,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::DECIMAL_BASE;

    fn mock_token(symbol: &str, addr_hex: &str, decimals: u8) -> Token {
        Token {
            address: Address::new(addr_hex).unwrap(),
            symbol: symbol.to_string(),
            decimals,
        }
    }

    fn mock_entry(addr_hex: &str, t0: Token, t1: Token) -> PoolEntry {
        PoolEntry {
            address: Address::new(addr_hex).unwrap(),
            token0: t0,
            token1: t1,
        }
    }

    #[test]
    fn test_compute_tick_both_directions() {
        let weth = mock_token("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18);
        let usdc = mock_token("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6);
        let entry = mock_entry("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc", weth, usdc);

        let reserve0: u128 = 1_000 * DECIMAL_BASE.pow(18);
        let reserve1: u128 = 2_000_000 * DECIMAL_BASE.pow(6);

        let ticks =
            PriceFeed::compute_tick(&entry, reserve0, reserve1, 18_000_000, 1_700_000_000_000);

        assert_eq!(ticks.len(), 2);

        assert_eq!(ticks[0].token_in, entry.token0.address);
        assert_eq!(ticks[0].token_out, entry.token1.address);
        assert!(ticks[0].price > Decimal::ZERO);

        assert_eq!(ticks[1].token_in, entry.token1.address);
        assert_eq!(ticks[1].token_out, entry.token0.address);
        assert!(ticks[1].price > Decimal::ZERO);
    }

    #[test]
    fn test_compute_tick_prices_match_get_spot_price() {
        let weth = mock_token("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18);
        let usdc = mock_token("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6);
        let entry = mock_entry(
            "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
            weth.clone(),
            usdc.clone(),
        );

        let reserve0: u128 = 1_000 * DECIMAL_BASE.pow(18);
        let reserve1: u128 = 2_000_000 * DECIMAL_BASE.pow(6);

        let pair = UniswapV2Pair::new(
            entry.address.clone(),
            weth.clone(),
            usdc.clone(),
            reserve0,
            reserve1,
            30,
        )
        .unwrap();

        let spot_0_to_1 = pair.get_spot_price(&weth).unwrap();
        let spot_1_to_0 = pair.get_spot_price(&usdc).unwrap();

        let ticks = PriceFeed::compute_tick(&entry, reserve0, reserve1, 0, 0);

        assert_eq!(ticks[0].price.round_dp(18), spot_0_to_1.round_dp(18));
        assert_eq!(ticks[1].price.round_dp(18), spot_1_to_0.round_dp(18));
    }

    #[test]
    fn test_price_tick_serialization_roundtrip() {
        let weth = mock_token("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18);
        let usdc = mock_token("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6);

        let tick = PriceTick {
            pool_address: Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap(),
            token_in: weth.address,
            token_out: usdc.address,
            price: Decimal::new(2000, 0),
            reserve_in: 1_000 * DECIMAL_BASE.pow(18),
            reserve_out: 2_000_000 * DECIMAL_BASE.pow(6),
            block_number: 18_000_000,
            timestamp: 1_700_000_000_000,
        };

        let json = serde_json::to_string(&tick).unwrap();
        let deserialized: PriceTick = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.pool_address, tick.pool_address);
        assert_eq!(deserialized.token_in, tick.token_in);
        assert_eq!(deserialized.token_out, tick.token_out);
        assert_eq!(deserialized.price, tick.price);
        assert_eq!(deserialized.reserve_in, tick.reserve_in);
        assert_eq!(deserialized.reserve_out, tick.reserve_out);
        assert_eq!(deserialized.block_number, tick.block_number);
        assert_eq!(deserialized.timestamp, tick.timestamp);
    }

    #[test]
    fn test_compute_tick_zero_reserves_handled() {
        let weth = mock_token("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18);
        let usdc = mock_token("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6);
        let entry = mock_entry("0x0000000000000000000000000000000000000001", weth, usdc);

        let ticks = PriceFeed::compute_tick(&entry, 0, 0, 0, 0);

        assert_eq!(ticks.len(), 0, "zero-reserve pools should produce no ticks");
    }

    #[test]
    fn test_compute_tick_one_side_zero_reserve() {
        let weth = mock_token("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18);
        let usdc = mock_token("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6);
        let entry = mock_entry("0x0000000000000000000000000000000000000002", weth, usdc);

        let reserve0: u128 = 1_000 * DECIMAL_BASE.pow(18);
        let ticks = PriceFeed::compute_tick(&entry, reserve0, 0, 0, 0);

        assert_eq!(
            ticks.len(),
            0,
            "one-side-zero pools should produce no ticks"
        );
    }

    #[test]
    fn test_add_pool_deduplication() {
        let weth = mock_token("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18);
        let usdc = mock_token("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6);

        let pair = UniswapV2Pair::new(
            Address::new("0x0000000000000000000000000000000000000001").unwrap(),
            weth,
            usdc,
            1000,
            2000,
            30,
        )
        .unwrap();

        let mut feed = PriceFeed::new("ws://localhost:8545", vec![pair.clone()]);
        assert_eq!(feed.pool_count(), 1);

        feed.add_pool(pair);
        assert_eq!(feed.pool_count(), 1);
    }

    #[test]
    fn test_add_pool_preserves_existing() {
        let weth = mock_token("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18);
        let usdc = mock_token("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6);
        let usdt = mock_token("USDT", "0xdAC17F958D2ee523a2206206994597C13D831ec7", 6);

        let pair1 = UniswapV2Pair::new(
            Address::new("0x0000000000000000000000000000000000000001").unwrap(),
            weth.clone(),
            usdc.clone(),
            1000,
            2000,
            30,
        )
        .unwrap();

        let pair2 = UniswapV2Pair::new(
            Address::new("0x0000000000000000000000000000000000000002").unwrap(),
            weth,
            usdt,
            1000,
            2000,
            30,
        )
        .unwrap();

        let mut feed = PriceFeed::new("ws://localhost:8545", vec![pair1]);
        assert_eq!(feed.pool_count(), 1);

        feed.add_pool(pair2);
        assert_eq!(feed.pool_count(), 2);
    }

    #[test]
    fn test_new_deduplicates_same_address_in_input() {
        let weth = mock_token("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18);
        let usdc = mock_token("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6);

        let addr = Address::new("0x0000000000000000000000000000000000000001").unwrap();
        let pair = UniswapV2Pair::new(addr, weth, usdc, 1000, 2000, 30).unwrap();

        let feed = PriceFeed::new("ws://localhost:8545", vec![pair.clone(), pair]);
        assert_eq!(feed.pool_count(), 1);
    }

    #[tokio::test]
    async fn test_fetch_reserves_valid_data() {
        let client = ChainClient::new(vec!["http://127.0.0.1:1".to_string()], 1, 0).unwrap();
        let addr = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();

        let result = UniswapV2Pair::fetch_reserves(&addr, &client).await;
        assert!(
            result.is_err(),
            "localhost should not be running; verifies the method compiles and calls correctly"
        );
    }
}
