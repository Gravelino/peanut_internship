use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Add;
use std::str::FromStr;

use ethers::types::{Address as EthAddress, Bytes, TransactionRequest as EthTransactionRequest, U256};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error("invalid Ethereum address: {0}")]
    InvalidAddress(String),
    #[error("invalid token amount: {0}")]
    InvalidTokenAmount(String),
    #[error("token amount decimals mismatch: left={left}, right={right}")]
    TokenDecimalsMismatch { left: u8, right: u8 },
    #[error("invalid transaction request: {0}")]
    InvalidTransactionRequest(String),
    #[error("invalid receipt: {0}")]
    InvalidReceipt(String),
    #[error("invalid serialization: {0}")]
    InvalidSerialization(String),
}

#[derive(Clone, Eq)]
pub struct Address {
    value: String,
}

impl Address {
    pub fn new(value: impl AsRef<str>) -> Result<Self, CoreError> {
        let raw = value.as_ref().trim();
        let parsed = EthAddress::from_str(raw).map_err(|_| CoreError::InvalidAddress(raw.to_string()))?;
        Ok(Self {
            value: ethers::utils::to_checksum(&parsed, None),
        })
    }

    pub fn from_string(value: &str) -> Result<Self, CoreError> {
        Self::new(value)
    }

    pub fn checksum(&self) -> String {
        self.value.clone()
    }

    pub fn lower(&self) -> String {
        self.value.to_lowercase()
    }

    pub fn as_eth_address(&self) -> EthAddress {
        EthAddress::from_str(&self.value).expect("checksummed address is valid")
    }
}

impl PartialEq for Address {
    fn eq(&self, other: &Self) -> bool {
        self.lower() == other.lower()
    }
}

impl Hash for Address {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.lower().hash(state);
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Address").field(&self.value).finish()
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

impl TryFrom<&str> for Address {
    type Error = CoreError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenAmount {
    pub raw: U256,
    pub decimals: u8,
    pub symbol: Option<String>,
}

impl TokenAmount {
    pub fn from_human(amount: impl ToString, decimals: u8, symbol: Option<String>) -> Result<Self, CoreError> {
        let amount_string = amount.to_string();
        let decimal = Decimal::from_str(&amount_string)
            .map_err(|_| CoreError::InvalidTokenAmount(amount_string.clone()))?;

        if decimal.is_sign_negative() {
            return Err(CoreError::InvalidTokenAmount("negative amounts are not supported".to_string()));
        }

        let scale = Decimal::from_u128(10u128.saturating_pow(decimals as u32))
            .ok_or_else(|| CoreError::InvalidTokenAmount("invalid decimal scale".to_string()))?;
        let raw_decimal = decimal * scale;

        if raw_decimal.fract() != Decimal::ZERO {
            return Err(CoreError::InvalidTokenAmount(format!(
                "amount {amount_string} cannot be represented exactly with {decimals} decimals"
            )));
        }

        let raw_string = raw_decimal.trunc().to_string();
        let raw = U256::from_dec_str(&raw_string)
            .map_err(|_| CoreError::InvalidTokenAmount(raw_string.clone()))?;

        Ok(Self { raw, decimals, symbol })
    }

    pub fn human(&self) -> Decimal {
        let raw_decimal = Decimal::from_str(&self.raw.to_string()).expect("U256 string is valid decimal");
        let scale = Decimal::from_u128(10u128.saturating_pow(self.decimals as u32)).expect("valid scale");
        raw_decimal / scale
    }

    pub fn checked_add(&self, other: &Self) -> Result<Self, CoreError> {
        if self.decimals != other.decimals {
            return Err(CoreError::TokenDecimalsMismatch {
                left: self.decimals,
                right: other.decimals,
            });
        }

        Ok(Self {
            raw: self.raw + other.raw,
            decimals: self.decimals,
            symbol: self.symbol.clone().or_else(|| other.symbol.clone()),
        })
    }

    pub fn checked_mul_decimal(&self, factor: Decimal) -> Result<Self, CoreError> {
        if factor.is_sign_negative() {
            return Err(CoreError::InvalidTokenAmount("negative factor is not supported".to_string()));
        }

        let product = self.human() * factor;
        Self::from_human(product, self.decimals, self.symbol.clone())
    }

    pub fn checked_mul_int(&self, factor: u64) -> Result<Self, CoreError> {
        Ok(Self {
            raw: self.raw * U256::from(factor),
            decimals: self.decimals,
            symbol: self.symbol.clone(),
        })
    }
}

impl fmt::Display for TokenAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(symbol) = &self.symbol {
            write!(f, "{}{}", self.human(), symbol)
        } else {
            write!(f, "{}", self.human())
        }
    }
}

#[derive(Clone, Debug, Eq)]
pub struct Token {
    pub address: Address,
    pub symbol: String,
    pub decimals: u8,
}

impl PartialEq for Token {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
    }
}

impl Hash for Token {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.symbol, self.address)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionRequest {
    pub to: Address,
    pub value: TokenAmount,
    pub data: Bytes,
    pub nonce: Option<u64>,
    pub gas_limit: Option<u64>,
    pub max_fee_per_gas: Option<U256>,
    pub max_priority_fee: Option<U256>,
    pub chain_id: u64,
}

impl TransactionRequest {
    pub fn to_dict(&self) -> BTreeMap<String, Value> {
        let mut result = BTreeMap::new();
        result.insert("to".to_string(), Value::String(self.to.checksum()));
        result.insert("value".to_string(), Value::String(self.value.raw.to_string()));
        result.insert("data".to_string(), Value::String(format!("0x{}", hex::encode(self.data.as_ref()))));
        result.insert("chainId".to_string(), Value::Number(self.chain_id.into()));

        if let Some(nonce) = self.nonce {
            result.insert("nonce".to_string(), Value::Number(nonce.into()));
        }

        if let Some(gas_limit) = self.gas_limit {
            result.insert("gas".to_string(), Value::Number(gas_limit.into()));
        }

        if let Some(max_fee_per_gas) = self.max_fee_per_gas {
            result.insert("maxFeePerGas".to_string(), Value::String(max_fee_per_gas.to_string()));
        }

        if let Some(max_priority_fee) = self.max_priority_fee {
            result.insert("maxPriorityFeePerGas".to_string(), Value::String(max_priority_fee.to_string()));
        }

        result
    }

    pub fn to_ethers_request(&self) -> EthTransactionRequest {
        let mut request = EthTransactionRequest::new();
        request.to = Some(self.to.as_eth_address().into());
        request.value = Some(self.value.raw);
        request.data = Some(self.data.clone());
        request.chain_id = Some(self.chain_id.into());
        request.gas_price = self.max_fee_per_gas.or(self.max_priority_fee);

        if let Some(nonce) = self.nonce {
            request.nonce = Some(nonce.into());
        }

        if let Some(gas_limit) = self.gas_limit {
            request.gas = Some(gas_limit.into());
        }

        request
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        if self.chain_id == 0 {
            return Err(CoreError::InvalidTransactionRequest(
                "chain_id must be non-zero".to_string(),
            ));
        }

        if let (Some(fee), Some(priority)) = (self.max_fee_per_gas, self.max_priority_fee) {
            if priority > fee {
                return Err(CoreError::InvalidTransactionRequest(
                    "maxPriorityFeePerGas cannot exceed maxFeePerGas".to_string(),
                ));
            }
        }

        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionReceipt {
    pub tx_hash: String,
    pub block_number: u64,
    pub status: bool,
    pub gas_used: U256,
    pub effective_gas_price: U256,
    pub logs: Vec<Value>,
}

impl TransactionReceipt {
    pub fn tx_fee(&self) -> TokenAmount {
        let raw = self.gas_used * self.effective_gas_price;
        TokenAmount {
            raw,
            decimals: 18,
            symbol: Some("ETH".to_string()),
        }
    }

    pub fn from_ethers(receipt: &ethers::types::TransactionReceipt) -> Result<Self, CoreError> {
        Ok(Self {
            tx_hash: format!("0x{}", hex::encode(receipt.transaction_hash.as_bytes())),
            block_number: receipt
                .block_number
                .map(|n| n.as_u64())
                .ok_or_else(|| CoreError::InvalidReceipt("missing block number".to_string()))?,
            status: receipt.status.map(|status| status.as_u64() == 1).unwrap_or(false),
            gas_used: receipt.gas_used.unwrap_or_default(),
            effective_gas_price: receipt.effective_gas_price.unwrap_or_default(),
            logs: receipt
                .logs
                .iter()
                .map(|log| serde_json::to_value(log).unwrap_or(Value::Null))
                .collect(),
        })
    }

    pub fn from_web3(receipt: &Value) -> Result<Self, CoreError> {
        let tx_hash = receipt
            .get("transactionHash")
            .or_else(|| receipt.get("tx_hash"))
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::InvalidReceipt("missing transaction hash".to_string()))?
            .to_string();

        let block_number = receipt
            .get("blockNumber")
            .or_else(|| receipt.get("block_number"))
            .and_then(Value::as_u64)
            .ok_or_else(|| CoreError::InvalidReceipt("missing block number".to_string()))?;

        let status = receipt
            .get("status")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| receipt.get("status").and_then(Value::as_u64).map(|value| value == 1).unwrap_or(false));

        let gas_used = parse_u256_field(receipt, &["gasUsed", "gas_used"]).ok_or_else(|| {
            CoreError::InvalidReceipt("missing gas used".to_string())
        })?;

        let effective_gas_price = parse_u256_field(receipt, &["effectiveGasPrice", "effective_gas_price"]).unwrap_or_default();

        let logs = receipt
            .get("logs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        Ok(Self {
            tx_hash,
            block_number,
            status,
            gas_used,
            effective_gas_price,
            logs,
        })
    }
}

fn parse_u256_field(value: &Value, keys: &[&str]) -> Option<U256> {
    for key in keys {
        if let Some(field) = value.get(key) {
            if let Some(raw) = field.as_u64() {
                return Some(U256::from(raw));
            }

            if let Some(raw) = field.as_str() {
                if let Ok(parsed) = U256::from_dec_str(raw) {
                    return Some(parsed);
                }

                if let Some(stripped) = raw.strip_prefix("0x") {
                    if let Ok(parsed) = U256::from_str_radix(stripped, 16) {
                        return Some(parsed);
                    }
                }
            }
        }
    }

    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GasPrice {
    pub base_fee: U256,
    pub priority_fee_low: U256,
    pub priority_fee_medium: U256,
    pub priority_fee_high: U256,
}

impl GasPrice {
    pub fn get_max_fee(&self, priority: &str, buffer: f64) -> U256 {
        let priority_fee = match priority {
            "low" => self.priority_fee_low,
            "high" => self.priority_fee_high,
            _ => self.priority_fee_medium,
        };

        let base_fee_decimal = Decimal::from_str(&self.base_fee.to_string()).expect("U256 string is valid decimal");
        let buffer_decimal = Decimal::from_f64(buffer).unwrap_or_else(|| Decimal::new(12, 1));
        let value = (base_fee_decimal * buffer_decimal).ceil() + Decimal::from_str(&priority_fee.to_string()).expect("U256 string is valid decimal");
        U256::from_dec_str(&value.trunc().to_string()).unwrap_or_default()
    }
}

impl Add for TokenAmount {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        self.checked_add(&rhs).expect("token decimals must match")
    }
}
