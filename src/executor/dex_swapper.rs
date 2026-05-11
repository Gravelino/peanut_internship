//! Live DEX execution: build/sign/send Uniswap V2 swaps and parse the
//! resulting receipt into actual filled amounts.
//!
//! This module exists so [`LiveLegs::execute_dex`](crate::executor::engine::LiveLegs)
//! has a real on-chain implementation instead of returning
//! `ExecutorError::NotImplemented`. It is intentionally Uniswap V2-flavoured
//! (single-hop `swapExactTokensForTokens`) — V3, multi-hop routing, and
//! Flashbots bundling are left as follow-up stretch goals.
//!
//! ## Flow
//! 1. Resolve [`PairTokens`] for the signal's pair from the address book.
//! 2. Quote expected output with the caller-supplied `min_out_calculator`
//!    (typically a [`pricing::UniswapV2Pair::get_amount_out`] call).
//! 3. Derive `min_out = expected * (10_000 - slippage_bps) / 10_000`.
//! 4. Ensure ERC-20 allowance on the router; if short, send an `approve(MAX)`
//!    tx and wait for its receipt first.
//! 5. Build + sign + broadcast the swap tx via [`TransactionBuilder`]; wait
//!    for its receipt.
//! 6. Parse `Transfer(token_out)` logs in the receipt whose `to` equals the
//!    recipient — sum them to get the precise `amount_out`.
//!
//! Integration tests against Anvil are **not** included — they need a fork
//! URL + funded account and are out of scope for the unit test suite. The
//! calldata builders and log parsers are, however, fully unit-tested.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ethers::abi::{ParamType, Token as AbiToken, encode as abi_encode};
use ethers::types::U256;
use ethers::utils::id;
use serde_json::Value;
use thiserror::Error;
use tracing::{info, instrument, warn};

use crate::chain::client::ChainClient;
use crate::chain::selectors::TRANSFER_TOPIC;
use crate::chain::{BundleRelay, BundleRequest, BundleTx, FlashbotsConfig};
use crate::chain::{NonceManager, SignedTransaction, TransactionBuilder};
use crate::core::types::{
    Address, BlockId, DEFAULT_RECEIPT_POLL_INTERVAL_SECS, GasPriority, MIN_GAS_LIMIT, TokenAmount,
    TransactionRequest, WEI_PER_GWEI,
};
use crate::core::wallet::WalletManager;
use crate::observability::metrics_handle;

// ---------------------------------------------------------------------------
// Function selectors (first 4 bytes of keccak256("<signature>")).
// Duplicated from `pricing::simulator` to keep this module free of upward
// dependencies on `pricing::`.
// ---------------------------------------------------------------------------

/// `swapExactTokensForTokens(uint256,uint256,address[],address,uint256)`.
pub const SWAP_EXACT_TOKENS_FOR_TOKENS_SELECTOR: [u8; 4] = [0x38, 0xed, 0x17, 0x39];
pub const GET_AMOUNTS_OUT_SELECTOR: [u8; 4] = [0xd0, 0x6c, 0xa6, 0x1f];
pub const V3_EXACT_INPUT_SINGLE_SELECTOR: [u8; 4] = [0x41, 0x4b, 0xf3, 0x89];
pub const V3_EXACT_INPUT_SELECTOR: [u8; 4] = [0xc0, 0x4b, 0x8d, 0x59];
const QUOTER_V2_EXACT_INPUT_SIGNATURE: &str = "quoteExactInput(bytes,uint256)";
const QUOTER_V2_EXACT_INPUT_SINGLE_SIGNATURE: &str =
    "quoteExactInputSingle((address,address,uint256,uint24,uint160))";
/// `approve(address,uint256)`.
pub const ERC20_APPROVE_SELECTOR: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];
/// `allowance(address,address)`.
pub const ERC20_ALLOWANCE_SELECTOR: [u8; 4] = [0xdd, 0x62, 0xed, 0x3e];
/// `balanceOf(address)`.
pub const ERC20_BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
/// `deposit()` — WETH wrap function (payable, no args).
pub const WETH_DEPOSIT_SELECTOR: [u8; 4] = [0xd0, 0xe3, 0x0d, 0xb0];

/// Known WETH contract addresses by chain. Used to auto-wrap native ETH.
const KNOWN_WETH_ADDRESSES: &[&str] = &[
    "0x82af49447d8a07e3bd95bd0d56f35241523fbab1", // Arbitrum
    "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // Ethereum Mainnet
];

/// Total basis points (100%).
const BPS_FULL: u64 = crate::core::types::BPS_SCALE;

// ---------------------------------------------------------------------------
// Config + address book
// ---------------------------------------------------------------------------

/// Tunables for live DEX execution.
#[derive(Debug, Clone)]
pub struct DexSwapperConfig {
    /// Router contract (Uniswap V2 Router02 on mainnet).
    pub router: Address,
    /// Slippage tolerance in bps applied to the quoted output.
    pub slippage_bps: u64,
    /// Deadline offset in seconds added to the current unix time.
    pub deadline_secs: u64,
    /// Gas-estimate buffer in bps forwarded to [`TransactionBuilder`].
    pub gas_buffer_bps: u64,
    /// Receipt-wait timeout (seconds) for each tx (approve + swap).
    pub receipt_timeout_secs: u64,
    /// Chain ID (mainnet default).
    pub chain_id: u64,
    /// Optional maximum EIP-1559 maxFeePerGas cap in gwei. When configured,
    /// DEX tx build aborts before signing if current fees exceed the cap.
    pub max_gas_gwei: Option<u64>,
}

impl Default for DexSwapperConfig {
    fn default() -> Self {
        Self {
            // Uniswap V2 Router02 on Ethereum mainnet.
            router: Address::new("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D")
                .expect("hard-coded mainnet router must parse"),
            slippage_bps: crate::core::types::DEFAULT_DEX_SLIPPAGE_BPS,
            deadline_secs: crate::core::types::DEFAULT_DEX_DEADLINE_SECS,
            gas_buffer_bps: crate::core::types::DEFAULT_GAS_BUFFER_BPS,
            receipt_timeout_secs: crate::core::types::DEFAULT_RECEIPT_TIMEOUT_SECS,
            chain_id: crate::core::types::MAINNET_CHAIN_ID,
            max_gas_gwei: None,
        }
    }
}

/// Token metadata for one trading pair.
#[derive(Debug, Clone)]
pub struct PairTokens {
    /// ERC-20 address of the "base" token (the one we buy/sell).
    pub base: Address,
    /// ERC-20 decimals of the base token.
    pub base_decimals: u8,
    /// ERC-20 address of the quote token (typically a stablecoin).
    pub quote: Address,
    /// ERC-20 decimals of the quote token.
    pub quote_decimals: u8,
    pub pool_kind: DexPoolKind,
    pub v3_fee: Option<u32>,
    pub v3_path: Option<Vec<Address>>,
    pub v3_fees: Option<Vec<u32>>,
    pub v3_quoter: Option<Address>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DexPoolKind {
    V2,
    V3,
}

/// Lookup table: pair symbol (e.g. `"ETH/USDT"`) → on-chain token metadata.
#[derive(Debug, Clone, Default)]
pub struct PairAddressBook {
    entries: HashMap<String, PairTokens>,
}

impl PairAddressBook {
    /// Creates an empty address book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or overwrites the metadata for `pair`.
    pub fn insert(&mut self, pair: impl Into<String>, tokens: PairTokens) {
        self.entries.insert(pair.into(), tokens);
    }

    /// Returns the tokens for `pair`, or `None` if unknown.
    pub fn get(&self, pair: &str) -> Option<&PairTokens> {
        self.entries.get(pair)
    }
}

// ---------------------------------------------------------------------------
// Swap outcome
// ---------------------------------------------------------------------------

/// Result of a successful (mined) swap.
#[derive(Debug, Clone)]
pub struct SwapResult {
    /// Transaction hash (0x-prefixed lowercase hex).
    pub tx_hash: String,
    /// Amount of the input token consumed (exact, == requested `amount_in`).
    pub amount_in: U256,
    /// Amount of the output token received by `recipient`, parsed from the
    /// receipt's `Transfer` logs.
    pub amount_out: U256,
    /// Gas used by the swap itself (does not include any approve tx).
    pub gas_used: U256,
    pub total_gas_used: U256,
    pub total_gas_fee_wei: U256,
    /// Whether the receipt reported `status == 1`.
    pub success: bool,
}

#[derive(Debug, Clone, Default)]
pub struct OnchainFeeSummary {
    pub gas_used: U256,
    pub fee_wei: U256,
    pub tx_hashes: Vec<String>,
}

impl OnchainFeeSummary {
    fn add_receipt(&mut self, receipt: &crate::core::types::TransactionReceipt) {
        self.gas_used += receipt.gas_used;
        self.fee_wei += receipt.gas_used * receipt.effective_gas_price;
        self.tx_hashes.push(receipt.tx_hash.clone());
    }
}

/// Result of broadcasting a swap transaction before receipt confirmation.
#[derive(Debug, Clone)]
pub struct SwapSubmission {
    /// Transaction hash (0x-prefixed lowercase hex) returned by
    /// `eth_sendRawTransaction`.
    pub tx_hash: String,
    pub nonce: Option<u64>,
    /// Amount of the input token submitted to the router.
    pub amount_in: U256,
    /// Token whose `Transfer(..., recipient, amount)` logs are parsed after
    /// the receipt is mined.
    pub token_out: Address,
    /// Recipient expected to receive `token_out`.
    pub recipient: Address,
    pub private_bundle: Option<PrivateSwapBundle>,
    pub pool_kind: DexPoolKind,
    pub pre_swap_fees: OnchainFeeSummary,
}

#[derive(Debug, Clone)]
pub struct PrivateSwapBundle {
    pub bundle_hash: String,
    pub target_block: u64,
}

#[derive(Debug, Clone)]
pub enum PendingSwapCancelOutcome {
    Cancelled { cancel_tx_hash: String },
    OriginalMined { tx_hash: String, success: bool },
    Unknown(String),
}

/// Errors surfaced by [`DexSwapper`] implementations.
#[derive(Debug, Error)]
pub enum SwapperError {
    /// The pair is not present in the address book.
    #[error("unknown pair: {0}")]
    UnknownPair(String),
    /// On-chain call (approve, swap, allowance, receipt) failed.
    #[error("chain error: {0}")]
    Chain(String),
    /// Could not decode a log / call return payload.
    #[error("decode error: {0}")]
    Decode(String),
    /// The swap transaction was mined with `status == 0` (revert).
    #[error("swap reverted on-chain (tx {0})")]
    Reverted(String),
    /// The supplied quote produced a min-out less than or equal to zero.
    #[error("invalid min_out: {0}")]
    InvalidMinOut(String),
    /// Signing or building the transaction failed.
    #[error("tx build error: {0}")]
    TxBuild(String),
    #[error("quote error: {0}")]
    Quote(String),
    #[error("bundle error: {0}")]
    Bundle(String),
    #[error("bundle not included before timeout (tx {tx_hash}, target block {target_block})")]
    BundleNotIncluded { tx_hash: String, target_block: u64 },
}

/// Convenience result alias.
pub type SwapperResult<T> = Result<T, SwapperError>;

#[derive(Clone)]
struct ReservedNonce {
    manager: NonceManager,
    chain_id: u64,
    address: Address,
    nonce: u64,
}

impl ReservedNonce {
    async fn rollback(self) {
        self.manager
            .mark_failed(self.chain_id, &self.address, self.nonce)
            .await;
    }
}

async fn rollback_reserved_nonce(reserved: Option<ReservedNonce>) {
    if let Some(reserved) = reserved {
        reserved.rollback().await;
    }
}

async fn cancel_pending_public_swap(
    client: &ChainClient,
    wallet: &WalletManager,
    config: &DexSwapperConfig,
    owner: Address,
    submission: &SwapSubmission,
) -> SwapperResult<PendingSwapCancelOutcome> {
    if let Some(receipt) = client
        .get_receipt(&submission.tx_hash)
        .await
        .map_err(|e| SwapperError::Chain(format!("original receipt refetch: {e}")))?
    {
        return Ok(PendingSwapCancelOutcome::OriginalMined {
            tx_hash: receipt.tx_hash,
            success: receipt.status,
        });
    }

    let Some(nonce) = submission.nonce else {
        return Ok(PendingSwapCancelOutcome::Unknown(
            "cannot cancel pending swap without original nonce".into(),
        ));
    };

    let builder = TransactionBuilder::new(client.clone(), wallet.clone())
        .to(owner)
        .value(TokenAmount::eth(0))
        .nonce(nonce)
        .gas_limit(MIN_GAS_LIMIT)
        .chain_id(config.chain_id)
        .with_gas_price(GasPriority::High)
        .await
        .map_err(|e| SwapperError::TxBuild(format!("cancel fee: {e}")))?;
    if let Err(e) = reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), config.max_gas_gwei) {
        return Ok(PendingSwapCancelOutcome::Unknown(format!(
            "cancel gas cap rejected: {e}"
        )));
    }
    let signed = builder
        .build_and_sign_with_hash()
        .await
        .map_err(|e| SwapperError::TxBuild(format!("cancel sign: {e}")))?;
    let cancel_tx_hash = match client.send_transaction(&signed.raw).await {
        Ok(tx_hash) => tx_hash,
        Err(e) => {
            if let Some(receipt) = client
                .get_receipt(&submission.tx_hash)
                .await
                .map_err(|e| SwapperError::Chain(format!("original receipt refetch: {e}")))?
            {
                return Ok(PendingSwapCancelOutcome::OriginalMined {
                    tx_hash: receipt.tx_hash,
                    success: receipt.status,
                });
            }
            return Ok(PendingSwapCancelOutcome::Unknown(format!(
                "cancel send failed: {e}"
            )));
        }
    };
    match client
        .wait_for_receipt(
            &cancel_tx_hash,
            config.receipt_timeout_secs,
            DEFAULT_RECEIPT_POLL_INTERVAL_SECS,
        )
        .await
    {
        Ok(receipt) if receipt.status => Ok(PendingSwapCancelOutcome::Cancelled { cancel_tx_hash }),
        Ok(receipt) => Ok(PendingSwapCancelOutcome::Unknown(format!(
            "cancel tx reverted: {}",
            receipt.tx_hash
        ))),
        Err(e) => {
            if let Some(receipt) = client
                .get_receipt(&submission.tx_hash)
                .await
                .map_err(|e| SwapperError::Chain(format!("original receipt refetch: {e}")))?
            {
                return Ok(PendingSwapCancelOutcome::OriginalMined {
                    tx_hash: receipt.tx_hash,
                    success: receipt.status,
                });
            }
            Ok(PendingSwapCancelOutcome::Unknown(format!(
                "cancel receipt unknown: {e}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// A live on-chain swap backend.
///
/// Split into its own trait so [`LiveLegs`](super::engine::LiveLegs) can be
/// constructed with or without a real chain connection, and so tests can
/// inject deterministic stubs.
#[async_trait]
pub trait DexSwapper: Send + Sync + std::fmt::Debug {
    /// Broadcasts the swap transaction and returns as soon as a tx hash is
    /// known. This is the critical first phase used by the executor to persist
    /// a reconcile handle before starting any external receipt timeout.
    async fn submit_swap(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SwapSubmission>;

    async fn submit_swap_for_pair(
        &self,
        tokens: &PairTokens,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SwapSubmission> {
        let _ = tokens;
        self.submit_swap(token_in, token_out, amount_in, min_out, recipient)
            .await
    }

    async fn quote_exact_input_for_pair(
        &self,
        _tokens: &PairTokens,
        _token_in: &Address,
        _token_out: &Address,
        _amount_in: U256,
    ) -> SwapperResult<Option<U256>> {
        Ok(None)
    }

    /// Waits for a previously-submitted swap transaction and parses the mined
    /// receipt into a [`SwapResult`].
    async fn wait_swap(&self, submission: SwapSubmission) -> SwapperResult<SwapResult>;

    async fn cancel_pending_swap(
        &self,
        _submission: &SwapSubmission,
    ) -> SwapperResult<PendingSwapCancelOutcome> {
        Ok(PendingSwapCancelOutcome::Unknown(
            "pending swap cancellation is not supported by this DEX backend".into(),
        ))
    }

    /// Swaps exactly `amount_in` of `token_in` for at least `min_out` of
    /// `token_out`. `recipient` receives the output token.
    async fn swap(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SwapResult> {
        let submission = self
            .submit_swap(token_in, token_out, amount_in, min_out, recipient)
            .await?;
        self.wait_swap(submission).await
    }
}

// ---------------------------------------------------------------------------
// Calldata builders (pure functions, extensively unit-tested)
// ---------------------------------------------------------------------------

/// Encodes `swapExactTokensForTokens(amountIn, amountOutMin, path[], to, deadline)`.
pub fn build_swap_calldata(
    amount_in: U256,
    min_out: U256,
    path: &[Address],
    recipient: &Address,
    deadline: U256,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 32 * (5 + path.len()));
    out.extend_from_slice(&SWAP_EXACT_TOKENS_FOR_TOKENS_SELECTOR);
    out.extend_from_slice(&abi_encode(&[
        AbiToken::Uint(amount_in),
        AbiToken::Uint(min_out),
        AbiToken::Array(
            path.iter()
                .map(|a| AbiToken::Address(a.as_eth_address()))
                .collect(),
        ),
        AbiToken::Address(recipient.as_eth_address()),
        AbiToken::Uint(deadline),
    ]));
    out
}

pub fn build_get_amounts_out_calldata(amount_in: U256, path: &[Address]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 32 * (3 + path.len()));
    out.extend_from_slice(&GET_AMOUNTS_OUT_SELECTOR);
    out.extend_from_slice(&abi_encode(&[
        AbiToken::Uint(amount_in),
        AbiToken::Array(
            path.iter()
                .map(|a| AbiToken::Address(a.as_eth_address()))
                .collect(),
        ),
    ]));
    out
}

/// Encodes `approve(spender, amount)`.
pub fn build_v3_exact_input_single_calldata(
    token_in: &Address,
    token_out: &Address,
    fee: u32,
    recipient: &Address,
    deadline: U256,
    amount_in: U256,
    min_out: U256,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 32 * 8);
    out.extend_from_slice(&V3_EXACT_INPUT_SINGLE_SELECTOR);
    out.extend_from_slice(&abi_encode(&[
        AbiToken::Address(token_in.as_eth_address()),
        AbiToken::Address(token_out.as_eth_address()),
        AbiToken::Uint(U256::from(fee)),
        AbiToken::Address(recipient.as_eth_address()),
        AbiToken::Uint(deadline),
        AbiToken::Uint(amount_in),
        AbiToken::Uint(min_out),
        AbiToken::Uint(U256::zero()),
    ]));
    out
}

pub fn encode_v3_path(path: &[Address], fees: &[u32]) -> Result<Vec<u8>, String> {
    if path.len() < 2 {
        return Err("V3 path requires at least two token addresses".into());
    }
    if fees.len() + 1 != path.len() {
        return Err(format!(
            "V3 path fee count mismatch: {} fees for {} tokens",
            fees.len(),
            path.len()
        ));
    }
    let mut out = Vec::with_capacity(path.len() * 20 + fees.len() * 3);
    for (idx, token) in path.iter().enumerate() {
        out.extend_from_slice(token.as_eth_address().as_bytes());
        if let Some(fee) = fees.get(idx) {
            if *fee > 0x00ff_ffff {
                return Err(format!("V3 fee {fee} exceeds uint24"));
            }
            out.push((fee >> 16) as u8);
            out.push((fee >> 8) as u8);
            out.push(*fee as u8);
        }
    }
    Ok(out)
}

pub fn build_v3_exact_input_calldata(
    path: &[Address],
    fees: &[u32],
    recipient: &Address,
    deadline: U256,
    amount_in: U256,
    min_out: U256,
) -> Result<Vec<u8>, String> {
    let encoded_path = encode_v3_path(path, fees)?;
    let mut out = Vec::with_capacity(4 + 32 * 5 + encoded_path.len());
    out.extend_from_slice(&V3_EXACT_INPUT_SELECTOR);
    out.extend_from_slice(&abi_encode(&[
        AbiToken::Bytes(encoded_path),
        AbiToken::Address(recipient.as_eth_address()),
        AbiToken::Uint(deadline),
        AbiToken::Uint(amount_in),
        AbiToken::Uint(min_out),
    ]));
    Ok(out)
}

pub fn build_v3_quoter_exact_input_single_calldata(
    token_in: &Address,
    token_out: &Address,
    fee: u32,
    amount_in: U256,
) -> Vec<u8> {
    let selector = id(QUOTER_V2_EXACT_INPUT_SINGLE_SIGNATURE);
    let mut out = Vec::with_capacity(4 + 32 * 5);
    out.extend_from_slice(&selector[..4]);
    out.extend_from_slice(&abi_encode(&[AbiToken::Tuple(vec![
        AbiToken::Address(token_in.as_eth_address()),
        AbiToken::Address(token_out.as_eth_address()),
        AbiToken::Uint(amount_in),
        AbiToken::Uint(U256::from(fee)),
        AbiToken::Uint(U256::zero()),
    ])]));
    out
}

pub fn build_v3_quoter_exact_input_calldata(
    path: &[Address],
    fees: &[u32],
    amount_in: U256,
) -> Result<Vec<u8>, String> {
    let selector = id(QUOTER_V2_EXACT_INPUT_SIGNATURE);
    let encoded_path = encode_v3_path(path, fees)?;
    let mut out = Vec::with_capacity(4 + 32 * 3 + encoded_path.len());
    out.extend_from_slice(&selector[..4]);
    out.extend_from_slice(&abi_encode(&[
        AbiToken::Bytes(encoded_path),
        AbiToken::Uint(amount_in),
    ]));
    Ok(out)
}

/// Encodes `approve(spender, amount)`.
pub fn build_approve_calldata(spender: &Address, amount: U256) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 64);
    out.extend_from_slice(&ERC20_APPROVE_SELECTOR);
    out.extend_from_slice(&abi_encode(&[
        AbiToken::Address(spender.as_eth_address()),
        AbiToken::Uint(amount),
    ]));
    out
}

/// Encodes `allowance(owner, spender)`.
pub fn build_allowance_calldata(owner: &Address, spender: &Address) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 64);
    out.extend_from_slice(&ERC20_ALLOWANCE_SELECTOR);
    out.extend_from_slice(&abi_encode(&[
        AbiToken::Address(owner.as_eth_address()),
        AbiToken::Address(spender.as_eth_address()),
    ]));
    out
}

/// Computes `min_out = expected * (BPS_FULL - slippage_bps) / BPS_FULL`.
/// Clamped at zero when `slippage_bps >= BPS_FULL`.
pub fn apply_slippage(expected: U256, slippage_bps: u64) -> U256 {
    if slippage_bps >= BPS_FULL {
        return U256::zero();
    }
    let remaining = BPS_FULL - slippage_bps;
    expected
        .checked_mul(U256::from(remaining))
        .unwrap_or(U256::zero())
        / U256::from(BPS_FULL)
}

pub fn v3_swap_calldata_for_pair(
    tokens: &PairTokens,
    token_in: &Address,
    token_out: &Address,
    recipient: &Address,
    deadline: U256,
    amount_in: U256,
    min_out: U256,
) -> SwapperResult<Vec<u8>> {
    match (&tokens.v3_path, &tokens.v3_fees) {
        (Some(path), Some(fees)) => {
            let forward = path.first().map(|first| first == token_in).unwrap_or(false)
                && path.last().map(|last| last == token_out).unwrap_or(false);
            let reverse = path
                .first()
                .map(|first| first == token_out)
                .unwrap_or(false)
                && path.last().map(|last| last == token_in).unwrap_or(false);
            if !forward && !reverse {
                return Err(SwapperError::TxBuild(
                    "V3 route endpoints do not match requested swap direction".into(),
                ));
            }
            let route_path;
            let route_fees;
            let (path_ref, fees_ref) = if forward {
                (path.as_slice(), fees.as_slice())
            } else {
                route_path = path.iter().cloned().rev().collect::<Vec<_>>();
                route_fees = fees.iter().cloned().rev().collect::<Vec<_>>();
                (route_path.as_slice(), route_fees.as_slice())
            };
            build_v3_exact_input_calldata(
                path_ref, fees_ref, recipient, deadline, amount_in, min_out,
            )
            .map_err(SwapperError::TxBuild)
        }
        (None, None) => {
            let fee = tokens.v3_fee.ok_or_else(|| {
                SwapperError::TxBuild("missing V3 fee tier for pair in address book".into())
            })?;
            Ok(build_v3_exact_input_single_calldata(
                token_in, token_out, fee, recipient, deadline, amount_in, min_out,
            ))
        }
        _ => Err(SwapperError::TxBuild(
            "V3 route requires both v3_path and v3_fees".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Receipt log parsing
// ---------------------------------------------------------------------------

/// Sums all `Transfer(_, recipient, value)` events in `receipt_logs` whose
/// emitting contract equals `token`. Returns the cumulative value.
///
/// Logs are consumed as `serde_json::Value` (see
/// `core::types::TransactionReceipt::from_ethers`). Each log is expected to
/// have shape:
/// ```json
/// { "address": "0x...",
///   "topics": ["0xddf2...", "0x000...from", "0x000...to"],
///   "data": "0x...<uint256>" }
/// ```
pub fn sum_transfer_to(receipt_logs: &[Value], token: &Address, recipient: &Address) -> U256 {
    let token_lower = token.lower();
    let recipient_padded = pad_address_lower(recipient);
    let transfer_topic_lower = TRANSFER_TOPIC.to_lowercase();

    let mut total = U256::zero();
    for log in receipt_logs {
        // Filter by emitter.
        let addr = match log.get("address").and_then(Value::as_str) {
            Some(a) => a.to_lowercase(),
            None => continue,
        };
        if addr != token_lower {
            continue;
        }

        let topics = match log.get("topics").and_then(Value::as_array) {
            Some(t) => t,
            None => continue,
        };
        // Need [topic0, topic1=from, topic2=to].
        if topics.len() < 3 {
            continue;
        }
        let topic0 = topics[0]
            .as_str()
            .map(str::to_lowercase)
            .unwrap_or_default();
        if topic0 != transfer_topic_lower {
            continue;
        }
        let to_topic = topics[2]
            .as_str()
            .map(str::to_lowercase)
            .unwrap_or_default();
        if to_topic != recipient_padded {
            continue;
        }

        // Amount sits in `data` as a 32-byte big-endian uint.
        let data = log.get("data").and_then(Value::as_str).unwrap_or("0x");
        match parse_u256_hex(data) {
            Ok(v) => total = total.saturating_add(v),
            Err(e) => warn!(error = %e, data, "Transfer log: failed to parse amount"),
        }
    }
    total
}

fn pad_address_lower(addr: &Address) -> String {
    // Topic storage is 32 bytes left-padded with zeros.
    let raw = addr.lower(); // "0x" + 40 hex
    let hex = raw.trim_start_matches("0x");
    format!("0x{}{}", "0".repeat(64 - hex.len()), hex)
}

fn parse_u256_hex(s: &str) -> Result<U256, String> {
    let trimmed = s.trim_start_matches("0x");
    if trimmed.is_empty() {
        return Ok(U256::zero());
    }
    U256::from_str_radix(trimmed, 16).map_err(|e| format!("bad hex: {e}"))
}

/// Decodes a single `uint256` from `eth_call` return data (e.g. `allowance`).
pub fn decode_uint256_return(data: &[u8]) -> Result<U256, String> {
    if data.len() < 32 {
        return Ok(U256::zero());
    }
    Ok(U256::from_big_endian(&data[..32]))
}

pub fn decode_get_amounts_out_return(data: &[u8]) -> Result<U256, String> {
    let decoded = ethers::abi::decode(&[ParamType::Array(Box::new(ParamType::Uint(256)))], data)
        .map_err(|e| format!("decode getAmountsOut: {e}"))?;
    decoded
        .first()
        .and_then(|token| token.clone().into_array())
        .and_then(|amounts| amounts.last().cloned())
        .and_then(|token| token.into_uint())
        .ok_or_else(|| "getAmountsOut response missing output amount".to_string())
}

fn v3_route_for_pair(
    tokens: &PairTokens,
    token_in: &Address,
    token_out: &Address,
) -> SwapperResult<(Vec<Address>, Vec<u32>)> {
    match (&tokens.v3_path, &tokens.v3_fees) {
        (Some(path), Some(fees)) => {
            let forward = path.first().map(|first| first == token_in).unwrap_or(false)
                && path.last().map(|last| last == token_out).unwrap_or(false);
            let reverse = path
                .first()
                .map(|first| first == token_out)
                .unwrap_or(false)
                && path.last().map(|last| last == token_in).unwrap_or(false);
            if !forward && !reverse {
                return Err(SwapperError::TxBuild(
                    "V3 route endpoints do not match requested swap direction".into(),
                ));
            }
            if forward {
                Ok((path.clone(), fees.clone()))
            } else {
                Ok((
                    path.iter().cloned().rev().collect(),
                    fees.iter().cloned().rev().collect(),
                ))
            }
        }
        (None, None) => {
            let fee = tokens.v3_fee.ok_or_else(|| {
                SwapperError::TxBuild("missing V3 fee tier for pair in address book".into())
            })?;
            Ok((vec![token_in.clone(), token_out.clone()], vec![fee]))
        }
        _ => Err(SwapperError::TxBuild(
            "V3 route requires both v3_path and v3_fees".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// UniswapV2Swapper
// ---------------------------------------------------------------------------

/// Production [`DexSwapper`] that broadcasts Uniswap V2 swaps.
///
/// `Debug` is implemented manually because neither [`ChainClient`] nor
/// [`WalletManager`] derive it (wallets hold secret material; we don't want
/// it accidentally surfacing in `{:?}` output).
#[derive(Clone)]
pub struct UniswapV2Swapper {
    client: ChainClient,
    wallet: WalletManager,
    config: Arc<DexSwapperConfig>,
    nonce_manager: Option<NonceManager>,
}

impl std::fmt::Debug for UniswapV2Swapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniswapV2Swapper")
            .field("router", &self.config.router)
            .field("slippage_bps", &self.config.slippage_bps)
            .field("chain_id", &self.config.chain_id)
            .finish()
    }
}

impl UniswapV2Swapper {
    /// Creates a new swapper bound to `client`, signing with `wallet`.
    pub fn new(client: ChainClient, wallet: WalletManager, config: DexSwapperConfig) -> Self {
        Self {
            client,
            wallet,
            config: Arc::new(config),
            nonce_manager: None,
        }
    }

    pub fn with_nonce_manager(mut self, nonce_manager: NonceManager) -> Self {
        self.nonce_manager = Some(nonce_manager);
        self
    }

    /// Returns the recipient (owner) address derived from the wallet.
    pub fn owner(&self) -> Result<Address, SwapperError> {
        Address::new(self.wallet.address())
            .map_err(|e| SwapperError::TxBuild(format!("wallet address parse: {e}")))
    }

    async fn apply_reserved_nonce(
        &self,
        builder: TransactionBuilder,
    ) -> SwapperResult<(TransactionBuilder, Option<ReservedNonce>)> {
        let Some(nonce_manager) = &self.nonce_manager else {
            return Ok((builder, None));
        };
        let owner = self.owner()?;
        let nonce = nonce_manager
            .reserve_next(&self.client, self.config.chain_id, &owner)
            .await
            .map_err(|e| SwapperError::TxBuild(format!("reserve nonce: {e}")))?;
        Ok((
            builder.nonce(nonce),
            Some(ReservedNonce {
                manager: nonce_manager.clone(),
                chain_id: self.config.chain_id,
                address: owner,
                nonce,
            }),
        ))
    }

    /// Reads the current ERC-20 allowance owner→spender.
    #[instrument(level = "debug", skip(self), fields(token = %token, owner = %owner))]
    pub async fn allowance(
        &self,
        token: &Address,
        owner: &Address,
        spender: &Address,
    ) -> SwapperResult<U256> {
        let data = build_allowance_calldata(owner, spender);
        let req = TransactionRequest {
            to: token.clone(),
            value: TokenAmount::eth(0),
            data: data.into(),
            nonce: None,
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id: self.config.chain_id,
        };
        let raw = self
            .client
            .call(&req, BlockId::Latest)
            .await
            .map_err(|e| SwapperError::Chain(format!("allowance call: {e}")))?;
        decode_uint256_return(&raw).map_err(SwapperError::Decode)
    }

    /// Ensures `owner` has allowed `spender` to spend at least `needed` of
    /// `token`. If short, sends an `approve(MAX)` and waits for its receipt.
    /// Returns `true` if a new approve tx was sent.
    pub async fn ensure_allowance(
        &self,
        token: &Address,
        spender: &Address,
        needed: U256,
    ) -> SwapperResult<bool> {
        let owner = self.owner()?;
        let current = self.allowance(token, &owner, spender).await?;
        if current >= needed {
            return Ok(false);
        }

        info!(
            token = %token, spender = %spender,
            current = %current, needed = %needed,
            "DEX: approving router (allowance insufficient)"
        );

        // SAFETY: approve(MAX) avoids per-swap approve transactions but
        // grants the router unlimited spend authority. If the router address
        // were ever substituted (e.g. malicious config reload or address-book
        // poisoning), the attacker could drain all approved tokens. Consider
        // a per-swap exact-amount approve for high-security deployments.
        let calldata = build_approve_calldata(spender, U256::MAX);
        let builder = TransactionBuilder::new(self.client.clone(), self.wallet.clone())
            .to(token.clone())
            .data(calldata)
            .chain_id(self.config.chain_id)
            .with_gas_estimate(Some(self.config.gas_buffer_bps))
            .await
            .map_err(|e| SwapperError::TxBuild(format!("approve gas: {e}")))?
            .with_gas_price(GasPriority::Medium)
            .await
            .map_err(|e| SwapperError::TxBuild(format!("approve fee: {e}")))?;
        let (builder, reserved_nonce) = self.apply_reserved_nonce(builder).await?;
        if let Err(e) =
            reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), self.config.max_gas_gwei)
        {
            rollback_reserved_nonce(reserved_nonce).await;
            return Err(e);
        }

        let signed = match builder.build_and_sign().await {
            Ok(signed) => signed,
            Err(e) => {
                rollback_reserved_nonce(reserved_nonce).await;
                return Err(SwapperError::TxBuild(format!("approve sign: {e}")));
            }
        };
        let tx_hash = self
            .client
            .send_transaction(&signed)
            .await
            .map_err(|e| SwapperError::Chain(format!("approve send: {e}")))?;
        let receipt = self
            .client
            .wait_for_receipt(&tx_hash, self.config.receipt_timeout_secs, 1.0)
            .await
            .map_err(|e| SwapperError::Chain(format!("approve receipt: {e}")))?;
        if !receipt.status {
            return Err(SwapperError::Reverted(receipt.tx_hash));
        }
        Ok(true)
    }

    async fn build_signed_swap_tx(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
        context: &str,
    ) -> SwapperResult<SignedTransaction> {
        if min_out.is_zero() {
            return Err(SwapperError::InvalidMinOut("min_out is zero".into()));
        }
        self.ensure_allowance(token_in, &self.config.router, amount_in)
            .await?;
        let deadline = U256::from(current_unix_ts().saturating_add(self.config.deadline_secs));
        let path = vec![token_in.clone(), token_out.clone()];
        let calldata = build_swap_calldata(amount_in, min_out, &path, recipient, deadline);
        let builder = TransactionBuilder::new(self.client.clone(), self.wallet.clone())
            .to(self.config.router.clone())
            .data(calldata)
            .chain_id(self.config.chain_id)
            .with_gas_estimate(Some(self.config.gas_buffer_bps))
            .await
            .map_err(|e| SwapperError::TxBuild(format!("{context} swap gas: {e}")))?
            .with_gas_price(GasPriority::Medium)
            .await
            .map_err(|e| SwapperError::TxBuild(format!("{context} swap fee: {e}")))?;
        let (builder, reserved_nonce) = self.apply_reserved_nonce(builder).await?;
        if let Err(e) =
            reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), self.config.max_gas_gwei)
        {
            rollback_reserved_nonce(reserved_nonce).await;
            return Err(e);
        }
        match builder.build_and_sign_with_hash().await {
            Ok(signed) => Ok(signed),
            Err(e) => {
                rollback_reserved_nonce(reserved_nonce).await;
                Err(SwapperError::TxBuild(format!("{context} swap sign: {e}")))
            }
        }
    }
}

#[derive(Clone)]
pub struct UniswapV3Swapper {
    client: ChainClient,
    wallet: WalletManager,
    config: Arc<DexSwapperConfig>,
    nonce_manager: Option<NonceManager>,
}

struct V3SwapTxRequest<'a> {
    tokens: &'a PairTokens,
    token_in: &'a Address,
    token_out: &'a Address,
    amount_in: U256,
    min_out: U256,
    recipient: &'a Address,
}

impl std::fmt::Debug for UniswapV3Swapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniswapV3Swapper")
            .field("router", &self.config.router)
            .field("slippage_bps", &self.config.slippage_bps)
            .field("chain_id", &self.config.chain_id)
            .finish()
    }
}

impl UniswapV3Swapper {
    pub fn new(client: ChainClient, wallet: WalletManager, config: DexSwapperConfig) -> Self {
        Self {
            client,
            wallet,
            config: Arc::new(config),
            nonce_manager: None,
        }
    }

    pub fn with_nonce_manager(mut self, nonce_manager: NonceManager) -> Self {
        self.nonce_manager = Some(nonce_manager);
        self
    }

    pub fn owner(&self) -> Result<Address, SwapperError> {
        Address::new(self.wallet.address())
            .map_err(|e| SwapperError::TxBuild(format!("wallet address parse: {e}")))
    }

    async fn apply_reserved_nonce(
        &self,
        builder: TransactionBuilder,
    ) -> SwapperResult<(TransactionBuilder, Option<ReservedNonce>)> {
        let Some(nonce_manager) = &self.nonce_manager else {
            return Ok((builder, None));
        };
        let owner = self.owner()?;
        let nonce = nonce_manager
            .reserve_next(&self.client, self.config.chain_id, &owner)
            .await
            .map_err(|e| SwapperError::TxBuild(format!("reserve nonce: {e}")))?;
        Ok((
            builder.nonce(nonce),
            Some(ReservedNonce {
                manager: nonce_manager.clone(),
                chain_id: self.config.chain_id,
                address: owner,
                nonce,
            }),
        ))
    }

    pub async fn allowance(
        &self,
        token: &Address,
        owner: &Address,
        spender: &Address,
    ) -> SwapperResult<U256> {
        let data = build_allowance_calldata(owner, spender);
        let req = TransactionRequest {
            to: token.clone(),
            value: TokenAmount::eth(0),
            data: data.into(),
            nonce: None,
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id: self.config.chain_id,
        };
        let raw = self
            .client
            .call(&req, BlockId::Latest)
            .await
            .map_err(|e| SwapperError::Chain(format!("allowance call: {e}")))?;
        decode_uint256_return(&raw).map_err(SwapperError::Decode)
    }

    pub async fn ensure_allowance(
        &self,
        token: &Address,
        spender: &Address,
        needed: U256,
    ) -> SwapperResult<bool> {
        let owner = self.owner()?;
        let current = self.allowance(token, &owner, spender).await?;
        if current >= needed {
            return Ok(false);
        }
        info!(
            token = %token, spender = %spender,
            current = %current, needed = %needed,
            "DEX V3: approving router (allowance insufficient)"
        );
        let calldata = build_approve_calldata(spender, U256::MAX);
        let builder = TransactionBuilder::new(self.client.clone(), self.wallet.clone())
            .to(token.clone())
            .data(calldata)
            .chain_id(self.config.chain_id)
            .with_gas_estimate(Some(self.config.gas_buffer_bps))
            .await
            .map_err(|e| SwapperError::TxBuild(format!("approve gas: {e}")))?
            .with_gas_price(GasPriority::Medium)
            .await
            .map_err(|e| SwapperError::TxBuild(format!("approve fee: {e}")))?;
        let (builder, reserved_nonce) = self.apply_reserved_nonce(builder).await?;
        if let Err(e) =
            reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), self.config.max_gas_gwei)
        {
            rollback_reserved_nonce(reserved_nonce).await;
            return Err(e);
        }
        let signed = match builder.build_and_sign().await {
            Ok(signed) => signed,
            Err(e) => {
                rollback_reserved_nonce(reserved_nonce).await;
                return Err(SwapperError::TxBuild(format!("approve sign: {e}")));
            }
        };
        let tx_hash = self
            .client
            .send_transaction(&signed)
            .await
            .map_err(|e| SwapperError::Chain(format!("approve send: {e}")))?;
        let receipt = self
            .client
            .wait_for_receipt(&tx_hash, self.config.receipt_timeout_secs, 1.0)
            .await
            .map_err(|e| SwapperError::Chain(format!("approve receipt: {e}")))?;
        if !receipt.status {
            return Err(SwapperError::Reverted(receipt.tx_hash));
        }
        Ok(true)
    }

    /// Returns `true` if `token` is a known WETH contract.
    fn is_weth(token: &Address) -> bool {
        let lower = token.lower();
        KNOWN_WETH_ADDRESSES.iter().any(|addr| lower == *addr)
    }

    /// Fetches the ERC-20 balance of `token` held by `owner`.
    async fn erc20_balance(&self, token: &Address, owner: &Address) -> SwapperResult<U256> {
        let mut calldata = ERC20_BALANCE_OF_SELECTOR.to_vec();
        calldata.extend_from_slice(&abi_encode(&[AbiToken::Address(owner.as_eth_address())]));
        let req = TransactionRequest {
            to: token.clone(),
            value: TokenAmount::eth(0),
            data: calldata.into(),
            nonce: None,
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id: self.config.chain_id,
        };
        let raw = self
            .client
            .call(&req, BlockId::Latest)
            .await
            .map_err(|e| SwapperError::Chain(format!("balanceOf call: {e}")))?;
        decode_uint256_return(&raw).map_err(SwapperError::Decode)
    }

    /// If `token_in` is WETH and the wallet doesn't hold enough, wraps
    /// native ETH by calling `WETH.deposit{value: shortfall}()`.
    async fn ensure_weth_balance(&self, token_in: &Address, needed: U256) -> SwapperResult<bool> {
        if !Self::is_weth(token_in) {
            return Ok(false);
        }
        let owner = self.owner()?;
        let current = self.erc20_balance(token_in, &owner).await?;
        if current >= needed {
            return Ok(false);
        }
        let shortfall = needed - current;
        info!(
            weth = %token_in, current = %current, needed = %needed,
            shortfall = %shortfall,
            "DEX V3: wrapping native ETH → WETH (insufficient WETH balance)"
        );
        let calldata = WETH_DEPOSIT_SELECTOR.to_vec();
        let builder = TransactionBuilder::new(self.client.clone(), self.wallet.clone())
            .to(token_in.clone())
            .value(TokenAmount::eth(shortfall))
            .data(calldata)
            .chain_id(self.config.chain_id)
            .with_gas_estimate(Some(self.config.gas_buffer_bps))
            .await
            .map_err(|e| SwapperError::TxBuild(format!("weth deposit gas: {e}")))?
            .with_gas_price(GasPriority::Medium)
            .await
            .map_err(|e| SwapperError::TxBuild(format!("weth deposit fee: {e}")))?;
        let (builder, reserved_nonce) = self.apply_reserved_nonce(builder).await?;
        if let Err(e) =
            reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), self.config.max_gas_gwei)
        {
            rollback_reserved_nonce(reserved_nonce).await;
            return Err(e);
        }
        let signed = match builder.build_and_sign().await {
            Ok(signed) => signed,
            Err(e) => {
                rollback_reserved_nonce(reserved_nonce).await;
                return Err(SwapperError::TxBuild(format!("weth deposit sign: {e}")));
            }
        };
        let tx_hash = self
            .client
            .send_transaction(&signed)
            .await
            .map_err(|e| SwapperError::Chain(format!("weth deposit send: {e}")))?;
        let receipt = self
            .client
            .wait_for_receipt(&tx_hash, self.config.receipt_timeout_secs, 1.0)
            .await
            .map_err(|e| SwapperError::Chain(format!("weth deposit receipt: {e}")))?;
        if !receipt.status {
            return Err(SwapperError::Reverted(receipt.tx_hash));
        }
        info!(tx = %receipt.tx_hash, wrapped = %shortfall, "WETH deposit confirmed");
        Ok(true)
    }

    async fn build_signed_swap_tx_for_pair(
        &self,
        req: V3SwapTxRequest<'_>,
        context: &str,
    ) -> SwapperResult<SignedTransaction> {
        if req.min_out.is_zero() {
            return Err(SwapperError::InvalidMinOut("min_out is zero".into()));
        }
        self.ensure_weth_balance(req.token_in, req.amount_in)
            .await?;
        self.ensure_allowance(req.token_in, &self.config.router, req.amount_in)
            .await?;
        let deadline = U256::from(current_unix_ts().saturating_add(self.config.deadline_secs));
        let calldata = v3_swap_calldata_for_pair(
            req.tokens,
            req.token_in,
            req.token_out,
            req.recipient,
            deadline,
            req.amount_in,
            req.min_out,
        )?;
        let builder = TransactionBuilder::new(self.client.clone(), self.wallet.clone())
            .to(self.config.router.clone())
            .data(calldata)
            .chain_id(self.config.chain_id)
            .with_gas_estimate(Some(self.config.gas_buffer_bps))
            .await
            .map_err(|e| SwapperError::TxBuild(format!("{context} v3 swap gas: {e}")))?
            .with_gas_price(GasPriority::Medium)
            .await
            .map_err(|e| SwapperError::TxBuild(format!("{context} v3 swap fee: {e}")))?;
        let (builder, reserved_nonce) = self.apply_reserved_nonce(builder).await?;
        if let Err(e) =
            reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), self.config.max_gas_gwei)
        {
            rollback_reserved_nonce(reserved_nonce).await;
            return Err(e);
        }
        match builder.build_and_sign_with_hash().await {
            Ok(signed) => Ok(signed),
            Err(e) => {
                rollback_reserved_nonce(reserved_nonce).await;
                Err(SwapperError::TxBuild(format!(
                    "{context} v3 swap sign: {e}"
                )))
            }
        }
    }
}

#[derive(Clone)]
enum FlashbotsInner {
    V2(UniswapV2Swapper),
    V3(UniswapV3Swapper),
}

impl std::fmt::Debug for FlashbotsInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::V2(inner) => f.debug_tuple("V2").field(inner).finish(),
            Self::V3(inner) => f.debug_tuple("V3").field(inner).finish(),
        }
    }
}

impl FlashbotsInner {
    fn client(&self) -> &ChainClient {
        match self {
            Self::V2(inner) => &inner.client,
            Self::V3(inner) => &inner.client,
        }
    }

    fn deadline_secs(&self) -> u64 {
        match self {
            Self::V2(inner) => inner.config.deadline_secs,
            Self::V3(inner) => inner.config.deadline_secs,
        }
    }

    async fn build_signed_swap_tx(
        &self,
        tokens: Option<&PairTokens>,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SignedTransaction> {
        match self {
            Self::V2(inner) => {
                inner
                    .build_signed_swap_tx(
                        token_in, token_out, amount_in, min_out, recipient, "bundle",
                    )
                    .await
            }
            Self::V3(inner) => {
                let tokens = tokens.ok_or_else(|| {
                    SwapperError::TxBuild(
                        "Uniswap V3 Flashbots swap requires submit_swap_for_pair".into(),
                    )
                })?;
                inner
                    .build_signed_swap_tx_for_pair(
                        V3SwapTxRequest {
                            tokens,
                            token_in,
                            token_out,
                            amount_in,
                            min_out,
                            recipient,
                        },
                        "bundle",
                    )
                    .await
            }
        }
    }

    async fn wait_public(&self, submission: SwapSubmission) -> SwapperResult<SwapResult> {
        match self {
            Self::V2(inner) => inner.wait_swap(submission).await,
            Self::V3(inner) => inner.wait_swap(submission).await,
        }
    }

    async fn quote_exact_input_for_pair(
        &self,
        tokens: &PairTokens,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
    ) -> SwapperResult<Option<U256>> {
        match self {
            Self::V2(inner) => {
                inner
                    .quote_exact_input_for_pair(tokens, token_in, token_out, amount_in)
                    .await
            }
            Self::V3(inner) => {
                inner
                    .quote_exact_input_for_pair(tokens, token_in, token_out, amount_in)
                    .await
            }
        }
    }
}

#[derive(Clone)]
pub struct FlashbotsSwapper {
    inner: FlashbotsInner,
    relay: Arc<dyn BundleRelay>,
    flashbots: FlashbotsConfig,
}

impl std::fmt::Debug for FlashbotsSwapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlashbotsSwapper")
            .field("inner", &self.inner)
            .field("relay", &self.relay)
            .field("flashbots", &self.flashbots)
            .finish()
    }
}

impl FlashbotsSwapper {
    pub fn new(
        inner: UniswapV2Swapper,
        relay: Arc<dyn BundleRelay>,
        flashbots: FlashbotsConfig,
    ) -> Self {
        Self {
            inner: FlashbotsInner::V2(inner),
            relay,
            flashbots,
        }
    }

    pub fn new_v3(
        inner: UniswapV3Swapper,
        relay: Arc<dyn BundleRelay>,
        flashbots: FlashbotsConfig,
    ) -> Self {
        Self {
            inner: FlashbotsInner::V3(inner),
            relay,
            flashbots,
        }
    }

    async fn submit_bundle_swap(
        &self,
        tokens: Option<&PairTokens>,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SwapSubmission> {
        if min_out.is_zero() {
            return Err(SwapperError::InvalidMinOut("min_out is zero".into()));
        }

        let signed = self
            .inner
            .build_signed_swap_tx(tokens, token_in, token_out, amount_in, min_out, recipient)
            .await?;
        let current_block = self
            .inner
            .client()
            .get_block_number()
            .await
            .map_err(|e| SwapperError::Chain(format!("bundle current block: {e}")))?;
        let tx_hash = signed.tx_hash.clone();
        let txs = vec![BundleTx {
            raw_tx: signed.raw_hex,
            tx_hash: signed.tx_hash.clone(),
        }];
        let mut submitted = None;
        for target_block in bundle_target_blocks(current_block, &self.flashbots) {
            let request = BundleRequest {
                txs: txs.clone(),
                target_block,
                min_timestamp: None,
                max_timestamp: Some(current_unix_ts().saturating_add(self.inner.deadline_secs())),
            };

            let simulation_started = Instant::now();
            if let Err(e) = self.relay.simulate_bundle(&request).await {
                metrics_handle().record_flashbots_simulation(
                    &self.flashbots.relay_url,
                    "error",
                    simulation_started.elapsed().as_secs_f64(),
                );
                metrics_handle()
                    .record_flashbots_relay_error(&self.flashbots.relay_url, "simulation");
                return Err(SwapperError::Bundle(format!("simulation: {e}")));
            }
            metrics_handle().record_flashbots_simulation(
                &self.flashbots.relay_url,
                "ok",
                simulation_started.elapsed().as_secs_f64(),
            );

            let submission = match self.relay.send_bundle(&request).await {
                Ok(submission) => submission,
                Err(e) => {
                    metrics_handle()
                        .record_flashbots_relay_error(&self.flashbots.relay_url, "send");
                    return Err(SwapperError::Bundle(format!("send: {e}")));
                }
            };
            metrics_handle().record_flashbots_bundle_submitted(&self.flashbots.relay_url);

            info!(
                tx = %tx_hash,
                bundle = %submission.bundle_hash,
                target_block,
                "DEX: submitted private Flashbots bundle"
            );
            submitted = Some(submission);
        }
        let submitted =
            submitted.ok_or_else(|| SwapperError::Bundle("no target blocks configured".into()))?;

        Ok(SwapSubmission {
            tx_hash,
            nonce: signed.nonce,
            amount_in,
            token_out: token_out.clone(),
            recipient: recipient.clone(),
            private_bundle: Some(PrivateSwapBundle {
                bundle_hash: submitted.bundle_hash,
                target_block: submitted.target_block,
            }),
            pool_kind: tokens.map(|t| t.pool_kind).unwrap_or(DexPoolKind::V2),
            pre_swap_fees: OnchainFeeSummary::default(),
        })
    }
}

#[async_trait]
impl DexSwapper for FlashbotsSwapper {
    #[instrument(level = "info", skip(self), fields(
        token_in = %token_in, token_out = %token_out,
        amount_in = %amount_in, min_out = %min_out, recipient = %recipient
    ))]
    async fn submit_swap(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SwapSubmission> {
        self.submit_bundle_swap(None, token_in, token_out, amount_in, min_out, recipient)
            .await
    }

    async fn submit_swap_for_pair(
        &self,
        tokens: &PairTokens,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SwapSubmission> {
        self.submit_bundle_swap(
            Some(tokens),
            token_in,
            token_out,
            amount_in,
            min_out,
            recipient,
        )
        .await
    }

    async fn quote_exact_input_for_pair(
        &self,
        tokens: &PairTokens,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
    ) -> SwapperResult<Option<U256>> {
        self.inner
            .quote_exact_input_for_pair(tokens, token_in, token_out, amount_in)
            .await
    }

    #[instrument(level = "info", skip(self, submission), fields(tx = %submission.tx_hash))]
    async fn wait_swap(&self, submission: SwapSubmission) -> SwapperResult<SwapResult> {
        let Some(private_bundle) = submission.private_bundle.as_ref() else {
            return self.inner.wait_public(submission).await;
        };
        let deadline = Instant::now() + Duration::from_secs(self.flashbots.inclusion_timeout_secs);
        let receipt = loop {
            if let Some(receipt) = self
                .inner
                .client()
                .get_receipt(&submission.tx_hash)
                .await
                .map_err(|e| SwapperError::Chain(format!("bundle receipt: {e}")))?
            {
                break receipt;
            }
            let current_block = self
                .inner
                .client()
                .get_block_number()
                .await
                .map_err(|e| SwapperError::Chain(format!("bundle current block: {e}")))?;
            if current_block > private_bundle.target_block || Instant::now() >= deadline {
                metrics_handle().record_flashbots_bundle_not_included(&self.flashbots.relay_url);
                return Err(SwapperError::BundleNotIncluded {
                    tx_hash: submission.tx_hash.clone(),
                    target_block: private_bundle.target_block,
                });
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        };

        if receipt.tx_hash.to_lowercase() != submission.tx_hash.to_lowercase() {
            metrics_handle().record_flashbots_bundle_not_included(&self.flashbots.relay_url);
            return Err(SwapperError::BundleNotIncluded {
                tx_hash: submission.tx_hash.clone(),
                target_block: private_bundle.target_block,
            });
        }

        metrics_handle().record_flashbots_bundle_included(
            &self.flashbots.relay_url,
            receipt
                .block_number
                .saturating_sub(private_bundle.target_block),
        );

        if !receipt.status {
            return Err(SwapperError::Reverted(receipt.tx_hash));
        }

        let amount_out =
            sum_transfer_to(&receipt.logs, &submission.token_out, &submission.recipient);
        if amount_out.is_zero() {
            warn!(
                tx = %receipt.tx_hash,
                bundle = %private_bundle.bundle_hash,
                "private DEX swap mined but no matching Transfer(token_out -> recipient) log; treating as revert"
            );
            return Err(SwapperError::Reverted(receipt.tx_hash));
        }

        let mut total_fees = submission.pre_swap_fees.clone();
        total_fees.add_receipt(&receipt);
        Ok(SwapResult {
            tx_hash: receipt.tx_hash,
            amount_in: submission.amount_in,
            amount_out,
            gas_used: receipt.gas_used,
            total_gas_used: total_fees.gas_used,
            total_gas_fee_wei: total_fees.fee_wei,
            success: true,
        })
    }

    async fn cancel_pending_swap(
        &self,
        _submission: &SwapSubmission,
    ) -> SwapperResult<PendingSwapCancelOutcome> {
        Ok(PendingSwapCancelOutcome::Unknown(
            "private bundle cancellation is not supported".into(),
        ))
    }
}

#[derive(Clone)]
pub struct CompositeDexSwapper {
    v2: Arc<dyn DexSwapper>,
    v3: Arc<dyn DexSwapper>,
}

impl std::fmt::Debug for CompositeDexSwapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeDexSwapper").finish()
    }
}

impl CompositeDexSwapper {
    pub fn new(v2: Arc<dyn DexSwapper>, v3: Arc<dyn DexSwapper>) -> Self {
        Self { v2, v3 }
    }

    fn inner_for_kind(&self, kind: DexPoolKind) -> &Arc<dyn DexSwapper> {
        match kind {
            DexPoolKind::V2 => &self.v2,
            DexPoolKind::V3 => &self.v3,
        }
    }
}

#[async_trait]
impl DexSwapper for CompositeDexSwapper {
    async fn submit_swap(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SwapSubmission> {
        self.v2
            .submit_swap(token_in, token_out, amount_in, min_out, recipient)
            .await
    }

    async fn submit_swap_for_pair(
        &self,
        tokens: &PairTokens,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SwapSubmission> {
        self.inner_for_kind(tokens.pool_kind)
            .submit_swap_for_pair(tokens, token_in, token_out, amount_in, min_out, recipient)
            .await
    }

    async fn quote_exact_input_for_pair(
        &self,
        tokens: &PairTokens,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
    ) -> SwapperResult<Option<U256>> {
        self.inner_for_kind(tokens.pool_kind)
            .quote_exact_input_for_pair(tokens, token_in, token_out, amount_in)
            .await
    }

    async fn wait_swap(&self, submission: SwapSubmission) -> SwapperResult<SwapResult> {
        self.inner_for_kind(submission.pool_kind)
            .wait_swap(submission)
            .await
    }

    async fn cancel_pending_swap(
        &self,
        submission: &SwapSubmission,
    ) -> SwapperResult<PendingSwapCancelOutcome> {
        self.inner_for_kind(submission.pool_kind)
            .cancel_pending_swap(submission)
            .await
    }
}

#[async_trait]
impl DexSwapper for UniswapV3Swapper {
    async fn submit_swap(
        &self,
        _token_in: &Address,
        _token_out: &Address,
        _amount_in: U256,
        _min_out: U256,
        _recipient: &Address,
    ) -> SwapperResult<SwapSubmission> {
        Err(SwapperError::TxBuild(
            "UniswapV3Swapper requires submit_swap_for_pair so fee tier is available".into(),
        ))
    }

    #[instrument(level = "info", skip(self, tokens), fields(
        token_in = %token_in, token_out = %token_out,
        amount_in = %amount_in, min_out = %min_out, recipient = %recipient
    ))]
    async fn submit_swap_for_pair(
        &self,
        tokens: &PairTokens,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SwapSubmission> {
        if min_out.is_zero() {
            return Err(SwapperError::InvalidMinOut("min_out is zero".into()));
        }
        self.ensure_weth_balance(token_in, amount_in).await?;
        self.ensure_allowance(token_in, &self.config.router, amount_in)
            .await?;
        let deadline = U256::from(current_unix_ts().saturating_add(self.config.deadline_secs));
        let calldata = v3_swap_calldata_for_pair(
            tokens, token_in, token_out, recipient, deadline, amount_in, min_out,
        )?;
        let builder = TransactionBuilder::new(self.client.clone(), self.wallet.clone())
            .to(self.config.router.clone())
            .data(calldata)
            .chain_id(self.config.chain_id)
            .with_gas_estimate(Some(self.config.gas_buffer_bps))
            .await
            .map_err(|e| SwapperError::TxBuild(format!("v3 swap gas: {e}")))?
            .with_gas_price(GasPriority::Medium)
            .await
            .map_err(|e| SwapperError::TxBuild(format!("v3 swap fee: {e}")))?;
        let (builder, reserved_nonce) = self.apply_reserved_nonce(builder).await?;
        if let Err(e) =
            reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), self.config.max_gas_gwei)
        {
            rollback_reserved_nonce(reserved_nonce).await;
            return Err(e);
        }
        let signed = match builder.build_and_sign_with_hash().await {
            Ok(signed) => signed,
            Err(e) => {
                rollback_reserved_nonce(reserved_nonce).await;
                return Err(SwapperError::TxBuild(format!("v3 swap sign: {e}")));
            }
        };
        let tx_hash = self
            .client
            .send_transaction(&signed.raw)
            .await
            .map_err(|e| SwapperError::Chain(format!("v3 swap send: {e}")))?;
        Ok(SwapSubmission {
            tx_hash,
            nonce: signed.nonce,
            amount_in,
            token_out: token_out.clone(),
            recipient: recipient.clone(),
            private_bundle: None,
            pool_kind: DexPoolKind::V3,
            pre_swap_fees: OnchainFeeSummary::default(),
        })
    }

    async fn quote_exact_input_for_pair(
        &self,
        tokens: &PairTokens,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
    ) -> SwapperResult<Option<U256>> {
        let quoter = tokens
            .v3_quoter
            .as_ref()
            .ok_or_else(|| SwapperError::Quote("missing V3 quoter in address book".into()))?;
        let (path, fees) = v3_route_for_pair(tokens, token_in, token_out)?;
        let calldata = if path.len() == 2 && fees.len() == 1 {
            build_v3_quoter_exact_input_single_calldata(token_in, token_out, fees[0], amount_in)
        } else {
            build_v3_quoter_exact_input_calldata(&path, &fees, amount_in)
                .map_err(SwapperError::Quote)?
        };
        let call = TransactionRequest {
            to: quoter.clone(),
            value: TokenAmount::eth(0),
            data: calldata.into(),
            nonce: None,
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id: self.config.chain_id,
        };
        let raw = self
            .client
            .call(&call, BlockId::Latest)
            .await
            .map_err(|e| SwapperError::Quote(format!("V3 quoter call: {e}")))?;
        decode_uint256_return(&raw)
            .map(Some)
            .map_err(SwapperError::Quote)
    }

    #[instrument(level = "info", skip(self, submission), fields(tx = %submission.tx_hash))]
    async fn wait_swap(&self, submission: SwapSubmission) -> SwapperResult<SwapResult> {
        let receipt = self
            .client
            .wait_for_receipt(&submission.tx_hash, self.config.receipt_timeout_secs, 1.0)
            .await
            .map_err(|e| SwapperError::Chain(format!("v3 swap receipt: {e}")))?;
        if !receipt.status {
            return Err(SwapperError::Reverted(receipt.tx_hash));
        }
        let amount_out =
            sum_transfer_to(&receipt.logs, &submission.token_out, &submission.recipient);
        if amount_out.is_zero() {
            warn!(
                tx = %receipt.tx_hash,
                "DEX V3 swap mined but no matching Transfer(token_out -> recipient) log; treating as revert"
            );
            return Err(SwapperError::Reverted(receipt.tx_hash));
        }
        let mut total_fees = submission.pre_swap_fees.clone();
        total_fees.add_receipt(&receipt);
        Ok(SwapResult {
            tx_hash: receipt.tx_hash,
            amount_in: submission.amount_in,
            amount_out,
            gas_used: receipt.gas_used,
            total_gas_used: total_fees.gas_used,
            total_gas_fee_wei: total_fees.fee_wei,
            success: true,
        })
    }

    async fn cancel_pending_swap(
        &self,
        submission: &SwapSubmission,
    ) -> SwapperResult<PendingSwapCancelOutcome> {
        cancel_pending_public_swap(
            &self.client,
            &self.wallet,
            &self.config,
            self.owner()?,
            submission,
        )
        .await
    }
}

#[async_trait]
impl DexSwapper for UniswapV2Swapper {
    #[instrument(level = "info", skip(self), fields(
        token_in = %token_in, token_out = %token_out,
        amount_in = %amount_in, min_out = %min_out, recipient = %recipient
    ))]
    async fn submit_swap(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        min_out: U256,
        recipient: &Address,
    ) -> SwapperResult<SwapSubmission> {
        if min_out.is_zero() {
            return Err(SwapperError::InvalidMinOut("min_out is zero".into()));
        }

        // Step 1: allowance.
        self.ensure_allowance(token_in, &self.config.router, amount_in)
            .await?;

        // Step 2: build + broadcast swap. Return immediately once tx_hash is
        // known; receipt waiting is the second phase.
        let deadline = U256::from(current_unix_ts().saturating_add(self.config.deadline_secs));
        let path = vec![token_in.clone(), token_out.clone()];
        let calldata = build_swap_calldata(amount_in, min_out, &path, recipient, deadline);

        let builder = TransactionBuilder::new(self.client.clone(), self.wallet.clone())
            .to(self.config.router.clone())
            .data(calldata)
            .chain_id(self.config.chain_id)
            .with_gas_estimate(Some(self.config.gas_buffer_bps))
            .await
            .map_err(|e| SwapperError::TxBuild(format!("swap gas: {e}")))?
            .with_gas_price(GasPriority::Medium)
            .await
            .map_err(|e| SwapperError::TxBuild(format!("swap fee: {e}")))?;
        let (builder, reserved_nonce) = self.apply_reserved_nonce(builder).await?;
        if let Err(e) =
            reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), self.config.max_gas_gwei)
        {
            rollback_reserved_nonce(reserved_nonce).await;
            return Err(e);
        }

        let signed = match builder.build_and_sign_with_hash().await {
            Ok(signed) => signed,
            Err(e) => {
                rollback_reserved_nonce(reserved_nonce).await;
                return Err(SwapperError::TxBuild(format!("swap sign: {e}")));
            }
        };
        let tx_hash = self
            .client
            .send_transaction(&signed.raw)
            .await
            .map_err(|e| SwapperError::Chain(format!("swap send: {e}")))?;

        Ok(SwapSubmission {
            tx_hash,
            nonce: signed.nonce,
            amount_in,
            token_out: token_out.clone(),
            recipient: recipient.clone(),
            private_bundle: None,
            pool_kind: DexPoolKind::V2,
            pre_swap_fees: OnchainFeeSummary::default(),
        })
    }

    async fn quote_exact_input_for_pair(
        &self,
        _tokens: &PairTokens,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
    ) -> SwapperResult<Option<U256>> {
        let path = vec![token_in.clone(), token_out.clone()];
        let calldata = build_get_amounts_out_calldata(amount_in, &path);
        let call = TransactionRequest {
            to: self.config.router.clone(),
            value: TokenAmount::eth(0),
            data: calldata.into(),
            nonce: None,
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id: self.config.chain_id,
        };
        let raw = self
            .client
            .call(&call, BlockId::Latest)
            .await
            .map_err(|e| SwapperError::Quote(format!("V2 getAmountsOut call: {e}")))?;
        decode_get_amounts_out_return(&raw)
            .map(Some)
            .map_err(SwapperError::Quote)
    }

    #[instrument(level = "info", skip(self, submission), fields(tx = %submission.tx_hash))]
    async fn wait_swap(&self, submission: SwapSubmission) -> SwapperResult<SwapResult> {
        let receipt = self
            .client
            .wait_for_receipt(&submission.tx_hash, self.config.receipt_timeout_secs, 1.0)
            .await
            .map_err(|e| SwapperError::Chain(format!("swap receipt: {e}")))?;

        if !receipt.status {
            return Err(SwapperError::Reverted(receipt.tx_hash));
        }

        // Step 3: sum Transfer(token_out -> recipient) in the logs.
        let amount_out =
            sum_transfer_to(&receipt.logs, &submission.token_out, &submission.recipient);
        if amount_out.is_zero() {
            // Safety net: the tx succeeded but we couldn't find a credit
            // Transfer log (wrong token? recipient mismatch?). Treat as
            // Reverted for the state machine — an operator can audit the
            // tx_hash.
            warn!(
                tx = %receipt.tx_hash,
                "DEX swap mined but no matching Transfer(token_out -> recipient) log; treating as revert"
            );
            return Err(SwapperError::Reverted(receipt.tx_hash));
        }

        let mut total_fees = submission.pre_swap_fees.clone();
        total_fees.add_receipt(&receipt);
        Ok(SwapResult {
            tx_hash: receipt.tx_hash,
            amount_in: submission.amount_in,
            amount_out,
            gas_used: receipt.gas_used,
            total_gas_used: total_fees.gas_used,
            total_gas_fee_wei: total_fees.fee_wei,
            success: true,
        })
    }

    async fn cancel_pending_swap(
        &self,
        submission: &SwapSubmission,
    ) -> SwapperResult<PendingSwapCancelOutcome> {
        cancel_pending_public_swap(
            &self.client,
            &self.wallet,
            &self.config,
            self.owner()?,
            submission,
        )
        .await
    }
}

fn current_unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn bundle_target_blocks(current_block: u64, config: &FlashbotsConfig) -> Vec<u64> {
    let first = current_block.saturating_add(config.target_block_offset.max(1));
    let count = config.max_blocks_to_try.max(1);
    (0..count)
        .map(|offset| first.saturating_add(offset))
        .collect()
}

fn reject_if_gas_cap_exceeded(
    max_fee_per_gas: Option<U256>,
    max_gas_gwei: Option<u64>,
) -> SwapperResult<()> {
    let Some(cap_gwei) = max_gas_gwei else {
        return Ok(());
    };
    let Some(max_fee) = max_fee_per_gas else {
        return Ok(());
    };
    let cap_wei = U256::from(cap_gwei) * U256::from(WEI_PER_GWEI);
    if max_fee > cap_wei {
        return Err(SwapperError::TxBuild(format!(
            "gas cap exceeded: max_fee_per_gas={max_fee} wei > cap={cap_gwei} gwei"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::{BundleResult, BundleSubmission};
    use async_trait::async_trait;
    use serde_json::json;

    #[derive(Debug)]
    struct NoopRelay;

    #[async_trait]
    impl BundleRelay for NoopRelay {
        async fn simulate_bundle(&self, _request: &BundleRequest) -> BundleResult<()> {
            Ok(())
        }

        async fn send_bundle(&self, request: &BundleRequest) -> BundleResult<BundleSubmission> {
            Ok(BundleSubmission {
                bundle_hash: "0xbundle".into(),
                tx_hashes: request.txs.iter().map(|tx| tx.tx_hash.clone()).collect(),
                target_block: request.target_block,
            })
        }
    }

    fn addr(h: &str) -> Address {
        Address::new(h).unwrap()
    }

    const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const RECIPIENT: &str = "0x0000000000000000000000000000000000000ABC";

    // ---- Calldata ------------------------------------------------------

    #[test]
    fn swap_calldata_starts_with_selector() {
        let data = build_swap_calldata(
            U256::from(1_000_000u64),
            U256::from(900_000u64),
            &[addr(WETH), addr(USDC)],
            &addr(RECIPIENT),
            U256::from(9_999_999u64),
        );
        assert_eq!(&data[..4], &SWAP_EXACT_TOKENS_FOR_TOKENS_SELECTOR);
        // 4 selector + 5 head words (amountIn, amountOutMin, path offset, to,
        // deadline) + 1 length word + 2 path elements = 4 + 32 * 8.
        assert_eq!(data.len(), 4 + 32 * 8);
    }

    #[test]
    fn v3_exact_input_single_calldata_starts_with_selector() {
        let data = build_v3_exact_input_single_calldata(
            &addr(WETH),
            &addr(USDC),
            500,
            &addr(RECIPIENT),
            U256::from(9_999_999u64),
            U256::from(1_000_000u64),
            U256::from(900_000u64),
        );
        assert_eq!(&data[..4], &V3_EXACT_INPUT_SINGLE_SELECTOR);
        assert_eq!(data.len(), 4 + 32 * 8);
    }

    #[test]
    fn v3_path_encoding_interleaves_tokens_and_uint24_fees() {
        let path = vec![addr(WETH), addr(USDC), addr(RECIPIENT)];
        let encoded = encode_v3_path(&path, &[500, 3000]).unwrap();
        assert_eq!(encoded.len(), 20 + 3 + 20 + 3 + 20);
        assert_eq!(&encoded[20..23], &[0x00, 0x01, 0xf4]);
        assert_eq!(&encoded[43..46], &[0x00, 0x0b, 0xb8]);
    }

    #[test]
    fn v3_exact_input_calldata_starts_with_selector() {
        let data = build_v3_exact_input_calldata(
            &[addr(WETH), addr(USDC), addr(RECIPIENT)],
            &[500, 3000],
            &addr(RECIPIENT),
            U256::from(9_999_999u64),
            U256::from(1_000_000u64),
            U256::from(900_000u64),
        )
        .unwrap();
        assert_eq!(&data[..4], &V3_EXACT_INPUT_SELECTOR);
    }

    #[test]
    fn approve_calldata_has_spender_and_max() {
        let spender = addr("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        let data = build_approve_calldata(&spender, U256::MAX);
        assert_eq!(&data[..4], &ERC20_APPROVE_SELECTOR);
        assert_eq!(data.len(), 4 + 64);
        // Last 32 bytes must be all 0xff (U256::MAX).
        assert!(data[4 + 32..].iter().all(|b| *b == 0xff));
    }

    #[test]
    fn allowance_calldata_has_owner_and_spender() {
        let owner = addr(RECIPIENT);
        let spender = addr(USDC);
        let data = build_allowance_calldata(&owner, &spender);
        assert_eq!(&data[..4], &ERC20_ALLOWANCE_SELECTOR);
        assert_eq!(data.len(), 4 + 64);
    }

    // ---- Slippage ------------------------------------------------------

    #[test]
    fn slippage_50bps_removes_half_percent() {
        let out = apply_slippage(U256::from(1_000_000u64), 50);
        assert_eq!(out, U256::from(995_000u64));
    }

    #[test]
    fn slippage_zero_is_noop() {
        let out = apply_slippage(U256::from(12345u64), 0);
        assert_eq!(out, U256::from(12345u64));
    }

    #[test]
    fn slippage_ge_full_clamps_to_zero() {
        assert_eq!(apply_slippage(U256::from(12345u64), 10_000), U256::zero());
        assert_eq!(apply_slippage(U256::from(12345u64), 99_999), U256::zero());
    }

    #[test]
    fn gas_cap_allows_fee_at_or_below_cap() {
        let gwei = U256::from(WEI_PER_GWEI);
        assert!(reject_if_gas_cap_exceeded(Some(U256::from(50u64) * gwei), Some(50)).is_ok());
        assert!(reject_if_gas_cap_exceeded(Some(U256::from(49u64) * gwei), Some(50)).is_ok());
    }

    #[test]
    fn gas_cap_rejects_fee_above_cap() {
        let gwei = U256::from(WEI_PER_GWEI);
        let err = reject_if_gas_cap_exceeded(Some(U256::from(51u64) * gwei), Some(50))
            .expect_err("fee above cap should fail");
        assert!(err.to_string().contains("gas cap exceeded"));
    }

    #[test]
    fn gas_cap_disabled_is_noop() {
        let gwei = U256::from(WEI_PER_GWEI);
        assert!(reject_if_gas_cap_exceeded(Some(U256::from(500u64) * gwei), None).is_ok());
        assert!(reject_if_gas_cap_exceeded(None, Some(50)).is_ok());
    }

    #[test]
    fn bundle_target_blocks_respects_offset_and_retry_count() {
        let config = FlashbotsConfig {
            relay_url: "http://relay.test".into(),
            target_block_offset: 2,
            max_blocks_to_try: 3,
            simulation_timeout_secs: 5,
            inclusion_timeout_secs: 30,
        };
        assert_eq!(bundle_target_blocks(100, &config), vec![102, 103, 104]);
    }

    #[test]
    fn bundle_target_blocks_clamps_zero_values() {
        let config = FlashbotsConfig {
            relay_url: "http://relay.test".into(),
            target_block_offset: 0,
            max_blocks_to_try: 0,
            simulation_timeout_secs: 5,
            inclusion_timeout_secs: 30,
        };
        assert_eq!(bundle_target_blocks(100, &config), vec![101]);
    }

    #[tokio::test]
    async fn v3_flashbots_requires_pair_metadata() {
        unsafe {
            std::env::set_var(
                "DEX_SWAPPER_TEST_PRIVATE_KEY",
                "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
            );
        }
        let client = ChainClient::new(vec!["http://127.0.0.1:8545".into()], 1, 0).unwrap();
        let wallet = WalletManager::from_env("DEX_SWAPPER_TEST_PRIVATE_KEY").unwrap();
        let inner = UniswapV3Swapper::new(client, wallet, DexSwapperConfig::default());
        let swapper = FlashbotsSwapper::new_v3(
            inner,
            Arc::new(NoopRelay),
            FlashbotsConfig {
                relay_url: "http://relay.test".into(),
                ..FlashbotsConfig::default()
            },
        );

        let err = swapper
            .submit_swap(
                &addr(WETH),
                &addr(USDC),
                U256::from(1_000_000u64),
                U256::from(900_000u64),
                &addr(RECIPIENT),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Uniswap V3 Flashbots swap requires submit_swap_for_pair")
        );
    }

    // ---- Log parsing ---------------------------------------------------

    fn transfer_log(emitter: &str, from: &str, to: &str, amount: U256) -> Value {
        let mut buf = [0u8; 32];
        amount.to_big_endian(&mut buf);
        let data_hex = format!("0x{}", hex::encode(buf));
        json!({
            "address": emitter.to_lowercase(),
            "topics": [
                TRANSFER_TOPIC,
                format!("0x000000000000000000000000{}", from.trim_start_matches("0x").to_lowercase()),
                format!("0x000000000000000000000000{}", to.trim_start_matches("0x").to_lowercase()),
            ],
            "data": data_hex,
        })
    }

    #[test]
    fn sums_matching_transfer_log() {
        let token = addr(USDC);
        let recipient = addr(RECIPIENT);
        let logs = vec![transfer_log(
            USDC,
            WETH,
            RECIPIENT,
            U256::from(1_500_000u64),
        )];
        assert_eq!(
            sum_transfer_to(&logs, &token, &recipient),
            U256::from(1_500_000u64)
        );
    }

    #[test]
    fn ignores_different_emitter() {
        let token = addr(USDC);
        let recipient = addr(RECIPIENT);
        // Transfer emitted by WETH contract — must NOT count against USDC.
        let logs = vec![transfer_log(WETH, USDC, RECIPIENT, U256::from(100u64))];
        assert_eq!(sum_transfer_to(&logs, &token, &recipient), U256::zero());
    }

    #[test]
    fn ignores_different_recipient() {
        let token = addr(USDC);
        let recipient = addr(RECIPIENT);
        let other = "0x000000000000000000000000000000000000dead";
        let logs = vec![transfer_log(USDC, WETH, other, U256::from(100u64))];
        assert_eq!(sum_transfer_to(&logs, &token, &recipient), U256::zero());
    }

    #[test]
    fn sums_multiple_matching_transfers() {
        let token = addr(USDC);
        let recipient = addr(RECIPIENT);
        let logs = vec![
            transfer_log(USDC, WETH, RECIPIENT, U256::from(100u64)),
            transfer_log(USDC, WETH, RECIPIENT, U256::from(200u64)),
            transfer_log(WETH, USDC, RECIPIENT, U256::from(999u64)), // wrong token
        ];
        assert_eq!(
            sum_transfer_to(&logs, &token, &recipient),
            U256::from(300u64)
        );
    }

    #[test]
    fn decodes_uint256_return_padding() {
        let mut data = [0u8; 32];
        data[31] = 0x42;
        assert_eq!(decode_uint256_return(&data).unwrap(), U256::from(0x42u64));
    }

    #[test]
    fn decodes_uint256_return_short_returns_zero() {
        assert_eq!(decode_uint256_return(&[]).unwrap(), U256::zero());
    }

    // ---- Address book --------------------------------------------------

    #[test]
    fn address_book_lookup_roundtrip() {
        let mut book = PairAddressBook::new();
        book.insert(
            "ETH/USDC",
            PairTokens {
                base: addr(WETH),
                base_decimals: 18,
                quote: addr(USDC),
                quote_decimals: 6,
                pool_kind: DexPoolKind::V2,
                v3_fee: None,
                v3_path: None,
                v3_fees: None,
                v3_quoter: None,
            },
        );
        let got = book.get("ETH/USDC").expect("present");
        assert_eq!(got.base_decimals, 18);
        assert_eq!(got.quote_decimals, 6);
        assert!(book.get("BTC/USDT").is_none());
    }
}
