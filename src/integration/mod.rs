//! # Integration Module
//!
//! Cross-module orchestration that combines exchange, pricing, and inventory
//! subsystems into end-to-end arbitrage checks and logging.

pub mod arb_checker;
pub mod arb_logger;

pub use arb_checker::{
    ArbCheckDetails, ArbCheckError, ArbCheckResult, ArbChecker, CrossDexOpportunity, DexPoolInfo,
    ForkSimInfo,
};
pub use arb_logger::ArbLogger;
