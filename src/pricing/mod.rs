//! # Pricing Module
//!
//! The brain of the arbitrage system. Contains:
//! - **AMM**: Exact Uniswap V2 math ([`UniswapV2Pair`], [`PriceImpactAnalyzer`])
//! - **Feed**: Real-time price feed via WebSocket ([`PriceFeed`], [`PriceTick`])
//! - **V3**: Uniswap V3 concentrated liquidity ([`UniswapV3Pool`], [`V3SwapQuote`])
//! - **Arb**: Arbitrage opportunity detector ([`ArbDetector`], [`ArbOpportunity`])
//! - **History**: Historical price impact analysis ([`HistoricalImpactAnalyzer`], [`ImpactSummary`])
//! - **Errors**: Shared error types ([`PricingError`], [`PricingResult`])

pub mod amm;
pub mod arb;
pub mod engine;
pub mod errors;
pub mod feed;
pub mod history;
pub mod mempool;
pub mod router;
pub mod simulator;
pub mod v3;

pub use arb::{ArbDetector, ArbKind, ArbOpportunity};
pub use amm::{ImpactRow, PriceImpactAnalyzer, TradeCost, UniswapV2Pair};
pub use engine::{PricingEngine, Quote, QuoteError, QuoteResult};
pub use errors::{PricingError, PricingResult};
pub use feed::{PriceFeed, PriceTick};
pub use history::{
    HistoricalImpactAnalyzer, HistoricalImpactPoint, ImpactSummary, SizeImpact, SizeImpactAvg,
};
pub use mempool::{MempoolMonitor, ParsedSwap};
pub use router::{PoolRef, Route, RouteComparison, RouteFinder};
pub use simulator::{
    AmountOutDecoder, ForkSimulation, ForkSimulator, SimulationComparison, SimulationResult,
    SimulationVerdict, SwapParams,
};
pub use v3::{UniswapV3Pool, V3SwapQuote};
