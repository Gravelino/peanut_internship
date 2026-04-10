use std::sync::Arc;

use ethers::prelude::*;
use ethers::providers::{Provider, Ws};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, warn};

use crate::core::types::Address;
use crate::pricing::errors::{PricingError, PricingResult};

/// Parsed swap transaction from mempool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedSwap {
    /// Pending transaction hash.
    pub tx_hash: String,
    /// Router contract receiving the call.
    pub router: Address,
    /// Human-readable DEX family name.
    pub dex: String,
    /// Decoded router method name.
    pub method: String,
    /// First token in swap path when known.
    pub token_in: Option<Address>,
    /// Last token in swap path when known.
    pub token_out: Option<Address>,
    /// Input amount in raw token units.
    pub amount_in: U256,
    /// Minimum accepted output amount from calldata.
    pub min_amount_out: U256,
    /// Swap deadline from calldata.
    pub deadline: U256,
    /// Transaction sender.
    pub sender: Address,
    /// Legacy gas price if present, otherwise zero.
    pub gas_price: U256,
}

impl ParsedSwap {
    /// Calculate implied slippage tolerance.
    ///
    /// Currently returns 0.0 until simulation is implemented.
    pub fn slippage_tolerance(&self) -> Decimal {
        Decimal::ZERO
    }
}

/// Monitors pending transactions for swap activity.
pub struct MempoolMonitor {
    ws_url: String,
}

#[derive(Debug, Clone)]
struct SwapContext {
    tx_hash: String,
    router: Address,
    dex: String,
    method: String,
    sender: Address,
    gas_price: U256,
}

impl MempoolMonitor {
    /// Buffer size for parsed swaps channel.
    const SWAP_CHANNEL_SIZE: usize = 100;

    /// Maximum number of in-flight tx fetches when hash fallback is used.
    const MAX_PENDING_FETCHES: usize = 64;

    /// Known DEX router selectors and their human-readable metadata.
    pub const SWAP_SELECTORS: &[(&str, &str, &str)] = &[
        ("0x38ed1739", "UniswapV2", "swapExactTokensForTokens"),
        ("0x7ff36ab5", "UniswapV2", "swapExactETHForTokens"),
        ("0x18cbafe5", "UniswapV2", "swapExactTokensForETH"),
        ("0x5ae401dc", "UniswapV3", "multicall"),
    ];

    /// Creates a new MempoolMonitor instance.
    pub fn new(ws_url: impl Into<String>) -> Self {
        Self {
            ws_url: ws_url.into(),
        }
    }

    /// Starts monitoring pending transactions and yields parsed swaps through a channel.
    ///
    /// The monitor prefers `eth_subscribe` with full pending transactions and
    /// falls back to hash subscriptions with on-demand transaction fetches.
    pub async fn start(&self) -> PricingResult<mpsc::Receiver<ParsedSwap>> {
        let (tx, rx) = mpsc::channel(Self::SWAP_CHANNEL_SIZE);
        let ws_url = self.ws_url.clone();

        info!(ws_url = %ws_url, "Starting MempoolMonitor on WebSocket");

        let probe = Provider::<Ws>::connect(&ws_url)
            .await
            .map_err(|e| PricingError::ChainCall(format!("websocket connect failed: {e}")))?;

        let full_supported = match probe.subscribe_full_pending_txs().await {
            Ok(_) => true,
            Err(full_err) => {
                warn!(
                    error = %full_err,
                    "Node does not support full pending tx subscription, probing hash fallback"
                );

                let _ = probe
                    .subscribe_pending_txs()
                    .await
                    .map_err(|hash_err| {
                        PricingError::ChainCall(format!(
                            "pending tx subscription failed (full stream unsupported: {full_err}; hash fallback failed: {hash_err})"
                        ))
                    })?;
                false
            }
        };

        drop(probe);

        tokio::spawn(async move {
            let provider = match Provider::<Ws>::connect(&ws_url).await {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    warn!(error = %e, "WebSocket provider lost before monitor task start");
                    return;
                }
            };

            if full_supported {
                if let Ok(mut full_stream) = provider.subscribe_full_pending_txs().await {
                    while let Some(tx_data) = full_stream.next().await {
                        if let Some(parsed) = Self::parse_transaction(&tx_data)
                            && let Err(e) = tx.send(parsed).await
                        {
                            warn!(error = %e, "Mempool notification channel closed");
                            break;
                        }
                    }
                    return;
                }
            }

            let mut hash_stream = match provider.subscribe_pending_txs().await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "Failed to subscribe to pending tx hashes in monitor task");
                    return;
                }
            };

            let semaphore = Arc::new(Semaphore::new(Self::MAX_PENDING_FETCHES));
            while let Some(tx_hash) = hash_stream.next().await {
                let provider = Arc::clone(&provider);
                let tx_sender = tx.clone();
                let semaphore = Arc::clone(&semaphore);

                tokio::spawn(async move {
                    let Ok(_permit) = semaphore.acquire_owned().await else {
                        return;
                    };

                    match provider.get_transaction(tx_hash).await {
                        Ok(Some(tx_data)) => {
                            if let Some(parsed) = Self::parse_transaction(&tx_data) {
                                let _ = tx_sender.send(parsed).await;
                            }
                        }
                        Ok(None) => {
                            debug!(hash = ?tx_hash, "Pending hash appeared before full transaction body was available");
                        }
                        Err(e) => {
                            debug!(hash = ?tx_hash, error = %e, "Failed to fetch pending transaction by hash");
                        }
                    }
                });
            }
        });

        Ok(rx)
    }

    /// Parses a raw transaction and extracts swap details for supported selectors.
    ///
    /// Returns `None` when the selector is unknown, calldata is malformed,
    /// or required transaction fields are missing.
    pub fn parse_transaction(tx: &Transaction) -> Option<ParsedSwap> {
        let data = &tx.input;
        if data.len() < 4 {
            return None;
        }

        let selector_hex = format!("0x{}", hex::encode(&data[..4]));
        let (dex, method) = match Self::SWAP_SELECTORS
            .iter()
            .find(|(s, _, _)| *s == selector_hex)
        {
            Some((_, d, m)) => (*d, *m),
            None => return None,
        };

        let router_raw = tx.to?;
        let tx_hash = format!("0x{:x}", tx.hash);
        let router = match Address::new(format!("0x{:x}", router_raw)) {
            Ok(a) => a,
            Err(_) => return None,
        };
        let sender = match Address::new(format!("0x{:x}", tx.from)) {
            Ok(a) => a,
            Err(_) => return None,
        };

        let gas_price = tx.gas_price.unwrap_or_default();
        let context = SwapContext {
            tx_hash,
            router,
            dex: dex.to_string(),
            method: method.to_string(),
            sender,
            gas_price,
        };

        match method {
            "swapExactTokensForTokens" => {
                Self::decode_v2_tokens_for_tokens(tx, &data[4..], &context)
            }
            "swapExactETHForTokens" => Self::decode_v2_eth_for_tokens(tx, &data[4..], &context),
            "swapExactTokensForETH" => Self::decode_v2_tokens_for_eth(tx, &data[4..], &context),
            _ => {
                debug!(method = %method, "Selector recognized but decoding not yet implemented");
                None
            }
        }
    }

    fn decode_v2_tokens_for_tokens(
        _tx: &Transaction,
        payload: &[u8],
        context: &SwapContext,
    ) -> Option<ParsedSwap> {
        let tokens = match ethers::abi::decode(
            &[
                ethers::abi::ParamType::Uint(256),
                ethers::abi::ParamType::Uint(256),
                ethers::abi::ParamType::Array(Box::new(ethers::abi::ParamType::Address)),
                ethers::abi::ParamType::Address,
                ethers::abi::ParamType::Uint(256),
            ],
            payload,
        ) {
            Ok(t) => t,
            Err(_) => return None,
        };

        let amount_in = tokens[0].clone().into_uint()?;
        let min_amount_out = tokens[1].clone().into_uint()?;
        let path = tokens[2].clone().into_array()?;
        let deadline = tokens[4].clone().into_uint()?;

        let token_in = path
            .first()
            .and_then(|t| t.clone().into_address())
            .and_then(|a| Address::new(format!("0x{:x}", a)).ok());
        let token_out = path
            .last()
            .and_then(|t| t.clone().into_address())
            .and_then(|a| Address::new(format!("0x{:x}", a)).ok());

        Some(Self::build_parsed_swap(
            context,
            token_in,
            token_out,
            amount_in,
            min_amount_out,
            deadline,
        ))
    }

    fn decode_v2_eth_for_tokens(
        tx: &Transaction,
        payload: &[u8],
        context: &SwapContext,
    ) -> Option<ParsedSwap> {
        let tokens = match ethers::abi::decode(
            &[
                ethers::abi::ParamType::Uint(256),
                ethers::abi::ParamType::Array(Box::new(ethers::abi::ParamType::Address)),
                ethers::abi::ParamType::Address,
                ethers::abi::ParamType::Uint(256),
            ],
            payload,
        ) {
            Ok(t) => t,
            Err(_) => return None,
        };

        let amount_in = tx.value;
        let min_amount_out = tokens[0].clone().into_uint()?;
        let path = tokens[1].clone().into_array()?;
        let deadline = tokens[3].clone().into_uint()?;

        let token_in = path
            .first()
            .and_then(|t| t.clone().into_address())
            .and_then(|a| Address::new(format!("0x{:x}", a)).ok());
        let token_out = path
            .last()
            .and_then(|t| t.clone().into_address())
            .and_then(|a| Address::new(format!("0x{:x}", a)).ok());

        Some(Self::build_parsed_swap(
            context,
            token_in,
            token_out,
            amount_in,
            min_amount_out,
            deadline,
        ))
    }

    fn decode_v2_tokens_for_eth(
        _tx: &Transaction,
        payload: &[u8],
        context: &SwapContext,
    ) -> Option<ParsedSwap> {
        let tokens = match ethers::abi::decode(
            &[
                ethers::abi::ParamType::Uint(256),
                ethers::abi::ParamType::Uint(256),
                ethers::abi::ParamType::Array(Box::new(ethers::abi::ParamType::Address)),
                ethers::abi::ParamType::Address,
                ethers::abi::ParamType::Uint(256),
            ],
            payload,
        ) {
            Ok(t) => t,
            Err(_) => return None,
        };

        let amount_in = tokens[0].clone().into_uint()?;
        let min_amount_out = tokens[1].clone().into_uint()?;
        let path = tokens[2].clone().into_array()?;
        let deadline = tokens[4].clone().into_uint()?;

        let token_in = path
            .first()
            .and_then(|t| t.clone().into_address())
            .and_then(|a| Address::new(format!("0x{:x}", a)).ok());
        let token_out = path
            .last()
            .and_then(|t| t.clone().into_address())
            .and_then(|a| Address::new(format!("0x{:x}", a)).ok());

        Some(Self::build_parsed_swap(
            context,
            token_in,
            token_out,
            amount_in,
            min_amount_out,
            deadline,
        ))
    }

    fn build_parsed_swap(
        context: &SwapContext,
        token_in: Option<Address>,
        token_out: Option<Address>,
        amount_in: U256,
        min_amount_out: U256,
        deadline: U256,
    ) -> ParsedSwap {
        ParsedSwap {
            tx_hash: context.tx_hash.clone(),
            router: context.router.clone(),
            dex: context.dex.clone(),
            method: context.method.clone(),
            token_in,
            token_out,
            amount_in,
            min_amount_out,
            deadline,
            sender: context.sender.clone(),
            gas_price: context.gas_price,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx_with_input(input: Vec<u8>, gas_price: Option<U256>) -> Transaction {
        Transaction {
            hash: H256::random(),
            to: Some(H160::random()),
            from: H160::random(),
            input: input.into(),
            value: U256::zero(),
            gas_price,
            ..Default::default()
        }
    }

    #[test]
    fn test_decode_v2_tokens_for_tokens() {
        let amount_in = U256::from(1000);
        let amount_out_min = U256::from(900);
        let token_in_addr = H160::from_low_u64_be(1);
        let token_out_addr = H160::from_low_u64_be(2);
        let to_addr = H160::from_low_u64_be(3);
        let deadline = U256::from(1712690000);

        let path = [token_in_addr, token_out_addr];

        let mut payload = Vec::new();
        payload.extend_from_slice(&ethers::abi::encode(&[
            ethers::abi::Token::Uint(amount_in),
            ethers::abi::Token::Uint(amount_out_min),
            ethers::abi::Token::Array(
                path.iter()
                    .map(|a| ethers::abi::Token::Address(*a))
                    .collect(),
            ),
            ethers::abi::Token::Address(to_addr),
            ethers::abi::Token::Uint(deadline),
        ]));

        let mut data = Vec::new();
        data.extend_from_slice(&hex::decode("38ed1739").unwrap()); // selector
        data.extend_from_slice(&payload);

        let tx = Transaction {
            hash: H256::random(),
            to: Some(H160::random()),
            from: H160::random(),
            input: data.into(),
            value: U256::zero(),
            gas_price: Some(U256::from(20000000000u64)),
            ..Default::default()
        };

        let parsed = MempoolMonitor::parse_transaction(&tx).unwrap();
        assert_eq!(parsed.method, "swapExactTokensForTokens");
        assert_eq!(parsed.amount_in, amount_in);
        assert_eq!(parsed.min_amount_out, amount_out_min);
        assert_eq!(parsed.token_in.unwrap().as_eth_address(), token_in_addr);
        assert_eq!(parsed.token_out.unwrap().as_eth_address(), token_out_addr);
        assert_eq!(parsed.deadline, deadline);
    }

    #[test]
    fn test_parse_ignores_tx_without_to() {
        let tx = Transaction {
            hash: H256::random(),
            to: None,
            from: H160::random(),
            input: hex::decode("38ed1739").unwrap().into(),
            value: U256::zero(),
            gas_price: Some(U256::from(1u64)),
            ..Default::default()
        };

        assert!(MempoolMonitor::parse_transaction(&tx).is_none());
    }

    #[test]
    fn test_parse_ignores_unknown_selector() {
        let tx = Transaction {
            hash: H256::random(),
            to: Some(H160::random()),
            from: H160::random(),
            input: hex::decode("deadbeef").unwrap().into(),
            value: U256::zero(),
            gas_price: Some(U256::from(1u64)),
            ..Default::default()
        };

        assert!(MempoolMonitor::parse_transaction(&tx).is_none());
    }

    #[test]
    fn test_parse_ignores_malformed_v2_payload() {
        let mut bad_data = hex::decode("38ed1739").unwrap();
        bad_data.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);

        let tx = tx_with_input(bad_data, Some(U256::from(1u64)));

        assert!(MempoolMonitor::parse_transaction(&tx).is_none());
    }

    #[test]
    fn test_parse_ignores_too_short_input() {
        let tx = tx_with_input(vec![0x38, 0xed, 0x17], Some(U256::from(1u64)));
        assert!(MempoolMonitor::parse_transaction(&tx).is_none());
    }

    #[test]
    fn test_parse_defaults_missing_gas_price_to_zero() {
        let amount_in = U256::from(1000u64);
        let amount_out_min = U256::from(900u64);
        let to_addr = H160::from_low_u64_be(3);
        let deadline = U256::from(1712690000u64);

        let mut data = hex::decode("38ed1739").unwrap();
        data.extend_from_slice(&ethers::abi::encode(&[
            ethers::abi::Token::Uint(amount_in),
            ethers::abi::Token::Uint(amount_out_min),
            ethers::abi::Token::Array(vec![
                ethers::abi::Token::Address(H160::from_low_u64_be(1)),
                ethers::abi::Token::Address(H160::from_low_u64_be(2)),
            ]),
            ethers::abi::Token::Address(to_addr),
            ethers::abi::Token::Uint(deadline),
        ]));

        let tx = tx_with_input(data, None);
        let parsed = MempoolMonitor::parse_transaction(&tx).unwrap();
        assert_eq!(parsed.gas_price, U256::zero());
    }

    #[test]
    fn test_parse_handles_empty_path_without_panicking() {
        let amount_in = U256::from(1000u64);
        let amount_out_min = U256::from(900u64);
        let to_addr = H160::from_low_u64_be(3);
        let deadline = U256::from(1712690000u64);

        let mut data = hex::decode("38ed1739").unwrap();
        data.extend_from_slice(&ethers::abi::encode(&[
            ethers::abi::Token::Uint(amount_in),
            ethers::abi::Token::Uint(amount_out_min),
            ethers::abi::Token::Array(vec![]),
            ethers::abi::Token::Address(to_addr),
            ethers::abi::Token::Uint(deadline),
        ]));

        let tx = tx_with_input(data, Some(U256::from(1u64)));
        let parsed = MempoolMonitor::parse_transaction(&tx).unwrap();
        assert!(parsed.token_in.is_none());
        assert!(parsed.token_out.is_none());
    }

    #[test]
    fn test_parse_ignores_malformed_eth_for_tokens_payload() {
        let mut bad_data = hex::decode("7ff36ab5").unwrap();
        bad_data.extend_from_slice(&[0x00, 0x01]);
        let tx = tx_with_input(bad_data, Some(U256::from(1u64)));
        assert!(MempoolMonitor::parse_transaction(&tx).is_none());
    }

    #[test]
    fn test_parse_ignores_malformed_tokens_for_eth_payload() {
        let mut bad_data = hex::decode("18cbafe5").unwrap();
        bad_data.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let tx = tx_with_input(bad_data, Some(U256::from(1u64)));
        assert!(MempoolMonitor::parse_transaction(&tx).is_none());
    }
}
