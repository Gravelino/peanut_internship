use thiserror::Error;

use crate::core::types::TransactionReceipt;

/// Represents errors that can occur during blockchain interactions.
#[derive(Debug, Error)]
pub enum ChainError {
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("rpc request failed: {message}")]
    RPCError {
        message: String,
        code: Option<i64>,
    },
    #[error("transaction reverted: {tx_hash}")]
    TransactionFailed {
        tx_hash: String,
        receipt: TransactionReceipt,
    },
    #[error("insufficient funds")]
    InsufficientFunds,
    #[error("nonce too low")]
    NonceTooLow,
    #[error("replacement transaction underpriced")]
    ReplacementUnderpriced,
    #[error("network timeout")]
    Timeout,
    #[error("invalid transaction hash")]
    InvalidTransactionHash,
    #[error("invalid transaction receipt data")]
    InvalidReceiptData,
    #[error("failed to initialize runtime")]
    RuntimeInit,
    #[error("invalid wallet address")]
    InvalidWalletAddress,
    #[error("failed to sign transaction")]
    SignTransactionFailed,
    #[error("missing destination address")]
    MissingDestinationAddress,
    #[error("missing transaction value")]
    MissingTransactionValue,
    #[error("other chain error: {0}")]
    Other(String),
}

/// A specialized [Result] type for chain operations.
pub type ChainResult<T> = Result<T, ChainError>;

pub use ChainError::{InsufficientFunds, NonceTooLow, ReplacementUnderpriced};
