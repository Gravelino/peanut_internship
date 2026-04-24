//! # Strategy module
//!
//! Detects arbitrage opportunities and scores them for the executor to act on.
//! Built on Weeks 1-3: consumes [`ExchangeClient`](crate::exchange::ExchangeClient),
//! [`InventoryTracker`](crate::inventory::InventoryTracker), and optionally
//! [`PricingEngine`](crate::pricing::PricingEngine) via the [`PriceSource`]
//! abstraction.

pub mod errors;
pub mod fees;
pub mod generator;
pub mod scorer;
pub mod signal;

pub use errors::{StrategyError, StrategyResult};
pub use fees::FeeStructure;
pub use generator::{
    GeneratorConfig, PriceSource, SignalGenerator, StubPriceSource, VenuePrices, split_pair,
};
pub use scorer::{ScorerConfig, SignalScorer};
pub use signal::{Direction, Signal, SignalParams};
