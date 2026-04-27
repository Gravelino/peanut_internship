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
use ethers::abi::{Token as AbiToken, encode as abi_encode};
use ethers::types::U256;
use serde_json::Value;
use thiserror::Error;
use tracing::{info, instrument, warn};

use crate::chain::builder::TransactionBuilder;
use crate::chain::client::ChainClient;
use crate::chain::selectors::TRANSFER_TOPIC;
use crate::chain::{BundleRelay, BundleRequest, BundleTx, FlashbotsConfig};
use crate::core::types::{
    Address, BlockId, GasPriority, TokenAmount, TransactionRequest, WEI_PER_GWEI,
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
/// `approve(address,uint256)`.
pub const ERC20_APPROVE_SELECTOR: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];
/// `allowance(address,address)`.
pub const ERC20_ALLOWANCE_SELECTOR: [u8; 4] = [0xdd, 0x62, 0xed, 0x3e];

/// Total basis points (100%).
const BPS_FULL: u64 = 10_000;

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
            slippage_bps: 50, // 0.50%
            deadline_secs: 60,
            gas_buffer_bps: 12_000, // 1.20×
            receipt_timeout_secs: 120,
            chain_id: 1,
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
    /// Whether the receipt reported `status == 1`.
    pub success: bool,
}

/// Result of broadcasting a swap transaction before receipt confirmation.
#[derive(Debug, Clone)]
pub struct SwapSubmission {
    /// Transaction hash (0x-prefixed lowercase hex) returned by
    /// `eth_sendRawTransaction`.
    pub tx_hash: String,
    /// Amount of the input token submitted to the router.
    pub amount_in: U256,
    /// Token whose `Transfer(..., recipient, amount)` logs are parsed after
    /// the receipt is mined.
    pub token_out: Address,
    /// Recipient expected to receive `token_out`.
    pub recipient: Address,
    pub private_bundle: Option<PrivateSwapBundle>,
}

#[derive(Debug, Clone)]
pub struct PrivateSwapBundle {
    pub bundle_hash: String,
    pub target_block: u64,
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
    #[error("bundle error: {0}")]
    Bundle(String),
    #[error("bundle not included before timeout (tx {tx_hash}, target block {target_block})")]
    BundleNotIncluded { tx_hash: String, target_block: u64 },
}

/// Convenience result alias.
pub type SwapperResult<T> = Result<T, SwapperError>;

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

    /// Waits for a previously-submitted swap transaction and parses the mined
    /// receipt into a [`SwapResult`].
    async fn wait_swap(&self, submission: SwapSubmission) -> SwapperResult<SwapResult>;

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
        }
    }

    /// Returns the recipient (owner) address derived from the wallet.
    pub fn owner(&self) -> Result<Address, SwapperError> {
        Address::new(self.wallet.address())
            .map_err(|e| SwapperError::TxBuild(format!("wallet address parse: {e}")))
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
        reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), self.config.max_gas_gwei)?;

        let receipt = builder
            .send_and_wait(self.config.receipt_timeout_secs)
            .await
            .map_err(|e| SwapperError::Chain(format!("approve send: {e}")))?;
        if !receipt.status {
            return Err(SwapperError::Reverted(receipt.tx_hash));
        }
        Ok(true)
    }
}

#[derive(Clone)]
pub struct FlashbotsSwapper {
    inner: UniswapV2Swapper,
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
            inner,
            relay,
            flashbots,
        }
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
        if min_out.is_zero() {
            return Err(SwapperError::InvalidMinOut("min_out is zero".into()));
        }

        self.inner
            .ensure_allowance(token_in, &self.inner.config.router, amount_in)
            .await?;

        let deadline =
            U256::from(current_unix_ts().saturating_add(self.inner.config.deadline_secs));
        let path = vec![token_in.clone(), token_out.clone()];
        let calldata = build_swap_calldata(amount_in, min_out, &path, recipient, deadline);

        let builder = TransactionBuilder::new(self.inner.client.clone(), self.inner.wallet.clone())
            .to(self.inner.config.router.clone())
            .data(calldata)
            .chain_id(self.inner.config.chain_id)
            .with_gas_estimate(Some(self.inner.config.gas_buffer_bps))
            .await
            .map_err(|e| SwapperError::TxBuild(format!("bundle swap gas: {e}")))?
            .with_gas_price(GasPriority::Medium)
            .await
            .map_err(|e| SwapperError::TxBuild(format!("bundle swap fee: {e}")))?;
        reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), self.inner.config.max_gas_gwei)?;

        let signed = builder
            .build_and_sign_with_hash()
            .await
            .map_err(|e| SwapperError::TxBuild(format!("bundle swap sign: {e}")))?;
        let current_block = self
            .inner
            .client
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
                max_timestamp: Some(
                    current_unix_ts().saturating_add(self.inner.config.deadline_secs),
                ),
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
            amount_in,
            token_out: token_out.clone(),
            recipient: recipient.clone(),
            private_bundle: Some(PrivateSwapBundle {
                bundle_hash: submitted.bundle_hash,
                target_block: submitted.target_block,
            }),
        })
    }

    #[instrument(level = "info", skip(self, submission), fields(tx = %submission.tx_hash))]
    async fn wait_swap(&self, submission: SwapSubmission) -> SwapperResult<SwapResult> {
        let Some(private_bundle) = submission.private_bundle.clone() else {
            return self.inner.wait_swap(submission).await;
        };
        let deadline = Instant::now() + Duration::from_secs(self.flashbots.inclusion_timeout_secs);
        let receipt = loop {
            if let Some(receipt) = self
                .inner
                .client
                .get_receipt(&submission.tx_hash)
                .await
                .map_err(|e| SwapperError::Chain(format!("bundle receipt: {e}")))?
            {
                break receipt;
            }
            let current_block = self
                .inner
                .client
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

        Ok(SwapResult {
            tx_hash: receipt.tx_hash,
            amount_in: submission.amount_in,
            amount_out,
            gas_used: receipt.gas_used,
            success: true,
        })
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
        reject_if_gas_cap_exceeded(builder.max_fee_per_gas(), self.config.max_gas_gwei)?;

        let tx_hash = builder
            .send()
            .await
            .map_err(|e| SwapperError::Chain(format!("swap send: {e}")))?;

        Ok(SwapSubmission {
            tx_hash,
            amount_in,
            token_out: token_out.clone(),
            recipient: recipient.clone(),
            private_bundle: None,
        })
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

        Ok(SwapResult {
            tx_hash: receipt.tx_hash,
            amount_in: submission.amount_in,
            amount_out,
            gas_used: receipt.gas_used,
            success: true,
        })
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
    use serde_json::json;

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
            },
        );
        let got = book.get("ETH/USDC").expect("present");
        assert_eq!(got.base_decimals, 18);
        assert_eq!(got.quote_decimals, 6);
        assert!(book.get("BTC/USDT").is_none());
    }
}
