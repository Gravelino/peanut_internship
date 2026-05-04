//! # Uniswap V3 Concentrated Liquidity
//!
//! Implements V3 pool math, tick spacing, and swap quoting:
//! - **Pool**: On-chain V3 pool loading and spot/quote calculations
//! - **Math**: Low-level sqrt-price and tick math (Uniswap V3 whitepaper)
//! - **Tick**: Fee-tier to tick-spacing mapping

pub mod math;
pub mod pool;
pub mod tick;

pub use pool::{UniswapV3Pool, V3QuoterConfig, V3QuoterKind, V3SwapQuote};
