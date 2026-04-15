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

const FEE_PPM: u64 = 1_000_000;

pub fn q96() -> U256 {
    U256([0, 0x100000000, 0, 0])
}

pub fn min_sqrt_ratio() -> U256 {
    MIN_SQRT_RATIO
}

pub fn max_sqrt_ratio() -> U256 {
    MAX_SQRT_RATIO
}

pub fn min_tick() -> i32 {
    MIN_TICK
}

pub fn max_tick() -> i32 {
    MAX_TICK
}

fn full_mul(a: U256, b: U256) -> [u64; 8] {
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
    [
        p[0] as u64,
        p[1] as u64,
        p[2] as u64,
        p[3] as u64,
        p[4] as u64,
        p[5] as u64,
        p[6] as u64,
        p[7] as u64,
    ]
}

fn full_mul_shr_128(a: U256, b: U256) -> U256 {
    let p = full_mul(a, b);
    U256([p[2], p[3], p[4], p[5]])
}

fn hex(s: &str) -> U256 {
    U256::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
}

fn mul_div_round_up(a: U256, b: U256, d: U256, label: &'static str) -> PricingResult<U256> {
    if d.is_zero() {
        return Err(PricingError::ArithmeticOverflow(label));
    }
    if d == U256_ONE {
        let p = full_mul(a, b);
        let lo_nonzero = p[0] != 0 || p[1] != 0;
        let hi = U256([p[2], p[3], p[4], p[5]]);
        Ok(if lo_nonzero { hi + U256_ONE } else { hi })
    } else if b == U256_ONE {
        let (q, r) = a.div_mod(d);
        Ok(if r.is_zero() { q } else { q + U256_ONE })
    } else if a == U256_ONE {
        let (q, r) = b.div_mod(d);
        Ok(if r.is_zero() { q } else { q + U256_ONE })
    } else {
        let p = full_mul(a, b);
        let q = div512_256(&p, &d);
        let qd = full_mul(q, d);
        let has_rem = p != qd;
        Ok(if has_rem { q + U256_ONE } else { q })
    }
}

fn div512_256(dividend: &[u64; 8], divisor: &U256) -> U256 {
    if divisor.is_zero() {
        return U256::zero();
    }
    let d0 = divisor.0[0] as u128;
    let d1 = divisor.0[1] as u128;
    if divisor.0[2] == 0 && divisor.0[3] == 0 {
        let d = d0 | (d1 << 64);
        if d == 0 {
            return U256::zero();
        }
        let mut rem = 0u128;
        let mut q = [0u64; 8];
        for i in (0..8).rev() {
            let cur = (rem << 64) | (dividend[i] as u128);
            q[i] = (cur / d) as u64;
            rem = cur % d;
        }
        return U256([q[0], q[1], q[2], q[3]]);
    }
    let mut result = U256::zero();
    let mut remainder = U256::zero();
    for i in (0..8).rev() {
        remainder <<= 64;
        remainder |= U256::from(dividend[i]);
        let digit = remainder / *divisor;
        remainder -= digit * *divisor;
        if i < 4 {
            result |= digit << (i * 64);
        }
    }
    result
}

fn shr_round_up(val: U256, n: usize, _label: &str) -> PricingResult<U256> {
    let result = val >> n;
    let mask = (U256_ONE << n) - U256_ONE;
    let should_round = (val & mask) > U256::zero();
    Ok(if should_round {
        result + U256_ONE
    } else {
        result
    })
}

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

fn get_amount0_delta(a: U256, b: U256, liq: u128, up: bool) -> PricingResult<u128> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let l = U256::from(liq);
    let num1 = l << 96;
    let num = if up {
        mul_div_round_up(num1, hi - lo, hi, "a0_num")?
    } else {
        num1 * (hi - lo) / hi
    };
    let r = if up {
        shr_round_up(num, 96, "a0_shr")?
    } else {
        num >> 96
    };
    Ok(r.as_u128())
}

fn get_amount1_delta(a: U256, b: U256, liq: u128, up: bool) -> PricingResult<u128> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let l = U256::from(liq);
    let q = q96();
    let r = if up {
        mul_div_round_up(l, hi - lo, q, "a1_up")?
    } else {
        l * (hi - lo) / q
    };
    Ok(r.as_u128())
}

pub fn get_amount0_delta_unsigned(a: U256, b: U256, liq: u128) -> PricingResult<u128> {
    if liq == 0 {
        return Err(PricingError::ZeroAmountIn);
    }
    get_amount0_delta(a, b, liq, false)
}

pub fn get_amount1_delta_unsigned(a: U256, b: U256, liq: u128) -> PricingResult<u128> {
    if liq == 0 {
        return Err(PricingError::ZeroAmountIn);
    }
    get_amount1_delta(a, b, liq, false)
}

#[derive(Debug, Clone)]
pub struct SwapStepResult {
    pub sqrt_ratio_next: U256,
    pub amount_in: u128,
    pub amount_out: u128,
    pub fee_amount: u128,
}

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
    let l = U256::from(liq);
    let n1 = l << 96;
    if add {
        let prod = n1
            .checked_mul(cur)
            .ok_or(PricingError::ArithmeticOverflow("ns0"))?;
        let den = n1 + U256::from(amt) * cur;
        Ok(prod / den)
    } else {
        let prod = n1
            .checked_mul(cur)
            .ok_or(PricingError::ArithmeticOverflow("ns0"))?;
        let den = n1 - U256::from(amt) * cur;
        if den.is_zero() {
            return Err(PricingError::ArithmeticOverflow("ns0_z"));
        }
        Ok(prod / den)
    }
}

fn next_sqrt1(cur: U256, liq: u128, amt: u128, add: bool) -> PricingResult<U256> {
    if add {
        Ok(cur + (U256::from(amt) << 96) / U256::from(liq))
    } else {
        let d = (U256::from(amt) << 96) / U256::from(liq);
        if d >= cur {
            return Err(PricingError::ArithmeticOverflow("ns1"));
        }
        Ok(cur - d)
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
