//! Error types for the `strategy` module.

use thiserror::Error;

/// Errors that can occur while generating or scoring arbitrage signals.
#[derive(Debug, Error)]
pub enum StrategyError {
    /// Exchange/CEX-side failure while fetching market data.
    #[error("exchange error: {0}")]
    Exchange(#[from] crate::exchange::errors::ExchangeError),

    /// Pricing/DEX-side failure while getting a quote.
    #[error("pricing error: {0}")]
    Pricing(String),

    /// Trading pair string could not be parsed into base/quote tokens.
    #[error("invalid pair '{0}'")]
    InvalidPair(String),

    /// Unknown asset symbol (not in the strategy's token registry).
    #[error("unknown token '{0}'")]
    UnknownToken(String),

    /// Order book missing both sides or with zero-sized best level.
    #[error("empty order book for {0}")]
    EmptyOrderBook(String),
}

/// Convenience result alias.
pub type StrategyResult<T> = Result<T, StrategyError>;
