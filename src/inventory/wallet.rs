use std::collections::HashMap;

use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use crate::chain::ChainClient;
use crate::core::types::{
    ARBITRUM_CHAIN_ID, Address, BlockId, DECIMAL_BASE, ETH_DECIMALS, MAINNET_CHAIN_ID, RPC_RETRIES,
    RPC_TIMEOUT_SECS, TransactionRequest,
};
use crate::inventory::errors::{InventoryError, InventoryResult};

const BALANCEOF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];

const EVM_WORD_LEN: usize = 32;

const WELL_KNOWN_TOKENS: &[(&str, &str, u8)] = &[
    ("ETH", "", 18),
    ("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18),
    ("USDT", "0xdAC17F958D2ee523a2206206994597C13D831ec7", 6),
    ("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6),
    ("WBTC", "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2F6F1", 8),
    ("DAI", "0x6B175474E89094C44Da98b954EedeAC495271d0F", 18),
    ("BNB", "0xB8c77482e45F1F44dE1745F52C74426C631bDD52", 18),
    ("FDUSD", "0xc5f0f7b66764F6ec8C8Dff7BA683102295E16409", 18),
];

/// Well-known ERC-20 tokens on Arbitrum One (chain ID [`ARBITRUM_CHAIN_ID`]).
const WELL_KNOWN_TOKENS_ARBITRUM: &[(&str, &str, u8)] = &[
    ("ETH", "", 18),
    ("WETH", "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1", 18),
    ("USDT", "0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9", 6),
    ("USDC", "0xaf88d065e77c8cC2239327C5EDb3A432268e5831", 6),
    ("USDC.e", "0xFF970A61A04b1cA14834A43f5dE4533eBDDB5CC8", 6),
    ("LINK", "0xf97f4df75117a78c1A5a0DBb814Af92458539FB4", 18),
    ("ARB", "0x912CE59144191C1204E64559FE8253a0e49E6548", 18),
    ("GMX", "0xfc5A1A6EB076a2C7aD06eD22C90d7E710E35ad0a", 18),
    ("PENDLE", "0x0c880f6761F1af8d9Aa9C466984b80DAb9a8c9e8", 18),
    ("LDO", "0x13Ad51ed4F1B7e9Dc168d8a00cB3f4dDD85EFA60", 18),
    ("DAI", "0xDA10009cBd5D07dd0CeCc66161FC93D7c9000da1", 18),
    ("WBTC", "0x2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f", 8),
];

/// Fetches native and ERC-20 token balances for an on-chain wallet.
#[derive(Clone)]
pub struct WalletBalanceFetcher {
    chain_client: ChainClient,
    wallet_address: Address,
    tokens: Vec<(String, String, u8)>,
    chain_id: u64,
}

impl std::fmt::Debug for WalletBalanceFetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletBalanceFetcher")
            .field("wallet_address", &self.wallet_address)
            .field("tokens", &self.tokens.len())
            .finish_non_exhaustive()
    }
}

impl WalletBalanceFetcher {
    /// Creates a new fetcher using the given RPC URL and wallet address.
    pub fn new(rpc_url: String, wallet_address: &str) -> InventoryResult<Self> {
        Self::new_multi(vec![rpc_url], wallet_address)
    }

    /// Creates a new fetcher using multiple RPC URLs and wallet address.
    pub fn new_multi(rpc_urls: Vec<String>, wallet_address: &str) -> InventoryResult<Self> {
        let chain_client =
            ChainClient::new(rpc_urls, RPC_TIMEOUT_SECS, RPC_RETRIES).map_err(|e| {
                InventoryError::AssetNotFound(format!("failed to create chain client: {e}"))
            })?;

        let wallet_address = Address::new(wallet_address)
            .map_err(|e| InventoryError::AssetNotFound(format!("invalid wallet address: {e}")))?;

        Ok(Self {
            chain_client,
            wallet_address,
            tokens: WELL_KNOWN_TOKENS
                .iter()
                .map(|(s, a, d)| (s.to_string(), a.to_string(), *d))
                .collect(),
            chain_id: MAINNET_CHAIN_ID,
        })
    }

    /// Replaces the default token list with a custom one (symbol, address, decimals).
    pub fn with_tokens(mut self, tokens: Vec<(&'static str, &'static str, u8)>) -> Self {
        self.tokens = tokens
            .into_iter()
            .map(|(s, a, d)| (s.to_string(), a.to_string(), d))
            .collect();
        self
    }

    /// Replaces the default token list with dynamically built entries.
    pub fn with_tokens_dynamic(mut self, tokens: Vec<(String, String, u8)>) -> Self {
        self.tokens = tokens;
        self
    }

    /// Sets the chain ID for `eth_call` requests (default: Ethereum mainnet).
    pub fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = chain_id;
        self
    }

    /// Returns the appropriate default token list for the given chain ID.
    pub fn default_tokens_for_chain(chain_id: u64) -> Vec<(String, String, u8)> {
        let list = match chain_id {
            ARBITRUM_CHAIN_ID => WELL_KNOWN_TOKENS_ARBITRUM,
            _ => WELL_KNOWN_TOKENS,
        };
        list.iter()
            .map(|(s, a, d)| (s.to_string(), a.to_string(), *d))
            .collect()
    }

    /// Returns the monitored wallet address.
    pub fn wallet_address(&self) -> &Address {
        &self.wallet_address
    }

    /// Fetches all native and ERC-20 balances, returning non-zero amounts keyed by ticker.
    pub async fn fetch_balances(&self) -> InventoryResult<HashMap<String, Decimal>> {
        let mut balances = HashMap::new();

        match self.fetch_native_balance().await {
            Ok(eth_bal) => {
                if eth_bal > Decimal::ZERO {
                    balances.insert("ETH".into(), eth_bal);
                }
                info!(balance = %eth_bal, "Fetched native ETH balance");
            }
            Err(e) => {
                warn!("Failed to fetch native ETH balance: {e}");
            }
        }

        for (symbol, address, decimals) in &self.tokens {
            if address.is_empty() {
                continue;
            }

            match self.fetch_erc20_balance(address, *decimals).await {
                Ok(bal) => {
                    if bal > Decimal::ZERO {
                        balances.insert(symbol.to_string(), bal);
                        info!(symbol, balance = %bal, "Fetched wallet token balance");
                    }
                }
                Err(e) => {
                    debug!(symbol, error = %e, "Skipping ERC-20 balance");
                }
            }
        }

        info!(
            wallet = %self.wallet_address,
            total_assets = balances.len(),
            "Wallet balance fetch complete"
        );

        Ok(balances)
    }

    async fn fetch_native_balance(&self) -> InventoryResult<Decimal> {
        let token_amount = self
            .chain_client
            .get_balance(&self.wallet_address)
            .await
            .map_err(|e| InventoryError::AssetNotFound(format!("native balance: {e}")))?;

        let wei = token_amount.raw.as_u128();
        Ok(Decimal::from(wei) / Decimal::from(DECIMAL_BASE.pow(ETH_DECIMALS as u32)))
    }

    async fn fetch_erc20_balance(
        &self,
        token_address: &str,
        decimals: u8,
    ) -> InventoryResult<Decimal> {
        let addr = Address::new(token_address)
            .map_err(|e| InventoryError::AssetNotFound(format!("invalid token address: {e}")))?;

        let mut calldata = BALANCEOF_SELECTOR.to_vec();
        calldata.resize(BALANCEOF_SELECTOR.len() + 12, 0);
        let wallet_bytes = self.wallet_address.as_eth_address().0;
        calldata.extend_from_slice(&wallet_bytes);

        let call = TransactionRequest::contract_call(addr, calldata, self.chain_id);

        let result = self
            .chain_client
            .call(&call, BlockId::Latest)
            .await
            .map_err(|e| InventoryError::AssetNotFound(format!("balanceOf call: {e}")))?;

        let raw_balance = decode_u128_from_slot(&result)?;

        Ok(Decimal::from(raw_balance) / Decimal::from(DECIMAL_BASE.pow(decimals as u32)))
    }
}

fn decode_u128_from_slot(slot: &[u8]) -> InventoryResult<u128> {
    if slot.len() < EVM_WORD_LEN {
        return Err(InventoryError::AssetNotFound(format!(
            "slot too short: {} bytes",
            slot.len()
        )));
    }
    let bytes: [u8; 16] = slot[EVM_WORD_LEN / 2..EVM_WORD_LEN]
        .try_into()
        .map_err(|_| InventoryError::AssetNotFound("slot conversion failed".into()))?;
    Ok(u128::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wallet_fetcher_constructs() {
        let fetcher = WalletBalanceFetcher::new(
            "http://127.0.0.1:1".to_string(),
            "0x0000000000000000000000000000000000000001",
        );
        assert!(fetcher.is_ok());
    }

    #[test]
    fn test_wallet_fetcher_rejects_bad_address() {
        let fetcher = WalletBalanceFetcher::new("http://127.0.0.1:1".to_string(), "0xinvalid");
        assert!(fetcher.is_err());
    }

    #[test]
    fn test_decode_u128_from_slot() {
        let mut slot = [0u8; 32];
        slot[16..32].copy_from_slice(&1000u128.to_be_bytes());
        let val = decode_u128_from_slot(&slot).unwrap();
        assert_eq!(val, 1000);
    }

    #[test]
    fn test_decode_u128_from_slot_too_short() {
        let slot = [0u8; 16];
        assert!(decode_u128_from_slot(&slot).is_err());
    }

    #[test]
    fn test_with_tokens_custom() {
        let fetcher = WalletBalanceFetcher::new(
            "http://127.0.0.1:1".to_string(),
            "0x0000000000000000000000000000000000000001",
        )
        .unwrap()
        .with_tokens(vec![(
            "FOO",
            "0x0000000000000000000000000000000000000002",
            18,
        )]);

        assert_eq!(fetcher.tokens.len(), 1);
        assert_eq!(fetcher.tokens[0].0, "FOO");
    }

    #[tokio::test]
    async fn test_fetch_balances_fails_gracefully_on_dead_rpc() {
        let fetcher = WalletBalanceFetcher::new(
            "http://127.0.0.1:1".to_string(),
            "0x0000000000000000000000000000000000000001",
        )
        .unwrap();

        let result = fetcher.fetch_balances().await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }
}
