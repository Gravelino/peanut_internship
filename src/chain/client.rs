use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{transaction::eip2718::TypedTransaction, BlockNumber, Bytes, H256, U256};

use crate::core::types::{Address, GasPrice, TokenAmount, TransactionReceipt, TransactionRequest, BlockId};

use super::errors::{ChainError, ChainResult};
use tracing::{debug, info, instrument, warn};

/// Minimum interval between polling for transaction receipts.
pub const MIN_POLL_INTERVAL: f64 = 0.1;

/// A high-level client for interacting with the Ethereum blockchain.
/// 
/// Supports multiple RPC endpoints with automatic failover and retries.
#[derive(Clone)]
pub struct ChainClient {
    rpc_urls: Vec<String>,
    timeout_secs: u64,
    max_retries: usize,
}

impl ChainClient {
    /// Creates a new ChainClient.
    pub fn new(rpc_urls: Vec<String>, timeout_secs: u64, max_retries: usize) -> Self {
        Self {
            rpc_urls,
            timeout_secs,
            max_retries,
        }
    }

    /// Fetches the native ETH balance for an address.
    #[instrument(skip(self), fields(address = %address.as_eth_address()))]
    pub async fn get_balance(&self, address: &Address) -> ChainResult<TokenAmount> {
        debug!("Fetching balance for address");
        let balance = self.with_provider(|provider| async move {
            provider.get_balance(address.as_eth_address(), None).await
        }).await?;

        Ok(TokenAmount::eth(balance))
    }

    /// Fetches the current nonce (transaction count) for an address.
    #[instrument(skip(self), fields(address = %address.as_eth_address(), block = ?block))]
    pub async fn get_nonce(&self, address: &Address, block: BlockId) -> ChainResult<u64> {
        debug!("Fetching nonce");
        let block_number = match block {
            BlockId::Latest => BlockNumber::Latest,
            BlockId::Pending => BlockNumber::Pending,
        };

        self.with_provider(|provider| async move {
            provider
                .get_transaction_count(address.as_eth_address(), Some(block_number.into()))
                .await
        })
        .await
        .map(|nonce| nonce.as_u64())
    }

    /// Fetches the current gas prices and base fee.
    pub async fn get_gas_price(&self) -> ChainResult<GasPrice> {
        let latest_block = self.with_provider(|provider| async move { provider.get_block(BlockNumber::Latest).await }).await?;
        let gas_price = self.with_provider(|provider| async move { provider.get_gas_price().await }).await?;

        let base_fee = latest_block
            .and_then(|block| block.base_fee_per_gas)
            .unwrap_or(gas_price);

        Ok(GasPrice {
            base_fee,
            priority_fee_low: gas_price / U256::from(4u64),
            priority_fee_medium: gas_price / U256::from(2u64),
            priority_fee_high: gas_price,
        })
    }

    /// Estimates the gas required to execute a transaction.
    pub async fn estimate_gas(&self, tx: &TransactionRequest) -> ChainResult<u64> {
        let request: Arc<TypedTransaction> = Arc::new(tx.to_ethers_request().into());
        self.with_provider(|provider| {
            let request = Arc::clone(&request);
            async move { provider.estimate_gas(&request, None).await }
        })
            .await
            .map(|gas| gas.as_u64())
    }

    /// Sends a raw signed transaction to the network.
    #[instrument(skip(self, signed_tx))]
    pub async fn send_transaction(&self, signed_tx: &[u8]) -> ChainResult<String> {
        debug!("Sending raw transaction ({} bytes)", signed_tx.len());
        let bytes = Bytes::from(signed_tx.to_vec());
        let tx_hash = self.with_provider(|provider| {
            let payload = bytes.clone();
            async move { provider.send_raw_transaction(payload).await.map(|pending| pending.tx_hash()) }
        }).await?;

        info!(hash = %tx_hash, "Transaction sent successfully");
        Ok(format!("0x{}", hex::encode(tx_hash.as_bytes())))
    }

    /// Fetches a transaction receipt.
    pub async fn get_receipt(&self, tx_hash: &str) -> ChainResult<Option<TransactionReceipt>> {
        let hash = H256::from_str(tx_hash).map_err(|_| ChainError::InvalidTransactionHash)?;
        match self.with_provider(|provider| async move { provider.get_transaction_receipt(hash).await }).await? {
            Some(receipt) => TransactionReceipt::from_ethers(&receipt)
                .map(Some)
                .map_err(|_| ChainError::InvalidReceiptData),
            None => Ok(None),
        }
    }

    /// Waits for a transaction to be confirmed on chain.
    #[instrument(skip(self))]
    pub async fn wait_for_receipt(
        &self,
        tx_hash: &str,
        timeout: u64,
        poll_interval: f64,
    ) -> ChainResult<TransactionReceipt> {
        info!(hash = %tx_hash, timeout_secs = timeout, "Waiting for transaction confirmation");
        let hash = H256::from_str(tx_hash).map_err(|_| ChainError::InvalidTransactionHash)?;
        let deadline = Instant::now() + Duration::from_secs(timeout.max(self.timeout_secs));
        let poll_duration = Duration::from_secs_f64(poll_interval.max(MIN_POLL_INTERVAL));

        loop {
            if Instant::now() >= deadline {
                return Err(ChainError::Timeout);
            }

            if let Some(receipt) = self.get_receipt_internal(hash).await? {
                return Ok(receipt);
            }

            tokio::time::sleep(poll_duration).await;
        }
    }

    /// Performs a read-only call to a smart contract.
    pub async fn call(&self, tx: &TransactionRequest, block: BlockId) -> ChainResult<Vec<u8>> {
        let request: Arc<TypedTransaction> = Arc::new(tx.to_ethers_request().into());
        let block_number = match block {
            BlockId::Latest => BlockNumber::Latest,
            BlockId::Pending => BlockNumber::Pending,
        };

        self.with_provider(|provider| {
            let request = Arc::clone(&request);
            async move { provider.call(&request, Some(block_number.into())).await }
        })
            .await
            .map(|bytes| bytes.to_vec())
    }

    async fn get_receipt_internal(&self, hash: H256) -> ChainResult<Option<TransactionReceipt>> {
        let receipt = self.with_provider(|provider| async move { provider.get_transaction_receipt(hash).await }).await?;
        match receipt {
            Some(value) => TransactionReceipt::from_ethers(&value)
                .map(Some)
                .map_err(|_| ChainError::InvalidReceiptData),
            None => Ok(None),
        }
    }

    async fn with_provider<T, F, Fut>(&self, mut operation: F) -> ChainResult<T>
    where
        F: FnMut(Provider<Http>) -> Fut,
        Fut: std::future::Future<Output = Result<T, ethers::providers::ProviderError>>,
    {
        let mut last_error: Option<ethers::providers::ProviderError> = None;

        for (url_idx, url) in self.rpc_urls.iter().enumerate() {
            let provider = Provider::<Http>::try_from(url.as_str()).map_err(|error| ChainError::Rpc(error.to_string()))?;
            for retry in 0..=self.max_retries {
                if retry > 0 {
                    debug!(url_idx, retry, "Retrying RPC operation");
                }
                let result = operation(provider.clone()).await;
                match result {
                    Ok(value) => return Ok(value),
                    Err(error) => {
                        warn!(url_idx, retry, error = %error, "RPC operation failed");
                        last_error = Some(error);
                    }
                }
            }
        }

        match last_error {
            Some(error) => Err(classify_rpc_error(&error.to_string())),
            None => Err(ChainError::Rpc("all RPC endpoints failed".to_string())),
        }
    }
}

/// Classifies a raw RPC error message into a structured [ChainError].
pub fn classify_rpc_error(msg: &str) -> ChainError {
    let lower = msg.to_lowercase();

    if lower.contains("insufficient funds") || lower.contains("insufficient balance") {
        return ChainError::InsufficientFunds;
    }

    if lower.contains("nonce too low") || lower.contains("nonce has already been used") {
        return ChainError::NonceTooLow;
    }

    if lower.contains("replacement transaction underpriced")
        || lower.contains("already known")
        || lower.contains("replacement fee too low")
    {
        return ChainError::ReplacementUnderpriced;
    }

    if lower.contains("timeout") || lower.contains("timed out") || lower.contains("deadline") {
        return ChainError::Timeout;
    }

    ChainError::Rpc(msg.to_string())
}
