use ethers::abi::{ParamType, Token as AbiToken};
use ethers::types::U256;
use ethers::utils::id;
use rust_decimal::Decimal;

use super::math;
use super::tick;
use crate::chain::client::ChainClient;
use crate::core::types::{
    Address, BlockId, DECIMAL_BASE, MAINNET_CHAIN_ID, Token, TokenAmount, TransactionRequest,
    V3_PRICE_PRECISION,
};
use crate::pricing::amm::{decode_address_from_slot, decode_u128_from_slot, fetch_token_metadata};
use crate::pricing::errors::{PricingError, PricingResult};

const EVM_WORD_LEN: usize = 32;

const SLOT0_SELECTOR: [u8; 4] = [0x38, 0x50, 0xc7, 0xbd];
const LIQUIDITY_SELECTOR: [u8; 4] = [0x1a, 0x68, 0x65, 0x02];
const FEE_SELECTOR: [u8; 4] = [0xdd, 0xca, 0x3f, 0x43];
const TOKEN0_SELECTOR: [u8; 4] = [0x0d, 0xfe, 0x16, 0x81];
const TOKEN1_SELECTOR: [u8; 4] = [0xd2, 0x12, 0x20, 0xa7];

const QUOTER_V2_EXACT_INPUT_SINGLE_SIGNATURE: &str =
    "quoteExactInputSingle((address,address,uint256,uint24,uint160))";
const QUOTER_V2_EXACT_OUTPUT_SINGLE_SIGNATURE: &str =
    "quoteExactOutputSingle((address,address,uint256,uint24,uint160))";
const QUOTER_V2_RETURN_TYPES: [ParamType; 4] = [
    ParamType::Uint(256),
    ParamType::Uint(160),
    ParamType::Uint(32),
    ParamType::Uint(256),
];

const SLOT0_RETURN_MIN: usize = EVM_WORD_LEN * 3;

/// Maximum iterations in the V3 swap step loop.
/// 2 steps covers most in-range swaps; ticks that require more are rare.
const MAX_SWAP_STEPS: usize = 50;

/// Base gas cost for a V3 swap (pool entry + exit overhead).
/// Source: Uniswap V3 audit / empirical measurement.
const V3_BASE_GAS: u128 = 80_000;
/// Additional gas per tick-crossing hop in a V3 swap.
const V3_GAS_PER_HOP: u128 = 60_000;

/// Returns the base gas cost for a V3 swap execution.
pub fn v3_base_gas() -> u128 {
    V3_BASE_GAS
}

/// Returns the additional gas cost per tick-crossing hop in a V3 swap.
pub fn v3_gas_per_hop() -> u128 {
    V3_GAS_PER_HOP
}

/// Uniswap V3 concentrated-liquidity pool. Swap quoting uses tick math.
#[derive(Debug, Clone)]
pub struct UniswapV3Pool {
    /// On-chain address of the pool contract.
    pub address: Address,
    /// The lower-address token in the pool.
    pub token0: Token,
    /// The higher-address token in the pool.
    pub token1: Token,
    /// Pool fee in basis points (e.g. 3000 = 0.3%).
    pub fee_bps: u32,
    /// Tick spacing for this fee tier.
    pub tick_spacing: i32,
    /// Current sqrt price as a Q64.96 fixed-point value.
    pub sqrt_price_x96: U256,
    /// Current active liquidity in the pool.
    pub liquidity: u128,
    /// Current active tick index.
    pub tick: i32,
}

/// Result of a V3 swap quote simulation.
#[derive(Debug, Clone)]
pub struct V3SwapQuote {
    /// Input amount consumed (may be less than requested on partial fill).
    pub amount_in: u128,
    /// Output amount produced in raw token units.
    pub amount_out: u128,
    /// Sqrt price after the swap (Q64.96).
    pub sqrt_price_after: U256,
    /// Tick index after the swap.
    pub tick_after: i32,
    /// Estimated gas units for the swap.
    pub gas_estimate: u64,
    /// `true` when the swap could not be fully filled within step limits.
    pub is_partial: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V3QuoterKind {
    QuoterV2,
}

#[derive(Debug, Clone)]
pub struct V3QuoterConfig {
    pub address: Address,
    pub kind: V3QuoterKind,
}

impl UniswapV3Pool {
    /// Creates a new V3 pool. Computes tick spacing from the fee tier.
    pub fn new(
        address: Address,
        token0: Token,
        token1: Token,
        fee_bps: u32,
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
    ) -> PricingResult<Self> {
        let tick_spacing = tick::fee_tier_to_tick_spacing(fee_bps);
        Ok(Self {
            address,
            token0,
            token1,
            fee_bps,
            tick_spacing,
            sqrt_price_x96,
            liquidity,
            tick,
        })
    }

    /// Returns the output token for a given input token.
    pub fn token_out_for<'a>(&'a self, token_in: &Token) -> PricingResult<&'a Token> {
        if *token_in == self.token0 {
            Ok(&self.token1)
        } else if *token_in == self.token1 {
            Ok(&self.token0)
        } else {
            Err(PricingError::UnknownToken(token_in.symbol.clone()))
        }
    }

    /// Returns the spot price of `token_in` in terms of the other token (display-only).
    pub fn get_spot_price(&self, token_in: &Token) -> PricingResult<Decimal> {
        let token_out = self.token_out_for(token_in)?;

        let sqrt_price = self.sqrt_price_x96;
        let q96 = math::q96();
        let prec = U256::from(V3_PRICE_PRECISION);

        let ratio_scaled = (sqrt_price * prec / q96).as_u128();
        let ratio = Decimal::from(ratio_scaled) / Decimal::from(V3_PRICE_PRECISION);
        let price_human = ratio * ratio;

        let scale_in = Decimal::from(DECIMAL_BASE.pow(token_in.decimals as u32));
        let scale_out = Decimal::from(DECIMAL_BASE.pow(token_out.decimals as u32));

        let zero_for_one = *token_in == self.token0;
        if zero_for_one {
            Ok(price_human * scale_in / scale_out)
        } else {
            let inv = Decimal::ONE / price_human;
            Ok(inv * scale_out / scale_in)
        }
    }

    /// Quotes a single-direction swap, simulating tick crossings up to `MAX_SWAP_STEPS`.
    pub fn quote_swap(&self, amount_in: u128, token_in: &Token) -> PricingResult<V3SwapQuote> {
        if amount_in == 0 {
            return Err(PricingError::ZeroAmountIn);
        }

        let zero_for_one = if *token_in == self.token0 {
            true
        } else if *token_in == self.token1 {
            false
        } else {
            return Err(PricingError::UnknownToken(token_in.symbol.clone()));
        };

        let mut sqrt_price_current = self.sqrt_price_x96;
        let current_liquidity = self.liquidity;
        let mut amount_remaining = amount_in;
        let mut total_out: u128 = 0;
        let mut steps = 0;
        let mut current_tick = self.tick;

        while amount_remaining > 0 && current_liquidity > 0 && steps < MAX_SWAP_STEPS {
            let next_tick = tick::next_tick_boundary(current_tick, self.tick_spacing, zero_for_one);

            if amount_remaining == 0 || current_liquidity == 0 {
                break;
            }

            if next_tick == current_tick {
                break;
            }

            let sqrt_ratio_target = math::get_sqrt_ratio_at_tick(next_tick)?;

            let (step_sqrt_target, step_liquidity) = if zero_for_one {
                if sqrt_ratio_target > sqrt_price_current {
                    (sqrt_price_current, current_liquidity)
                } else {
                    (sqrt_ratio_target, current_liquidity)
                }
            } else {
                if sqrt_ratio_target < sqrt_price_current {
                    (sqrt_price_current, current_liquidity)
                } else {
                    (sqrt_ratio_target, current_liquidity)
                }
            };

            let step_result = math::compute_swap_step(
                sqrt_price_current,
                step_sqrt_target,
                step_liquidity,
                amount_remaining,
                self.fee_bps,
            )?;

            total_out = total_out.saturating_add(step_result.amount_out);

            if step_result.amount_in >= amount_remaining {
                amount_remaining = 0;
            } else {
                amount_remaining = amount_remaining.saturating_sub(step_result.amount_in);
            }

            sqrt_price_current = step_result.sqrt_ratio_next;

            if sqrt_price_current == step_sqrt_target && amount_remaining > 0 {
                current_tick = next_tick;
            } else {
                current_tick =
                    math::get_tick_at_sqrt_ratio(sqrt_price_current).unwrap_or(current_tick);
            }

            steps += 1;
        }

        let is_partial = amount_remaining > 0 && steps >= MAX_SWAP_STEPS;
        let gas_estimate = (V3_BASE_GAS + V3_GAS_PER_HOP * (steps as u128)) as u64;

        Ok(V3SwapQuote {
            amount_in: amount_in.saturating_sub(amount_remaining),
            amount_out: total_out,
            sqrt_price_after: sqrt_price_current,
            tick_after: current_tick,
            gas_estimate,
            is_partial,
        })
    }

    /// Estimates gas units for a V3 swap with the given number of hops.
    pub fn estimate_gas(num_hops: usize) -> u128 {
        V3_BASE_GAS + V3_GAS_PER_HOP * (num_hops as u128)
    }

    pub async fn quote_exact_input_single(
        &self,
        quoter: &V3QuoterConfig,
        client: &ChainClient,
        amount_in: u128,
        token_in: &Token,
    ) -> PricingResult<u128> {
        let token_out = self.token_out_for(token_in)?;
        match quoter.kind {
            V3QuoterKind::QuoterV2 => {
                let data = encode_quoter_v2_single(
                    QUOTER_V2_EXACT_INPUT_SINGLE_SIGNATURE,
                    token_in,
                    token_out,
                    amount_in,
                    self.fee_bps,
                );
                let call = TransactionRequest::contract_call(
                    quoter.address.clone(),
                    data,
                    MAINNET_CHAIN_ID,
                );
                let raw = client
                    .call(&call, BlockId::Latest)
                    .await
                    .map_err(|e| PricingError::ChainCall(e.to_string()))?;
                decode_quoter_v2_amount(&raw)
            }
        }
    }

    pub async fn quote_exact_output_single(
        &self,
        quoter: &V3QuoterConfig,
        client: &ChainClient,
        amount_out: u128,
        token_out: &Token,
    ) -> PricingResult<u128> {
        let token_in = if *token_out == self.token1 {
            &self.token0
        } else if *token_out == self.token0 {
            &self.token1
        } else {
            return Err(PricingError::UnknownToken(token_out.symbol.clone()));
        };
        match quoter.kind {
            V3QuoterKind::QuoterV2 => {
                let data = encode_quoter_v2_single(
                    QUOTER_V2_EXACT_OUTPUT_SINGLE_SIGNATURE,
                    token_in,
                    token_out,
                    amount_out,
                    self.fee_bps,
                );
                let call = TransactionRequest::contract_call(
                    quoter.address.clone(),
                    data,
                    MAINNET_CHAIN_ID,
                );
                let raw = client
                    .call(&call, BlockId::Latest)
                    .await
                    .map_err(|e| PricingError::ChainCall(e.to_string()))?;
                decode_quoter_v2_amount(&raw)
            }
        }
    }

    /// Fetches full pool state and token metadata from an on-chain V3 contract.
    pub async fn from_chain(address: Address, client: &ChainClient) -> PricingResult<Self> {
        let call = |data: Vec<u8>| {
            TransactionRequest::contract_call(address.clone(), data, MAINNET_CHAIN_ID)
        };

        let slot0_raw = client
            .call(&call(SLOT0_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;

        if slot0_raw.len() < SLOT0_RETURN_MIN {
            return Err(PricingError::AbiDecode(format!(
                "slot0 returned {} bytes, expected {SLOT0_RETURN_MIN}",
                slot0_raw.len()
            )));
        }

        let sqrt_price_x96 = U256::from_big_endian(&slot0_raw[0..EVM_WORD_LEN]);
        let tick_raw = decode_i24_from_slot(&slot0_raw[EVM_WORD_LEN..EVM_WORD_LEN * 2])?;

        let liquidity_raw = client
            .call(&call(LIQUIDITY_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;

        let liquidity = if liquidity_raw.len() >= EVM_WORD_LEN {
            decode_u128_from_slot(&liquidity_raw[0..EVM_WORD_LEN])?
        } else {
            return Err(PricingError::AbiDecode(format!(
                "liquidity returned {} bytes, expected {EVM_WORD_LEN}",
                liquidity_raw.len()
            )));
        };

        let fee_raw = client
            .call(&call(FEE_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;

        let fee_24bit = if fee_raw.len() >= EVM_WORD_LEN {
            let bytes: [u8; EVM_WORD_LEN] = fee_raw[..EVM_WORD_LEN]
                .try_into()
                .map_err(|_| PricingError::AbiDecode("fee slot conversion".into()))?;
            U256::from_big_endian(&bytes).as_u64() as u32
        } else {
            return Err(PricingError::AbiDecode(format!(
                "fee returned {} bytes, expected {EVM_WORD_LEN}",
                fee_raw.len()
            )));
        };
        let fee_bps = fee_24bit;

        let token0_raw = client
            .call(&call(TOKEN0_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;
        let token0_addr = decode_address_from_slot(&token0_raw)?;

        let token1_raw = client
            .call(&call(TOKEN1_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;
        let token1_addr = decode_address_from_slot(&token1_raw)?;

        let zero_value = TokenAmount::eth(0u64);
        let token0 = fetch_token_metadata(&token0_addr, client, zero_value.clone()).await?;
        let token1 = fetch_token_metadata(&token1_addr, client, zero_value).await?;

        Self::new(
            address,
            token0,
            token1,
            fee_bps,
            sqrt_price_x96,
            liquidity,
            tick_raw,
        )
    }

    /// Fetches only `(sqrt_price_x96, liquidity, tick)` from an on-chain V3 pool.
    pub async fn fetch_state(
        address: &Address,
        client: &ChainClient,
    ) -> PricingResult<(U256, u128, i32)> {
        let call = |data: Vec<u8>| {
            TransactionRequest::contract_call(address.clone(), data, MAINNET_CHAIN_ID)
        };

        let slot0_raw = client
            .call(&call(SLOT0_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;

        if slot0_raw.len() < SLOT0_RETURN_MIN {
            return Err(PricingError::AbiDecode(format!(
                "slot0 returned {} bytes, expected {SLOT0_RETURN_MIN}",
                slot0_raw.len()
            )));
        }

        let sqrt_price_x96 = U256::from_big_endian(&slot0_raw[0..EVM_WORD_LEN]);
        let tick = decode_i24_from_slot(&slot0_raw[EVM_WORD_LEN..EVM_WORD_LEN * 2])?;

        let liquidity_raw = client
            .call(&call(LIQUIDITY_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;

        let liquidity = if liquidity_raw.len() >= EVM_WORD_LEN {
            decode_u128_from_slot(&liquidity_raw[0..EVM_WORD_LEN])?
        } else {
            0
        };

        Ok((sqrt_price_x96, liquidity, tick))
    }
}

fn encode_quoter_v2_single(
    signature: &str,
    token_in: &Token,
    token_out: &Token,
    amount: u128,
    fee: u32,
) -> Vec<u8> {
    let selector = id(signature);
    let mut data = Vec::with_capacity(4 + 160);
    data.extend_from_slice(&selector[..4]);
    data.extend_from_slice(&ethers::abi::encode(&[AbiToken::Tuple(vec![
        AbiToken::Address(token_in.address.as_eth_address()),
        AbiToken::Address(token_out.address.as_eth_address()),
        AbiToken::Uint(U256::from(amount)),
        AbiToken::Uint(U256::from(fee)),
        AbiToken::Uint(U256::zero()),
    ])]));
    data
}

fn decode_quoter_v2_amount(raw: &[u8]) -> PricingResult<u128> {
    let decoded = ethers::abi::decode(&QUOTER_V2_RETURN_TYPES, raw)
        .map_err(|e| PricingError::AbiDecode(format!("decode QuoterV2 response: {e}")))?;
    decoded
        .first()
        .and_then(|token| token.clone().into_uint())
        .map(|value| value.as_u128())
        .ok_or_else(|| PricingError::AbiDecode("QuoterV2 response missing amount".into()))
}

fn decode_i24_from_slot(slot: &[u8]) -> PricingResult<i32> {
    if slot.len() != EVM_WORD_LEN {
        return Err(PricingError::AbiDecode(format!(
            "int24 slot returned {} bytes, expected {EVM_WORD_LEN}",
            slot.len()
        )));
    }
    let raw = ((slot[29] as i32) << 16) | ((slot[30] as i32) << 8) | slot[31] as i32;
    if raw & 0x80_0000 != 0 {
        Ok(raw | !0xff_ffff)
    } else {
        Ok(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Address;

    fn mock_token(symbol: &str, addr_hex: &str, decimals: u8) -> Token {
        Token {
            address: Address::new(addr_hex).unwrap(),
            symbol: symbol.to_string(),
            decimals,
        }
    }

    fn mock_v3_pool(tick: i32, liquidity: u128, fee_bps: u32) -> UniswapV3Pool {
        let weth = mock_token("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18);
        let usdc = mock_token("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6);
        let sqrt_price = math::get_sqrt_ratio_at_tick(tick).unwrap();

        UniswapV3Pool::new(
            Address::new("0x0000000000000000000000000000000000000001").unwrap(),
            weth,
            usdc,
            fee_bps,
            sqrt_price,
            liquidity,
            tick,
        )
        .unwrap()
    }

    #[test]
    fn test_v3_pool_spot_price_at_tick_zero() {
        let pool = mock_v3_pool(0, 1_000_000, 3000);
        let price = pool.get_spot_price(&pool.token0).unwrap();
        assert!(price > Decimal::ZERO, "price should be positive");
    }

    #[test]
    fn test_v3_pool_spot_price_matches_formula() {
        let pool = mock_v3_pool(0, 1_000_000, 3000);
        let sqrt_p = pool.sqrt_price_x96;
        let q96 = math::q96();
        let prec = U256::from(10_000_000_000_000_000_000u128);

        let ratio_scaled = (sqrt_p * prec / q96).as_u128();
        let ratio = Decimal::from(ratio_scaled) / Decimal::from(10_000_000_000_000_000_000u128);
        let expected_price = ratio * ratio;

        let spot = pool.get_spot_price(&pool.token0).unwrap();
        let scale_in = Decimal::from(DECIMAL_BASE.pow(pool.token0.decimals as u32));
        let scale_out = Decimal::from(DECIMAL_BASE.pow(pool.token1.decimals as u32));

        assert_eq!(
            spot.round_dp(12),
            (expected_price * scale_in / scale_out).round_dp(12)
        );
    }
    #[test]
    fn test_v3_pool_spot_price_realistic() {
        // Tick for ~2300 USDC/ETH (Token0=WETH, Token1=USDC)
        // P_raw = 2300 * 10^6 / 10^18 = 2.3 * 10^-9
        // tick = log(2.3e-9) / log(1.0001) ~= -198900
        let tick = -198900;
        let pool = mock_v3_pool(tick, 10u128.pow(18), 500);
        let spot = pool.get_spot_price(&pool.token0).unwrap();

        // Spot should be around 2300
        assert!(spot > Decimal::from(2000u64));
        assert!(spot < Decimal::from(3000u64));
    }

    #[test]
    fn test_v3_pool_quote_swap_within_range() {
        let pool = mock_v3_pool(0, 10u128.pow(18), 3000);
        let small_amount = 1_000_000u128;

        let quote = pool.quote_swap(small_amount, &pool.token0).unwrap();
        assert!(quote.amount_out > 0, "should produce some output");
        assert!(!quote.is_partial, "small swap should not be partial");
    }

    #[test]
    fn test_v3_pool_quote_swap_zero_fails() {
        let pool = mock_v3_pool(0, 10u128.pow(18), 3000);
        assert!(pool.quote_swap(0, &pool.token0).is_err());
    }

    #[test]
    fn test_v3_pool_quote_swap_unknown_token() {
        let pool = mock_v3_pool(0, 10u128.pow(18), 3000);
        let unknown = mock_token("FOO", "0x00000000000000000000000000000000000000ff", 18);
        assert!(pool.quote_swap(1000, &unknown).is_err());
    }

    #[test]
    fn test_v3_pool_fee_tier_tick_spacing() {
        assert_eq!(tick::fee_tier_to_tick_spacing(100), 1);
        assert_eq!(tick::fee_tier_to_tick_spacing(500), 10);
        assert_eq!(tick::fee_tier_to_tick_spacing(3000), 60);
        assert_eq!(tick::fee_tier_to_tick_spacing(10000), 200);
    }

    #[test]
    fn test_v3_gas_estimate_lower_than_v2() {
        let v2_gas = 150_000 + 100_000;
        let v3_gas = UniswapV3Pool::estimate_gas(1);
        assert!(
            v3_gas < v2_gas,
            "V3 gas ({v3_gas}) should be lower than V2 ({v2_gas})"
        );
    }

    #[test]
    fn test_v3_pool_quote_swap_crosses_tick() {
        let pool = mock_v3_pool(0, 1_000u128, 3000);
        let huge_amount = 10u128.pow(22);

        let quote = pool.quote_swap(huge_amount, &pool.token0).unwrap();
        assert!(quote.amount_out > 0, "should produce some output");
    }

    #[tokio::test]
    async fn test_v3_pool_from_chain_compiles() {
        let client = ChainClient::new(vec!["http://127.0.0.1:1".to_string()], 1, 0).unwrap();
        let addr = Address::new("0x0000000000000000000000000000000000000001").unwrap();

        let result = UniswapV3Pool::from_chain(addr, &client).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_v3_pool_new_computes_tick_spacing() {
        let pool = mock_v3_pool(0, 1_000_000, 3000);
        assert_eq!(pool.tick_spacing, 60);
    }

    #[test]
    fn test_v3_pool_new_tick_spacing_500() {
        let pool = mock_v3_pool(0, 1_000_000, 500);
        assert_eq!(pool.tick_spacing, 10);
    }

    #[test]
    fn test_v3_pool_new_tick_spacing_10000() {
        let pool = mock_v3_pool(0, 1_000_000, 10000);
        assert_eq!(pool.tick_spacing, 200);
    }

    #[test]
    fn test_v3_pool_quote_swap_zero_liquidity_returns_zero() {
        let pool = mock_v3_pool(0, 0, 3000);
        let quote = pool.quote_swap(1_000_000, &pool.token0).unwrap();
        assert_eq!(quote.amount_out, 0);
    }

    #[test]
    fn test_v3_pool_token_out_for_returns_correct_token() {
        let pool = mock_v3_pool(0, 1_000_000, 3000);
        assert_eq!(*pool.token_out_for(&pool.token0).unwrap(), pool.token1);
        assert_eq!(*pool.token_out_for(&pool.token1).unwrap(), pool.token0);
    }

    #[test]
    fn test_v3_pool_token_out_for_unknown_returns_error() {
        let pool = mock_v3_pool(0, 1_000_000, 3000);
        let unknown = mock_token("FOO", "0x00000000000000000000000000000000000000ff", 18);
        assert!(pool.token_out_for(&unknown).is_err());
    }

    #[tokio::test]
    async fn test_v3_fetch_state_fails_without_node() {
        let client = ChainClient::new(vec!["http://127.0.0.1:1".to_string()], 1, 0).unwrap();
        let addr = Address::new("0x0000000000000000000000000000000000000001").unwrap();
        let result = UniswapV3Pool::fetch_state(&addr, &client).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_v3_gas_estimate_increases_with_hops() {
        let gas1 = UniswapV3Pool::estimate_gas(1);
        let gas2 = UniswapV3Pool::estimate_gas(2);
        assert!(gas2 > gas1);
    }
}
