//! # Chain Module
//!
//! Components for interacting with Ethereum nodes and analyzing on-chain data:
//! - **Chain Client**: Multi-endpoint RPC client with automatic retries and failover.
//! - **Transaction Builder**: Fluent interface for preparing and sending transactions.
//! - **Analyzer**: Decodes function selectors, event logs, and extracts revert reasons.
//! - **Selectors**: Database of known function and event signatures (ERC-20, Uniswap).

pub mod analyzer;
pub mod builder;
pub mod client;
pub mod errors;
pub mod flashbots;
pub mod selectors;

pub use builder::{SignedTransaction, TransactionBuilder};
pub use client::{ChainClient, RpcHealth};
pub use errors::{ChainError, ChainResult, InsufficientFunds, NonceTooLow, ReplacementUnderpriced};
pub use flashbots::{
    BundleRelay, BundleRequest, BundleResult, BundleStatus, BundleSubmission, BundleTx,
    FlashbotsConfig, FlashbotsRelayClient,
};
