use ethers::types::U256;
use rust_decimal::Decimal;

use super::math;
use super::tick;
use crate::chain::client::ChainClient;
use crate::core::types::{
    Address, BlockId, DECIMAL_BASE, MAINNET_CHAIN_ID, Token, TokenAmount,
    TransactionRequest,
};
use crate::pricing::amm::{decode_address_from_slot, decode_u128_from_slot, fetch_token_metadata};
use crate::pricing::errors::{PricingError, PricingResult};

const EVM_WORD_LEN: usize = 32;

const SLOT0_SELECTOR: [u8; 4] = [0x38, 0x50, 0xc7, 0xb6];
const LIQUIDITY_SELECTOR: [u8; 4] = [0x1a, 0x68, 0x66, 0x50];
const FEE_SELECTOR: [u8; 4] = [0xdd, 0xca, 0x3f, 0x43];
const TOKEN0_SELECTOR: [u8; 4] = [0x0d, 0xfe, 0x16, 0x81];
const TOKEN1_SELECTOR: [u8; 4] = [0xd2, 0x12, 0x20, 0xa7];

const SLOT0_RETURN_MIN: usize = EVM_WORD_LEN * 3;

const MAX_SWAP_STEPS: usize = 2;

const V3_BASE_GAS: u128 = 80_000;
const V3_GAS_PER_HOP: u128 = 60_000;

pub fn v3_base_gas() -> u128 {
    V3_BASE_GAS
}

pub fn v3_gas_per_hop() -> u128 {
    V3_GAS_PER_HOP
}

#[derive(Debug, Clone)]
pub struct UniswapV3Pool {
    pub address: Address,
    pub token0: Token,
    pub token1: Token,
    pub fee_bps: u32,
    pub tick_spacing: i32,
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
}

#[derive(Debug, Clone)]
pub struct V3SwapQuote {
    pub amount_in: u128,
    pub amount_out: u128,
    pub sqrt_price_after: U256,
    pub tick_after: i32,
    pub gas_estimate: u64,
    pub is_partial: bool,
}

impl UniswapV3Pool {
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

    pub fn token_out_for<'a>(&'a self, token_in: &Token) -> PricingResult<&'a Token> {
        if *token_in == self.token0 {
            Ok(&self.token1)
        } else if *token_in == self.token1 {
            Ok(&self.token0)
        } else {
            Err(PricingError::UnknownToken(token_in.symbol.clone()))
        }
    }

    pub fn get_spot_price(&self, token_in: &Token) -> PricingResult<Decimal> {
        let token_out = self.token_out_for(token_in)?;

        let sqrt_price = self.sqrt_price_x96;
        let q96 = math::q96();
        let prec = U256::from(10_000_000_000_000_000_000u128);

        let ratio_scaled = (sqrt_price * prec / q96).as_u128();
        let ratio = Decimal::from(ratio_scaled) / Decimal::from(10_000_000_000_000_000_000u128);
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
                current_tick = math::get_tick_at_sqrt_ratio(sqrt_price_current).unwrap_or(current_tick);
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

    pub fn estimate_gas(num_hops: usize) -> u128 {
        V3_BASE_GAS + V3_GAS_PER_HOP * (num_hops as u128)
    }

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
        let tick_i256 = U256::from_big_endian(&slot0_raw[EVM_WORD_LEN..EVM_WORD_LEN * 2]);
        let tick_raw = tick_i256.as_u128() as i32;

        let liquidity_raw = client
            .call(&call(LIQUIDITY_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;

        let liquidity = if liquidity_raw.len() >= EVM_WORD_LEN {
            decode_u128_from_slot(&liquidity_raw[0..EVM_WORD_LEN])?
        } else {
            0
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
            3000
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

        Self::new(address, token0, token1, fee_bps, sqrt_price_x96, liquidity, tick_raw)
    }

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
        let tick_i256 = U256::from_big_endian(&slot0_raw[EVM_WORD_LEN..EVM_WORD_LEN * 2]);
        let tick = tick_i256.as_u128() as i32;

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

        assert_eq!(spot.round_dp(12), (expected_price * scale_in / scale_out).round_dp(12));
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
        assert!(v3_gas < v2_gas, "V3 gas ({v3_gas}) should be lower than V2 ({v2_gas})");
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
        let client =
            ChainClient::new(vec!["http://127.0.0.1:1".to_string()], 1, 0).unwrap();
        let addr = Address::new("0x0000000000000000000000000000000000000001").unwrap();

        let result = UniswapV3Pool::from_chain(addr, &client).await;
        assert!(result.is_err());
    }
}
