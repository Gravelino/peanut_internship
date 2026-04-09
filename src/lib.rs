//! # Peanut Internship Rust Project
//! 
//! A high-level library for Ethereum blockchain interaction, transaction building, 
//! and analysis. Designed for the Peanut Trade internship program.
//! 
//! ## Features
//! - **Core**: Types for [Address], [TokenAmount], and [WalletManager] for signing.
//! - **Chain**: [ChainClient] for RPC interaction and [TransactionBuilder] for fluent transaction creation.
//! - **Analysis**: Transaction analysis and event log decoding (ERC-20, Uniswap).
//! - **Pricing**: Exact Uniswap V2 math, price impact analysis, and route finding.
//! 
//! ## Example: Transferring ETH
//! ```no_run
//! use peanut_internship_rust::{ChainClient, WalletManager, TransactionBuilder, TokenAmount, Address};
//! 
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = ChainClient::new(vec!["http://localhost:8545".to_string()], 30, 3);
//!     let wallet = WalletManager::from_env("PRIVATE_KEY")?;
//!     let to = Address::new("0x...")?;
//!     let amount = TokenAmount::from_eth("0.1")?;
//! 
//!     let tx_hash = TransactionBuilder::new(client, wallet)
//!         .to(to)
//!         .value(amount)
//!         .send()
//!         .await?;
//! 
//!     println!("Transaction sent: {}", tx_hash);
//!     Ok(())
//! }
//! ```

pub mod chain;
pub mod core;
pub mod pricing;

pub use chain::{
    ChainClient,
    ChainError,
    ChainResult,
    InsufficientFunds,
    NonceTooLow,
    ReplacementUnderpriced,
    TransactionBuilder,
};
pub use core::serializer::CanonicalSerializer;
pub use core::types::{
    Address, BlockId, CoreError, GasPriority, GasPrice, Token, TokenAmount, TransactionReceipt,
    TransactionRequest, TransactionStatus, DEFAULT_GAS_BUFFER, MIN_GAS_LIMIT, ETH_DECIMALS,
    ETH_SYMBOL, MAINNET_CHAIN_ID, SEPOLIA_CHAIN_ID, RECEIPT_STATUS_SUCCESS, RECEIPT_STATUS_FAILED,
};
pub use core::wallet::{WalletError, WalletManager};
pub use pricing::{ImpactRow, PriceImpactAnalyzer, PricingError, PricingResult, TradeCost, UniswapV2Pair};
