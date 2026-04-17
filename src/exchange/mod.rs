pub mod client;
pub mod config;
pub mod errors;
pub mod orderbook;
pub mod price_oracle;
pub mod rate_limiter;
pub mod types;

pub use client::ExchangeClient;
pub use config::{BINANCE_TESTNET_BASE_URL, BINANCE_TESTNET_WS_URL, BinanceConfig};
pub use errors::{ExchangeError, ExchangeResult};
pub use orderbook::OrderBookAnalyzer;
pub use price_oracle::{AggregatedPrice, PriceOracle, PriceSource};
pub use rate_limiter::RateLimiter;
pub use types::{
    CanExecuteResult, FeeStructure, FillLevel, MyTrade, NormalizedBalance, OrderBookSnapshot, OrderResult,
    PortfolioSnapshot, SkewResult, VenueSkew, WalkResult,
};
