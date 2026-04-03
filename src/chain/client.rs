use std::str::FromStr;
use std::time::{Duration, Instant};

use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{transaction::eip2718::TypedTransaction, BlockNumber, Bytes, H256, U256};

use crate::core::types::{Address, GasPrice, TokenAmount, TransactionReceipt, TransactionRequest};

use super::errors::{ChainError, ChainResult};

#[derive(Clone)]
pub struct ChainClient {
    rpc_urls: Vec<String>,
    timeout_secs: u64,
    max_retries: usize,
}

impl ChainClient {
    pub fn new(rpc_urls: Vec<String>, timeout_secs: u64, max_retries: usize) -> Self {
        Self {
            rpc_urls,
            timeout_secs,
            max_retries,
        }
    }

    pub fn get_balance(&self, address: &Address) -> ChainResult<TokenAmount> {
        let balance = self.with_provider(|provider| async move {
            provider.get_balance(address.as_eth_address(), None).await
        })?;

        Ok(TokenAmount {
            raw: balance,
            decimals: 18,
            symbol: Some("ETH".to_string()),
        })
    }

    pub fn get_nonce(&self, address: &Address, block: &str) -> ChainResult<u64> {
        let block_number = match block {
            "latest" => BlockNumber::Latest,
            "pending" => BlockNumber::Pending,
            _ => BlockNumber::Pending,
        };

        self.with_provider(|provider| async move {
            provider
                .get_transaction_count(address.as_eth_address(), Some(block_number.into()))
                .await
        })
        .map(|nonce| nonce.as_u64())
    }

    pub fn get_gas_price(&self) -> ChainResult<GasPrice> {
        let latest_block = self.with_provider(|provider| async move { provider.get_block(BlockNumber::Latest).await })?;
        let gas_price = self.with_provider(|provider| async move { provider.get_gas_price().await })?;

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

    pub fn estimate_gas(&self, tx: &TransactionRequest) -> ChainResult<u64> {
        let request: TypedTransaction = tx.to_ethers_request().into();
        self.with_provider(|provider| {
            let value = request.clone();
            async move { provider.estimate_gas(&value, None).await }
        })
            .map(|gas| gas.as_u64())
    }

    pub fn send_transaction(&self, signed_tx: &[u8]) -> ChainResult<String> {
        let bytes = Bytes::from(signed_tx.to_vec());
        let tx_hash = self.with_provider(|provider| {
            let payload = bytes.clone();
            async move { provider.send_raw_transaction(payload).await.map(|pending| pending.tx_hash()) }
        })?;

        Ok(format!("0x{}", hex::encode(tx_hash.as_bytes())))
    }

    pub fn get_receipt(&self, tx_hash: &str) -> ChainResult<Option<TransactionReceipt>> {
        let hash = H256::from_str(tx_hash).map_err(|error| ChainError::Other(error.to_string()))?;
        match self.with_provider(|provider| async move { provider.get_transaction_receipt(hash).await })? {
            Some(receipt) => TransactionReceipt::from_ethers(&receipt)
                .map(Some)
                .map_err(|error| ChainError::Other(error.to_string())),
            None => Ok(None),
        }
    }

    pub fn wait_for_receipt(
        &self,
        tx_hash: &str,
        timeout: u64,
        poll_interval: f64,
    ) -> ChainResult<TransactionReceipt> {
        let hash = H256::from_str(tx_hash).map_err(|error| ChainError::Other(error.to_string()))?;
        let deadline = Instant::now() + Duration::from_secs(timeout.max(self.timeout_secs));
        let poll_duration = Duration::from_secs_f64(poll_interval.max(0.1));

        loop {
            if Instant::now() >= deadline {
                return Err(ChainError::Timeout);
            }

            if let Some(receipt) = self.get_receipt_internal(hash)? {
                return Ok(receipt);
            }

            std::thread::sleep(poll_duration);
        }
    }

    pub fn call(&self, tx: &TransactionRequest, block: &str) -> ChainResult<Vec<u8>> {
        let request: TypedTransaction = tx.to_ethers_request().into();
        let block_number = match block {
            "latest" => BlockNumber::Latest,
            "pending" => BlockNumber::Pending,
            _ => BlockNumber::Latest,
        };

        self.with_provider(|provider| {
            let value = request.clone();
            async move { provider.call(&value, Some(block_number.into())).await }
        })
            .map(|bytes| bytes.to_vec())
    }

    fn get_receipt_internal(&self, hash: H256) -> ChainResult<Option<TransactionReceipt>> {
        let receipt = self.with_provider(|provider| async move { provider.get_transaction_receipt(hash).await })?;
        match receipt {
            Some(value) => TransactionReceipt::from_ethers(&value)
                .map(Some)
                .map_err(|error| ChainError::Other(error.to_string())),
            None => Ok(None),
        }
    }

    fn with_provider<T, F, Fut>(&self, mut operation: F) -> ChainResult<T>
    where
        F: FnMut(Provider<Http>) -> Fut,
        Fut: std::future::Future<Output = Result<T, ethers::providers::ProviderError>>,
    {
        let runtime = tokio::runtime::Runtime::new().map_err(|error| ChainError::Other(error.to_string()))?;
        let mut last_error = None;

        for url in &self.rpc_urls {
            let provider = Provider::<Http>::try_from(url.as_str()).map_err(|error| ChainError::Rpc(error.to_string()))?;
            for _ in 0..=self.max_retries {
                let result = runtime.block_on(operation(provider.clone()));
                match result {
                    Ok(value) => return Ok(value),
                    Err(error) => last_error = Some(error.to_string()),
                }
            }
        }

        let msg = last_error.unwrap_or_else(|| "all RPC endpoints failed".to_string());
        Err(classify_rpc_error(&msg))
    }
}

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

