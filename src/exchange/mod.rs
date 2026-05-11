//! # Exchange Module
//!
//! CEX integration for Binance testnet:
//! - **Client**: Authenticated REST client with rate-limit awareness ([`ExchangeClient`])
//! - **Config**: Environment-based Binance configuration ([`BinanceConfig`])
//! - **HTTP Client**: Shared HTTP transport with retry and rate-limit tracking ([`HttpClient`])
//! - **Order Book**: L2 order book analysis and walk-the-book ([`OrderBookAnalyzer`])
//! - **Price Oracle**: Multi-source price aggregation ([`PriceOracle`])
//! - **Rate Limiter**: Dynamic rate-limit discovery from response headers ([`RateLimiter`])

pub mod bybit;
pub mod bybit_config;
pub mod client;
pub mod config;
pub mod errors;
pub mod http_client;
pub mod orderbook;
pub mod price_oracle;
pub mod rate_limiter;
pub mod traits;
pub mod types;
pub mod ws;

pub use bybit::BybitAdapter;
pub use bybit_config::{BYBIT_TESTNET_BASE_URL, BYBIT_TESTNET_WS_URL, BybitConfig};
pub use client::ExchangeClient;
pub use config::{
    BINANCE_PRODUCTION_BASE_URL, BINANCE_PRODUCTION_WS_URL, BINANCE_TESTNET_BASE_URL,
    BINANCE_TESTNET_WS_URL, BinanceConfig,
};
pub use errors::{ExchangeError, ExchangeResult};
pub use http_client::{HttpClient, RetryConfig};
pub use orderbook::OrderBookAnalyzer;
pub use price_oracle::{AggregatedPrice, PriceOracle, PriceSource};
pub use rate_limiter::{ApiQuota, LimitInterval, LimitKey, LimitType, RateLimiter};
pub use traits::{ExchangeAdapter, ExchangeConfig};
pub use types::{
    CanExecuteResult, CapitalCoinConfig, CapitalNetworkConfig, DepositRecord, FeeStructure,
    FillLevel, MyTrade, NormalizedBalance, OrderBookSnapshot, OrderResult, PortfolioSnapshot,
    SkewResult, VenueSkew, WalkResult, WithdrawalRecord,
};
pub use ws::{
    BookTickerEvent, DepthEvent, DepthSnapshot, DepthUpdate, LocalOrderBook, SequenceStatus,
    subscribe_book_ticker_stream, subscribe_depth_stream,
};
