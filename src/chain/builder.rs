use ethers::types::Bytes;
use ethers::types::U256;

use crate::core::types::{
    Address, BPS_SCALE, BlockId, DEFAULT_GAS_BUFFER_BPS, GasPriority, MAINNET_CHAIN_ID,
    TokenAmount, TransactionReceipt, TransactionRequest,
};
use crate::core::wallet::WalletManager;

use super::client::ChainClient;
use super::errors::{ChainError, ChainResult};

/// Minimum buffer in basis points for gas estimation (1.0× = 10_000 bps).
const MIN_GAS_ESTIMATE_BUFFER_BPS: u64 = BPS_SCALE;

/// Default poll interval for transaction confirmations in seconds.
const DEFAULT_POLL_INTERVAL_SECS: f64 = 1.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedTransaction {
    pub raw: Vec<u8>,
    pub raw_hex: String,
    pub tx_hash: String,
}

/// A fluent builder for creating and sending Ethereum transactions.
#[derive(Clone)]
pub struct TransactionBuilder {
    client: ChainClient,
    wallet: WalletManager,
    to: Option<Address>,
    value: Option<TokenAmount>,
    data: Option<Bytes>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    max_fee_per_gas: Option<U256>,
    max_priority_fee: Option<U256>,
    chain_id: u64,
}

impl TransactionBuilder {
    /// Creates a new builder for the given client and wallet.
    ///
    /// Defaults:
    /// - zero ETH value
    /// - empty calldata
    /// - mainnet chain id
    /// - nonce/gas/fees resolved later
    pub fn new(client: ChainClient, wallet: WalletManager) -> Self {
        Self {
            client,
            wallet,
            to: None,
            value: Some(TokenAmount::eth(0)),
            data: Some(Bytes::new()),
            nonce: None,
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id: MAINNET_CHAIN_ID,
        }
    }

    /// Sets the destination address.
    pub fn to(mut self, address: Address) -> Self {
        self.to = Some(address);
        self
    }

    /// Sets the transaction value (amount).
    pub fn value(mut self, amount: TokenAmount) -> Self {
        self.value = Some(amount);
        self
    }

    /// Sets the transaction input data (calldata).
    pub fn data(mut self, calldata: Vec<u8>) -> Self {
        self.data = Some(Bytes::from(calldata));
        self
    }

    /// Sets a custom nonce.
    pub fn nonce(mut self, nonce: u64) -> Self {
        self.nonce = Some(nonce);
        self
    }

    /// Sets a custom gas limit.
    pub fn gas_limit(mut self, limit: u64) -> Self {
        self.gas_limit = Some(limit);
        self
    }

    /// Sets the target chain ID.
    pub fn chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = chain_id;
        self
    }

    /// Returns the configured EIP-1559 max fee per gas, if resolved.
    pub fn max_fee_per_gas(&self) -> Option<U256> {
        self.max_fee_per_gas
    }

    /// Estimates gas for the transaction and applies a buffer in basis points.
    ///
    /// When `buffer_bps` is `None` or below [`MIN_GAS_ESTIMATE_BUFFER_BPS`],
    /// [`DEFAULT_GAS_BUFFER_BPS`] is applied (12_000 bps = 1.2×).
    pub async fn with_gas_estimate(mut self, buffer_bps: Option<u64>) -> ChainResult<Self> {
        let request = self.build_partial_request(self.nonce)?;
        let estimated = self.client.estimate_gas(&request).await?;
        let multiplier_bps = buffer_bps
            .filter(|&b| b >= MIN_GAS_ESTIMATE_BUFFER_BPS)
            .unwrap_or(DEFAULT_GAS_BUFFER_BPS);
        let limit =
            (U256::from(estimated) * U256::from(multiplier_bps) / U256::from(BPS_SCALE)).as_u64();
        self.gas_limit = Some(limit);
        Ok(self)
    }

    /// Fetches current gas prices and sets max fees based on priority (Low, Medium, High).
    pub async fn with_gas_price(mut self, priority: GasPriority) -> ChainResult<Self> {
        let gas = self.client.get_gas_price().await?;
        let priority_fee = match priority {
            GasPriority::Low => gas.priority_fee_low,
            GasPriority::High => gas.priority_fee_high,
            GasPriority::Medium => gas.priority_fee_medium,
        };
        self.max_priority_fee = Some(priority_fee);
        self.max_fee_per_gas = Some(gas.get_max_fee(priority, DEFAULT_GAS_BUFFER_BPS));
        Ok(self)
    }

    /// Builds the final TransactionRequest, fetching the nonce if not set.
    pub async fn build(self) -> ChainResult<TransactionRequest> {
        let nonce = match self.nonce {
            Some(value) => Some(value),
            None => {
                let wallet_address = Address::new(self.wallet.address())
                    .map_err(|_| ChainError::InvalidWalletAddress)?;
                Some(
                    self.client
                        .get_nonce(&wallet_address, BlockId::Pending)
                        .await?,
                )
            }
        };

        self.build_partial_request(nonce)
    }

    /// Builds and signs the transaction.
    pub async fn build_and_sign(self) -> ChainResult<Vec<u8>> {
        let wallet = self.wallet.clone();
        let request = self.build().await?;
        wallet
            .sign_transaction_bytes(&request)
            .await
            .map_err(|e| ChainError::SignTransactionFailed(e.to_string()))
    }

    pub async fn build_and_sign_with_hash(self) -> ChainResult<SignedTransaction> {
        let raw = self.build_and_sign().await?;
        let hash = ethers::utils::keccak256(&raw);
        Ok(SignedTransaction {
            raw_hex: format!("0x{}", hex::encode(&raw)),
            tx_hash: format!("0x{}", hex::encode(hash)),
            raw,
        })
    }

    /// Builds, signs, and sends the transaction to the network.
    pub async fn send(self) -> ChainResult<String> {
        let wallet = self.wallet.clone();
        let client = self.client.clone();
        let request = self.build().await?;
        let signed = wallet
            .sign_transaction_bytes(&request)
            .await
            .map_err(|e| ChainError::SignTransactionFailed(e.to_string()))?;
        client.send_transaction(&signed).await
    }

    /// Sends the transaction and waits for it to be confirmed.
    pub async fn send_and_wait(self, timeout: u64) -> ChainResult<TransactionReceipt> {
        let client = self.client.clone();
        let tx_hash = self.send().await?;
        client
            .wait_for_receipt(&tx_hash, timeout, DEFAULT_POLL_INTERVAL_SECS)
            .await
    }

    fn build_partial_request(&self, nonce: Option<u64>) -> ChainResult<TransactionRequest> {
        let to = self
            .to
            .clone()
            .ok_or(ChainError::MissingDestinationAddress)?;
        let value = self
            .value
            .clone()
            .ok_or(ChainError::MissingTransactionValue)?;

        Ok(TransactionRequest {
            to,
            value,
            data: self.data.clone().expect("data always initialized in new()"),
            nonce,
            gas_limit: self.gas_limit,
            max_fee_per_gas: self.max_fee_per_gas,
            max_priority_fee: self.max_priority_fee,
            chain_id: self.chain_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::MIN_GAS_LIMIT;

    const TEST_RPC_URL: &str = "http://localhost:8545";
    const TEST_TIMEOUT: u64 = 5;
    const TEST_RETRIES: usize = 0;
    const TEST_RECIPIENT: &str = "0x0000000000000000000000000000000000000001";

    fn setup() -> (ChainClient, WalletManager) {
        let client =
            ChainClient::new(vec![TEST_RPC_URL.to_string()], TEST_TIMEOUT, TEST_RETRIES).unwrap();
        let wallet = WalletManager::generate().unwrap();
        (client, wallet)
    }

    fn test_address() -> Address {
        Address::new(TEST_RECIPIENT).unwrap()
    }

    #[tokio::test]
    async fn builder_requires_destination() {
        let (client, wallet) = setup();
        let result = TransactionBuilder::new(client, wallet)
            .nonce(0)
            .gas_limit(MIN_GAS_LIMIT)
            .build()
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn builder_uses_default_eth_zero() {
        let (client, wallet) = setup();
        let to = test_address();
        let tx = TransactionBuilder::new(client, wallet)
            .to(to)
            .nonce(0)
            .gas_limit(MIN_GAS_LIMIT)
            .build()
            .await;
        assert!(tx.is_ok());
        assert_eq!(tx.unwrap().value, TokenAmount::eth(0));
    }

    #[tokio::test]
    async fn builder_uses_default_mainnet_chain_id() {
        let (client, wallet) = setup();
        let to = test_address();
        let tx = TransactionBuilder::new(client, wallet)
            .to(to)
            .nonce(0)
            .gas_limit(MIN_GAS_LIMIT)
            .build()
            .await
            .unwrap();
        assert_eq!(tx.chain_id, MAINNET_CHAIN_ID);
    }

    #[tokio::test]
    async fn builder_preserves_custom_data() {
        let (client, wallet) = setup();
        let to = test_address();
        let data = vec![1, 2, 3, 4];
        let tx = TransactionBuilder::new(client, wallet)
            .to(to)
            .data(data.clone())
            .nonce(0)
            .gas_limit(MIN_GAS_LIMIT)
            .build()
            .await
            .unwrap();
        assert_eq!(tx.data, Bytes::from(data));
    }

    #[tokio::test]
    async fn builder_preserves_custom_nonce() {
        let (client, wallet) = setup();
        let to = test_address();
        let tx = TransactionBuilder::new(client, wallet)
            .to(to)
            .nonce(42)
            .gas_limit(MIN_GAS_LIMIT)
            .build()
            .await
            .unwrap();
        assert_eq!(tx.nonce, Some(42));
    }

    #[tokio::test]
    async fn builder_builds_with_explicit_fields() {
        let (client, wallet) = setup();
        let to = test_address();
        let amount = TokenAmount::from_eth("0.001").unwrap();
        let tx = TransactionBuilder::new(client, wallet)
            .to(to.clone())
            .value(amount.clone())
            .nonce(0)
            .gas_limit(MIN_GAS_LIMIT)
            .chain_id(11155111)
            .build()
            .await
            .unwrap();
        assert_eq!(tx.to, to);
        assert_eq!(tx.value, amount);
        assert_eq!(tx.nonce, Some(0));
        assert_eq!(tx.gas_limit, Some(MIN_GAS_LIMIT));
        assert_eq!(tx.chain_id, 11155111);
    }

    #[tokio::test]
    async fn builder_signs_transaction_bytes() {
        let (client, wallet) = setup();
        let to = test_address();
        let signed = TransactionBuilder::new(client, wallet)
            .to(to)
            .value(TokenAmount::eth(1u64))
            .nonce(0)
            .gas_limit(MIN_GAS_LIMIT)
            .chain_id(11155111)
            .build_and_sign()
            .await
            .unwrap();
        assert!(!signed.is_empty());
    }
}
