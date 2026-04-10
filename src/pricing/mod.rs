//! # Pricing Module
//!
//! The brain of the arbitrage system. Contains:
//! - **AMM**: Exact Uniswap V2 math ([`UniswapV2Pair`], [`PriceImpactAnalyzer`])
//! - **Errors**: Shared error types ([`PricingError`], [`PricingResult`])

pub mod amm;
pub mod engine;
pub mod errors;
pub mod mempool;
pub mod router;
pub mod simulator;

pub use amm::{ImpactRow, PriceImpactAnalyzer, TradeCost, UniswapV2Pair};
pub use engine::{PricingEngine, Quote, QuoteError, QuoteResult};
pub use errors::{PricingError, PricingResult};
pub use mempool::{MempoolMonitor, ParsedSwap};
pub use router::{Route, RouteComparison, RouteFinder};
pub use simulator::{
    AmountOutDecoder, ForkSimulation, ForkSimulator, SimulationComparison, SimulationResult,
    SimulationVerdict, SwapParams,
};
