use rust_decimal::Decimal;
use std::collections::HashMap;
use std::future::Future;

use crate::exchange::errors::ExchangeResult;
use crate::exchange::types::{
    FeeStructure, MyTrade, NormalizedBalance, OrderBookSnapshot, OrderResult,
};

/// Unified configuration for any exchange.
#[derive(Debug, Clone)]
pub enum ExchangeConfig {
    /// Binance testnet configuration.
    Binance(crate::exchange::config::BinanceConfig),
    /// Bybit testnet configuration.
    Bybit(crate::exchange::bybit_config::BybitConfig),
}

impl ExchangeConfig {
    /// Returns the display name of the exchange.
    pub fn exchange_name(&self) -> &'static str {
        match self {
            Self::Binance(_) => "Binance",
            Self::Bybit(_) => "Bybit",
        }
    }
}

/// Trait abstracting exchange operations for multi-exchange support.
///
/// Each exchange implements this trait with its own signing, error mapping, and URL scheme.
/// Async methods return `impl Future + Send` so the returned futures are `Send`-safe.
pub trait ExchangeAdapter: Send + Sync {
    /// Checks connectivity by pinging the exchange server.
    fn health_check(&self) -> impl Future<Output = ExchangeResult<u64>> + Send;

    /// Fetches and applies rate-limit quotas from the exchange info endpoint.
    fn fetch_rate_limits(&self) -> impl Future<Output = ExchangeResult<()>> + Send;

    /// Fetches the order book snapshot for the given symbol and depth limit.
    fn fetch_order_book(
        &self,
        symbol: &str,
        limit: u32,
    ) -> impl Future<Output = ExchangeResult<OrderBookSnapshot>> + Send;

    /// Fetches the account balance for all non-zero assets.
    fn fetch_balance(
        &self,
    ) -> impl Future<Output = ExchangeResult<HashMap<String, NormalizedBalance>>> + Send;

    /// Places a limit order with the given time-in-force policy.
    #[allow(clippy::too_many_arguments)]
    fn create_limit_order(
        &self,
        symbol: &str,
        side: &str,
        amount: f64,
        price: f64,
        time_in_force: &str,
    ) -> impl Future<Output = ExchangeResult<OrderResult>> + Send;

    /// Places a market order.
    fn create_market_order(
        &self,
        symbol: &str,
        side: &str,
        amount: f64,
    ) -> impl Future<Output = ExchangeResult<OrderResult>> + Send;

    /// Cancels an existing order by its ID and symbol.
    fn cancel_order(
        &self,
        order_id: &str,
        symbol: &str,
    ) -> impl Future<Output = ExchangeResult<OrderResult>> + Send;

    /// Fetches the current status of an order.
    fn fetch_order_status(
        &self,
        order_id: &str,
        symbol: &str,
    ) -> impl Future<Output = ExchangeResult<OrderResult>> + Send;

    /// Fetches the maker and taker trading fees for a symbol.
    fn get_trading_fees(
        &self,
        symbol: &str,
    ) -> impl Future<Output = ExchangeResult<FeeStructure>> + Send;

    /// Fetches recent trades for the given symbol.
    fn fetch_my_trades(
        &self,
        symbol: &str,
        limit: u32,
    ) -> impl Future<Output = ExchangeResult<Vec<MyTrade>>> + Send;

    /// Fetches the current price for a symbol.
    fn fetch_price(&self, symbol: &str) -> impl Future<Output = ExchangeResult<Decimal>> + Send;

    /// Returns the exchange configuration.
    fn config(&self) -> &ExchangeConfig;
}
