use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Add;
use std::str::FromStr;

use ethers::types::{
    Address as EthAddress, Bytes, TransactionRequest as EthTransactionRequest, U256,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Minimum gas limit for a simple ETH transfer.
pub const MIN_GAS_LIMIT: u64 = 21_000;

/// Default multiplier for gas estimation buffers.
pub const DEFAULT_GAS_BUFFER: f64 = 1.2;

/// Basis points scale (10,000 = 100%).
pub const BPS_SCALE: u64 = 10_000;

/// Minimum allowed gas buffer multiplier.
pub const MIN_GAS_BUFFER_THRESHOLD: f64 = 0.0;

/// Status code for a successful transaction receipt.
pub const RECEIPT_STATUS_SUCCESS: u64 = 1;

/// Status code for a failed transaction receipt.
pub const RECEIPT_STATUS_FAILED: u64 = 0;

/// Default Chain ID for Ethereum Mainnet.
pub const MAINNET_CHAIN_ID: u64 = 1;

/// Chain ID for Sepolia Testnet.
pub const SEPOLIA_CHAIN_ID: u64 = 11155111;

/// Multiplier to convert Gwei to Wei.
pub const WEI_PER_GWEI: u128 = 1_000_000_000;

/// Divisor for low-priority fee suggestions (`base / 4`).
pub const PRIORITY_FEE_LOW_DIVISOR: u64 = 4;

/// Divisor for medium-priority fee suggestions (`base / 2`).
pub const PRIORITY_FEE_MEDIUM_DIVISOR: u64 = 2;

/// String representation of success in hex.
pub const SUCCESS_HEX: &str = "0x1";

/// String representation of success as a decimal.
pub const SUCCESS_STR: &str = "1";

/// String representation of success as a boolean string.
pub const SUCCESS_BOOL_STR: &str = "true";

/// Base for decimal scale calculations.
pub const DECIMAL_BASE: u128 = 10;

/// Number of decimals for the native ETH token.
pub const ETH_DECIMALS: u8 = 18;

/// Ticker symbol for the native ETH token.
pub const ETH_SYMBOL: &str = "ETH";

/// Status of an Ethereum transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionStatus {
    /// Transaction succeeded and was mined.
    Success,
    /// Transaction failed (e.g., execution reverted).
    Failed,
    /// Transaction is still in the mempool or unknown.
    Pending,
}

impl fmt::Display for TransactionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => write!(f, "SUCCESS"),
            Self::Failed => write!(f, "FAILED"),
            Self::Pending => write!(f, "PENDING"),
        }
    }
}

/// Priority levels for gas price estimation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GasPriority {
    /// Low priority (conservative estimation).
    Low,
    /// Medium priority (balanced estimation).
    Medium,
    /// High priority (aggressive estimation).
    High,
}

impl FromStr for GasPriority {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            "medium" => Ok(Self::Medium),
            _ => Ok(Self::Medium),
        }
    }
}

/// Identifiers for Ethereum blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockId {
    /// The most recent mined block.
    Latest,
    /// Transaction being processed in the current mempool.
    Pending,
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Latest => write!(f, "latest"),
            Self::Pending => write!(f, "pending"),
        }
    }
}

impl FromStr for BlockId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "latest" => Ok(Self::Latest),
            "pending" => Ok(Self::Pending),
            _ => Err(format!("invalid block id: {s}")),
        }
    }
}

/// Core error types for the library.
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
    InvalidReceipt(ReceiptError),
    #[error("invalid serialization: {0}")]
    InvalidSerialization(SerializationError),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReceiptError {
    #[error("missing block number")]
    MissingBlockNumber,
    #[error("missing transaction hash")]
    MissingTransactionHash,
    #[error("missing gas used")]
    MissingGasUsed,
    #[error("missing effective gas price")]
    MissingEffectiveGasPrice,
    #[error("failed to serialize receipt log entry")]
    LogSerializationFailed,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SerializationError {
    #[error("failed to convert value to JSON")]
    ConvertToValue,
    #[error("failed to serialize canonical JSON")]
    SerializeCanonical,
    #[error("floating point numbers are not supported in canonical serialization")]
    FloatingPointUnsupported,
}

/// A validated Ethereum address.
///
/// Ensures the address is a valid 20-byte hex string and provides
/// checksum support.
#[derive(Clone, Eq)]
pub struct Address {
    value: String,
    parsed: EthAddress,
}

impl Serialize for Address {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.value)
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Address::new(&s).map_err(serde::de::Error::custom)
    }
}

impl Address {
    /// Creates a new Address from a hex string.
    ///
    /// Validates the format and stores both the original and checksummed versions.
    pub fn new(value: impl AsRef<str>) -> Result<Self, CoreError> {
        let raw = value.as_ref().trim();
        let parsed =
            EthAddress::from_str(raw).map_err(|_| CoreError::InvalidAddress(raw.to_string()))?;
        Ok(Self {
            value: ethers::utils::to_checksum(&parsed, None),
            parsed,
        })
    }

    /// Alternative constructor for creating an Address from a string.
    pub fn from_string(value: &str) -> Result<Self, CoreError> {
        Self::new(value)
    }

    /// Returns the checksummed hex representation of the address.
    pub fn checksum(&self) -> String {
        self.value.clone()
    }

    /// Returns the lowercased hex representation of the address.
    pub fn lower(&self) -> String {
        self.value.to_lowercase()
    }

    /// Returns the underlying ethers-core Address type.
    pub fn as_eth_address(&self) -> EthAddress {
        self.parsed
    }
}

impl PartialEq for Address {
    fn eq(&self, other: &Self) -> bool {
        self.parsed == other.parsed
    }
}

impl Hash for Address {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parsed.hash(state);
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple(stringify!(Address))
            .field(&self.value)
            .finish()
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

/// Represents an amount of a specific token with its decimals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenAmount {
    /// Raw integer value (e.g., in wei).
    pub raw: U256,
    /// Number of decimals (e.g., ETH_DECIMALS for ETH).
    pub decimals: u8,
    /// Optional ticker symbol.
    pub symbol: Option<String>,
}

impl Serialize for TokenAmount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.raw.to_string())
    }
}

impl<'de> Deserialize<'de> for TokenAmount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = U256::deserialize(deserializer)?;
        Ok(Self::eth(raw))
    }
}

impl TokenAmount {
    /// Creates a TokenAmount for the native ETH token with a raw value (e.g., in wei).
    pub fn eth(raw: impl Into<U256>) -> Self {
        Self {
            raw: raw.into(),
            decimals: ETH_DECIMALS,
            symbol: Some(ETH_SYMBOL.to_string()),
        }
    }

    /// Creates a TokenAmount for the native ETH token from a human-readable string (e.g., "0.1").
    pub fn from_eth(amount: impl ToString) -> Result<Self, CoreError> {
        Self::from_human(amount, ETH_DECIMALS, Some(ETH_SYMBOL.to_string()))
    }

    /// Creates a TokenAmount from a human-readable string (e.g., "0.1").
    pub fn from_human(
        amount: impl ToString,
        decimals: u8,
        symbol: Option<String>,
    ) -> Result<Self, CoreError> {
        let amount_string = amount.to_string();
        let decimal = match Decimal::from_str(&amount_string) {
            Ok(decimal) => decimal,
            Err(_) => return Err(CoreError::InvalidTokenAmount(amount_string)),
        };

        if decimal.is_sign_negative() {
            return Err(CoreError::InvalidTokenAmount(
                "negative amounts are not supported".to_string(),
            ));
        }

        let scale = get_scale(decimals);
        let raw_decimal = decimal * scale;

        if raw_decimal.fract() != Decimal::ZERO {
            return Err(CoreError::InvalidTokenAmount(format!(
                "amount {amount_string} cannot be represented exactly with {decimals} decimals"
            )));
        }

        let raw_string = raw_decimal.trunc().to_string();
        let raw = match U256::from_dec_str(&raw_string) {
            Ok(raw) => raw,
            Err(_) => return Err(CoreError::InvalidTokenAmount(raw_string)),
        };

        Ok(Self {
            raw,
            decimals,
            symbol,
        })
    }

    /// Converts the amount to its human-readable decimal representation.
    pub fn human(&self) -> Decimal {
        let raw_decimal = Decimal::from_str(&self.raw.to_string()).unwrap_or(Decimal::ZERO);
        let scale = get_scale(self.decimals);
        raw_decimal / scale
    }

    /// Adds two TokenAmounts, ensuring they have the same decimal scale.
    pub fn checked_add(self, other: Self) -> Result<Self, CoreError> {
        if self.decimals != other.decimals {
            return Err(CoreError::TokenDecimalsMismatch {
                left: self.decimals,
                right: other.decimals,
            });
        }

        Ok(Self {
            raw: self.raw + other.raw,
            decimals: self.decimals,
            symbol: self.symbol.or(other.symbol),
        })
    }

    /// Multiplies the amount by a decimal factor, returning a new TokenAmount.
    pub fn checked_mul_decimal(self, factor: Decimal) -> Result<Self, CoreError> {
        if factor.is_sign_negative() {
            return Err(CoreError::InvalidTokenAmount(
                "negative factor is not supported".to_string(),
            ));
        }

        let product = self.human() * factor;
        Self::from_human(product, self.decimals, self.symbol)
    }

    /// Multiplies the amount by an integer factor.
    pub fn checked_mul_int(self, factor: u64) -> Result<Self, CoreError> {
        Ok(Self {
            raw: self.raw * U256::from(factor),
            decimals: self.decimals,
            symbol: self.symbol,
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

#[derive(Clone, Debug, Eq, Serialize, Deserialize)]
pub struct Token {
    /// Contract address of the token.
    pub address: Address,
    /// Human-readable ticker symbol.
    pub symbol: String,
    /// Number of decimal places used by the token.
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

/// A request to perform a transaction on Ethereum.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionRequest {
    /// Destination address.
    pub to: Address,
    /// Amount of ETH to transfer.
    pub value: TokenAmount,
    /// Transaction input data.
    pub data: Bytes,
    /// Optional custom nonce.
    pub nonce: Option<u64>,
    /// Gas limit.
    #[serde(rename = "gas")]
    pub gas_limit: Option<u64>,
    /// Maximum total fee per gas unit.
    #[serde(rename = "maxFeePerGas")]
    pub max_fee_per_gas: Option<U256>,
    /// Maximum priority fee per gas unit (tip).
    #[serde(rename = "maxPriorityFeePerGas")]
    pub max_priority_fee: Option<U256>,
    /// Target chain ID.
    #[serde(rename = "chainId")]
    pub chain_id: u64,
}

impl TransactionRequest {
    /// Converts the request to a BTreeMap for structured logging or serialization.
    pub fn to_dict(&self) -> BTreeMap<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map.into_iter().collect(),
            _ => BTreeMap::new(),
        }
    }

    /// Converts the request to an ethers-core TransactionRequest.
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

    /// Performs sanity checks on the transaction request.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.chain_id == 0 {
            return Err(CoreError::InvalidTransactionRequest(
                "chain_id must be non-zero".to_string(),
            ));
        }

        if let Some(gas) = self.gas_limit
            && gas < MIN_GAS_LIMIT
        {
            return Err(CoreError::InvalidTransactionRequest(format!(
                "gas_limit {gas} is too low; minimum is {MIN_GAS_LIMIT}"
            )));
        }

        if let (Some(fee), Some(priority)) = (self.max_fee_per_gas, self.max_priority_fee)
            && priority > fee
        {
            return Err(CoreError::InvalidTransactionRequest(
                "maxPriorityFeePerGas cannot exceed maxFeePerGas".to_string(),
            ));
        }

        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionReceipt {
    #[serde(alias = "transactionHash", alias = "tx_hash")]
    pub tx_hash: String,
    #[serde(alias = "blockNumber", alias = "block_number")]
    pub block_number: u64,
    #[serde(deserialize_with = "deserialize_status")]
    pub status: bool,
    #[serde(alias = "gasUsed", alias = "gas_used")]
    pub gas_used: U256,
    #[serde(alias = "effectiveGasPrice", alias = "effective_gas_price")]
    pub effective_gas_price: U256,
    pub logs: Vec<Value>,
}

fn deserialize_status<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Value::deserialize(deserializer)?;
    if let Some(b) = v.as_bool() {
        Ok(b)
    } else if let Some(n) = v.as_u64() {
        Ok(n == RECEIPT_STATUS_SUCCESS)
    } else if let Some(s) = v.as_str() {
        Ok(s == SUCCESS_HEX || s == SUCCESS_STR || s == SUCCESS_BOOL_STR)
    } else {
        Ok(false)
    }
}

fn get_scale(decimals: u8) -> Decimal {
    Decimal::from_u128(DECIMAL_BASE.saturating_pow(decimals as u32)).unwrap_or(Decimal::ONE)
}

impl TransactionReceipt {
    pub fn tx_fee(&self) -> TokenAmount {
        let raw = self.gas_used * self.effective_gas_price;
        TokenAmount::eth(raw)
    }

    pub fn from_ethers(receipt: &ethers::types::TransactionReceipt) -> Result<Self, CoreError> {
        let logs = receipt
            .logs
            .iter()
            .map(|log| {
                serde_json::to_value(log)
                    .map_err(|_| CoreError::InvalidReceipt(ReceiptError::LogSerializationFailed))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            tx_hash: format!("0x{}", hex::encode(receipt.transaction_hash.as_bytes())),
            block_number: receipt
                .block_number
                .map(|n| n.as_u64())
                .ok_or(CoreError::InvalidReceipt(ReceiptError::MissingBlockNumber))?,
            status: receipt
                .status
                .map(|status| status.as_u64() == RECEIPT_STATUS_SUCCESS)
                .unwrap_or(false),
            gas_used: receipt
                .gas_used
                .ok_or(CoreError::InvalidReceipt(ReceiptError::MissingGasUsed))?,
            effective_gas_price: receipt
                .effective_gas_price
                .ok_or(CoreError::InvalidReceipt(
                    ReceiptError::MissingEffectiveGasPrice,
                ))?,
            logs,
        })
    }

    pub fn from_web3(receipt: &Value) -> Result<Self, CoreError> {
        serde_json::from_value(receipt.clone()).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("effective_gas_price") || msg.contains("effectiveGasPrice") {
                CoreError::InvalidReceipt(ReceiptError::MissingEffectiveGasPrice)
            } else if msg.contains("gas_used") || msg.contains("gasUsed") {
                CoreError::InvalidReceipt(ReceiptError::MissingGasUsed)
            } else if msg.contains("block_number") || msg.contains("blockNumber") {
                CoreError::InvalidReceipt(ReceiptError::MissingBlockNumber)
            } else if msg.contains("tx_hash") || msg.contains("transactionHash") {
                CoreError::InvalidReceipt(ReceiptError::MissingTransactionHash)
            } else {
                CoreError::InvalidReceipt(ReceiptError::MissingGasUsed)
            }
        })
    }
}

/// Detailed gas prices and priority fees derived from a block or gas station.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GasPrice {
    /// The base fee of the block.
    pub base_fee: U256,
    /// Estimated priority fee for low priority transactions.
    pub priority_fee_low: U256,
    /// Estimated priority fee for medium priority transactions.
    pub priority_fee_medium: U256,
    /// Estimated priority fee for high priority transactions.
    pub priority_fee_high: U256,
}

impl GasPrice {
    /// Calculates the maximum fee per gas unit based on priority and a safety buffer.
    pub fn get_max_fee(&self, priority: GasPriority, buffer: f64) -> U256 {
        let priority_fee = match priority {
            GasPriority::Low => self.priority_fee_low,
            GasPriority::High => self.priority_fee_high,
            GasPriority::Medium => self.priority_fee_medium,
        };

        let normalized_buffer = if buffer.is_finite() && buffer > MIN_GAS_BUFFER_THRESHOLD {
            buffer
        } else {
            DEFAULT_GAS_BUFFER
        };
        let buffer_bps = (normalized_buffer * (BPS_SCALE as f64)).ceil() as u64;

        let ratio_base = U256::from(BPS_SCALE);
        let ratio = U256::from(buffer_bps);
        let numerator = self.base_fee.checked_mul(ratio).unwrap_or(U256::MAX);

        let buffered_base = numerator
            .checked_add(ratio_base - U256::from(1u64))
            .map(|value| value / ratio_base)
            .unwrap_or(U256::MAX);

        buffered_base.checked_add(priority_fee).unwrap_or(U256::MAX)
    }
}

impl Add for TokenAmount {
    type Output = Result<Self, CoreError>;

    fn add(self, rhs: Self) -> Self::Output {
        self.checked_add(rhs)
    }
}
