//! # Exchange Module
//!
//! CEX integration for Binance testnet:
//! - **Client**: Authenticated REST client with rate-limit awareness ([`ExchangeClient`])
//! - **Config**: Environment-based Binance configuration ([`BinanceConfig`])
//! - **HTTP Client**: Shared HTTP transport with retry and rate-limit tracking ([`HttpClient`])
//! - **Order Book**: L2 order book analysis and walk-the-book ([`OrderBookAnalyzer`])
//! - **Price Oracle**: Multi-source price aggregation ([`PriceOracle`])
//! - **Rate Limiter**: Dynamic rate-limit discovery from response headers ([`RateLimiter`])

pub mod client;
pub mod config;
pub mod errors;
pub mod http_client;
pub mod orderbook;
pub mod price_oracle;
pub mod rate_limiter;
pub mod types;

pub use client::ExchangeClient;
pub use config::{BINANCE_TESTNET_BASE_URL, BINANCE_TESTNET_WS_URL, BinanceConfig};
pub use errors::{ExchangeError, ExchangeResult};
pub use http_client::{HttpClient, RetryConfig};
pub use orderbook::OrderBookAnalyzer;
pub use price_oracle::{AggregatedPrice, PriceOracle, PriceSource};
pub use rate_limiter::{ApiQuota, LimitInterval, LimitKey, LimitType, RateLimiter};
pub use types::{
    CanExecuteResult, FeeStructure, FillLevel, MyTrade, NormalizedBalance, OrderBookSnapshot,
    OrderResult, PortfolioSnapshot, SkewResult, VenueSkew, WalkResult,
};
