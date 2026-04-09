//! # Pricing Module
//!
//! The brain of the arbitrage system. Contains:
//! - **AMM**: Exact Uniswap V2 math ([`UniswapV2Pair`], [`PriceImpactAnalyzer`])
//! - **Errors**: Shared error types ([`PricingError`], [`PricingResult`])

pub mod amm;
pub mod errors;

pub use amm::{ImpactRow, PriceImpactAnalyzer, TradeCost, UniswapV2Pair};
pub use errors::{PricingError, PricingResult};
