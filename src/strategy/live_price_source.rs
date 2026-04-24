//! Live on-chain DEX pricing via Uniswap V2 pool reserves.
//!
//! `LivePriceSource` refreshes pool reserves on every tick and computes
//! execution prices using the exact constant-product math in
//! [`crate::pricing::UniswapV2Pair`]. CEX prices still come from the
//! exchange order-book (same path as [`StubPriceSource`]) so the
//! returned [`VenuePrices`] are directly comparable.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rust_decimal::{Decimal, prelude::FromPrimitive};
use tokio::sync::RwLock;
use tracing::debug;

use crate::chain::ChainClient;
use crate::core::types::{Address, Token};
use crate::exchange::client::ExchangeClient;
use crate::pricing::UniswapV2Pair;
use crate::strategy::errors::{StrategyError, StrategyResult};
use crate::strategy::generator::{PriceSource, StubPriceSource, VenuePrices};

/// One pair's live-pricing state: pool address, cached pool metadata (with
/// most-recently-read reserves), and the base/quote [`Token`]s so we can
/// feed the right asset into [`UniswapV2Pair::get_amount_out`] /
/// [`UniswapV2Pair::get_amount_in`].
struct PoolEntry {
    address: Address,
    /// Cached pool — reserves are refreshed on each `fetch_prices` call, but
    /// token metadata (decimals, fee) never changes for a given pool.
    pair: RwLock<UniswapV2Pair>,
    base: Token,
    quote: Token,
}

/// Live price source backed by Uniswap V2 pool reserves.
///
/// Construct via [`LivePriceSource::new`], providing a `ChainClient` for
/// on-chain reads, an `ExchangeClient` for CEX order-book access, and a
/// per-pair mapping `(pool_address, base_token, quote_token)`. Pool
/// metadata is fetched once at construction; only reserves are re-read
/// per tick.
pub struct LivePriceSource {
    exchange: Arc<ExchangeClient>,
    client: ChainClient,
    pools: HashMap<String, PoolEntry>,
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
        let mut entries = HashMap::with_capacity(pools.len());
        for (pair_name, address, base, quote) in pools {
            let pair = UniswapV2Pair::from_chain(address.clone(), &client)
                .await
                .map_err(|e| {
                    StrategyError::Pricing(format!("load pool {pair_name} at {address}: {e}"))
                })?;
            debug!(
                pair = %pair_name,
                pool = %address,
                reserve0 = pair.reserve0,
                reserve1 = pair.reserve1,
                "loaded live DEX pool"
            );
            entries.insert(
                pair_name,
                PoolEntry {
                    address,
                    pair: RwLock::new(pair),
                    base,
                    quote,
                },
            );
        }
        Ok(Self {
            exchange,
            client,
            pools: entries,
        })
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

        // CEX side: same code-path as StubPriceSource — re-read the best
        // bid/ask from the exchange order book.
        let ob = self.exchange.fetch_order_book(pair, 20).await?;
        let cex_bid = ob
            .best_bid
            .map(|(p, _)| p)
            .ok_or_else(|| StrategyError::EmptyOrderBook(pair.to_string()))?;
        let cex_ask = ob
            .best_ask
            .map(|(p, _)| p)
            .ok_or_else(|| StrategyError::EmptyOrderBook(pair.to_string()))?;

        // DEX side: refresh reserves from the pool, then quote both sides
        // for the caller-supplied base-asset `size`.
        let (r0, r1) = UniswapV2Pair::fetch_reserves(&entry.address, &self.client)
            .await
            .map_err(|e| StrategyError::Pricing(format!("fetch_reserves {pair}: {e}")))?;
        {
            let mut guard = entry.pair.write().await;
            guard.reserve0 = r0;
            guard.reserve1 = r1;
        }
        let pool = entry.pair.read().await;

        let size_raw = decimal_to_u128_scaled(size, entry.base.decimals).ok_or_else(|| {
            StrategyError::Pricing(format!("size {size} does not fit in u128 base units"))
        })?;

        // dex_sell: sell `size` base → receive N quote → price = N / size.
        let out_raw = pool
            .get_amount_out(size_raw, &entry.base)
            .map_err(|e| StrategyError::Pricing(format!("get_amount_out {pair}: {e}")))?;
        let quote_received = u128_to_decimal_scaled(out_raw, entry.quote.decimals);
        let dex_sell = if size > Decimal::ZERO {
            quote_received / size
        } else {
            Decimal::ZERO
        };

        // dex_buy: buy `size` base → pay M quote → price = M / size.
        let in_raw = pool
            .get_amount_in(size_raw, &entry.base)
            .map_err(|e| StrategyError::Pricing(format!("get_amount_in {pair}: {e}")))?;
        let quote_paid = u128_to_decimal_scaled(in_raw, entry.quote.decimals);
        let dex_buy = if size > Decimal::ZERO {
            quote_paid / size
        } else {
            Decimal::ZERO
        };

        Ok(VenuePrices {
            cex_bid,
            cex_ask,
            dex_buy,
            dex_sell,
        })
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
