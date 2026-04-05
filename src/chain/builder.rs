use ethers::types::U256;
use ethers::types::Bytes;

use crate::core::types::{Address, TokenAmount, TransactionReceipt, TransactionRequest, GasPriority, BlockId, DEFAULT_GAS_BUFFER, MAINNET_CHAIN_ID};
use crate::core::wallet::WalletManager;

use super::client::ChainClient;
use super::errors::{ChainError, ChainResult};

/// Minimum threshold for gas buffer multiplier (1.0 = no buffer).
const MIN_MULTIPLIER_THRESHOLD: f64 = 1.0;

/// Default poll interval for transaction confirmations in seconds.
const DEFAULT_POLL_INTERVAL_SECS: f64 = 1.0;

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

    /// Estimates gas for the transaction and applies a buffer.
    pub async fn with_gas_estimate(mut self, buffer: Option<f64>) -> ChainResult<Self> {
        let request = self.build_partial_request(self.nonce)?;
        let estimated = self.client.estimate_gas(&request).await?;
        let multiplier = buffer.filter(|&b| b.is_finite() && b > MIN_MULTIPLIER_THRESHOLD).unwrap_or(DEFAULT_GAS_BUFFER);
        let limit = ((estimated as f64) * multiplier).ceil() as u64;
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
        self.max_fee_per_gas = Some(gas.get_max_fee(priority, DEFAULT_GAS_BUFFER));
        Ok(self)
    }

    /// Builds the final TransactionRequest, fetching the nonce if not set.
    pub async fn build(&self) -> ChainResult<TransactionRequest> {
        let nonce = match self.nonce {
            Some(value) => Some(value),
            None => {
                let wallet_address = Address::new(self.wallet.address())
                    .map_err(|_| ChainError::InvalidWalletAddress)?;
                Some(self.client.get_nonce(&wallet_address, BlockId::Pending).await?)
            }
        };

        self.build_partial_request(nonce)
    }

    /// Builds and signs the transaction.
    pub async fn build_and_sign(&self) -> ChainResult<Vec<u8>> {
        let request = self.build().await?;
        self.wallet
            .sign_transaction_bytes(&request)
            .await
            .map_err(|_| ChainError::SignTransactionFailed)
    }

    /// Builds, signs, and sends the transaction to the network.
    pub async fn send(&self) -> ChainResult<String> {
        let signed = self.build_and_sign().await?;
        self.client.send_transaction(&signed).await
    }

    /// Sends the transaction and waits for it to be confirmed.
    pub async fn send_and_wait(&self, timeout: u64) -> ChainResult<TransactionReceipt> {
        let tx_hash = self.send().await?;
        self.client.wait_for_receipt(&tx_hash, timeout, DEFAULT_POLL_INTERVAL_SECS).await
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
            data: self.data.clone().unwrap_or_default(),
            nonce,
            gas_limit: self.gas_limit,
            max_fee_per_gas: self.max_fee_per_gas,
            max_priority_fee: self.max_priority_fee,
            chain_id: self.chain_id,
        })
    }
}
