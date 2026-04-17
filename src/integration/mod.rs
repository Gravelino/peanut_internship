//! # Integration Module
//!
//! Cross-module orchestration that combines exchange, pricing, and inventory
//! subsystems into end-to-end arbitrage checks.

pub mod arb_checker;

pub use arb_checker::{
    ArbCheckDetails, ArbCheckError, ArbCheckResult, ArbChecker, CrossDexOpportunity, DexPoolInfo,
    ForkSimInfo,
};
