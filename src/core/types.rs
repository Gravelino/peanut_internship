use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Add;
use std::str::FromStr;

use ethers::types::{
    Address as EthAddress, Bytes, Eip1559TransactionRequest,
    TransactionRequest as EthTransactionRequest, U256,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use serde::de;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Minimum gas limit for a simple ETH transfer.
pub const MIN_GAS_LIMIT: u64 = 21_000;

/// Default buffer for gas estimation in basis points (1.2× = 12_000 bps).
pub const DEFAULT_GAS_BUFFER_BPS: u64 = 12_000;

/// Basis points scale (10,000 = 100%).
pub const BPS_SCALE: u64 = 10_000;

/// Minimum meaningful gas buffer in basis points (1.0× = 10_000 bps).
pub const MIN_GAS_BUFFER_BPS: u64 = 10_000;

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

/// Default HTTP client timeout in seconds for exchange API calls.
pub const HTTP_TIMEOUT_SECS: u64 = 10;

/// Binance API recvWindow parameter in milliseconds.
/// See: https://binance-docs.github.io/apidocs/spot/en/#recvwindow
pub const BINANCE_RECV_WINDOW_MS: u64 = 60_000;

/// Binance API error code: Too many requests (rate limit exceeded).
pub const BINANCE_ERR_RATE_LIMIT: i64 = -1015;
/// Binance API error code: Insufficient account balance.
pub const BINANCE_ERR_INSUFFICIENT_FUNDS: i64 = -2010;
/// Binance API error code: Invalid symbol.
pub const BINANCE_ERR_INVALID_SYMBOL: i64 = -1121;
/// Binance API error code: Invalid quantity / filter failure.
pub const BINANCE_ERR_INVALID_QUANTITY: i64 = -1013;

/// Binance endpoint weight: ticker/price (1 weight per minute).
pub const BINANCE_WEIGHT_TICKER_PRICE: u32 = 1;
/// Binance endpoint weight: server time / connectivity check (1 weight).
pub const BINANCE_WEIGHT_SERVER_TIME: u32 = 1;
/// Binance endpoint weight: exchange info (1 weight).
pub const BINANCE_WEIGHT_EXCHANGE_INFO: u32 = 1;
/// Binance endpoint weight: order book depth limit ≤ 100 (1 weight).
pub const BINANCE_WEIGHT_DEPTH_100: u32 = 1;
/// Binance endpoint weight: order book depth limit ≤ 500 (5 weight).
pub const BINANCE_WEIGHT_DEPTH_500: u32 = 5;
/// Binance endpoint weight: order book depth limit ≤ 1000 (10 weight).
pub const BINANCE_WEIGHT_DEPTH_1000: u32 = 10;
/// Binance endpoint weight: order book depth limit ≤ 5000 (50 weight).
pub const BINANCE_WEIGHT_DEPTH_5000: u32 = 50;
/// Binance endpoint weight: account information (10 weight).
pub const BINANCE_WEIGHT_ACCOUNT: u32 = 10;
/// Binance endpoint weight: place/cancel order (1 weight, also counts toward order limit).
pub const BINANCE_WEIGHT_ORDER: u32 = 1;
/// Binance endpoint weight: query order status (2 weight).
pub const BINANCE_WEIGHT_ORDER_STATUS: u32 = 2;
/// Binance endpoint weight: my trades (5 weight).
pub const BINANCE_WEIGHT_MY_TRADES: u32 = 5;
/// Binance endpoint weight: trading fee (1 weight).
pub const BINANCE_WEIGHT_TRADE_FEE: u32 = 1;

/// Default inventory deviation threshold (in percent) that triggers a rebalance recommendation.
pub const REBALANCE_DEVIATION_THRESHOLD_PCT: f64 = 30.0;

/// Minimum amount in wei for cross-DEX arb detection (1 ETH).
pub const MIN_CROSS_DEX_AMOUNT_WEI: u128 = 1_000_000_000_000_000_000;

/// Minimum total hops for a triangular arb to be considered non-trivial.
pub const MIN_TRIANGULAR_ARB_HOPS: usize = 3;

/// Default retry-after duration in seconds when the server does not provide one.
pub const DEFAULT_RETRY_AFTER_SECS: u64 = 10;

/// Estimated bid/ask spread used to derive best_bid/best_ask from mid_price (1 bps = 0.01%).
pub const ESTIMATED_SPREAD_BPS: &str = "0.0001";

/// Default RPC client timeout in seconds for on-chain queries.
pub const RPC_TIMEOUT_SECS: u64 = 30;

/// Default number of RPC retry attempts for transient failures.
pub const RPC_RETRIES: usize = 2;

/// Default estimated transfer time in minutes when fee info is unavailable.
pub const DEFAULT_TRANSFER_TIME_MIN: u32 = 15;

/// Ethereum standard block confirmations for finality (12 blocks ≈ 3 minutes).
pub const ETH_CONFIRMATIONS: u32 = 12;

/// Precision multiplier for V3 spot price calculation (10^18).
/// Used to avoid precision loss when converting sqrtPriceX96 to a Decimal ratio.
pub const V3_PRICE_PRECISION: u128 = 1_000_000_000_000_000_000;

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
            other => Err(format!(
                "invalid gas priority: {other}; expected low, medium, or high"
            )),
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

    /// Creates an Address from an ethers H160 value.
    pub fn from_eth_address(eth: EthAddress) -> Self {
        Self {
            value: ethers::utils::to_checksum(&eth, None),
            parsed: eth,
        }
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
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("TokenAmount", 3)?;
        s.serialize_field("raw", &self.raw.to_string())?;
        s.serialize_field("decimals", &self.decimals)?;
        s.serialize_field("symbol", &self.symbol)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for TokenAmount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct TokenAmountFields {
            raw: String,
            decimals: u8,
            symbol: Option<String>,
        }

        let fields = TokenAmountFields::deserialize(deserializer)?;
        let raw = U256::from_dec_str(&fields.raw)
            .map_err(|e| de::Error::custom(format!("invalid raw value: {e}")))?;
        Ok(Self {
            raw,
            decimals: fields.decimals,
            symbol: fields.symbol,
        })
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
    ///
    /// Returns `None` if the raw value overflows `Decimal` precision.
    pub fn human(&self) -> Option<Decimal> {
        let raw_decimal = Decimal::from_str(&self.raw.to_string()).ok()?;
        let scale = get_scale(self.decimals);
        Some(raw_decimal / scale)
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

        let human = self.human().ok_or_else(|| {
            CoreError::InvalidTokenAmount("raw value overflows decimal precision".to_string())
        })?;
        let product = human * factor;
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
        let human = self
            .human()
            .map_or_else(|| self.raw.to_string(), |d| d.to_string());
        if let Some(symbol) = &self.symbol {
            write!(f, "{human}{symbol}")
        } else {
            write!(f, "{human}")
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
    /// Creates a read-only contract call request (eth_call) with zero value and no gas/nonce.
    pub fn contract_call(to: Address, data: Vec<u8>, chain_id: u64) -> Self {
        Self {
            to,
            value: TokenAmount::eth(0u64),
            data: Bytes::from(data),
            nonce: None,
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id,
        }
    }

    /// Converts the request to a BTreeMap for structured logging or serialization.
    pub fn to_dict(&self) -> BTreeMap<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map.into_iter().collect(),
            _ => BTreeMap::new(),
        }
    }

    /// Converts the request into a `TypedTransaction` suitable for signing or sending.
    ///
    /// When EIP-1559 fee fields are present, builds an `Eip1559TransactionRequest`.
    /// Otherwise falls back to a legacy `TransactionRequest`.
    pub fn to_ethers_typed(&self) -> ethers::types::transaction::eip2718::TypedTransaction {
        if self.max_fee_per_gas.is_some() || self.max_priority_fee.is_some() {
            let mut req = Eip1559TransactionRequest::new();
            req.to = Some(self.to.as_eth_address().into());
            req.value = Some(self.value.raw);
            req.data = Some(self.data.clone());
            req.chain_id = Some(self.chain_id.into());
            req.max_fee_per_gas = self.max_fee_per_gas;
            req.max_priority_fee_per_gas = self.max_priority_fee;
            if let Some(nonce) = self.nonce {
                req.nonce = Some(nonce.into());
            }
            if let Some(gas_limit) = self.gas_limit {
                req.gas = Some(gas_limit.into());
            }
            req.into()
        } else {
            self.to_ethers_request().into()
        }
    }

    /// Converts the request to a legacy ethers-core TransactionRequest.
    pub fn to_ethers_request(&self) -> EthTransactionRequest {
        let mut request = EthTransactionRequest::new();
        request.to = Some(self.to.as_eth_address().into());
        request.value = Some(self.value.raw);
        request.data = Some(self.data.clone());
        request.chain_id = Some(self.chain_id.into());

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
        Err(de::Error::custom(format!(
            "cannot deserialize receipt status from {v}"
        )))
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
    /// Calculates the maximum fee per gas unit based on priority and a safety buffer (in bps).
    ///
    /// `buffer_bps` is expressed in basis points: 10_000 = 1.0×, 12_000 = 1.2×, etc.
    /// Falls back to [`DEFAULT_GAS_BUFFER_BPS`] when the value is below [`MIN_GAS_BUFFER_BPS`].
    pub fn get_max_fee(&self, priority: GasPriority, buffer_bps: u64) -> U256 {
        let priority_fee = match priority {
            GasPriority::Low => self.priority_fee_low,
            GasPriority::High => self.priority_fee_high,
            GasPriority::Medium => self.priority_fee_medium,
        };

        let effective_buffer = if buffer_bps >= MIN_GAS_BUFFER_BPS {
            buffer_bps
        } else {
            DEFAULT_GAS_BUFFER_BPS
        };

        let ratio = U256::from(effective_buffer);
        let bps_base = U256::from(BPS_SCALE);

        let numerator = self.base_fee.checked_mul(ratio).unwrap_or(U256::MAX);
        let buffered_base = numerator
            .checked_add(bps_base - U256::from(1u64))
            .map(|value| value / bps_base)
            .unwrap_or(U256::MAX);

        buffered_base.checked_add(priority_fee).unwrap_or(U256::MAX)
    }
}

impl Add for TokenAmount {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        self.checked_add(rhs)
            .expect("token amount decimals mismatch in add")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR_1: &str = "0x0000000000000000000000000000000000000001";
    const ADDR_CHECKSUM: &str = "0x52908400098527886E0F7030069857D2E4169EE7";

    // ── Address ────────────────────────────────────────────────────────────

    #[test]
    fn address_lower_returns_lowercase_hex() {
        let addr = Address::new(ADDR_CHECKSUM).unwrap();
        let lower = addr.lower();
        assert_eq!(lower, lower.to_lowercase());
        assert!(lower.starts_with("0x"));
    }

    #[test]
    fn address_from_eth_address_round_trips() {
        let original = Address::new(ADDR_1).unwrap();
        let eth_addr = original.as_eth_address();
        let reconstructed = Address::from_eth_address(eth_addr);
        assert_eq!(original, reconstructed);
    }

    #[test]
    fn address_try_from_str_valid() {
        let addr: Result<Address, _> = ADDR_CHECKSUM.try_into();
        assert!(addr.is_ok());
    }

    #[test]
    fn address_try_from_str_invalid() {
        let addr: Result<Address, _> = "not_an_address".try_into();
        assert!(addr.is_err());
    }

    #[test]
    fn address_checksum_and_lower_are_consistent() {
        let addr = Address::new(ADDR_CHECKSUM).unwrap();
        assert_eq!(
            addr.checksum().to_lowercase(),
            addr.lower().to_lowercase()
        );
    }

    #[test]
    fn address_display_matches_checksum() {
        let addr = Address::new(ADDR_CHECKSUM).unwrap();
        assert_eq!(format!("{addr}"), addr.checksum());
    }

    // ── TokenAmount ────────────────────────────────────────────────────────

    #[test]
    fn token_amount_eth_constructor_sets_decimals_and_symbol() {
        let amount = TokenAmount::eth(1_000u64);
        assert_eq!(amount.decimals, ETH_DECIMALS);
        assert_eq!(amount.symbol.as_deref(), Some(ETH_SYMBOL));
        assert_eq!(amount.raw, U256::from(1_000u64));
    }

    #[test]
    fn token_amount_from_eth_zero_is_valid() {
        let amount = TokenAmount::from_eth("0").unwrap();
        assert_eq!(amount.raw, U256::zero());
        assert_eq!(amount.decimals, ETH_DECIMALS);
    }

    #[test]
    fn token_amount_from_human_rejects_negative() {
        assert!(TokenAmount::from_human("-1", 18, None).is_err());
    }

    #[test]
    fn token_amount_from_human_rejects_too_many_decimals() {
        // 18 decimals → "0.0000000000000000001" has 19 decimal places
        assert!(TokenAmount::from_human("0.0000000000000000001", 18, None).is_err());
    }

    #[test]
    fn token_amount_from_human_rejects_non_numeric() {
        assert!(TokenAmount::from_human("abc", 18, None).is_err());
    }

    #[test]
    fn token_amount_checked_mul_decimal_scales_correctly() {
        let one_eth = TokenAmount::from_eth("1").unwrap();
        let half = one_eth.checked_mul_decimal(Decimal::new(5, 1)).unwrap();
        assert_eq!(half.human(), Some(Decimal::new(5, 1)));
    }

    #[test]
    fn token_amount_checked_mul_decimal_rejects_negative_factor() {
        let amount = TokenAmount::from_eth("1").unwrap();
        assert!(amount.checked_mul_decimal(Decimal::from(-1)).is_err());
    }

    #[test]
    fn token_amount_checked_mul_int_scales_raw() {
        let one_eth = TokenAmount::from_eth("2").unwrap();
        let three_eth = one_eth.checked_mul_int(3).unwrap();
        assert_eq!(three_eth.human(), Some(Decimal::from(6)));
    }

    #[test]
    fn token_amount_add_operator_works_same_decimals() {
        let a = TokenAmount::from_eth("1").unwrap();
        let b = TokenAmount::from_eth("2").unwrap();
        let sum = a + b;
        assert_eq!(sum.human(), Some(Decimal::from(3)));
    }

    #[test]
    fn token_amount_display_includes_symbol() {
        let amount = TokenAmount::from_eth("1.5").unwrap();
        let display = format!("{amount}");
        assert!(display.contains("ETH"));
        assert!(display.contains("1.5"));
    }

    #[test]
    fn token_amount_display_without_symbol_shows_value() {
        let amount = TokenAmount::from_human("2.5", 6, None).unwrap();
        let display = format!("{amount}");
        assert_eq!(display, "2.5");
    }

    // ── TransactionStatus ──────────────────────────────────────────────────

    #[test]
    fn transaction_status_display_variants() {
        assert_eq!(format!("{}", TransactionStatus::Success), "SUCCESS");
        assert_eq!(format!("{}", TransactionStatus::Failed), "FAILED");
        assert_eq!(format!("{}", TransactionStatus::Pending), "PENDING");
    }

    // ── GasPriority ────────────────────────────────────────────────────────

    #[test]
    fn gas_priority_from_str_valid_all_cases() {
        assert_eq!("low".parse::<GasPriority>().unwrap(), GasPriority::Low);
        assert_eq!("medium".parse::<GasPriority>().unwrap(), GasPriority::Medium);
        assert_eq!("high".parse::<GasPriority>().unwrap(), GasPriority::High);
        // Case-insensitive
        assert_eq!("HIGH".parse::<GasPriority>().unwrap(), GasPriority::High);
        assert_eq!("Low".parse::<GasPriority>().unwrap(), GasPriority::Low);
    }

    #[test]
    fn gas_priority_from_str_invalid_returns_error() {
        assert!("ultra".parse::<GasPriority>().is_err());
        assert!("".parse::<GasPriority>().is_err());
    }

    // ── BlockId ────────────────────────────────────────────────────────────

    #[test]
    fn block_id_from_str_and_display_round_trips() {
        assert_eq!("latest".parse::<BlockId>().unwrap(), BlockId::Latest);
        assert_eq!("pending".parse::<BlockId>().unwrap(), BlockId::Pending);
        assert_eq!(format!("{}", BlockId::Latest), "latest");
        assert_eq!(format!("{}", BlockId::Pending), "pending");
    }

    #[test]
    fn block_id_from_str_invalid_returns_error() {
        assert!("finalized".parse::<BlockId>().is_err());
    }

    // ── TransactionRequest ─────────────────────────────────────────────────

    #[test]
    fn transaction_request_contract_call_has_no_value_or_gas() {
        let addr = Address::new(ADDR_1).unwrap();
        let data = vec![0xde, 0xad, 0xbe, 0xef];
        let req = TransactionRequest::contract_call(addr.clone(), data.clone(), MAINNET_CHAIN_ID);
        assert_eq!(req.to, addr);
        assert_eq!(req.value, TokenAmount::eth(0u64));
        assert_eq!(req.nonce, None);
        assert_eq!(req.gas_limit, None);
        assert_eq!(req.chain_id, MAINNET_CHAIN_ID);
        assert_eq!(req.data, Bytes::from(data));
    }

    #[test]
    fn transaction_request_validate_rejects_zero_chain_id() {
        let addr = Address::new(ADDR_1).unwrap();
        let req = TransactionRequest {
            to: addr,
            value: TokenAmount::eth(0u64),
            data: Bytes::new(),
            nonce: None,
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id: 0,
        };
        assert!(req.validate().is_err());
    }

    #[test]
    fn transaction_request_validate_rejects_low_gas_limit() {
        let addr = Address::new(ADDR_1).unwrap();
        let req = TransactionRequest {
            to: addr,
            value: TokenAmount::eth(0u64),
            data: Bytes::new(),
            nonce: None,
            gas_limit: Some(1_000), // below MIN_GAS_LIMIT
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id: MAINNET_CHAIN_ID,
        };
        assert!(req.validate().is_err());
    }

    #[test]
    fn transaction_request_validate_rejects_priority_exceeding_max_fee() {
        let addr = Address::new(ADDR_1).unwrap();
        let req = TransactionRequest {
            to: addr,
            value: TokenAmount::eth(0u64),
            data: Bytes::new(),
            nonce: None,
            gas_limit: Some(MIN_GAS_LIMIT),
            max_fee_per_gas: Some(U256::from(1_000_000_000u64)),
            max_priority_fee: Some(U256::from(2_000_000_000u64)),
            chain_id: MAINNET_CHAIN_ID,
        };
        assert!(req.validate().is_err());
    }

    #[test]
    fn transaction_request_validate_passes_for_valid_request() {
        let addr = Address::new(ADDR_1).unwrap();
        let req = TransactionRequest {
            to: addr,
            value: TokenAmount::eth(0u64),
            data: Bytes::new(),
            nonce: Some(1),
            gas_limit: Some(MIN_GAS_LIMIT),
            max_fee_per_gas: Some(U256::from(2_000_000_000u64)),
            max_priority_fee: Some(U256::from(1_000_000_000u64)),
            chain_id: MAINNET_CHAIN_ID,
        };
        assert!(req.validate().is_ok());
    }

    #[test]
    fn transaction_request_to_dict_contains_chain_id() {
        let addr = Address::new(ADDR_1).unwrap();
        let req = TransactionRequest::contract_call(addr, vec![], MAINNET_CHAIN_ID);
        let dict = req.to_dict();
        assert!(dict.contains_key("chainId"));
        assert!(!dict.contains_key("chain_id"));
    }

    // ── TransactionReceipt ─────────────────────────────────────────────────

    #[test]
    fn transaction_receipt_tx_fee_multiplies_gas_used_and_price() {
        let receipt = TransactionReceipt {
            tx_hash: "0xabc".into(),
            block_number: 100,
            status: true,
            gas_used: U256::from(21_000u64),
            effective_gas_price: U256::from(1_000_000_000u64), // 1 Gwei
            logs: vec![],
        };
        let fee = receipt.tx_fee();
        // 21000 * 1e9 = 21000 Gwei in wei
        assert_eq!(fee.raw, U256::from(21_000u64) * U256::from(1_000_000_000u64));
        assert_eq!(fee.decimals, ETH_DECIMALS);
    }

    #[test]
    fn transaction_receipt_from_web3_parses_valid_json() {
        let json = serde_json::json!({
            "transactionHash": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "blockNumber": 12345678,
            "status": "0x1",
            "gasUsed": "0x5208",  // 21000 in hex
            "effectiveGasPrice": "0x3b9aca00",  // 1 Gwei in hex
            "logs": []
        });
        let receipt = TransactionReceipt::from_web3(&json).unwrap();
        assert!(receipt.status);
        assert_eq!(receipt.block_number, 12_345_678);
        assert_eq!(receipt.gas_used, U256::from(21_000u64));
    }

    #[test]
    fn transaction_receipt_deserialize_status_from_bool() {
        let json = serde_json::json!({
            "transactionHash": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "blockNumber": 1,
            "status": false,
            "gasUsed": "21000",
            "effectiveGasPrice": "1000000000",
            "logs": []
        });
        let receipt = TransactionReceipt::from_web3(&json).unwrap();
        assert!(!receipt.status);
    }

    #[test]
    fn transaction_receipt_deserialize_status_from_integer() {
        let json = serde_json::json!({
            "transactionHash": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "blockNumber": 1,
            "status": 1,
            "gasUsed": "21000",
            "effectiveGasPrice": "1000000000",
            "logs": []
        });
        let receipt = TransactionReceipt::from_web3(&json).unwrap();
        assert!(receipt.status);
    }

    // ── GasPrice ───────────────────────────────────────────────────────────

    fn make_gas_price() -> GasPrice {
        GasPrice {
            base_fee: U256::from(10_000_000_000u64), // 10 Gwei
            priority_fee_low: U256::from(1_000_000_000u64),
            priority_fee_medium: U256::from(2_000_000_000u64),
            priority_fee_high: U256::from(3_000_000_000u64),
        }
    }

    #[test]
    fn gas_price_get_max_fee_low_priority_uses_low_tip() {
        let gas = make_gas_price();
        let max_fee = gas.get_max_fee(GasPriority::Low, DEFAULT_GAS_BUFFER_BPS);
        // buffered base = ceil(10_000_000_000 * 12000 / 10000) + 1_000_000_000 priority_low
        assert!(max_fee > gas.priority_fee_low);
    }

    #[test]
    fn gas_price_get_max_fee_high_greater_than_low() {
        let gas = make_gas_price();
        let low = gas.get_max_fee(GasPriority::Low, DEFAULT_GAS_BUFFER_BPS);
        let high = gas.get_max_fee(GasPriority::High, DEFAULT_GAS_BUFFER_BPS);
        assert!(high > low);
    }

    #[test]
    fn gas_price_get_max_fee_below_min_buffer_falls_back_to_default() {
        let gas = make_gas_price();
        // buffer_bps=0 is below MIN_GAS_BUFFER_BPS, should use DEFAULT_GAS_BUFFER_BPS
        let fee_zero_buffer = gas.get_max_fee(GasPriority::Medium, 0);
        let fee_default = gas.get_max_fee(GasPriority::Medium, DEFAULT_GAS_BUFFER_BPS);
        assert_eq!(fee_zero_buffer, fee_default);
    }

    #[test]
    fn gas_price_get_max_fee_medium_is_between_low_and_high() {
        let gas = make_gas_price();
        let low = gas.get_max_fee(GasPriority::Low, DEFAULT_GAS_BUFFER_BPS);
        let mid = gas.get_max_fee(GasPriority::Medium, DEFAULT_GAS_BUFFER_BPS);
        let high = gas.get_max_fee(GasPriority::High, DEFAULT_GAS_BUFFER_BPS);
        assert!(low <= mid);
        assert!(mid <= high);
    }
}
