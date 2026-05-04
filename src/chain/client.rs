use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{BlockNumber, Bytes, H256, U64, U256, transaction::eip2718::TypedTransaction};

use crate::core::types::{
    Address, BlockId, GasPrice, PRIORITY_FEE_LOW_DIVISOR, PRIORITY_FEE_MEDIUM_DIVISOR, TokenAmount,
    TransactionReceipt, TransactionRequest,
};

use super::errors::{ChainError, ChainResult};
use tracing::{debug, info, instrument, warn};

/// Minimum interval between polling for transaction receipts.
pub const MIN_POLL_INTERVAL: f64 = 0.1;

/// A high-level client for interacting with the Ethereum blockchain.
///
/// Supports multiple RPC endpoints with automatic failover and retries.
/// Providers are created once at construction and reused across calls.
#[derive(Clone)]
pub struct ChainClient {
    rpc_urls: Vec<String>,
    providers: Vec<Arc<Provider<Http>>>,
    timeout_secs: u64,
    max_retries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcHealth {
    pub url: String,
    pub healthy: bool,
    pub block_number: Option<u64>,
    pub error: Option<String>,
}

impl ChainClient {
    /// Creates a new ChainClient.
    ///
    /// `rpc_urls` are tried in order; each endpoint is retried up to
    /// `max_retries` times for each operation.
    pub fn new(rpc_urls: Vec<String>, timeout_secs: u64, max_retries: usize) -> ChainResult<Self> {
        let providers = rpc_urls
            .iter()
            .map(|url| {
                Provider::<Http>::try_from(url.as_str())
                    .map(Arc::new)
                    .map_err(|error| ChainError::Rpc(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            rpc_urls,
            providers,
            timeout_secs,
            max_retries,
        })
    }

    pub fn rpc_urls(&self) -> &[String] {
        &self.rpc_urls
    }

    /// Returns a reference to the first (primary) provider, if any.
    pub fn provider(&self) -> Option<Arc<Provider<Http>>> {
        self.providers.first().cloned()
    }

    pub async fn health_check(&self) -> Vec<RpcHealth> {
        let mut out = Vec::with_capacity(self.providers.len());
        for (idx, provider) in self.providers.iter().enumerate() {
            match provider.get_block_number().await {
                Ok(block) => out.push(RpcHealth {
                    url: self.rpc_urls.get(idx).cloned().unwrap_or_default(),
                    healthy: true,
                    block_number: Some(block.as_u64()),
                    error: None,
                }),
                Err(error) => out.push(RpcHealth {
                    url: self.rpc_urls.get(idx).cloned().unwrap_or_default(),
                    healthy: false,
                    block_number: None,
                    error: Some(error.to_string()),
                }),
            }
        }
        out
    }

    pub async fn get_block_number(&self) -> ChainResult<u64> {
        self.with_provider(|provider| async move { provider.get_block_number().await })
            .await
            .map(|block| block.as_u64())
    }

    /// Fetches the native ETH balance for an address.
    #[instrument(skip(self), fields(address = %address.as_eth_address()))]
    pub async fn get_balance(&self, address: &Address) -> ChainResult<TokenAmount> {
        debug!("Fetching balance for address");
        let balance = self
            .with_provider(|provider| async move {
                provider.get_balance(address.as_eth_address(), None).await
            })
            .await?;

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
        match self.get_fee_history_gas_price(20).await {
            Ok(gas) => return Ok(gas),
            Err(error) => {
                warn!(error = %error, "eth_feeHistory failed; falling back to legacy gas price");
            }
        }

        self.get_legacy_gas_price().await
    }

    async fn get_fee_history_gas_price(&self, block_count: u64) -> ChainResult<GasPrice> {
        let percentiles = vec![25.0_f64, 50.0_f64, 75.0_f64];
        let history = self
            .with_provider(|provider| {
                let percentiles = percentiles.clone();
                async move {
                    provider
                        .fee_history(U256::from(block_count), BlockNumber::Latest, &percentiles)
                        .await
                }
            })
            .await?;

        gas_price_from_fee_history(&history.base_fee_per_gas, &history.reward)
            .ok_or_else(|| ChainError::Rpc("fee history response missing fee samples".into()))
    }

    async fn get_legacy_gas_price(&self) -> ChainResult<GasPrice> {
        let latest_block = self
            .with_provider(|provider| async move { provider.get_block(BlockNumber::Latest).await })
            .await?;
        let gas_price = self
            .with_provider(|provider| async move { provider.get_gas_price().await })
            .await?;

        let base_fee = latest_block
            .and_then(|block| block.base_fee_per_gas)
            .unwrap_or(gas_price);

        Ok(GasPrice {
            base_fee,
            priority_fee_low: gas_price / U256::from(PRIORITY_FEE_LOW_DIVISOR),
            priority_fee_medium: gas_price / U256::from(PRIORITY_FEE_MEDIUM_DIVISOR),
            priority_fee_high: gas_price,
        })
    }

    /// Estimates the gas required to execute a transaction.
    pub async fn estimate_gas(&self, tx: &TransactionRequest) -> ChainResult<u64> {
        let request: Arc<TypedTransaction> = Arc::new(tx.to_ethers_typed());
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
        let tx_hash = self
            .with_provider(|provider| {
                let payload = bytes.clone();
                async move {
                    provider
                        .send_raw_transaction(payload)
                        .await
                        .map(|pending| pending.tx_hash())
                }
            })
            .await?;

        info!(hash = %tx_hash, "Transaction sent successfully");
        Ok(format!("0x{}", hex::encode(tx_hash.as_bytes())))
    }

    /// Fetches a transaction receipt.
    pub async fn get_receipt(&self, tx_hash: &str) -> ChainResult<Option<TransactionReceipt>> {
        let hash = H256::from_str(tx_hash).map_err(|_| ChainError::InvalidTransactionHash)?;
        match self
            .with_provider(|provider| async move { provider.get_transaction_receipt(hash).await })
            .await?
        {
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
    ///
    /// When `BlockId::Latest` is requested, we first resolve it to a concrete
    /// block number.  Public RPC endpoints (e.g. `arb1.arbitrum.io/rpc`)
    /// cache `eth_call` responses keyed by the `"latest"` tag, so passing a
    /// numeric block ID forces a fresh read every time.
    pub async fn call(&self, tx: &TransactionRequest, block: BlockId) -> ChainResult<Vec<u8>> {
        let request: Arc<TypedTransaction> = Arc::new(tx.to_ethers_typed());
        let block_number = match block {
            BlockId::Latest => {
                let num = self.get_block_number().await?;
                BlockNumber::Number(U64::from(num))
            }
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
        let receipt = self
            .with_provider(|provider| async move { provider.get_transaction_receipt(hash).await })
            .await?;
        match receipt {
            Some(value) => TransactionReceipt::from_ethers(&value)
                .map(Some)
                .map_err(|_| ChainError::InvalidReceiptData),
            None => Ok(None),
        }
    }

    async fn with_provider<T, F, Fut>(&self, mut operation: F) -> ChainResult<T>
    where
        F: FnMut(Arc<Provider<Http>>) -> Fut,
        Fut: std::future::Future<Output = Result<T, ethers::providers::ProviderError>>,
    {
        let mut last_error: Option<ethers::providers::ProviderError> = None;

        for (url_idx, provider) in self.providers.iter().enumerate() {
            for retry in 0..=self.max_retries {
                if retry > 0 {
                    debug!(
                        url_idx,
                        url = %self.rpc_urls.get(url_idx).map(String::as_str).unwrap_or("<unknown>"),
                        retry,
                        "Retrying RPC operation"
                    );
                    tokio::time::sleep(Duration::from_millis(100 * 2u64.pow(retry as u32 - 1)))
                        .await;
                }
                let result = operation(Arc::clone(provider)).await;
                match result {
                    Ok(value) => return Ok(value),
                    Err(error) => {
                        warn!(
                            url_idx,
                            url = %self.rpc_urls.get(url_idx).map(String::as_str).unwrap_or("<unknown>"),
                            retry,
                            error = %error,
                            "RPC operation failed"
                        );
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

fn gas_price_from_fee_history(base_fees: &[U256], rewards: &[Vec<U256>]) -> Option<GasPrice> {
    let base_fee = *base_fees.last()?;
    let mut low = Vec::new();
    let mut medium = Vec::new();
    let mut high = Vec::new();

    for row in rewards {
        if row.len() >= 3 {
            low.push(row[0]);
            medium.push(row[1]);
            high.push(row[2]);
        }
    }

    Some(GasPrice {
        base_fee,
        priority_fee_low: average_u256(&low)?,
        priority_fee_medium: average_u256(&medium)?,
        priority_fee_high: average_u256(&high)?,
    })
}

fn average_u256(values: &[U256]) -> Option<U256> {
    if values.is_empty() {
        return None;
    }
    let sum = values
        .iter()
        .copied()
        .fold(U256::zero(), |acc, v| acc.saturating_add(v));
    Some(sum / U256::from(values.len()))
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

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_RPC_URL: &str = "http://127.0.0.1:1";
    const TEST_TIMEOUT: u64 = 1;
    const TEST_RETRIES: usize = 0;
    const TEST_RECIPIENT: &str = "0x0000000000000000000000000000000000000001";

    fn setup_failing_client() -> (ChainClient, Address) {
        let client =
            ChainClient::new(vec![TEST_RPC_URL.to_string()], TEST_TIMEOUT, TEST_RETRIES).unwrap();
        let address = Address::new(TEST_RECIPIENT).unwrap();
        (client, address)
    }

    #[test]
    fn client_preserves_rpc_urls_for_diagnostics() {
        let client = ChainClient::new(
            vec![
                "http://127.0.0.1:1".to_string(),
                "http://127.0.0.1:2".to_string(),
            ],
            TEST_TIMEOUT,
            TEST_RETRIES,
        )
        .unwrap();
        assert_eq!(
            client.rpc_urls(),
            &[
                "http://127.0.0.1:1".to_string(),
                "http://127.0.0.1:2".to_string()
            ]
        );
    }

    #[test]
    fn classify_insufficient_funds() {
        let err = classify_rpc_error("execution reverted: insufficient funds for transfer");
        assert!(matches!(err, ChainError::InsufficientFunds));
    }

    #[test]
    fn classify_insufficient_balance() {
        let err = classify_rpc_error("sender has insufficient balance");
        assert!(matches!(err, ChainError::InsufficientFunds));
    }

    #[test]
    fn classify_nonce_too_low() {
        let err = classify_rpc_error("nonce too low");
        assert!(matches!(err, ChainError::NonceTooLow));
    }

    #[test]
    fn classify_nonce_already_used() {
        let err = classify_rpc_error("nonce has already been used");
        assert!(matches!(err, ChainError::NonceTooLow));
    }

    #[test]
    fn classify_replacement_underpriced() {
        let err = classify_rpc_error("replacement transaction underpriced");
        assert!(matches!(err, ChainError::ReplacementUnderpriced));
    }

    #[test]
    fn classify_already_known() {
        let err = classify_rpc_error("already known");
        assert!(matches!(err, ChainError::ReplacementUnderpriced));
    }

    #[test]
    fn classify_timeout() {
        let err = classify_rpc_error("request timed out after 30s");
        assert!(matches!(err, ChainError::Timeout));
    }

    #[test]
    fn classify_generic_rpc_error() {
        let err = classify_rpc_error("some random RPC failure xyz");
        assert!(matches!(err, ChainError::Rpc(_)));
    }

    #[test]
    fn gas_price_from_fee_history_uses_latest_base_and_average_tips() {
        let gwei = U256::from(1_000_000_000u64);
        let gas = gas_price_from_fee_history(
            &[
                U256::from(30u64) * gwei,
                U256::from(31u64) * gwei,
                U256::from(32u64) * gwei,
            ],
            &[
                vec![gwei, U256::from(2u64) * gwei, U256::from(3u64) * gwei],
                vec![
                    U256::from(3u64) * gwei,
                    U256::from(4u64) * gwei,
                    U256::from(5u64) * gwei,
                ],
            ],
        )
        .unwrap();

        assert_eq!(gas.base_fee, U256::from(32u64) * gwei);
        assert_eq!(gas.priority_fee_low, U256::from(2u64) * gwei);
        assert_eq!(gas.priority_fee_medium, U256::from(3u64) * gwei);
        assert_eq!(gas.priority_fee_high, U256::from(4u64) * gwei);
    }

    #[test]
    fn gas_price_from_fee_history_rejects_missing_reward_samples() {
        let gas = gas_price_from_fee_history(&[U256::from(1u64)], &[vec![U256::from(1u64)]]);
        assert!(gas.is_none());
    }

    #[tokio::test]
    async fn client_with_zero_retries_fails_immediately() {
        let (client, address) = setup_failing_client();
        let result = client.get_balance(&address).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn client_retry_exhausts_all_urls() {
        let client = ChainClient::new(
            vec![
                "http://127.0.0.1:1".to_string(),
                "http://127.0.0.1:2".to_string(),
            ],
            TEST_TIMEOUT,
            1,
        )
        .unwrap();
        let address = Address::new(TEST_RECIPIENT).unwrap();
        let result = client.get_balance(&address).await;
        assert!(result.is_err());
    }
}
