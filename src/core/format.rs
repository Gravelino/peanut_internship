use rust_decimal::Decimal;

/// Formats a USD value with $ prefix and 2-6 decimal places.
pub fn fmt_usd(value: Decimal) -> String {
    format!("${:.6}", value.trunc_with_scale(6))
}

/// Formats basis points (bps) with 2 decimal places.
pub fn fmt_bps(value: Decimal) -> String {
    format!("{:.2} bps", value)
}

/// Formats a quantity (size) with up to 6 decimal places.
pub fn fmt_qty(value: Decimal) -> String {
    format!("{:.6}", value.trunc_with_scale(6))
}

/// Formats a price with up to 8 decimal places.
pub fn fmt_price(value: Decimal) -> String {
    format!("{:.8}", value.trunc_with_scale(8))
}

/// Formats an optional USD value.
pub fn fmt_optional_usd(value: Option<Decimal>) -> String {
    value.map(fmt_usd).unwrap_or_else(|| "n/a".to_string())
}
