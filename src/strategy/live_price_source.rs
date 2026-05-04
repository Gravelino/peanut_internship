//! Live on-chain DEX pricing via Uniswap V2/V3 pools.
//!
//! `LivePriceSource` subscribes to `newHeads` via WebSocket and refreshes
//! pool state on every new block — exactly like CEX bookTicker streams
//! give real-time bid/ask. DEX prices are always block-fresh; CEX prices
//! come from the exchange order-book.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ethers::prelude::StreamExt;
use ethers::providers::{Middleware, Provider, Ws};
use ethers::types::U64;
use rust_decimal::{Decimal, prelude::FromPrimitive};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::chain::ChainClient;
use crate::core::types::{Address, Token};
use crate::exchange::client::ExchangeClient;
use crate::pricing::{UniswapV2Pair, UniswapV3Pool, V3QuoterConfig};
use crate::strategy::errors::{StrategyError, StrategyResult};
use crate::strategy::generator::{
    CexOrderBookSource, PriceSource, RestCexOrderBookSource, StubPriceSource, VenuePrices,
};

/// One pair's live-pricing state: pool address, cached pool metadata (with
/// most-recently-read reserves), and the base/quote [`Token`]s so we can
/// feed the right asset into [`UniswapV2Pair::get_amount_out`] /
/// [`UniswapV2Pair::get_amount_in`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePoolKind {
    V2,
    V3,
}

pub struct LivePoolConfig {
    pub pair_name: String,
    pub address: Address,
    pub base: Token,
    pub quote: Token,
    pub kind: LivePoolKind,
    pub quoter: Option<V3QuoterConfig>,
}

#[derive(Clone)]
enum LivePool {
    V2(Arc<RwLock<UniswapV2Pair>>),
    V3(Arc<RwLock<UniswapV3Pool>>),
}

#[derive(Clone)]
struct PoolEntry {
    address: Address,
    pool: LivePool,
    base: Token,
    quote: Token,
    quoter: Option<V3QuoterConfig>,
}

/// Live price source backed by Uniswap V2 pool reserves.
///
/// Construct via [`LivePriceSource::new`], providing a `ChainClient` for
/// on-chain reads, an `ExchangeClient` for CEX order-book access, and a
/// per-pair mapping `(pool_address, base_token, quote_token)`. Pool
/// metadata is fetched once at construction; only reserves are re-read
/// per tick.
/// Cached DEX prices for a single pair, updated on every new block
/// by the background WS listener.
struct DexStateCache {
    updated_at: std::time::Instant,
}

/// If the WS block feed is down and the cache is older than this,
/// fall back to a synchronous RPC fetch per tick.
const DEX_CACHE_STALE_SECS: u64 = 5;

pub struct LivePriceSource {
    cex: Arc<dyn CexOrderBookSource>,
    client: ChainClient,
    pools: HashMap<String, PoolEntry>,
    dex_cache: Arc<RwLock<HashMap<String, DexStateCache>>>,
}

impl LivePriceSource {
    /// Loads pool metadata for every `(pair, pool, base, quote)` tuple and
    /// returns a ready-to-use source. Fails fast on the first pool whose
    /// on-chain metadata cannot be read — a silent fallback here would
    /// mask a misconfigured address book.
    pub async fn new(
        exchange: Arc<ExchangeClient>,
        client: ChainClient,
        pools: Vec<(String, Address, Token, Token)>,
    ) -> StrategyResult<Self> {
        let pools = pools
            .into_iter()
            .map(|(pair_name, address, base, quote)| LivePoolConfig {
                pair_name,
                address,
                base,
                quote,
                kind: LivePoolKind::V2,
                quoter: None,
            })
            .collect();
        Self::new_with_order_books(
            Arc::new(RestCexOrderBookSource::new(exchange)),
            client,
            pools,
        )
        .await
    }

    pub async fn new_with_order_books(
        cex: Arc<dyn CexOrderBookSource>,
        client: ChainClient,
        pools: Vec<LivePoolConfig>,
    ) -> StrategyResult<Self> {
        let mut entries = HashMap::with_capacity(pools.len());
        for config in pools {
            let pair_name = config.pair_name;
            let address = config.address;
            let base = config.base;
            let quote = config.quote;
            let quoter = config.quoter;
            let pool = match config.kind {
                LivePoolKind::V2 => {
                    let pair = UniswapV2Pair::from_chain(address.clone(), &client)
                        .await
                        .map_err(|e| {
                            StrategyError::Pricing(format!(
                                "load pool {pair_name} at {address}: {e}"
                            ))
                        })?;
                    debug!(
                        pair = %pair_name,
                        pool = %address,
                        reserve0 = pair.reserve0,
                        reserve1 = pair.reserve1,
                        "loaded live DEX pool"
                    );
                    LivePool::V2(Arc::new(RwLock::new(pair)))
                }
                LivePoolKind::V3 => {
                    let pool = UniswapV3Pool::from_chain(address.clone(), &client)
                        .await
                        .map_err(|e| {
                            StrategyError::Pricing(format!(
                                "load pool {pair_name} at {address}: {e}"
                            ))
                        })?;
                    debug!(
                        pair = %pair_name,
                        pool = %address,
                        fee = pool.fee_bps,
                        liquidity = pool.liquidity,
                        tick = pool.tick,
                        "loaded live DEX pool"
                    );
                    LivePool::V3(Arc::new(RwLock::new(pool)))
                }
            };
            entries.insert(
                pair_name.clone(),
                PoolEntry {
                    address,
                    pool,
                    base,
                    quote,
                    quoter,
                },
            );
        }

        // Initialize cache with fresh state since we just fetched it from chain
        let mut initial_cache = HashMap::new();
        for k in entries.keys() {
            initial_cache.insert(
                k.clone(),
                DexStateCache {
                    updated_at: std::time::Instant::now(),
                },
            );
        }

        Ok(Self {
            cex,
            client,
            pools: entries,
            dex_cache: Arc::new(RwLock::new(initial_cache)),
        })
    }

    /// Starts the background block-listener that refreshes DEX prices on
    /// every new Arbitrum block (~250 ms). Must be called after
    /// construction; `fetch_prices` falls back to a synchronous RPC
    /// fetch until the first block event arrives.
    pub async fn start_block_feed(&self, ws_url: &str, _size: Decimal) -> StrategyResult<()> {
        // Probe: verify WS connectivity before spawning the long-lived task.
        let probe = Provider::<Ws>::connect(ws_url)
            .await
            .map_err(|e| StrategyError::Pricing(format!("WS connect failed: {e}")))?;
        let _ = probe
            .subscribe_blocks()
            .await
            .map_err(|e| StrategyError::Pricing(format!("subscribe_blocks probe failed: {e}")))?;
        drop(probe);

        // Reconnect inside the spawned task so the provider lives as long
        // as the stream (same pattern as PriceFeed).
        let live_ws = Provider::<Ws>::connect(ws_url)
            .await
            .map_err(|e| StrategyError::Pricing(format!("WS reconnect failed: {e}")))?;

        let client = self.client.clone();
        let dex_cache = self.dex_cache.clone();

        // Clone pool entries for the background task.
        let pool_entries: Vec<(String, PoolEntry)> = self
            .pools
            .iter()
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();

        tokio::spawn(async move {
            let mut block_stream = match live_ws.subscribe_blocks().await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "Failed to subscribe to blocks in DEX feed task");
                    return;
                }
            };

            while let Some(block) = block_stream.next().await {
                let block_number = block.number.map(|n: U64| n.as_u64()).unwrap_or(0);

                for (pair_name, entry) in &pool_entries {
                    let mut updated = false;

                    match &entry.pool {
                        LivePool::V2(pair_lock) => {
                            match UniswapV2Pair::fetch_reserves(&entry.address, &client).await {
                                Ok((r0, r1)) => {
                                    let mut guard = pair_lock.write().await;
                                    guard.reserve0 = r0;
                                    guard.reserve1 = r1;
                                    updated = true;
                                }
                                Err(e) => {
                                    debug!(
                                        pair = %pair_name,
                                        error = %e,
                                        "fetch_reserves failed in block feed"
                                    );
                                }
                            }
                        }
                        LivePool::V3(pool_lock) => {
                            match UniswapV3Pool::fetch_state(&entry.address, &client).await {
                                Ok((sqrt_price_x96, liquidity, tick)) => {
                                    let mut guard = pool_lock.write().await;
                                    guard.sqrt_price_x96 = sqrt_price_x96;
                                    guard.liquidity = liquidity;
                                    guard.tick = tick;
                                    updated = true;
                                }
                                Err(e) => {
                                    debug!(
                                        pair = %pair_name,
                                        error = %e,
                                        "fetch_state failed in block feed"
                                    );
                                }
                            }
                        }
                    };

                    if updated {
                        debug!(
                            pair = %pair_name,
                            block = block_number,
                            "DEX pool state updated from block"
                        );
                        let mut cache = dex_cache.write().await;
                        cache.insert(
                            pair_name.clone(),
                            DexStateCache {
                                updated_at: std::time::Instant::now(),
                            },
                        );
                    }
                }
            }
            warn!("DEX block feed stream ended");
        });

        Ok(())
    }
}

/// Scales a [`Decimal`] by `10^decimals` and truncates to `u128`. Used to
/// convert human-scale trade sizes into the raw integer units expected by
/// [`UniswapV2Pair`]. Returns `None` on overflow or negative input.
fn decimal_to_u128_scaled(value: Decimal, decimals: u8) -> Option<u128> {
    if value < Decimal::ZERO {
        return None;
    }
    let scale = Decimal::from_u128(10u128.pow(decimals as u32))?;
    let scaled = (value * scale).trunc();
    // `to_u128` on a non-negative truncated Decimal is the canonical path.
    use rust_decimal::prelude::ToPrimitive;
    scaled.to_u128()
}

/// Inverse of [`decimal_to_u128_scaled`]. Always succeeds — `u128` fits in
/// `Decimal`'s mantissa for the notional sizes we care about in arb
/// (loses precision only beyond ~28 significant digits).
fn u128_to_decimal_scaled(value: u128, decimals: u8) -> Decimal {
    let raw = Decimal::from_u128(value).unwrap_or(Decimal::ZERO);
    let scale = Decimal::from_u128(10u128.pow(decimals as u32)).unwrap_or(Decimal::ONE);
    raw / scale
}

#[async_trait]
impl PriceSource for LivePriceSource {
    async fn fetch_prices(&self, pair: &str, size: Decimal) -> StrategyResult<VenuePrices> {
        let entry = self.pools.get(pair).ok_or_else(|| {
            StrategyError::Pricing(format!(
                "pair '{pair}' is not configured in the live DEX address book"
            ))
        })?;

        // CEX side: re-read the best bid/ask from the exchange order book.
        let ob = self.cex.fetch_order_book(pair, 20).await?;
        let cex_bid = ob
            .best_bid
            .map(|(p, _)| p)
            .ok_or_else(|| StrategyError::EmptyOrderBook(pair.to_string()))?;
        let cex_ask = ob
            .best_ask
            .map(|(p, _)| p)
            .ok_or_else(|| StrategyError::EmptyOrderBook(pair.to_string()))?;

        // DEX side: if the WS block feed is active (cache fresh), read from
        // it. Otherwise fetch fresh on-chain prices every tick — no stale
        // cache, prices are always live.
        let is_fresh = {
            let cache = self.dex_cache.read().await;
            if let Some(cached) = cache.get(pair) {
                cached.updated_at.elapsed().as_secs() < DEX_CACHE_STALE_SECS
            } else {
                false
            }
        };

        let size_raw = decimal_to_u128_scaled(size, entry.base.decimals)
            .ok_or_else(|| StrategyError::Pricing(format!("size {size} does not fit in u128")))?;

        let (dex_buy, dex_sell) = if is_fresh {
            // Compute locally from locked state
            match &entry.pool {
                LivePool::V2(pair_lock) => {
                    let pool = pair_lock.read().await;
                    let out_raw = pool
                        .get_amount_out(size_raw, &entry.base)
                        .map_err(|e| StrategyError::Pricing(format!("get_amount_out: {e}")))?;
                    let in_raw = pool
                        .get_amount_in(size_raw, &entry.base)
                        .map_err(|e| StrategyError::Pricing(format!("get_amount_in: {e}")))?;
                    let dex_sell = if size > Decimal::ZERO {
                        u128_to_decimal_scaled(out_raw, entry.quote.decimals) / size
                    } else {
                        Decimal::ZERO
                    };
                    let dex_buy = if size > Decimal::ZERO {
                        u128_to_decimal_scaled(in_raw, entry.quote.decimals) / size
                    } else {
                        Decimal::ZERO
                    };
                    (dex_buy, dex_sell)
                }
                LivePool::V3(pool_lock) => {
                    let pool = pool_lock.read().await.clone();

                    // Spot price as base anchor (no RPC, pure local math)
                    let spot_price = pool.get_spot_price(&entry.base).unwrap_or(Decimal::ZERO);

                    // dex_sell: exact input — sell `size` base, get quote
                    let dex_sell = if size_raw > 0 {
                        match pool.quote_swap(size_raw, &entry.base) {
                            Ok(q) if q.amount_out > 0 => {
                                u128_to_decimal_scaled(q.amount_out, entry.quote.decimals) / size
                            }
                            _ => spot_price, // fall back to spot
                        }
                    } else {
                        spot_price
                    };

                    // dex_buy: estimate how much quote to spend for `size` base.
                    // Use dex_sell as the quote estimate (sell price ≈ buy price at small spread),
                    // then simulate the reverse swap.
                    let quote_est = if dex_sell > Decimal::ZERO {
                        dex_sell
                    } else {
                        spot_price
                    };
                    let quote_est_amount =
                        decimal_to_u128_scaled(size * quote_est, entry.quote.decimals).unwrap_or(0);

                    let dex_buy = if quote_est_amount > 0 {
                        match pool.quote_swap(quote_est_amount, &entry.quote) {
                            Ok(q) if q.amount_out > 0 => {
                                let quote_in =
                                    u128_to_decimal_scaled(quote_est_amount, entry.quote.decimals);
                                let base_out =
                                    u128_to_decimal_scaled(q.amount_out, entry.base.decimals);
                                quote_in / base_out
                            }
                            _ => spot_price, // fall back to spot
                        }
                    } else {
                        spot_price
                    };

                    (dex_buy, dex_sell)
                }
            }
        } else {
            // No WS feed (or stale). Fetch live prices via RPC and update state.
            fetch_dex_prices_sync(entry, &self.client, size, size_raw).await?
        };

        Ok(VenuePrices {
            cex_bid,
            cex_ask,
            dex_buy,
            dex_sell,
        })
    }

    async fn get_latest_price(&self, pair: &str) -> StrategyResult<Decimal> {
        // CEX mid price as a baseline
        let ob = self.cex.fetch_order_book(pair, 5).await?;
        let bid = ob
            .best_bid
            .map(|(p, _)| p)
            .ok_or_else(|| StrategyError::EmptyOrderBook(pair.to_string()))?;
        let ask = ob
            .best_ask
            .map(|(p, _)| p)
            .ok_or_else(|| StrategyError::EmptyOrderBook(pair.to_string()))?;
        Ok((bid + ask) / Decimal::TWO)
    }
}

/// One-shot synchronous DEX price fetch (used before the WS feed warms up).
async fn fetch_dex_prices_sync(
    entry: &PoolEntry,
    client: &ChainClient,
    size: Decimal,
    size_raw: u128,
) -> StrategyResult<(Decimal, Decimal)> {
    // Log the current block number so we can verify the RPC is returning
    // fresh data and not a cached response.
    let block_num = client.get_block_number().await.unwrap_or(0);
    debug!(pool = %entry.address, block = block_num, "fetch_dex_prices_sync");
    match &entry.pool {
        LivePool::V2(pair_lock) => {
            let (r0, r1) = UniswapV2Pair::fetch_reserves(&entry.address, client)
                .await
                .map_err(|e| StrategyError::Pricing(format!("fetch_reserves: {e}")))?;
            {
                let mut guard = pair_lock.write().await;
                guard.reserve0 = r0;
                guard.reserve1 = r1;
            }
            let pool = pair_lock.read().await;
            let out_raw = pool
                .get_amount_out(size_raw, &entry.base)
                .map_err(|e| StrategyError::Pricing(format!("get_amount_out: {e}")))?;
            let in_raw = pool
                .get_amount_in(size_raw, &entry.base)
                .map_err(|e| StrategyError::Pricing(format!("get_amount_in: {e}")))?;
            let dex_sell = if size > Decimal::ZERO {
                u128_to_decimal_scaled(out_raw, entry.quote.decimals) / size
            } else {
                Decimal::ZERO
            };
            let dex_buy = if size > Decimal::ZERO {
                u128_to_decimal_scaled(in_raw, entry.quote.decimals) / size
            } else {
                Decimal::ZERO
            };
            Ok((dex_buy, dex_sell))
        }
        LivePool::V3(pool_lock) => {
            let (sqrt_price_x96, liquidity, tick) =
                UniswapV3Pool::fetch_state(&entry.address, client)
                    .await
                    .map_err(|e| StrategyError::Pricing(format!("fetch_state: {e}")))?;
            {
                let mut guard = pool_lock.write().await;
                guard.sqrt_price_x96 = sqrt_price_x96;
                guard.liquidity = liquidity;
                guard.tick = tick;
            }
            let pool = pool_lock.read().await.clone();
            let quoter = entry
                .quoter
                .as_ref()
                .ok_or_else(|| StrategyError::Pricing("V3 pool missing quoter config".into()))?;
            let sell_raw = pool
                .quote_exact_input_single(quoter, client, size_raw, &entry.base)
                .await
                .map_err(|e| StrategyError::Pricing(format!("quoteExactInputSingle: {e}")))?;
            let buy_raw = pool
                .quote_exact_output_single(quoter, client, size_raw, &entry.base)
                .await
                .map_err(|e| StrategyError::Pricing(format!("quoteExactOutputSingle: {e}")))?;
            let dex_sell = if size > Decimal::ZERO {
                u128_to_decimal_scaled(sell_raw, entry.quote.decimals) / size
            } else {
                Decimal::ZERO
            };
            let dex_buy = if size > Decimal::ZERO {
                u128_to_decimal_scaled(buy_raw, entry.quote.decimals) / size
            } else {
                Decimal::ZERO
            };
            Ok((dex_buy, dex_sell))
        }
    }
}

/// Small dispatch enum so `arb_bot` can plumb either a stub or a live
/// price source into a single [`SignalGenerator`] without paying the cost
/// of a trait object or double-monomorphising the bot loop.
pub enum AnyPriceSource {
    /// Synthetic DEX prices for simulation / demo runs.
    Stub(StubPriceSource),
    /// Real Uniswap V2 pool reserves.
    Live(LivePriceSource),
}

#[async_trait]
impl PriceSource for AnyPriceSource {
    async fn fetch_prices(&self, pair: &str, size: Decimal) -> StrategyResult<VenuePrices> {
        match self {
            Self::Stub(s) => s.fetch_prices(pair, size).await,
            Self::Live(l) => l.fetch_prices(pair, size).await,
        }
    }

    async fn get_latest_price(&self, pair: &str) -> StrategyResult<Decimal> {
        match self {
            Self::Stub(s) => s.get_latest_price(pair).await,
            Self::Live(l) => l.get_latest_price(pair).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_to_u128_roundtrip_18_decimals() {
        let raw = decimal_to_u128_scaled(Decimal::new(15, 1), 18).unwrap(); // 1.5 ETH
        assert_eq!(raw, 1_500_000_000_000_000_000u128);
        let back = u128_to_decimal_scaled(raw, 18);
        assert_eq!(back, Decimal::new(15, 1));
    }

    #[test]
    fn decimal_to_u128_rejects_negative() {
        assert!(decimal_to_u128_scaled(Decimal::new(-1, 0), 6).is_none());
    }

    #[test]
    fn u128_to_decimal_6_decimals() {
        // 1_500_000 (USDC, 6 decimals) → 1.5
        assert_eq!(u128_to_decimal_scaled(1_500_000, 6), Decimal::new(15, 1));
    }
}
