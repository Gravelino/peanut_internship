use thiserror::Error;

use crate::core::types::TransactionReceipt;

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
    #[error("other chain error: {0}")]
    Other(String),
}

pub type ChainResult<T> = Result<T, ChainError>;

pub use ChainError::{InsufficientFunds, NonceTooLow, ReplacementUnderpriced};

#[derive(Debug, Error)]
#[error("rpc request failed: {message}")]
pub struct RPCError {
    pub message: String,
    pub code: Option<i64>,
}

#[derive(Debug, Error)]
#[error("transaction {tx_hash} reverted")]
pub struct TransactionFailed {
    pub tx_hash: String,
    pub receipt: TransactionReceipt,
}
