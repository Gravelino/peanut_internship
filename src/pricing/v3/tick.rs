use super::math;

/// Maps Uniswap V3 fee tier (in bps) to its tick spacing.
/// See: https://docs.uniswap.org/protocol/concepts/V3-overview/concentrated-liquidity#tick-spacing
pub fn fee_tier_to_tick_spacing(fee_bps: u32) -> i32 {
    match fee_bps {
        100 => 1,
        500 => 10,
        3000 => 60,
        10000 => 200,
        _ => 60, // default to 0.3% tier spacing for unknown fee tiers
    }
}

fn floor_div_tick(tick: i32, spacing: i32) -> i32 {
    let q = tick / spacing;
    if tick < 0 && tick % spacing != 0 {
        q - 1
    } else {
        q
    }
}

pub fn nearest_usable_tick(tick: i32, tick_spacing: i32) -> i32 {
    let min = math::min_tick();
    let max = math::max_tick();

    let spacing = tick_spacing.max(1);
    let compressed = floor_div_tick(tick, spacing);
    let rounded_down = compressed * spacing;
    let rounded_up = rounded_down + spacing;

    let result = if (tick - rounded_down).unsigned_abs() <= (tick - rounded_up).unsigned_abs() {
        rounded_down
    } else {
        rounded_up
    };

    if result < min {
        min + (min % spacing).unsigned_abs() as i32
    } else if result > max {
        max - (max % spacing).unsigned_abs() as i32
    } else {
        result
    }
}

pub fn next_tick_boundary(tick: i32, tick_spacing: i32, zero_for_one: bool) -> i32 {
    let spacing = tick_spacing.max(1);
    let compressed = floor_div_tick(tick, spacing);

    if zero_for_one {
        if tick % spacing == 0 {
            ((compressed - 1) * spacing).max(math::min_tick())
        } else {
            (compressed * spacing).max(math::min_tick())
        }
    } else {
        ((compressed + 1) * spacing).min(math::max_tick())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fee_tier_tick_spacing() {
        assert_eq!(fee_tier_to_tick_spacing(100), 1);
        assert_eq!(fee_tier_to_tick_spacing(500), 10);
        assert_eq!(fee_tier_to_tick_spacing(3000), 60);
        assert_eq!(fee_tier_to_tick_spacing(10000), 200);
    }

    #[test]
    fn test_fee_tier_unknown_defaults_to_60() {
        assert_eq!(fee_tier_to_tick_spacing(999), 60);
    }

    #[test]
    fn test_nearest_usable_tick_at_spacing_boundary() {
        assert_eq!(nearest_usable_tick(60, 60), 60);
        assert_eq!(nearest_usable_tick(120, 60), 120);
        assert_eq!(nearest_usable_tick(0, 60), 0);
    }

    #[test]
    fn test_nearest_usable_tick_rounds_down() {
        assert_eq!(nearest_usable_tick(59, 60), 60);
        assert_eq!(nearest_usable_tick(1, 60), 0);
        assert_eq!(nearest_usable_tick(61, 60), 60);
    }

    #[test]
    fn test_nearest_usable_tick_negative() {
        assert_eq!(nearest_usable_tick(-59, 60), -60);
        assert_eq!(nearest_usable_tick(-1, 60), 0);
        assert_eq!(nearest_usable_tick(-61, 60), -60);
    }

    #[test]
    fn test_next_tick_boundary_zero_for_one() {
        assert_eq!(next_tick_boundary(100, 60, true), 60);
        assert_eq!(next_tick_boundary(60, 60, true), 0);
        assert_eq!(next_tick_boundary(0, 60, true), -60);
    }

    #[test]
    fn test_next_tick_boundary_one_for_zero() {
        assert_eq!(next_tick_boundary(100, 60, false), 120);
        assert_eq!(next_tick_boundary(0, 60, false), 60);
        assert_eq!(next_tick_boundary(-60, 60, false), 0);
    }

    #[test]
    fn test_next_tick_boundary_small_spacing() {
        assert_eq!(next_tick_boundary(5, 1, true), 4);
        assert_eq!(next_tick_boundary(5, 1, false), 6);
    }

    #[test]
    fn test_next_tick_boundary_at_min_max() {
        let min = math::min_tick();
        let max = math::max_tick();
        let result = next_tick_boundary(min, 60, true);
        assert!(result >= min, "next boundary below min tick should clamp");
        let result = next_tick_boundary(max, 60, false);
        assert!(result <= max, "next boundary above max tick should clamp");
    }
}
