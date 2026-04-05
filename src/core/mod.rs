//! # Core Module
//! 
//! Provides the fundamental building blocks for Ethereum interaction:
//! - **Addresses**: Checksummed Ethereum addresses with case-insensitive comparison.
//! - **Token Amounts**: Type-safe representation of currency amounts with varying decimals.
//! - **Wallet Management**: Encrypted keystore support and transaction signing.
//! - **Serialization**: Deterministic canonical JSON serialization for signing.

pub mod serializer;
pub mod types;
pub mod wallet;
