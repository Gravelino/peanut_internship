//! Signal generator: probes a pair for opportunities and emits validated [`Signal`]s.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::exchange::client::ExchangeClient;
use crate::inventory::tracker::InventoryTracker;
use crate::inventory::types::Venue;
use crate::strategy::errors::{StrategyError, StrategyResult};
use crate::strategy::fees::FeeStructure;
use crate::strategy::signal::{Direction, Signal, SignalParams};

/// Best-of-book prices for both venues, normalised to quote-per-base.
#[derive(Debug, Clone, Copy)]
pub struct VenuePrices {
    /// Best bid on the CEX (what we can sell to).
    pub cex_bid: Decimal,
    /// Best ask on the CEX (what we must pay to buy).
    pub cex_ask: Decimal,
    /// Price we must pay to buy 1 base unit on the DEX.
    pub dex_buy: Decimal,
    /// Price we receive when selling 1 base unit on the DEX.
    pub dex_sell: Decimal,
}

/// Abstracts the source of venue prices so generators can be tested with
/// pure stubs (no network, no fork, no exchange).
///
/// Using `async_trait` here is the pragmatic choice: trait objects of async
/// fns are not stable in Rust 1.79 yet, and the macro's heap allocation cost
/// is negligible once per tick.
#[async_trait]
pub trait PriceSource: Send + Sync {
    /// Fetches CEX+DEX prices for the given pair and base-asset size.
    async fn fetch_prices(&self, pair: &str, size: Decimal) -> StrategyResult<VenuePrices>;
}

/// Tunables for the signal generator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratorConfig {
    /// Minimum spread in basis points required to consider an opportunity.
    pub min_spread_bps: Decimal,
    /// Minimum net profit in USD required to emit a signal.
    pub min_profit_usd: Decimal,
    /// Maximum notional of a single trade in USD.
    pub max_position_usd: Decimal,
    /// How long an emitted signal remains valid.
    pub signal_ttl: Duration,
    /// Minimum gap between consecutive signals for the same pair.
    pub cooldown: Duration,
}

impl Default for GeneratorConfig {
    fn default() -> Self {
        Self {
            min_spread_bps: Decimal::from(50),
            min_profit_usd: Decimal::from(5),
            max_position_usd: Decimal::from(10_000),
            signal_ttl: Duration::from_secs(5),
            cooldown: Duration::from_secs(2),
        }
    }
}

/// Emits arbitrage signals once prices + inventory + profitability line up.
pub struct SignalGenerator<P: PriceSource> {
    prices: Arc<P>,
    inventory: Arc<RwLock<InventoryTracker>>,
    fees: FeeStructure,
    config: GeneratorConfig,
    last_signal_time: HashMap<String, Instant>,
}

impl<P: PriceSource> SignalGenerator<P> {
    /// Constructs a new generator.
    pub fn new(
        prices: Arc<P>,
        inventory: Arc<RwLock<InventoryTracker>>,
        fees: FeeStructure,
        config: GeneratorConfig,
    ) -> Self {
        Self {
            prices,
            inventory,
            fees,
            config,
            last_signal_time: HashMap::new(),
        }
    }

    /// Returns a shared reference to the inventory tracker.
    pub fn tracker(&self) -> &Arc<RwLock<InventoryTracker>> {
        &self.inventory
    }

    /// Attempts to generate a fresh signal for `pair` at the given base-asset `size`.
    ///
    /// Returns `Ok(None)` when no opportunity exists (cooldown, small spread,
    /// sub-threshold profit). Returns `Err` only for transport/lookup failures.
    pub async fn generate(&mut self, pair: &str, size: Decimal) -> StrategyResult<Option<Signal>> {
        if self.in_cooldown(pair) {
            debug!(pair, "signal generator in cooldown");
            return Ok(None);
        }

        let prices = self.prices.fetch_prices(pair, size).await?;

        if prices.cex_bid <= Decimal::ZERO || prices.cex_ask <= Decimal::ZERO {
            return Err(StrategyError::EmptyOrderBook(pair.to_string()));
        }

        // Two-way spread computation.
        //
        // `spread_buy_cex_sell_dex` — we buy at cex_ask, sell at dex_sell.
        // `spread_buy_dex_sell_cex` — we buy at dex_buy, sell at cex_bid.
        let bps = Decimal::from(10_000);
        let spread_buy_cex_sell_dex = if prices.cex_ask > Decimal::ZERO {
            (prices.dex_sell - prices.cex_ask) / prices.cex_ask * bps
        } else {
            warn!(
                pair,
                cex_ask = %prices.cex_ask,
                "cex_ask is non-positive; treating buy_cex_sell_dex spread as 0"
            );
            Decimal::ZERO
        };
        let spread_buy_dex_sell_cex = if prices.dex_buy > Decimal::ZERO {
            (prices.cex_bid - prices.dex_buy) / prices.dex_buy * bps
        } else {
            warn!(
                pair,
                dex_buy = %prices.dex_buy,
                "dex_buy is non-positive; treating buy_dex_sell_cex spread as 0"
            );
            Decimal::ZERO
        };

        let (direction, spread, cex_price, dex_price) = if spread_buy_cex_sell_dex
            > spread_buy_dex_sell_cex
            && spread_buy_cex_sell_dex >= self.config.min_spread_bps
        {
            (
                Direction::BuyCexSellDex,
                spread_buy_cex_sell_dex,
                prices.cex_ask,
                prices.dex_sell,
            )
        } else if spread_buy_dex_sell_cex >= self.config.min_spread_bps {
            (
                Direction::BuyDexSellCex,
                spread_buy_dex_sell_cex,
                prices.cex_bid,
                prices.dex_buy,
            )
        } else {
            debug!(
                pair,
                cex_bid = %prices.cex_bid,
                cex_ask = %prices.cex_ask,
                dex_buy = %prices.dex_buy,
                dex_sell = %prices.dex_sell,
                spread_cex_to_dex_bps = %spread_buy_cex_sell_dex,
                spread_dex_to_cex_bps = %spread_buy_dex_sell_cex,
                min_spread_bps = %self.config.min_spread_bps,
                "no direction meets min_spread_bps threshold"
            );
            return Ok(None);
        };

        // Economics.
        let trade_value = size * cex_price;
        let gross_pnl = spread / bps * trade_value;
        let fees_usd = self.fees.total_fee_bps(trade_value) / bps * trade_value;
        let net_pnl = gross_pnl - fees_usd;

        if net_pnl < self.config.min_profit_usd {
            debug!(pair, %spread, %net_pnl, "below profit threshold");
            return Ok(None);
        }

        // Validation.
        let inventory_ok = self
            .check_inventory(pair, direction, size, cex_price)
            .await?;
        let within_limits = trade_value <= self.config.max_position_usd;

        let signal = Signal::new(SignalParams {
            pair: pair.to_string(),
            direction,
            cex_price,
            dex_price,
            spread_bps: spread,
            size,
            expected_gross_pnl: gross_pnl,
            expected_fees: fees_usd,
            expected_net_pnl: net_pnl,
            ttl: chrono::Duration::from_std(self.config.signal_ttl)
                .unwrap_or_else(|_| chrono::Duration::seconds(5)),
            inventory_ok,
            within_limits,
        });

        self.last_signal_time
            .insert(pair.to_string(), Instant::now());
        crate::observability::metrics_handle()
            .record_signal_generated(pair, &signal.direction.to_string());
        Ok(Some(signal))
    }

    fn in_cooldown(&self, pair: &str) -> bool {
        match self.last_signal_time.get(pair) {
            Some(t) => t.elapsed() < self.config.cooldown,
            None => false,
        }
    }

    async fn check_inventory(
        &self,
        pair: &str,
        direction: Direction,
        size: Decimal,
        price: Decimal,
    ) -> StrategyResult<bool> {
        let (base, quote) = split_pair(pair)?;
        let buffer = Decimal::new(101, 2); // 1.01
        let inv = self.inventory.read().await;

        // Helper: log-and-default when a venue/asset pair is unknown to the
        // tracker. We treat "unknown" as "zero", but loudly so that missing
        // balance-sync regressions do not silently block execution.
        let available = |venue: Venue, asset: &str| {
            inv.get_available(venue, asset).unwrap_or_else(|| {
                warn!(
                    pair,
                    %venue,
                    asset,
                    "inventory lookup missing; treating available balance as 0"
                );
                Decimal::ZERO
            })
        };

        let ok = match direction {
            Direction::BuyCexSellDex => {
                let quote_needed = size * price * buffer;
                let quote_have = available(Venue::Binance, quote);
                let base_have = available(Venue::Wallet, base);
                quote_have >= quote_needed && base_have >= size
            }
            Direction::BuyDexSellCex => {
                let quote_needed = size * price * buffer;
                let base_have = available(Venue::Binance, base);
                let quote_have = available(Venue::Wallet, quote);
                base_have >= size && quote_have >= quote_needed
            }
        };
        Ok(ok)
    }
}

/// Splits "BASE/QUOTE" into `(base, quote)`.
pub fn split_pair(pair: &str) -> StrategyResult<(&str, &str)> {
    let mut parts = pair.split('/');
    let base = parts
        .next()
        .ok_or_else(|| StrategyError::InvalidPair(pair.to_string()))?;
    let quote = parts
        .next()
        .ok_or_else(|| StrategyError::InvalidPair(pair.to_string()))?;
    if base.is_empty() || quote.is_empty() || parts.next().is_some() {
        return Err(StrategyError::InvalidPair(pair.to_string()));
    }
    Ok((base, quote))
}

/// A [`PriceSource`] backed by a live [`ExchangeClient`] for CEX data and a
/// simulated DEX mid-spread fallback. Used by the runtime bot when a full
/// [`PricingEngine`](crate::pricing::PricingEngine) is not wired up.
///
/// The simulated branch is clearly marked in logs so it never gets mistaken
/// for a production price.
pub struct StubPriceSource {
    exchange: Arc<ExchangeClient>,
    dex_buy_mul: Decimal,
    dex_sell_mul: Decimal,
}

impl StubPriceSource {
    /// Creates a stub price source using fixed DEX spread multipliers around CEX mid.
    pub fn new(exchange: Arc<ExchangeClient>) -> Self {
        Self {
            exchange,
            dex_buy_mul: Decimal::new(1005, 3),  // 1.005
            dex_sell_mul: Decimal::new(1008, 3), // 1.008
        }
    }
}

#[async_trait]
impl PriceSource for StubPriceSource {
    async fn fetch_prices(&self, pair: &str, _size: Decimal) -> StrategyResult<VenuePrices> {
        let ob = self.exchange.fetch_order_book(pair, 20).await?;
        let cex_bid = ob
            .best_bid
            .map(|(p, _)| p)
            .ok_or_else(|| StrategyError::EmptyOrderBook(pair.to_string()))?;
        let cex_ask = ob
            .best_ask
            .map(|(p, _)| p)
            .ok_or_else(|| StrategyError::EmptyOrderBook(pair.to_string()))?;
        let mid = (cex_bid + cex_ask) / Decimal::TWO;
        warn!(pair, "using stub DEX prices (no PricingEngine configured)");
        Ok(VenuePrices {
            cex_bid,
            cex_ask,
            dex_buy: mid * self.dex_buy_mul,
            dex_sell: mid * self.dex_sell_mul,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedPrices(VenuePrices);

    #[async_trait]
    impl PriceSource for FixedPrices {
        async fn fetch_prices(&self, _pair: &str, _size: Decimal) -> StrategyResult<VenuePrices> {
            Ok(self.0)
        }
    }

    fn d(v: &str) -> Decimal {
        Decimal::from_str_exact(v).unwrap()
    }

    fn make_tracker() -> Arc<RwLock<InventoryTracker>> {
        use crate::exchange::types::NormalizedBalance;
        use std::collections::HashMap;
        let mut t = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet]);
        let mut cex = HashMap::new();
        cex.insert(
            "USDT".into(),
            NormalizedBalance {
                free: d("1000000"),
                locked: Decimal::ZERO,
                total: d("1000000"),
            },
        );
        cex.insert(
            "ETH".into(),
            NormalizedBalance {
                free: d("1000"),
                locked: Decimal::ZERO,
                total: d("1000"),
            },
        );
        t.update_from_cex(Venue::Binance, cex);
        let mut wallet = HashMap::new();
        wallet.insert("ETH".into(), d("1000"));
        wallet.insert("USDT".into(), d("1000000"));
        t.update_from_wallet(Venue::Wallet, wallet);
        Arc::new(RwLock::new(t))
    }

    fn make_gen(prices: VenuePrices) -> SignalGenerator<FixedPrices> {
        SignalGenerator::new(
            Arc::new(FixedPrices(prices)),
            make_tracker(),
            FeeStructure::default(),
            GeneratorConfig::default(),
        )
    }

    #[tokio::test]
    async fn generates_signal_when_profitable() {
        // cex_ask=2000, dex_sell=2030 -> spread = 150 bps.
        // trade_value = 1 * 2000 = $2000, fees ~ 10+30+(5/2000*10000)=65 bps = $13.
        // gross = 150 bps * $2000 = $30 -> net = $17 > min_profit $5.
        let p = VenuePrices {
            cex_bid: d("1995"),
            cex_ask: d("2000"),
            dex_buy: d("2025"),
            dex_sell: d("2030"),
        };
        let mut g = make_gen(p);
        let sig = g.generate("ETH/USDT", Decimal::ONE).await.unwrap();
        let s = sig.expect("signal");
        assert_eq!(s.direction, Direction::BuyCexSellDex);
        assert!(s.spread_bps >= d("140"));
        assert!(s.expected_net_pnl > Decimal::from(5));
        assert!(s.inventory_ok);
        assert!(s.within_limits);
    }

    #[tokio::test]
    async fn no_signal_when_spread_too_small() {
        let p = VenuePrices {
            cex_bid: d("2000"),
            cex_ask: d("2001"),
            dex_buy: d("2002"),
            dex_sell: d("2003"),
        };
        let mut g = make_gen(p);
        let sig = g.generate("ETH/USDT", Decimal::ONE).await.unwrap();
        assert!(sig.is_none());
    }

    #[tokio::test]
    async fn cooldown_blocks_second_signal() {
        let p = VenuePrices {
            cex_bid: d("1995"),
            cex_ask: d("2000"),
            dex_buy: d("2025"),
            dex_sell: d("2030"),
        };
        let mut g = make_gen(p);
        let first = g.generate("ETH/USDT", Decimal::ONE).await.unwrap();
        assert!(first.is_some());
        let second = g.generate("ETH/USDT", Decimal::ONE).await.unwrap();
        assert!(second.is_none(), "second call should hit cooldown");
    }

    #[tokio::test]
    async fn picks_direction_with_larger_spread() {
        // Skew: DEX much cheaper than CEX -> buy DEX, sell CEX.
        let p = VenuePrices {
            cex_bid: d("2050"),
            cex_ask: d("2055"),
            dex_buy: d("2000"),
            dex_sell: d("2005"),
        };
        let mut g = make_gen(p);
        let s = g.generate("ETH/USDT", Decimal::ONE).await.unwrap().unwrap();
        assert_eq!(s.direction, Direction::BuyDexSellCex);
    }

    #[test]
    fn split_pair_ok() {
        assert_eq!(split_pair("ETH/USDT").unwrap(), ("ETH", "USDT"));
    }

    #[test]
    fn split_pair_err() {
        assert!(split_pair("ETHUSDT").is_err());
        assert!(split_pair("/USDT").is_err());
        assert!(split_pair("ETH/USDT/EXTRA").is_err());
    }
}
