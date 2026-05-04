use ethers::types::U256;

use crate::pricing::errors::{PricingError, PricingResult};

const U256_ONE: U256 = U256([1, 0, 0, 0]);

const MIN_TICK: i32 = -887272;
const MAX_TICK: i32 = 887272;

const MIN_SQRT_RATIO: U256 = U256([4295128739, 0, 0, 0]);
const MAX_SQRT_RATIO: U256 = U256([
    158556616351758596,
    16871241472139834863,
    8689426018648659796,
    288,
]);

/// Fee scale in parts-per-million used by Uniswap V3 swap step math.
/// 1_000_000 PPM = 100% fee; actual fee = fee_bps * FEE_PPM / 10_000.
const FEE_PPM: u64 = 1_000_000;

/// Returns 2^96 as a U256, the Q64.96 fixed-point scale factor.
pub fn q96() -> U256 {
    U256([0, 0x100000000, 0, 0])
}

/// Returns the minimum valid sqrt price ratio for a V3 pool.
pub fn min_sqrt_ratio() -> U256 {
    MIN_SQRT_RATIO
}

/// Returns the maximum valid sqrt price ratio for a V3 pool.
pub fn max_sqrt_ratio() -> U256 {
    MAX_SQRT_RATIO
}

/// Returns the minimum tick index for a V3 pool (-887272).
pub fn min_tick() -> i32 {
    MIN_TICK
}

/// Returns the maximum tick index for a V3 pool (887272).
pub fn max_tick() -> i32 {
    MAX_TICK
}

fn full_mul_shr_128(a: U256, b: U256) -> U256 {
    let a_l = a.0;
    let b_l = b.0;
    let mut p = [0u128; 8];
    for i in 0..4 {
        let mut carry = 0u128;
        for j in 0..4 {
            let v = (a_l[i] as u128) * (b_l[j] as u128) + p[i + j] + carry;
            p[i + j] = v & u64::MAX as u128;
            carry = v >> 64;
        }
        p[i + 4] += carry;
    }
    U256([p[2] as u64, p[3] as u64, p[4] as u64, p[5] as u64])
}

fn hex(s: &str) -> U256 {
    U256::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
}

/// Computes the sqrt price ratio (Q64.96) at the given tick index.
pub fn get_sqrt_ratio_at_tick(tick: i32) -> PricingResult<U256> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return Err(PricingError::AbiDecode(format!(
            "tick {tick} out of range [{MIN_TICK},{MAX_TICK}]"
        )));
    }
    let abs = tick.unsigned_abs();

    let mut ratio = if (abs & 1) != 0 {
        hex("0xfffcb933bd6fad37aa2d162d1a594001")
    } else {
        hex("0x100000000000000000000000000000000")
    };

    if (abs >> 1) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xfff97272373d413259a46990580e213a"));
    }
    if (abs >> 2) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xfff2e50f5f656932ef12357cf3c7fdcc"));
    }
    if (abs >> 3) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xffe5caca7e10e4e61c3624eaa0941cd0"));
    }
    if (abs >> 4) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xffcb9843d60f6159c9db58835c926644"));
    }
    if (abs >> 5) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xff973b41fa98c081472e6896dfb254c0"));
    }
    if (abs >> 6) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xff2ea16466c96a3843ec78b326b52861"));
    }
    if (abs >> 7) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xfe5dee046a99a2a811c461f1969c3053"));
    }
    if (abs >> 8) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xfcbe86c7900a88aedcffc83b479aa3a4"));
    }
    if (abs >> 9) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xf987a7253ac413176f2b074cf7815e54"));
    }
    if (abs >> 10) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xf3392b0822b70005940c7a398e4b70f3"));
    }
    if (abs >> 11) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xe7159475a2c29b7443b29c7fa6e889d9"));
    }
    if (abs >> 12) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xd097f3bdfd2022b8845ad8f792aa5825"));
    }
    if (abs >> 13) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0xa9f746462d870fdf8a65dc1f90e061e5"));
    }
    if (abs >> 14) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0x70d869a156d2a1b890bb3df62baf32f7"));
    }
    if (abs >> 15) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0x31be135f97d08fd981231505542fcfa6"));
    }
    if (abs >> 16) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0x9aa508b5b7a84e1c677de54f3e99bc9"));
    }
    if (abs >> 17) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0x5d6af8dedb81196699c329225ee604"));
    }
    if (abs >> 18) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0x2216e584f5fa1ea926041bedfe98"));
    }
    if (abs >> 19) & 1 != 0 {
        ratio = full_mul_shr_128(ratio, hex("0x48a170391f7dc42444e8fa2"));
    }

    if tick > 0 {
        ratio = U256::MAX / ratio;
    }

    let result = (ratio >> 32) + ((ratio >> 31) & U256_ONE);
    if result > MAX_SQRT_RATIO {
        return Err(PricingError::AbiDecode("sqrt ratio exceeds max".into()));
    }
    if result < MIN_SQRT_RATIO {
        Ok(MIN_SQRT_RATIO)
    } else {
        Ok(result)
    }
}

/// Returns the tick index corresponding to the given sqrt price ratio (Q64.96).
pub fn get_tick_at_sqrt_ratio(sqrt_price: U256) -> PricingResult<i32> {
    if sqrt_price < MIN_SQRT_RATIO || sqrt_price >= MAX_SQRT_RATIO {
        return Err(PricingError::AbiDecode("sqrtPrice out of range".into()));
    }
    let mut lo = MIN_TICK;
    let mut hi = MAX_TICK;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let mid_sqrt = get_sqrt_ratio_at_tick(mid)?;
        if mid_sqrt <= sqrt_price {
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    Ok(hi)
}

use primitive_types::U512;

fn to_u512(x: U256) -> U512 {
    let mut bytes = [0u8; 32];
    x.to_little_endian(&mut bytes);
    U512::from_little_endian(&bytes)
}

fn from_u512(x: U512) -> U256 {
    let bytes = x.to_little_endian();
    U256::from_little_endian(&bytes[0..32])
}

fn get_amount0_delta(a: U256, b: U256, liq: u128, up: bool) -> PricingResult<u128> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    if lo.is_zero() {
        return Ok(0);
    }
    let l = U512::from(liq);
    let diff = to_u512(hi - lo);
    let lo_512 = to_u512(lo);
    let hi_512 = to_u512(hi);

    // amount0 = (L * 2^96 * (hi - lo)) / (hi * lo)
    let num = (l << 96) * diff;
    let den = hi_512 * lo_512;

    let res = if up { (num + den - 1) / den } else { num / den };
    Ok(from_u512(res).as_u128())
}

fn get_amount1_delta(a: U256, b: U256, liq: u128, up: bool) -> PricingResult<u128> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let l = U512::from(liq);
    let diff = to_u512(hi - lo);
    let q = U512::from(1) << 96;

    let num = l * diff;
    let res = if up { (num + q - 1) / q } else { num / q };
    Ok(from_u512(res).as_u128())
}

/// Computes the amount of token0 between two sqrt prices for a given liquidity (unsigned, rounded down).
pub fn get_amount0_delta_unsigned(a: U256, b: U256, liq: u128) -> PricingResult<u128> {
    if liq == 0 {
        return Err(PricingError::ZeroAmountIn);
    }
    get_amount0_delta(a, b, liq, false)
}

/// Computes the amount of token1 between two sqrt prices for a given liquidity (unsigned, rounded down).
pub fn get_amount1_delta_unsigned(a: U256, b: U256, liq: u128) -> PricingResult<u128> {
    if liq == 0 {
        return Err(PricingError::ZeroAmountIn);
    }
    get_amount1_delta(a, b, liq, false)
}

/// Result of a single V3 swap step computation.
#[derive(Debug, Clone)]
pub struct SwapStepResult {
    /// Sqrt price after this step (Q64.96).
    pub sqrt_ratio_next: U256,
    /// Input amount consumed in this step.
    pub amount_in: u128,
    /// Output amount produced in this step.
    pub amount_out: u128,
    /// Fee amount charged in this step.
    pub fee_amount: u128,
}

/// Computes a single swap step: moves price toward `tgt`, consuming up to `rem` input with `fee` BPS.
pub fn compute_swap_step(
    cur: U256,
    tgt: U256,
    liq: u128,
    rem: u128,
    fee: u32,
) -> PricingResult<SwapStepResult> {
    let zfo = cur >= tgt;
    let fp = fee as u64;

    let (next, ain, aout) = if zfo {
        let ain = get_amount0_delta(tgt, cur, liq, true)?;
        if U256::from(rem) >= U256::from(ain) {
            (tgt, ain, get_amount1_delta(tgt, cur, liq, false)?)
        } else {
            let less = U256::from(rem)
                .checked_mul(U256::from(FEE_PPM - fp))
                .ok_or(PricingError::ArithmeticOverflow("step_fee"))?
                / U256::from(FEE_PPM);
            let nx = next_sqrt0(cur, liq, less.as_u128(), true)?;
            (nx, rem, get_amount1_delta(nx, cur, liq, false)?)
        }
    } else {
        let ain = get_amount1_delta(cur, tgt, liq, true)?;
        if U256::from(rem) >= U256::from(ain) {
            (tgt, ain, get_amount0_delta(cur, tgt, liq, false)?)
        } else {
            let less = U256::from(rem)
                .checked_mul(U256::from(FEE_PPM - fp))
                .ok_or(PricingError::ArithmeticOverflow("step_fee"))?
                / U256::from(FEE_PPM);
            let nx = next_sqrt1(cur, liq, less.as_u128(), true)?;
            (nx, rem, get_amount0_delta(cur, nx, liq, false)?)
        }
    };

    let famt = if fee == 0 {
        0
    } else if ain < rem {
        rem - ain
    } else {
        (U256::from(ain) * U256::from(fp) / U256::from(FEE_PPM - fp)).as_u128()
    };

    Ok(SwapStepResult {
        sqrt_ratio_next: next,
        amount_in: ain,
        amount_out: aout,
        fee_amount: famt,
    })
}

fn next_sqrt0(cur: U256, liq: u128, amt: u128, add: bool) -> PricingResult<U256> {
    if amt == 0 {
        return Ok(cur);
    }
    let l = U512::from(liq);
    let cur_512 = to_u512(cur);
    let amt_512 = U512::from(amt);
    let n1 = l << 96;

    if add {
        // next = (L * 2^96 * cur) / (L * 2^96 + amt * cur)
        let num = n1 * cur_512;
        let den = n1 + amt_512 * cur_512;
        Ok(from_u512(num / den))
    } else {
        // next = (L * 2^96 * cur) / (L * 2^96 - amt * cur)
        let num = n1 * cur_512;
        let den = n1 - amt_512 * cur_512;
        if den.is_zero() {
            return Err(PricingError::ArithmeticOverflow("ns0_z"));
        }
        Ok(from_u512(num / den))
    }
}

fn next_sqrt1(cur: U256, liq: u128, amt: u128, add: bool) -> PricingResult<U256> {
    let cur_512 = to_u512(cur);
    let amt_512 = U512::from(amt);
    let l = U512::from(liq);

    if add {
        // next = cur + (amt * 2^96) / L
        Ok(from_u512(cur_512 + (amt_512 << 96) / l))
    } else {
        // next = cur - (amt * 2^96) / L
        let d = (amt_512 << 96) / l;
        if d >= cur_512 {
            return Err(PricingError::ArithmeticOverflow("ns1"));
        }
        Ok(from_u512(cur_512 - d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqrt_min_tick() {
        assert!(get_sqrt_ratio_at_tick(MIN_TICK).unwrap() >= MIN_SQRT_RATIO);
    }

    #[test]
    fn test_sqrt_max_tick() {
        assert!(get_sqrt_ratio_at_tick(MAX_TICK).unwrap() <= MAX_SQRT_RATIO);
    }

    #[test]
    fn test_sqrt_tick_zero() {
        assert_eq!(get_sqrt_ratio_at_tick(0).unwrap(), q96());
    }

    #[test]
    fn test_tick_roundtrip() {
        for t in [-200, -100, -1, 0, 1, 100, 200] {
            let s = get_sqrt_ratio_at_tick(t).unwrap();
            let r = get_tick_at_sqrt_ratio(s).unwrap();
            assert!((r - t).unsigned_abs() <= 1, "tick {t} -> {r}");
        }
    }

    #[test]
    fn test_tick_oob() {
        assert!(get_tick_at_sqrt_ratio(MIN_SQRT_RATIO - U256_ONE).is_err());
        assert!(get_tick_at_sqrt_ratio(MAX_SQRT_RATIO).is_err());
    }

    #[test]
    fn test_a0_sym() {
        let a = get_sqrt_ratio_at_tick(-100).unwrap();
        let b = get_sqrt_ratio_at_tick(100).unwrap();
        assert_eq!(
            get_amount0_delta_unsigned(a, b, 1e9 as u128).unwrap(),
            get_amount0_delta_unsigned(b, a, 1e9 as u128).unwrap()
        );
    }

    #[test]
    fn test_a1_sym() {
        let a = get_sqrt_ratio_at_tick(-100).unwrap();
        let b = get_sqrt_ratio_at_tick(100).unwrap();
        assert_eq!(
            get_amount1_delta_unsigned(a, b, 1e9 as u128).unwrap(),
            get_amount1_delta_unsigned(b, a, 1e9 as u128).unwrap()
        );
    }

    #[test]
    fn test_delta_zero_liq() {
        let a = get_sqrt_ratio_at_tick(-10).unwrap();
        let b = get_sqrt_ratio_at_tick(10).unwrap();
        assert!(get_amount0_delta_unsigned(a, b, 0).is_err());
        assert!(get_amount1_delta_unsigned(a, b, 0).is_err());
    }

    #[test]
    fn test_step_zfo() {
        let c = get_sqrt_ratio_at_tick(0).unwrap();
        let t = get_sqrt_ratio_at_tick(-60).unwrap();
        let r = compute_swap_step(c, t, 1_000_000_000_000_000_000, 1_000_000_000, 3000).unwrap();
        assert!(r.amount_out > 0);
        assert!(r.sqrt_ratio_next <= c);
    }

    #[test]
    fn test_step_ofz() {
        let c = get_sqrt_ratio_at_tick(0).unwrap();
        let t = get_sqrt_ratio_at_tick(60).unwrap();
        let r = compute_swap_step(c, t, 1_000_000_000_000_000_000, 1_000_000_000, 3000).unwrap();
        assert!(r.amount_out > 0);
        assert!(r.sqrt_ratio_next >= c);
    }

    #[test]
    fn test_step_tiny() {
        let c = get_sqrt_ratio_at_tick(0).unwrap();
        let t = get_sqrt_ratio_at_tick(-60).unwrap();
        let r = compute_swap_step(c, t, 1_000_000_000_000_000_000, 100, 3000).unwrap();
        assert!(r.sqrt_ratio_next > t);
        assert!(r.sqrt_ratio_next < c);
    }

    #[test]
    fn test_step_huge() {
        let c = get_sqrt_ratio_at_tick(0).unwrap();
        let t = get_sqrt_ratio_at_tick(-60).unwrap();
        let r = compute_swap_step(c, t, 1_000, 1_000_000_000_000_000_000_000_000_000_000, 3000)
            .unwrap();
        assert_eq!(r.sqrt_ratio_next, t);
    }

    #[test]
    fn test_step_no_fee() {
        let c = get_sqrt_ratio_at_tick(0).unwrap();
        let t = get_sqrt_ratio_at_tick(-60).unwrap();
        let r = compute_swap_step(c, t, 1_000_000_000_000_000_000, 1_000_000_000, 0).unwrap();
        assert_eq!(r.fee_amount, 0);
    }

    #[test]
    fn test_sqrt_oob() {
        assert!(get_sqrt_ratio_at_tick(MIN_TICK - 1).is_err());
        assert!(get_sqrt_ratio_at_tick(MAX_TICK + 1).is_err());
    }
}
