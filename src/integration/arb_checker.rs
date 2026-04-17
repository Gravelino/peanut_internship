use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::chain::ChainClient;
use crate::core::types::Address;
use crate::exchange::client::ExchangeClient;
use crate::exchange::orderbook::OrderBookAnalyzer;
use crate::exchange::price_oracle::{AggregatedPrice, PriceOracle};
use crate::inventory::pnl::PnLEngine;
use crate::inventory::tracker::InventoryTracker;
use crate::inventory::types::Venue;
use crate::pricing::amm::UniswapV2Pair;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbCheckResult {
    pub pair: String,
    pub timestamp: String,
    pub dex_price: Decimal,
    pub dex_price_source: String,
    pub cex_bid: Decimal,
    pub cex_ask: Decimal,
    pub gap_bps: Decimal,
    pub direction: Option<String>,
    pub estimated_costs_bps: Decimal,
    pub estimated_net_pnl_bps: Decimal,
    pub inventory_ok: bool,
    pub executable: bool,
    pub details: ArbCheckDetails,
    pub price_sources: Option<AggregatedPrice>,
    pub dex_pool_info: Option<DexPoolInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DexPoolInfo {
    pub pool_address: String,
    pub reserve0: String,
    pub reserve1: String,
    pub token0: String,
    pub token1: String,
    pub spot_price: Decimal,
    pub execution_price: Decimal,
    pub price_impact_bps: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbCheckDetails {
    pub dex_price_impact_bps: Decimal,
    pub cex_slippage_bps: Decimal,
    pub cex_fee_bps: Decimal,
    pub dex_fee_bps: Decimal,
    pub gas_cost_usd: Decimal,
}

#[derive(Debug)]
pub struct ArbChecker {
    exchange_client: ExchangeClient,
    price_oracle: PriceOracle,
    inventory_tracker: InventoryTracker,
    #[allow(dead_code)]
    pnl_engine: PnLEngine,
}

impl ArbChecker {
    pub fn new(
        exchange_client: ExchangeClient,
        inventory_tracker: InventoryTracker,
        pnl_engine: PnLEngine,
    ) -> Self {
        let binance_url = exchange_client.config().base_url.clone();
        let price_oracle = PriceOracle::new(binance_url);

        Self {
            exchange_client,
            price_oracle,
            inventory_tracker,
            pnl_engine,
        }
    }

    pub async fn check_with_dex(
        &self,
        pair: &str,
        size: Decimal,
        dex_fee_bps: Decimal,
        gas_cost_usd: Decimal,
        fork_url: &str,
        pool_address: &str,
    ) -> Result<ArbCheckResult, ArbCheckError> {
        info!(pair, size = %size, fork_url, pool_address, "Running arb check with DEX fork");

        let chain_client = ChainClient::new(vec![fork_url.to_string()], 20, 2)
            .map_err(|e| ArbCheckError::DexError(e.to_string()))?;

        let pool_addr = Address::new(pool_address)
            .map_err(|e| ArbCheckError::DexError(e.to_string()))?;

        let pool = UniswapV2Pair::from_chain(pool_addr, &chain_client)
            .await
            .map_err(|e| ArbCheckError::DexError(e.to_string()))?;

        let base_asset = pair.split('/').next().unwrap_or("ETH");

        let weth_alias = |s: &str| s == "WETH" || s == "ETH";
        let token_in = if weth_alias(pool.token0.symbol.as_str()) && weth_alias(base_asset) {
            pool.token0.clone()
        } else if weth_alias(pool.token1.symbol.as_str()) && weth_alias(base_asset) {
            pool.token1.clone()
        } else if pool.token0.symbol == base_asset {
            pool.token0.clone()
        } else {
            pool.token1.clone()
        };

        let token_out = if token_in.address == pool.token0.address {
            pool.token1.clone()
        } else {
            pool.token0.clone()
        };

        let amount_in: u128 = (size * Decimal::from(10u64.pow(token_in.decimals as u32)))
            .to_u128()
            .unwrap_or(1_000_000_000_000_000_000);

        let spot_price = pool
            .get_spot_price(&token_in)
            .map_err(|e| ArbCheckError::DexError(e.to_string()))?;

        let amount_out = pool
            .get_amount_out(amount_in, &token_in)
            .map_err(|e| ArbCheckError::DexError(e.to_string()))?;

        let dex_price = if amount_in > 0 && amount_out > 0 {
            let base_units = Decimal::from(amount_in)
                / Decimal::from(10u64.pow(token_in.decimals as u32));
            let quote_units = Decimal::from(amount_out)
                / Decimal::from(10u64.pow(token_out.decimals as u32));
            if base_units > Decimal::ZERO {
                quote_units / base_units
            } else {
                spot_price
            }
        } else {
            spot_price
        };

        let execution_price = pool
            .get_execution_price(amount_in, &token_in)
            .unwrap_or(dex_price);

        let price_impact = pool
            .get_price_impact(amount_in, &token_in)
            .unwrap_or(Decimal::ZERO);
        let price_impact_bps = price_impact * Decimal::from(10000);

        let dex_pool_info = DexPoolInfo {
            pool_address: pool_address.to_string(),
            reserve0: pool.reserve0.to_string(),
            reserve1: pool.reserve1.to_string(),
            token0: pool.token0.symbol.clone(),
            token1: pool.token1.symbol.clone(),
            spot_price,
            execution_price,
            price_impact_bps,
        };

        info!(
            pair,
            dex_price = %dex_price,
            spot_price = %spot_price,
            impact_bps = %price_impact_bps,
            "DEX price from Uniswap V2 fork"
        );

        let orderbook = self
            .exchange_client
            .fetch_order_book(pair, 20)
            .await
            .map_err(ArbCheckError::Exchange)?;

        let analyzer = OrderBookAnalyzer::new(orderbook);

        let cex_bid = analyzer
            .orderbook()
            .best_bid
            .map(|(p, _)| p)
            .unwrap_or(Decimal::ZERO);
        let cex_ask = analyzer
            .orderbook()
            .best_ask
            .map(|(p, _)| p)
            .unwrap_or(Decimal::ZERO);

        let buy_dex_sell_cex_gap = if dex_price > Decimal::ZERO && cex_bid > Decimal::ZERO {
            (cex_bid - dex_price) / dex_price * Decimal::from(10000)
        } else {
            Decimal::ZERO
        };

        let buy_cex_sell_dex_gap = if cex_ask > Decimal::ZERO && dex_price > Decimal::ZERO {
            (dex_price - cex_ask) / cex_ask * Decimal::from(10000)
        } else {
            Decimal::ZERO
        };

        let (direction, gap_bps) = if buy_dex_sell_cex_gap > buy_cex_sell_dex_gap {
            (Some("buy_dex_sell_cex".into()), buy_dex_sell_cex_gap)
        } else if buy_cex_sell_dex_gap > Decimal::ZERO {
            (Some("buy_cex_sell_dex".into()), buy_cex_sell_dex_gap)
        } else {
            (None, Decimal::ZERO)
        };

        let cex_fee_bps = Decimal::from(10);
        let walk_buy = analyzer.walk_the_book("buy", size);
        let walk_sell = analyzer.walk_the_book("sell", size);
        let cex_slippage_bps = walk_buy.slippage_bps.max(walk_sell.slippage_bps);

        let mid_price = analyzer.orderbook().mid_price;
        let gas_cost_bps = if mid_price > Decimal::ZERO && size > Decimal::ZERO {
            gas_cost_usd / (size * mid_price) * Decimal::from(10000)
        } else {
            Decimal::ZERO
        };

        let estimated_costs_bps =
            dex_fee_bps + price_impact_bps + cex_fee_bps + cex_slippage_bps + gas_cost_bps;
        let estimated_net_pnl_bps = gap_bps - estimated_costs_bps;

        let quote_asset = pair.split('/').next_back().unwrap_or("USDT");
        let quote_needed = size * dex_price;

        let inventory_ok = match direction.as_deref() {
            Some("buy_dex_sell_cex") => {
                self.inventory_tracker
                    .can_execute(Venue::Wallet, quote_asset, quote_needed, Venue::Binance, base_asset, size)
                    .can_execute
            }
            Some("buy_cex_sell_dex") => {
                self.inventory_tracker
                    .can_execute(Venue::Binance, quote_asset, quote_needed, Venue::Wallet, base_asset, size)
                    .can_execute
            }
            _ => false,
        };

        let executable = estimated_net_pnl_bps > Decimal::ZERO && inventory_ok && direction.is_some();

        Ok(ArbCheckResult {
            pair: pair.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            dex_price,
            dex_price_source: "uniswap_v2_fork".into(),
            cex_bid,
            cex_ask,
            gap_bps,
            direction,
            estimated_costs_bps,
            estimated_net_pnl_bps,
            inventory_ok,
            executable,
            details: ArbCheckDetails {
                dex_price_impact_bps: price_impact_bps,
                cex_slippage_bps,
                cex_fee_bps,
                dex_fee_bps,
                gas_cost_usd,
            },
            price_sources: None,
            dex_pool_info: Some(dex_pool_info),
        })
    }

    pub async fn check(
        &self,
        pair: &str,
        size: Decimal,
        dex_fee_bps: Decimal,
        gas_cost_usd: Decimal,
    ) -> crate::exchange::errors::ExchangeResult<ArbCheckResult> {
        info!(pair, size = %size, "Running arb check");

        let orderbook = self.exchange_client.fetch_order_book(pair, 20).await?;
        let cex_mid = Some(orderbook.mid_price);
        let analyzer = OrderBookAnalyzer::new(orderbook);

        let agg_price = self.price_oracle.fetch_aggregated(pair, cex_mid).await?;
        let dex_price = agg_price.median;

        let cex_bid = analyzer
            .orderbook()
            .best_bid
            .map(|(p, _)| p)
            .unwrap_or(Decimal::ZERO);
        let cex_ask = analyzer
            .orderbook()
            .best_ask
            .map(|(p, _)| p)
            .unwrap_or(Decimal::ZERO);

        let buy_dex_sell_cex_gap = if dex_price > Decimal::ZERO && cex_bid > Decimal::ZERO {
            (cex_bid - dex_price) / dex_price * Decimal::from(10000)
        } else {
            Decimal::ZERO
        };

        let buy_cex_sell_dex_gap = if cex_ask > Decimal::ZERO && dex_price > Decimal::ZERO {
            (dex_price - cex_ask) / cex_ask * Decimal::from(10000)
        } else {
            Decimal::ZERO
        };

        let (direction, gap_bps) = if buy_dex_sell_cex_gap > buy_cex_sell_dex_gap {
            (Some("buy_dex_sell_cex".into()), buy_dex_sell_cex_gap)
        } else if buy_cex_sell_dex_gap > Decimal::ZERO {
            (Some("buy_cex_sell_dex".into()), buy_cex_sell_dex_gap)
        } else {
            (None, Decimal::ZERO)
        };

        let cex_fee_bps = Decimal::from(10);
        let walk_buy = analyzer.walk_the_book("buy", size);
        let walk_sell = analyzer.walk_the_book("sell", size);
        let cex_slippage_bps = walk_buy.slippage_bps.max(walk_sell.slippage_bps);

        let dex_price_impact_bps = Decimal::from(5);

        let total_cost_bps = dex_fee_bps + dex_price_impact_bps + cex_fee_bps + cex_slippage_bps;

        let mid_price = analyzer.orderbook().mid_price;
        let gas_cost_bps = if mid_price > Decimal::ZERO && size > Decimal::ZERO {
            gas_cost_usd / (size * mid_price) * Decimal::from(10000)
        } else {
            Decimal::ZERO
        };

        let estimated_costs_bps = total_cost_bps + gas_cost_bps;
        let estimated_net_pnl_bps = gap_bps - estimated_costs_bps;

        let base_asset = pair.split('/').next().unwrap_or("ETH");
        let quote_asset = pair.split('/').next_back().unwrap_or("USDT");
        let quote_needed = size * dex_price;

        let inventory_ok = match direction.as_deref() {
            Some("buy_dex_sell_cex") => {
                let check = self.inventory_tracker.can_execute(
                    Venue::Wallet,
                    quote_asset,
                    quote_needed,
                    Venue::Binance,
                    base_asset,
                    size,
                );
                check.can_execute
            }
            Some("buy_cex_sell_dex") => {
                let check = self.inventory_tracker.can_execute(
                    Venue::Binance,
                    quote_asset,
                    quote_needed,
                    Venue::Wallet,
                    base_asset,
                    size,
                );
                check.can_execute
            }
            _ => false,
        };

        let executable =
            estimated_net_pnl_bps > Decimal::ZERO && inventory_ok && direction.is_some();

        Ok(ArbCheckResult {
            pair: pair.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            dex_price,
            dex_price_source: "oracle_median".into(),
            cex_bid,
            cex_ask,
            gap_bps,
            direction,
            estimated_costs_bps,
            estimated_net_pnl_bps,
            inventory_ok,
            executable,
            details: ArbCheckDetails {
                dex_price_impact_bps,
                cex_slippage_bps,
                cex_fee_bps,
                dex_fee_bps,
                gas_cost_usd,
            },
            price_sources: Some(agg_price),
            dex_pool_info: None,
        })
    }
}

#[derive(Debug)]
pub enum ArbCheckError {
    Exchange(crate::exchange::errors::ExchangeError),
    DexError(String),
}

impl std::fmt::Display for ArbCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArbCheckError::Exchange(e) => write!(f, "exchange error: {e}"),
            ArbCheckError::DexError(e) => write!(f, "DEX error: {e}"),
        }
    }
}

impl std::error::Error for ArbCheckError {}
