use std::collections::HashMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use tracing::{info, warn};

use crate::exchange::errors::ExchangeError;
use crate::exchange::{ExchangeClient, OrderBookAnalyzer};
use crate::inventory::pnl::PnLEngine;
use crate::inventory::tracker::InventoryTracker;
use crate::inventory::types::{
    ExecutorConfig, RebalanceResult, RebalanceStatus, RebalanceStep, TradeStep, Venue,
};

/// Executes rebalance steps by placing IOC limit orders on CEX venues.
///
/// Safety guards: slippage check, balance pre-flight, max notional cap,
/// partial-fill threshold, dry-run mode, sequential execution with stop-on-failure.
pub struct RebalanceExecutor {
    clients: HashMap<Venue, ExchangeClient>,
    tracker: Arc<tokio::sync::Mutex<InventoryTracker>>,
    #[allow(dead_code)]
    pnl: Arc<tokio::sync::Mutex<PnLEngine>>,
    config: ExecutorConfig,
}

impl RebalanceExecutor {
    /// Creates a new executor with per-venue exchange clients and shared state.
    pub fn new(
        clients: HashMap<Venue, ExchangeClient>,
        tracker: Arc<tokio::sync::Mutex<InventoryTracker>>,
        pnl: Arc<tokio::sync::Mutex<PnLEngine>>,
        config: ExecutorConfig,
    ) -> Self {
        Self {
            clients,
            tracker,
            pnl,
            config,
        }
    }

    /// Returns a reference to the executor configuration.
    pub fn config(&self) -> &ExecutorConfig {
        &self.config
    }

    /// Executes a single rebalance step.
    pub async fn execute_step(&self, step: &RebalanceStep) -> RebalanceResult {
        match step {
            RebalanceStep::Trade(trade) => self.execute_trade(trade).await,
            RebalanceStep::Withdraw(_) => RebalanceResult {
                step: step.clone(),
                order_id: None,
                amount_filled: Decimal::ZERO,
                avg_price: Decimal::ZERO,
                fee: Decimal::ZERO,
                fee_asset: String::new(),
                status: RebalanceStatus::NotSupported,
            },
        }
    }

    /// Executes a sequence of steps, stopping on the first non-success status.
    ///
    /// Remaining steps after a failure receive `RebalanceStatus::Aborted`.
    pub async fn execute_plan(&self, steps: &[RebalanceStep]) -> Vec<RebalanceResult> {
        let mut results = Vec::with_capacity(steps.len());
        let mut failed = false;

        for step in steps {
            if failed {
                results.push(RebalanceResult {
                    step: step.clone(),
                    order_id: None,
                    amount_filled: Decimal::ZERO,
                    avg_price: Decimal::ZERO,
                    fee: Decimal::ZERO,
                    fee_asset: String::new(),
                    status: RebalanceStatus::Aborted,
                });
                continue;
            }

            let result = self.execute_step(step).await;
            let status = result.status;
            results.push(result);

            if status != RebalanceStatus::Executed
                && status != RebalanceStatus::PartiallyFilled
                && status != RebalanceStatus::DryRun
            {
                failed = true;
                warn!(?status, "Rebalance step failed, aborting remaining steps");
            }
        }

        results
    }

    /// Core trade execution flow for a single `TradeStep`.
    async fn execute_trade(&self, trade: &TradeStep) -> RebalanceResult {
        let client = match self.client_for(trade.venue) {
            Some(c) => c,
            None => {
                warn!(venue = %trade.venue, "No exchange client configured for venue");
                return make_rejected_result(trade);
            }
        };

        {
            let estimated_price = match self.estimate_price(client, &trade.symbol).await {
                Some(p) => p,
                None => {
                    warn!(symbol = %trade.symbol, "Cannot estimate price for pre-flight check");
                    return make_rejected_result(trade);
                }
            };
            let tracker = self.tracker.lock().await;
            let (need_asset, need_amount) = match trade.side.as_str() {
                "BUY" => (trade.quote_asset.as_str(), trade.amount * estimated_price),
                _ => (trade.base_asset.as_str(), trade.amount),
            };

            let available = tracker
                .get_available(trade.venue, need_asset)
                .unwrap_or_else(|| {
                    warn!(
                        venue = %trade.venue,
                        asset = need_asset,
                        "No balance data loaded, treating available as zero"
                    );
                    Decimal::ZERO
                });

            if available < need_amount {
                warn!(
                    venue = %trade.venue,
                    asset = need_asset,
                    need = %need_amount,
                    have = %available,
                    "Insufficient balance for rebalance trade"
                );
                return RebalanceResult {
                    step: RebalanceStep::Trade(trade.clone()),
                    order_id: None,
                    amount_filled: Decimal::ZERO,
                    avg_price: Decimal::ZERO,
                    fee: Decimal::ZERO,
                    fee_asset: String::new(),
                    status: RebalanceStatus::InsufficientBalance,
                };
            }
        }

        let ob = match client.fetch_order_book(&trade.symbol, 20).await {
            Ok(ob) => ob,
            Err(e) => {
                warn!(error = %e, "Failed to fetch order book for slippage check");
                return make_rejected_result(trade);
            }
        };
        let analyzer = OrderBookAnalyzer::new(ob);
        let walk = match analyzer.walk_the_book(&trade.side.to_lowercase(), trade.amount) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "Book walk failed for slippage check");
                return make_rejected_result(trade);
            }
        };

        if walk.slippage_bps > trade.max_slippage_bps {
            warn!(
                slippage_bps = %walk.slippage_bps,
                max_bps = %trade.max_slippage_bps,
                "Slippage exceeds limit, aborting trade"
            );
            return RebalanceResult {
                step: RebalanceStep::Trade(trade.clone()),
                order_id: None,
                amount_filled: Decimal::ZERO,
                avg_price: walk.avg_price,
                fee: Decimal::ZERO,
                fee_asset: String::new(),
                status: RebalanceStatus::SlippageExceeded,
            };
        }

        let notional = trade.amount * walk.avg_price;
        if notional > self.config.max_single_trade_usd {
            warn!(
                notional_usd = %notional,
                max_usd = %self.config.max_single_trade_usd,
                "Trade notional exceeds safety limit"
            );
            return make_rejected_result(trade);
        }

        if self.config.dry_run {
            info!(
                venue = %trade.venue,
                symbol = %trade.symbol,
                side = %trade.side,
                amount = %trade.amount,
                estimated_price = %walk.avg_price,
                slippage_bps = %walk.slippage_bps,
                "[DRY RUN] Would place IOC limit order"
            );
            return RebalanceResult {
                step: RebalanceStep::Trade(trade.clone()),
                order_id: None,
                amount_filled: Decimal::ZERO,
                avg_price: walk.avg_price,
                fee: Decimal::ZERO,
                fee_asset: String::new(),
                status: RebalanceStatus::DryRun,
            };
        }

        let mid_price = analyzer.orderbook().mid_price.unwrap_or_else(|| {
            warn!("No mid_price in order book, using walk avg_price as IOC price");
            walk.avg_price
        });
        let amount_f64 = match trade.amount.to_f64() {
            Some(a) => a,
            None => {
                warn!("Trade amount too large for f64 conversion");
                return make_rejected_result(trade);
            }
        };
        let price_f64 = match mid_price.to_f64() {
            Some(p) => p,
            None => {
                warn!("Mid price too large for f64 conversion");
                return make_rejected_result(trade);
            }
        };

        info!(
            venue = %trade.venue,
            symbol = %trade.symbol,
            side = %trade.side,
            amount = amount_f64,
            price = price_f64,
            "Placing IOC limit order at mid price"
        );

        let order_result = match client
            .create_limit_ioc_order(&trade.symbol, &trade.side, amount_f64, price_f64)
            .await
        {
            Ok(r) => r,
            Err(ExchangeError::RateLimit(msg)) => {
                warn!(msg, "Rate limited, backing off");
                return RebalanceResult {
                    step: RebalanceStep::Trade(trade.clone()),
                    order_id: None,
                    amount_filled: Decimal::ZERO,
                    avg_price: Decimal::ZERO,
                    fee: Decimal::ZERO,
                    fee_asset: String::new(),
                    status: RebalanceStatus::RateLimited,
                };
            }
            Err(ExchangeError::InsufficientFunds(msg)) => {
                warn!(msg, "Insufficient funds for order");
                return RebalanceResult {
                    step: RebalanceStep::Trade(trade.clone()),
                    order_id: None,
                    amount_filled: Decimal::ZERO,
                    avg_price: Decimal::ZERO,
                    fee: Decimal::ZERO,
                    fee_asset: String::new(),
                    status: RebalanceStatus::InsufficientBalance,
                };
            }
            Err(e) => {
                warn!(error = %e, "Order rejected by exchange");
                return RebalanceResult {
                    step: RebalanceStep::Trade(trade.clone()),
                    order_id: None,
                    amount_filled: Decimal::ZERO,
                    avg_price: Decimal::ZERO,
                    fee: Decimal::ZERO,
                    fee_asset: String::new(),
                    status: RebalanceStatus::OrderRejected,
                };
            }
        };

        let order_result = if order_result.amount_filled == Decimal::ZERO {
            let retry_price = match trade.side.as_str() {
                "BUY" => analyzer.orderbook().best_ask.map(|(p, _)| p),
                _ => analyzer.orderbook().best_bid.map(|(p, _)| p),
            };
            match retry_price {
                Some(p) => {
                    let p_f64 = match p.to_f64() {
                        Some(v) => v,
                        None => return self.finalize_order(order_result, trade).await,
                    };
                    info!(
                        order_id = %order_result.id,
                        retry_price = p_f64,
                        "No fill at mid, retrying at best level"
                    );
                    match client
                        .create_limit_ioc_order(&trade.symbol, &trade.side, amount_f64, p_f64)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(error = %e, "Retry order failed");
                            order_result
                        }
                    }
                }
                None => order_result,
            }
        } else {
            order_result
        };

        self.finalize_order(order_result, trade).await
    }

    /// Finalizes an order result: classifies fill, and on success records the trade in the tracker.
    async fn finalize_order(
        &self,
        order: crate::exchange::types::OrderResult,
        trade: &TradeStep,
    ) -> RebalanceResult {
        let amount_filled = order.amount_filled;
        let avg_price = order.avg_fill_price;

        let status = if amount_filled <= Decimal::ZERO {
            RebalanceStatus::OrderRejected
        } else if trade.amount > Decimal::ZERO {
            let fill_ratio = (amount_filled / trade.amount).to_f64().unwrap_or(0.0);
            if fill_ratio + f64::EPSILON >= self.config.min_fill_pct {
                RebalanceStatus::Executed
            } else {
                RebalanceStatus::PartiallyFilled
            }
        } else {
            RebalanceStatus::Executed
        };

        if amount_filled > Decimal::ZERO {
            let quote_amount = amount_filled * avg_price;
            let side_lower = trade.side.to_lowercase();
            let fee_asset = if order.fee_asset.is_empty() {
                trade.quote_asset.as_str()
            } else {
                order.fee_asset.as_str()
            };
            let mut tracker = self.tracker.lock().await;
            if let Err(e) = tracker.record_trade(
                trade.venue,
                &side_lower,
                &trade.base_asset,
                &trade.quote_asset,
                amount_filled,
                quote_amount,
                order.fee,
                fee_asset,
            ) {
                warn!(error = %e, "Failed to record rebalance trade in tracker");
            }
        }

        RebalanceResult {
            step: RebalanceStep::Trade(trade.clone()),
            order_id: Some(order.id.clone()),
            amount_filled,
            avg_price,
            fee: order.fee,
            fee_asset: order.fee_asset.clone(),
            status,
        }
    }

    /// Gets a rough price estimate from a cached/fresh order book.
    async fn estimate_price(&self, client: &ExchangeClient, symbol: &str) -> Option<Decimal> {
        client
            .fetch_order_book(symbol, 5)
            .await
            .ok()
            .and_then(|ob| OrderBookAnalyzer::new(ob).orderbook().mid_price)
    }

    /// Returns the exchange client for a given venue, if configured.
    fn client_for(&self, venue: Venue) -> Option<&ExchangeClient> {
        self.clients.get(&venue)
    }
}

fn make_rejected_result(trade: &TradeStep) -> RebalanceResult {
    RebalanceResult {
        step: RebalanceStep::Trade(trade.clone()),
        order_id: None,
        amount_filled: Decimal::ZERO,
        avg_price: Decimal::ZERO,
        fee: Decimal::ZERO,
        fee_asset: String::new(),
        status: RebalanceStatus::OrderRejected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::types::WithdrawStep;

    #[test]
    fn test_executor_config_defaults() {
        let config = ExecutorConfig::default();
        assert_eq!(config.max_slippage_bps, Decimal::from(50));
        assert_eq!(config.max_single_trade_usd, Decimal::from(1000));
        assert!((config.min_fill_pct - 0.8).abs() < f64::EPSILON);
        assert_eq!(config.order_poll_interval_ms, 500);
        assert_eq!(config.order_poll_max_attempts, 10);
        assert!(!config.dry_run);
    }

    #[test]
    fn test_rebalance_status_is_copy() {
        let a = RebalanceStatus::Executed;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn test_make_rejected_result() {
        let trade = TradeStep {
            venue: Venue::Binance,
            symbol: "ETHUSDT".to_string(),
            side: "SELL".to_string(),
            base_asset: "ETH".to_string(),
            quote_asset: "USDT".to_string(),
            amount: Decimal::ONE,
            max_slippage_bps: Decimal::from(50),
        };
        let result = make_rejected_result(&trade);
        assert_eq!(result.status, RebalanceStatus::OrderRejected);
        assert_eq!(result.amount_filled, Decimal::ZERO);
        assert!(result.order_id.is_none());
    }

    #[test]
    fn test_venue_is_cex() {
        assert!(Venue::Binance.is_cex());
        assert!(Venue::Bybit.is_cex());
        assert!(!Venue::Wallet.is_cex());
    }

    #[test]
    fn test_execute_plan_aborts_on_failure() {
        let steps = vec![
            RebalanceStep::Withdraw(WithdrawStep {
                from_venue: Venue::Binance,
                to_venue: Venue::Wallet,
                asset: "ETH".to_string(),
                amount: Decimal::ONE,
                fee: Decimal::from_str_exact("0.005").unwrap(),
            }),
            RebalanceStep::Withdraw(WithdrawStep {
                from_venue: Venue::Binance,
                to_venue: Venue::Wallet,
                asset: "USDT".to_string(),
                amount: Decimal::from(100),
                fee: Decimal::ONE,
            }),
        ];

        let tracker = Arc::new(tokio::sync::Mutex::new(InventoryTracker::new(vec![
            Venue::Binance,
            Venue::Wallet,
        ])));
        let pnl = Arc::new(tokio::sync::Mutex::new(PnLEngine::new()));
        let executor =
            RebalanceExecutor::new(HashMap::new(), tracker, pnl, ExecutorConfig::default());

        let rt = tokio::runtime::Runtime::new().unwrap();
        let results = rt.block_on(executor.execute_plan(&steps));

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].status, RebalanceStatus::NotSupported);
        assert_eq!(results[1].status, RebalanceStatus::Aborted);
    }

    #[test]
    fn test_trade_without_client_returns_rejected() {
        let trade = TradeStep {
            venue: Venue::Binance,
            symbol: "ETHUSDT".to_string(),
            side: "BUY".to_string(),
            base_asset: "ETH".to_string(),
            quote_asset: "USDT".to_string(),
            amount: Decimal::ONE,
            max_slippage_bps: Decimal::from(50),
        };

        let tracker = Arc::new(tokio::sync::Mutex::new(InventoryTracker::new(vec![
            Venue::Binance,
        ])));
        let pnl = Arc::new(tokio::sync::Mutex::new(PnLEngine::new()));
        let config = ExecutorConfig {
            dry_run: true,
            ..ExecutorConfig::default()
        };
        let executor = RebalanceExecutor::new(HashMap::new(), tracker, pnl, config);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(executor.execute_step(&RebalanceStep::Trade(trade)));

        assert_eq!(result.status, RebalanceStatus::OrderRejected);
        assert!(result.order_id.is_none());
    }

    #[test]
    fn test_finalize_order_classifies_fill_status() {
        use crate::exchange::types::OrderResult;
        let trade = TradeStep {
            venue: Venue::Binance,
            symbol: "ETHUSDT".to_string(),
            side: "BUY".to_string(),
            base_asset: "ETH".to_string(),
            quote_asset: "USDT".to_string(),
            amount: Decimal::from(10),
            max_slippage_bps: Decimal::from(50),
        };

        let tracker = Arc::new(tokio::sync::Mutex::new(InventoryTracker::new(vec![
            Venue::Binance,
        ])));
        let pnl = Arc::new(tokio::sync::Mutex::new(PnLEngine::new()));
        let executor = RebalanceExecutor::new(
            HashMap::new(),
            tracker.clone(),
            pnl,
            ExecutorConfig::default(), // min_fill_pct = 0.8
        );

        let make_order = |filled: Decimal| OrderResult {
            id: "o1".into(),
            symbol: trade.symbol.clone(),
            side: "buy".into(),
            order_type: "limit".into(),
            time_in_force: "IOC".into(),
            amount_requested: trade.amount,
            amount_filled: filled,
            avg_fill_price: Decimal::from(2000),
            fee: Decimal::ZERO,
            fee_asset: "USDT".into(),
            status: "filled".into(),
            timestamp: 0,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();

        let r = rt.block_on(executor.finalize_order(make_order(Decimal::from(10)), &trade));
        assert_eq!(r.status, RebalanceStatus::Executed);

        let r = rt.block_on(executor.finalize_order(make_order(Decimal::from(5)), &trade));
        assert_eq!(r.status, RebalanceStatus::PartiallyFilled);

        let r = rt.block_on(executor.finalize_order(make_order(Decimal::ZERO), &trade));
        assert_eq!(r.status, RebalanceStatus::OrderRejected);
    }
}
