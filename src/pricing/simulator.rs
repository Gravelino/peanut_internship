//! # Fork Simulator
//! Simulates transactions on a forked/local node before execution.

use std::sync::Arc;

use ethers::abi::ParamType;
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{BlockNumber, Bytes, TransactionRequest as EthTransactionRequest, U256};
use serde::{Deserialize, Serialize};

use crate::core::types::{Address, BlockId, Token as CoreToken};
use crate::pricing::UniswapV2Pair;
use crate::pricing::errors::{PricingError, PricingResult};
use crate::pricing::router::Route;

/// Fallback amount used when call output cannot be decoded.
const DEFAULT_AMOUNT_OUT: u128 = 0;

/// Fallback gas value used when estimation fails.
const DEFAULT_GAS_USED: u64 = 0;

/// Final simulation verdict used by execution logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationVerdict {
    /// Transaction call succeeded against the selected block context.
    Executable,
    /// Transaction likely reverts on-chain.
    RevertLikely,
}

/// Strategy for decoding `amount_out` from returned call bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AmountOutDecoder {
    /// Decode as UniswapV2 style `uint256[] amounts` and take the last element.
    UniswapV2Amounts,
    /// Decode as a single `uint256` value.
    SingleUint,
    /// Skip decoding and report `amount_out = 0`.
    None,
}

/// Parameters needed to dry-run a router swap call.
#[derive(Debug, Clone)]
pub struct SwapParams {
    /// ABI-encoded router calldata.
    pub calldata: Bytes,
    /// Native token value to send with the call.
    pub value: U256,
    /// Optional gas override for the call.
    pub gas_limit: Option<u64>,
    /// Strategy used to decode `amount_out` from return data.
    pub decoder: AmountOutDecoder,
}

impl Default for SwapParams {
    fn default() -> Self {
        Self {
            calldata: Bytes::new(),
            value: U256::zero(),
            gas_limit: None,
            decoder: AmountOutDecoder::UniswapV2Amounts,
        }
    }
}

/// Result of local fork simulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationResult {
    /// `true` when the simulated call succeeded.
    pub success: bool,
    /// Decoded output amount (raw units).
    pub amount_out: u128,
    /// Estimated gas required for the call.
    pub gas_used: u64,
    /// Optional revert reason or call error message.
    pub error: Option<String>,
    /// Human-readable hop/event notes emitted by the simulator.
    pub logs: Vec<String>,
}

impl SimulationResult {
    /// Converts `success` flag into a stable verdict enum.
    pub fn verdict(&self) -> SimulationVerdict {
        if self.success {
            SimulationVerdict::Executable
        } else {
            SimulationVerdict::RevertLikely
        }
    }

    /// Returns true when simulation result is executable.
    pub fn is_executable(&self) -> bool {
        self.success
    }
}

/// Backward-compatible alias for earlier module naming.
pub type ForkSimulation = SimulationResult;

/// Output of AMM math-vs-simulation validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationComparison {
    /// Deterministic output from AMM formula.
    pub calculated: u128,
    /// Output from simulation path.
    pub simulated: u128,
    /// Absolute difference between formula and simulation.
    pub difference: u128,
    /// True when values match exactly.
    pub is_match: bool,
}

/// Simulator wrapper around [ChainClient] for pre-trade verification.
#[derive(Clone)]
pub struct ForkSimulator {
    provider: Arc<Provider<Http>>,
    block: BlockId,
}

impl ForkSimulator {
    /// Creates simulator bound to a local Anvil/Hardhat fork URL.
    pub fn new(fork_url: impl AsRef<str>) -> PricingResult<Self> {
        let provider = Provider::<Http>::try_from(fork_url.as_ref())
            .map_err(|e| PricingError::ChainCall(format!("invalid fork_url: {e}")))?;

        Ok(Self {
            provider: Arc::new(provider),
            block: BlockId::Pending,
        })
    }

    /// Creates simulator pinned to a specific block context.
    pub fn with_block(fork_url: impl AsRef<str>, block: BlockId) -> PricingResult<Self> {
        let mut simulator = Self::new(fork_url)?;
        simulator.block = block;
        Ok(simulator)
    }

    /// Simulate a single router swap call using local fork state.
    pub async fn simulate_swap(
        &self,
        router: Address,
        swap_params: SwapParams,
        sender: Address,
    ) -> PricingResult<SimulationResult> {
        let mut tx = EthTransactionRequest::new();
        tx.to = Some(router.as_eth_address().into());
        tx.from = Some(sender.as_eth_address());
        tx.value = Some(swap_params.value);
        tx.data = Some(swap_params.calldata);
        if let Some(gas) = swap_params.gas_limit {
            tx.gas = Some(gas.into());
        }

        let typed = tx.into();
        let block = Some(to_eth_block(self.block).into());
        let gas_used = self
            .provider
            .estimate_gas(&typed, block)
            .await
            .map(|g| g.as_u64())
            .unwrap_or(DEFAULT_GAS_USED);

        match self.provider.call(&typed, block).await {
            Ok(bytes) => {
                let amount_out = decode_amount_out(bytes.as_ref(), swap_params.decoder)
                    .unwrap_or(DEFAULT_AMOUNT_OUT);
                Ok(SimulationResult {
                    success: true,
                    amount_out,
                    gas_used,
                    error: None,
                    logs: Vec::new(),
                })
            }
            Err(err) => {
                let message = err.to_string();
                Ok(SimulationResult {
                    success: false,
                    amount_out: DEFAULT_AMOUNT_OUT,
                    gas_used,
                    error: extract_revert_reason(&message).or(Some(message)),
                    logs: Vec::new(),
                })
            }
        }
    }

    /// Simulate a multi-hop route with exact pool math and route gas model.
    pub async fn simulate_route(
        &self,
        route: &Route,
        amount_in: u128,
        sender: Address,
    ) -> PricingResult<SimulationResult> {
        let amounts = route.get_intermediate_amounts(amount_in)?;
        let amount_out = amounts.last().copied().unwrap_or(DEFAULT_AMOUNT_OUT);
        let mut logs = Vec::with_capacity(route.num_hops() + 1);
        logs.push(format!("simulated_for_sender={sender}"));

        for i in 0..route.num_hops() {
            let token_in = &route.path[i].symbol;
            let token_out = &route.path[i + 1].symbol;
            logs.push(format!(
                "hop{} {}->{} in={} out={}",
                i + 1,
                token_in,
                token_out,
                amounts[i],
                amounts[i + 1]
            ));
        }

        Ok(SimulationResult {
            success: true,
            amount_out,
            gas_used: route.estimate_gas() as u64,
            error: None,
            logs,
        })
    }

    /// Compare deterministic AMM math vs local simulated swap progression.
    pub fn compare_simulation_vs_calculation(
        &self,
        pair: &UniswapV2Pair,
        amount_in: u128,
        token_in: &CoreToken,
    ) -> PricingResult<SimulationComparison> {
        let calculated = pair.get_amount_out(amount_in, token_in)?;
        let simulated_pair = pair.simulate_swap(amount_in, token_in)?;

        let simulated = if *token_in == pair.token0 {
            pair.reserve1.saturating_sub(simulated_pair.reserve1)
        } else {
            pair.reserve0.saturating_sub(simulated_pair.reserve0)
        };

        let difference = calculated.abs_diff(simulated);
        Ok(SimulationComparison {
            calculated,
            simulated,
            difference,
            is_match: difference == 0,
        })
    }
}

fn to_eth_block(block: BlockId) -> BlockNumber {
    match block {
        BlockId::Latest => BlockNumber::Latest,
        BlockId::Pending => BlockNumber::Pending,
    }
}

fn decode_amount_out(raw: &[u8], decoder: AmountOutDecoder) -> Option<u128> {
    match decoder {
        AmountOutDecoder::None => Some(DEFAULT_AMOUNT_OUT),
        AmountOutDecoder::SingleUint => {
            let tokens = ethers::abi::decode(&[ParamType::Uint(256)], raw).ok()?;
            let out = tokens.first()?.clone().into_uint()?;
            Some(out.as_u128())
        }
        AmountOutDecoder::UniswapV2Amounts => {
            let tokens =
                ethers::abi::decode(&[ParamType::Array(Box::new(ParamType::Uint(256)))], raw)
                    .ok()?;
            let arr = tokens.first()?.clone().into_array()?;
            let last = arr.last()?.clone().into_uint()?;
            Some(last.as_u128())
        }
    }
}

fn extract_revert_reason(msg: &str) -> Option<String> {
    let lowered = msg.to_lowercase();

    if let Some(idx) = lowered.find("execution reverted:") {
        let reason = msg[idx + "execution reverted:".len()..].trim();
        if !reason.is_empty() {
            return Some(trim_quotes(reason).to_string());
        }
    }

    if let Some(idx) = lowered.find("revert ") {
        let reason = msg[idx + "revert ".len()..].trim();
        if !reason.is_empty() {
            return Some(trim_quotes(reason).to_string());
        }
    }

    None
}

fn trim_quotes(s: &str) -> &str {
    s.trim_matches('"').trim_matches('\'')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{Address, Token};
    use crate::pricing::UniswapV2Pair;
    use crate::pricing::router::Route;

    #[test]
    fn test_decode_amount_out_uniswap_v2_array() {
        let encoded = ethers::abi::encode(&[ethers::abi::Token::Array(vec![
            ethers::abi::Token::Uint(U256::from(1u64)),
            ethers::abi::Token::Uint(U256::from(2u64)),
            ethers::abi::Token::Uint(U256::from(777u64)),
        ])]);

        let decoded = decode_amount_out(&encoded, AmountOutDecoder::UniswapV2Amounts);
        assert_eq!(decoded, Some(777));
    }

    #[test]
    fn test_decode_amount_out_single_uint() {
        let encoded = ethers::abi::encode(&[ethers::abi::Token::Uint(U256::from(42u64))]);
        let decoded = decode_amount_out(&encoded, AmountOutDecoder::SingleUint);
        assert_eq!(decoded, Some(42));
    }

    #[test]
    fn test_extract_revert_reason_execution_reverted() {
        let msg = "rpc error: execution reverted: UniswapV2Router: INSUFFICIENT_OUTPUT_AMOUNT";
        let reason = extract_revert_reason(msg);
        assert_eq!(
            reason.as_deref(),
            Some("UniswapV2Router: INSUFFICIENT_OUTPUT_AMOUNT")
        );
    }

    #[test]
    fn test_extract_revert_reason_plain_revert() {
        let msg = "vm error: revert transfer amount exceeds balance";
        let reason = extract_revert_reason(msg);
        assert_eq!(reason.as_deref(), Some("transfer amount exceeds balance"));
    }

    #[test]
    fn test_compare_simulation_vs_calculation_match() {
        let token0 = Token {
            address: Address::new("0x0000000000000000000000000000000000000001").unwrap(),
            symbol: "A".to_string(),
            decimals: 18,
        };
        let token1 = Token {
            address: Address::new("0x0000000000000000000000000000000000000002").unwrap(),
            symbol: "B".to_string(),
            decimals: 18,
        };
        let pair = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000000").unwrap(),
            token0.clone(),
            token1,
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let simulator = ForkSimulator::new("http://127.0.0.1:8545").unwrap();
        let cmp = simulator
            .compare_simulation_vs_calculation(&pair, 1_000_000_000_000_000_000, &token0)
            .unwrap();

        assert!(cmp.is_match);
        assert_eq!(cmp.difference, 0);
    }

    #[test]
    fn test_compare_simulation_vs_calculation_match_reverse_direction() {
        let token0 = Token {
            address: Address::new("0x0000000000000000000000000000000000000001").unwrap(),
            symbol: "A".to_string(),
            decimals: 18,
        };
        let token1 = Token {
            address: Address::new("0x0000000000000000000000000000000000000002").unwrap(),
            symbol: "B".to_string(),
            decimals: 18,
        };
        let pair = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000000").unwrap(),
            token0,
            token1.clone(),
            1_500_000_000_000_000_000_000,
            2_500_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let simulator = ForkSimulator::new("http://127.0.0.1:8545").unwrap();
        let cmp = simulator
            .compare_simulation_vs_calculation(&pair, 2_000_000_000_000_000_000, &token1)
            .unwrap();

        assert!(cmp.is_match);
        assert_eq!(cmp.difference, 0);
    }

    #[test]
    fn test_compare_simulation_vs_calculation_rejects_unknown_token() {
        let token0 = Token {
            address: Address::new("0x0000000000000000000000000000000000000001").unwrap(),
            symbol: "A".to_string(),
            decimals: 18,
        };
        let token1 = Token {
            address: Address::new("0x0000000000000000000000000000000000000002").unwrap(),
            symbol: "B".to_string(),
            decimals: 18,
        };
        let token_unknown = Token {
            address: Address::new("0x0000000000000000000000000000000000000003").unwrap(),
            symbol: "C".to_string(),
            decimals: 18,
        };

        let pair = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000000").unwrap(),
            token0,
            token1,
            1_000_000_000_000_000_000_000,
            2_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let simulator = ForkSimulator::new("http://127.0.0.1:8545").unwrap();
        let err = simulator
            .compare_simulation_vs_calculation(&pair, 1_000_000_000_000_000_000, &token_unknown)
            .unwrap_err();
        assert!(matches!(err, PricingError::UnknownToken(_)));
    }

    #[tokio::test]
    async fn test_simulate_route_produces_consistent_gas_and_logs() {
        let token_a = Token {
            address: Address::new("0x00000000000000000000000000000000000000a1").unwrap(),
            symbol: "A".to_string(),
            decimals: 18,
        };
        let token_b = Token {
            address: Address::new("0x00000000000000000000000000000000000000b1").unwrap(),
            symbol: "B".to_string(),
            decimals: 18,
        };
        let token_c = Token {
            address: Address::new("0x00000000000000000000000000000000000000c1").unwrap(),
            symbol: "C".to_string(),
            decimals: 18,
        };

        let pool_ab = UniswapV2Pair::new(
            Address::new("0x1000000000000000000000000000000000000001").unwrap(),
            token_a.clone(),
            token_b.clone(),
            1_000_000_000_000_000_000_000,
            1_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();
        let pool_bc = UniswapV2Pair::new(
            Address::new("0x2000000000000000000000000000000000000001").unwrap(),
            token_b.clone(),
            token_c.clone(),
            1_000_000_000_000_000_000_000,
            1_000_000_000_000_000_000_000,
            30,
        )
        .unwrap();

        let route = Route::new(
            vec![pool_ab, pool_bc],
            vec![token_a.clone(), token_b.clone(), token_c.clone()],
        );
        let simulator = ForkSimulator::new("http://127.0.0.1:8545").unwrap();
        let sender = Address::new("0x00000000000000000000000000000000000000aa").unwrap();

        let result = simulator
            .simulate_route(&route, 1_000_000_000_000_000_000, sender)
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.amount_out > 0);
        assert_eq!(result.gas_used, route.estimate_gas() as u64);
        assert_eq!(result.logs.len(), route.num_hops() + 1);
        assert!(result.logs[1].contains("hop1"));
        assert!(result.logs[2].contains("hop2"));
    }
}
