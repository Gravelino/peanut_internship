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
use crate::exchange::types::OrderBookSnapshot;
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
    /// Fetches only the latest mid-price for a pair (used for risk limit conversion).
    async fn get_latest_price(&self, pair: &str) -> StrategyResult<Decimal>;
}

#[async_trait]
pub trait CexOrderBookSource: Send + Sync {
    async fn fetch_order_book(&self, pair: &str, limit: u32) -> StrategyResult<OrderBookSnapshot>;
}

pub struct RestCexOrderBookSource {
    exchange: Arc<ExchangeClient>,
}

impl RestCexOrderBookSource {
    pub fn new(exchange: Arc<ExchangeClient>) -> Self {
        Self { exchange }
    }
}

#[async_trait]
impl CexOrderBookSource for RestCexOrderBookSource {
    async fn fetch_order_book(&self, pair: &str, limit: u32) -> StrategyResult<OrderBookSnapshot> {
        Ok(self.exchange.fetch_order_book(pair, limit).await?)
    }
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

#[derive(Debug, Clone)]
pub struct MarketState {
    pub cex_bid: Decimal,
    pub cex_ask: Decimal,
    pub dex_buy: Decimal,
    pub dex_sell: Decimal,
    pub spread_buy_cex_bps: Decimal,
    pub spread_buy_dex_bps: Decimal,
    pub size: Decimal,
    pub quote_usd_price: Decimal,
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

    pub fn set_fees(&mut self, fees: FeeStructure) {
        self.fees = fees;
    }

    /// Attempts to generate a fresh signal for `pair` at the given base-asset `size`.
    ///
    /// Returns `Ok(Some(signal))` when an opportunity exists.
    /// Returns `Ok(None)` with market info when no opportunity exists.
    pub async fn generate(
        &mut self,
        pair: &str,
        _cli_size: Decimal,
    ) -> StrategyResult<(Option<Signal>, Option<MarketState>)> {
        if self.in_cooldown(pair) {
            debug!(pair, "signal generator in cooldown");
            return Ok((None, None));
        }

        // 1. Probe spot price with a minimal size to calculate boundaries.
        let probe_size = Decimal::new(1, 3); // 0.001
        let probe_prices = match self.prices.fetch_prices(pair, probe_size).await {
            Ok(p) => p,
            Err(_) => return Ok((None, None)),
        };
        if probe_prices.cex_bid <= Decimal::ZERO || probe_prices.cex_ask <= Decimal::ZERO {
            return Err(StrategyError::EmptyOrderBook(pair.to_string()));
        }

        let mid_price = (probe_prices.cex_bid + probe_prices.cex_ask) / Decimal::TWO;
        let bps = Decimal::from(10_000);
        let spread_buy_cex =
            (probe_prices.dex_sell - probe_prices.cex_ask) / probe_prices.cex_ask * bps;
        let spread_buy_dex =
            (probe_prices.cex_bid - probe_prices.dex_buy) / probe_prices.dex_buy * bps;

        let mut market_state = MarketState {
            cex_bid: probe_prices.cex_bid,
            cex_ask: probe_prices.cex_ask,
            dex_buy: probe_prices.dex_buy,
            dex_sell: probe_prices.dex_sell,
            spread_buy_cex_bps: spread_buy_cex,
            spread_buy_dex_bps: spread_buy_dex,
            size: Decimal::ZERO,
            quote_usd_price: Decimal::ONE,
        };

        let (base, quote) = split_pair(pair)?;

        // 2. Determine conversion price to USD for risk limits
        let quote_usd_price =
            if quote == "USDC" || quote == "USDT" || quote == "USD" || quote == "DAI" {
                Decimal::ONE
            } else {
                let conv_pair = if quote == "WETH" {
                    "ETH/USDC".to_string()
                } else {
                    format!("{}/USDC", quote)
                };
                self.prices
                    .get_latest_price(&conv_pair)
                    .await
                    .map_err(|e| {
                        StrategyError::Pricing(format!(
                            "missing USD conversion price for quote '{quote}' via {conv_pair}: {e}"
                        ))
                    })?
            };
        if quote_usd_price <= Decimal::ZERO {
            return Err(StrategyError::Pricing(format!(
                "invalid USD conversion price for quote '{quote}': {quote_usd_price}"
            )));
        }
        market_state.quote_usd_price = quote_usd_price;

        let price_in_usd = mid_price * quote_usd_price;
        let max_risk_size = if price_in_usd > Decimal::ZERO {
            self.config.max_position_usd / price_in_usd
        } else {
            Decimal::ZERO
        };

        // 3. Read inventory to find theoretical maximums for both directions.
        let inv = self.inventory.read().await;
        let base_wallet = inv
            .get_available(Venue::Wallet, base)
            .unwrap_or(Decimal::ZERO);
        let base_cex = inv
            .get_available(Venue::Binance, base)
            .unwrap_or(Decimal::ZERO);
        let quote_wallet = inv
            .get_available(Venue::Wallet, quote)
            .unwrap_or(Decimal::ZERO);
        let quote_cex = inv
            .get_available(Venue::Binance, quote)
            .unwrap_or(Decimal::ZERO);
        drop(inv);

        let buffer = Decimal::new(101, 2); // 1.01

        let max_base_buy_cex = if probe_prices.cex_ask > Decimal::ZERO {
            std::cmp::min(quote_cex / (probe_prices.cex_ask * buffer), base_wallet)
        } else {
            Decimal::ZERO
        };

        let max_base_buy_dex = if probe_prices.dex_buy > Decimal::ZERO {
            std::cmp::min(quote_wallet / (probe_prices.dex_buy * buffer), base_cex)
        } else {
            Decimal::ZERO
        };

        let max_inv_size = std::cmp::max(max_base_buy_cex, max_base_buy_dex);
        let mut max_size = std::cmp::min(max_inv_size, max_risk_size);

        // Respect CLI size as an upper bound if specified
        if _cli_size > Decimal::ZERO {
            max_size = std::cmp::min(max_size, _cli_size);
        }

        // Fallback to risk limit if inventory is zero or if no manual cap is set
        if max_size <= Decimal::ZERO {
            max_size = if _cli_size > Decimal::ZERO {
                std::cmp::min(_cli_size, max_risk_size)
            } else {
                max_risk_size
            };
        }
        market_state.size = max_size;

        if max_size <= Decimal::ZERO {
            return Ok((None, Some(market_state)));
        }

        // 3. Grid search to find the optimal size (highest net PnL).
        let steps = 10;
        let step_size = max_size / Decimal::from(steps);

        let mut best_signal: Option<Signal> = None;
        let mut best_pnl = Decimal::ZERO;

        debug!(
            pair,
            min_size = %step_size,
            max_size = %max_size,
            steps,
            "probing sizes for optimal PnL"
        );

        for i in 1..=steps {
            let test_size = step_size * Decimal::from(i);
            if let Ok(Some(signal)) = self.evaluate_size(pair, test_size, quote_usd_price).await
                && signal.expected_net_pnl > best_pnl
                && signal.inventory_ok
                && signal.within_limits
            {
                best_pnl = signal.expected_net_pnl;
                best_signal = Some(signal);
            }
        }

        if let Some(signal) = best_signal {
            self.last_signal_time
                .insert(pair.to_string(), Instant::now());
            crate::observability::metrics_handle()
                .record_signal_generated(pair, &signal.direction.to_string());
            Ok((Some(signal), Some(market_state)))
        } else {
            Ok((None, Some(market_state)))
        }
    }

    /// Evaluates a specific trade size and returns the resulting signal if viable.
    async fn evaluate_size(
        &self,
        pair: &str,
        size: Decimal,
        quote_usd_price: Decimal,
    ) -> StrategyResult<Option<Signal>> {
        let prices = self.prices.fetch_prices(pair, size).await?;

        if prices.cex_bid <= Decimal::ZERO || prices.cex_ask <= Decimal::ZERO {
            return Err(StrategyError::EmptyOrderBook(pair.to_string()));
        }

        let bps = Decimal::from(10_000);
        let spread_buy_cex_sell_dex = if prices.cex_ask > Decimal::ZERO {
            (prices.dex_sell - prices.cex_ask) / prices.cex_ask * bps
        } else {
            Decimal::ZERO
        };

        let spread_buy_dex_sell_cex = if prices.dex_buy > Decimal::ZERO {
            (prices.cex_bid - prices.dex_buy) / prices.dex_buy * bps
        } else {
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
            // Only log if it's a significant size to avoid log spam during grid search
            if size > Decimal::new(1, 1) {
                debug!(
                    pair,
                    cex_bid = %prices.cex_bid,
                    cex_ask = %prices.cex_ask,
                    dex_buy = %prices.dex_buy,
                    dex_sell = %prices.dex_sell,
                    spread_cex_to_dex_bps = %spread_buy_cex_sell_dex,
                    spread_dex_to_cex_bps = %spread_buy_dex_sell_cex,
                    min_spread_bps = %self.config.min_spread_bps,
                    size = %size,
                    "no direction meets min_spread_bps threshold"
                );
            }
            return Ok(None);
        };

        let trade_value = size * cex_price * quote_usd_price;
        let gross_pnl = spread / bps * trade_value;
        let fees_usd = self.fees.breakdown(trade_value).total_fee_usd;
        let net_pnl = gross_pnl - fees_usd;

        if net_pnl < self.config.min_profit_usd {
            if size > Decimal::new(1, 1) {
                debug!(
                    pair,
                    %spread,
                    trade_value = %trade_value,
                    gross_pnl = %gross_pnl,
                    fees_usd = %fees_usd,
                    net_pnl = %net_pnl,
                    min_profit_usd = %self.config.min_profit_usd,
                    size = %size,
                    "no signal: below profit threshold"
                );
            }
            return Ok(None);
        }

        let inventory_ok = self
            .check_inventory(pair, direction, size, cex_price)
            .await?;
        let within_limits = trade_value <= self.config.max_position_usd;

        let mut signal = Signal::new(SignalParams {
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
        signal.notional_usd = trade_value;

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
    cex: Arc<dyn CexOrderBookSource>,
    dex_buy_mul: Decimal,
    dex_sell_mul: Decimal,
}

impl StubPriceSource {
    /// Creates a stub price source using fixed DEX spread multipliers around CEX mid.
    pub fn new(exchange: Arc<ExchangeClient>) -> Self {
        Self::new_with_order_books(Arc::new(RestCexOrderBookSource::new(exchange)))
    }

    pub fn new_with_order_books(cex: Arc<dyn CexOrderBookSource>) -> Self {
        Self {
            cex,
            dex_buy_mul: Decimal::new(1005, 3),  // 1.005
            dex_sell_mul: Decimal::new(1008, 3), // 1.008
        }
    }
}

#[async_trait]
impl PriceSource for StubPriceSource {
    async fn fetch_prices(&self, pair: &str, _size: Decimal) -> StrategyResult<VenuePrices> {
        let ob = self.cex.fetch_order_book(pair, 20).await?;
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

    async fn get_latest_price(&self, pair: &str) -> StrategyResult<Decimal> {
        let book = self.cex.fetch_order_book(pair, 5).await?;
        let bid = book
            .best_bid
            .map(|(p, _)| p)
            .ok_or_else(|| StrategyError::EmptyOrderBook(pair.to_string()))?;
        let ask = book
            .best_ask
            .map(|(p, _)| p)
            .ok_or_else(|| StrategyError::EmptyOrderBook(pair.to_string()))?;
        let mid = (bid + ask) / Decimal::TWO;
        Ok(mid)
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

        async fn get_latest_price(&self, _pair: &str) -> StrategyResult<Decimal> {
            Ok((self.0.cex_bid + self.0.cex_ask) / Decimal::TWO)
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
        let (sig, _) = g.generate("ETH/USDT", Decimal::ONE).await.unwrap();
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
        let (sig, _) = g.generate("ETH/USDT", Decimal::ONE).await.unwrap();
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
        let (first, _) = g.generate("ETH/USDT", Decimal::ONE).await.unwrap();
        assert!(first.is_some());
        let (second, _) = g.generate("ETH/USDT", Decimal::ONE).await.unwrap();
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
        let (s, _) = g.generate("ETH/USDT", Decimal::ONE).await.unwrap();
        assert_eq!(s.expect("signal").direction, Direction::BuyDexSellCex);
    }

    #[tokio::test]
    async fn eth_quoted_pair_uses_quote_usd_for_notional_and_risk() {
        struct LinkEthPrices;

        #[async_trait]
        impl PriceSource for LinkEthPrices {
            async fn fetch_prices(
                &self,
                _pair: &str,
                _size: Decimal,
            ) -> StrategyResult<VenuePrices> {
                Ok(VenuePrices {
                    cex_bid: d("0.0045"),
                    cex_ask: d("0.0046"),
                    dex_buy: d("0.0040"),
                    dex_sell: d("0.0041"),
                })
            }

            async fn get_latest_price(&self, pair: &str) -> StrategyResult<Decimal> {
                if pair == "ETH/USDC" {
                    Ok(Decimal::from(3000))
                } else {
                    Ok(Decimal::ONE)
                }
            }
        }

        use crate::exchange::types::NormalizedBalance;
        use std::collections::HashMap;
        let mut tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet]);
        let mut cex = HashMap::new();
        cex.insert(
            "LINK".into(),
            NormalizedBalance {
                free: Decimal::from(100),
                locked: Decimal::ZERO,
                total: Decimal::from(100),
            },
        );
        let mut wallet = HashMap::new();
        wallet.insert("ETH".into(), Decimal::ONE);
        tracker.update_from_cex(Venue::Binance, cex);
        tracker.update_from_wallet(Venue::Wallet, wallet);

        let config = GeneratorConfig {
            max_position_usd: Decimal::from(5),
            min_profit_usd: Decimal::ZERO,
            ..Default::default()
        };
        let fees = FeeStructure {
            cex_taker_bps: Decimal::from(10),
            dex_swap_bps: Decimal::ZERO,
            gas_cost_usd: Decimal::ZERO,
        };
        let mut generator = SignalGenerator::new(
            Arc::new(LinkEthPrices),
            Arc::new(RwLock::new(tracker)),
            fees,
            config,
        );

        let (signal, market) = generator
            .generate("LINK/ETH", Decimal::from(10))
            .await
            .unwrap();
        let signal = signal.expect("signal");
        let market = market.expect("market");
        assert_eq!(market.quote_usd_price, Decimal::from(3000));
        assert!(signal.notional_usd <= Decimal::from(5));
        assert!(signal.notional_usd > Decimal::from(4));
        assert!(signal.size * signal.cex_price < Decimal::new(1, 2));
    }

    #[tokio::test]
    async fn non_usd_quote_requires_explicit_conversion_price() {
        struct MissingConversionPrices;

        #[async_trait]
        impl PriceSource for MissingConversionPrices {
            async fn fetch_prices(
                &self,
                _pair: &str,
                _size: Decimal,
            ) -> StrategyResult<VenuePrices> {
                Ok(VenuePrices {
                    cex_bid: d("0.0045"),
                    cex_ask: d("0.0046"),
                    dex_buy: d("0.0040"),
                    dex_sell: d("0.0041"),
                })
            }

            async fn get_latest_price(&self, pair: &str) -> StrategyResult<Decimal> {
                Err(StrategyError::Pricing(format!("no price for {pair}")))
            }
        }

        let mut generator = SignalGenerator::new(
            Arc::new(MissingConversionPrices),
            make_tracker(),
            FeeStructure::default(),
            GeneratorConfig::default(),
        );

        let err = generator
            .generate("LINK/ETH", Decimal::ONE)
            .await
            .expect_err("missing ETH/USDC conversion must fail fast")
            .to_string();
        assert!(err.contains("missing USD conversion price"));
        assert!(err.contains("ETH/USDC"));
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
