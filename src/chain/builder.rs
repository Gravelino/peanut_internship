use ethers::types::U256;

use crate::core::types::{Address, TokenAmount, TransactionReceipt, TransactionRequest};
use crate::core::wallet::WalletManager;

use super::client::ChainClient;
use super::errors::{ChainError, ChainResult};

#[derive(Clone)]
pub struct TransactionBuilder {
    client: ChainClient,
    wallet: WalletManager,
    to: Option<Address>,
    value: Option<TokenAmount>,
    data: Option<Vec<u8>>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    max_fee_per_gas: Option<U256>,
    max_priority_fee: Option<U256>,
    chain_id: u64,
}

impl TransactionBuilder {
    pub fn new(client: ChainClient, wallet: WalletManager) -> Self {
        Self {
            client,
            wallet,
            to: None,
            value: Some(TokenAmount {
                raw: U256::zero(),
                decimals: 18,
                symbol: Some("ETH".to_string()),
            }),
            data: Some(Vec::new()),
            nonce: None,
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id: 1,
        }
    }

    pub fn to(mut self, address: Address) -> Self {
        self.to = Some(address);
        self
    }

    pub fn value(mut self, amount: TokenAmount) -> Self {
        self.value = Some(amount);
        self
    }

    pub fn data(mut self, calldata: Vec<u8>) -> Self {
        self.data = Some(calldata);
        self
    }

    pub fn nonce(mut self, nonce: u64) -> Self {
        self.nonce = Some(nonce);
        self
    }

    pub fn gas_limit(mut self, limit: u64) -> Self {
        self.gas_limit = Some(limit);
        self
    }

    pub fn chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = chain_id;
        self
    }

    pub fn with_gas_estimate(mut self, buffer: f64) -> ChainResult<Self> {
        let request = self.build_partial_request()?;
        let estimated = self.client.estimate_gas(&request)?;
        let multiplier = if buffer.is_finite() && buffer > 1.0 { buffer } else { 1.2 };
        let limit = ((estimated as f64) * multiplier).ceil() as u64;
        self.gas_limit = Some(limit);
        Ok(self)
    }

    pub fn with_gas_price(mut self, priority: &str) -> ChainResult<Self> {
        let gas = self.client.get_gas_price()?;
        let priority_fee = match priority {
            "low" => gas.priority_fee_low,
            "high" => gas.priority_fee_high,
            _ => gas.priority_fee_medium,
        };
        self.max_priority_fee = Some(priority_fee);
        self.max_fee_per_gas = Some(gas.get_max_fee(priority, 1.2));
        Ok(self)
    }

    pub fn build(mut self) -> ChainResult<TransactionRequest> {
        if self.nonce.is_none() {
            let wallet_address = Address::new(self.wallet.address()).map_err(|error| ChainError::Other(error.to_string()))?;
            self.nonce = Some(self.client.get_nonce(&wallet_address, "pending")?);
        }

        self.build_partial_request()
    }

    pub fn build_and_sign(self) -> ChainResult<Vec<u8>> {
        let wallet = self.wallet.clone();
        let request = self.build()?;
        wallet
            .sign_transaction_bytes(&request)
            .map_err(|error| ChainError::Other(error.to_string()))
    }

    pub fn send(self) -> ChainResult<String> {
        let client = self.client.clone();
        let signed = self.build_and_sign()?;
        client.send_transaction(&signed)
    }

    pub fn send_and_wait(self, timeout: u64) -> ChainResult<TransactionReceipt> {
        let client = self.client.clone();
        let tx_hash = self.send()?;
        client.wait_for_receipt(&tx_hash, timeout, 1.0)
    }

    fn build_partial_request(&self) -> ChainResult<TransactionRequest> {
        let to = self
            .to
            .clone()
            .ok_or_else(|| ChainError::Other("missing destination address".to_string()))?;
        let value = self
            .value
            .clone()
            .ok_or_else(|| ChainError::Other("missing value".to_string()))?;

        Ok(TransactionRequest {
            to,
            value,
            data: self.data.clone().unwrap_or_default().into(),
            nonce: self.nonce,
            gas_limit: self.gas_limit,
            max_fee_per_gas: self.max_fee_per_gas,
            max_priority_fee: self.max_priority_fee,
            chain_id: self.chain_id,
        })
    }
}
