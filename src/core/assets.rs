/// Returns the canonical symbol for a given asset.
/// Primarily used to map native ETH to WETH for consistency across CEX/DEX,
/// and to handle L2 variations like USDC.e.
pub fn canonicalize_asset(asset: &str) -> String {
    match asset.to_uppercase().as_str() {
        "ETH" => "WETH".to_string(),
        "USDC.E" => "USDC".to_string(),
        _ => asset.to_uppercase(),
    }
}

/// Returns a list of equivalent asset symbols.
/// Used for inventory tracking to treat native and wrapped versions as the same pool.
pub fn get_equivalent_assets(asset: &str) -> Vec<String> {
    match asset.to_uppercase().as_str() {
        "ETH" | "WETH" => vec!["ETH".to_string(), "WETH".to_string()],
        "USDC" | "USDC.E" => vec!["USDC".to_string(), "USDC.E".to_string()],
        _ => vec![asset.to_uppercase()],
    }
}

/// Returns the "display" symbol for an asset (e.g., ETH for WETH if user prefers).
/// Currently just returns the asset as-is, but unified here for future polish.
pub fn display_asset(asset: &str) -> String {
    asset.to_uppercase()
}

/// Helper to check if two asset symbols should be treated as equivalent.
pub fn are_assets_equivalent(a: &str, b: &str) -> bool {
    if a.to_uppercase() == b.to_uppercase() {
        return true;
    }
    let equivs = get_equivalent_assets(a);
    equivs.iter().any(|e| e == &b.to_uppercase())
}
