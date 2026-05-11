use std::collections::HashMap;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use tracing::{debug, info, warn};

use crate::core::types::REBALANCE_DEVIATION_THRESHOLD_PCT;
use crate::exchange::types::{
    CanExecuteResult, NormalizedBalance, PortfolioSnapshot, SkewResult, VenueSkew,
};
use crate::inventory::errors::{InventoryError, InventoryResult};
use crate::inventory::types::Venue;

/// Describes a discrepancy between tracked and actual balance for a single asset.
#[derive(Debug, Clone)]
pub struct BalanceMismatch {
    /// Venue where the mismatch was detected.
    pub venue: Venue,
    /// Asset symbol (e.g. "ETH", "USDC").
    pub asset: String,
    /// Balance according to the tracker (expected).
    pub tracked: Decimal,
    /// Balance reported by the exchange/wallet (actual).
    pub actual: Decimal,
    /// Absolute difference `|tracked - actual|`.
    pub diff: Decimal,
}

#[derive(Debug, Clone)]
struct VenueBalance {
    free: Decimal,
    locked: Decimal,
    reserved: Decimal,
}

/// Tracks asset balances across multiple venues and provides skew/rebalance analysis.
#[derive(Debug, Clone)]
pub struct InventoryTracker {
    balances: HashMap<(Venue, String), VenueBalance>,
    venues: Vec<Venue>,
    chain_id: u64,
}

impl InventoryTracker {
    /// Creates a new tracker for the given list of venues.
    pub fn new(venues: Vec<Venue>, chain_id: u64) -> Self {
        Self {
            balances: HashMap::new(),
            venues,
            chain_id,
        }
    }

    /// Returns the list of venues this tracker monitors.
    pub fn venues(&self) -> &[Venue] {
        &self.venues
    }

    /// Replaces all balances for a CEX venue with fresh data.
    pub fn update_from_cex(&mut self, venue: Venue, balances: HashMap<String, NormalizedBalance>) {
        let mut old_reserved = HashMap::new();
        for ((v, asset), bal) in &self.balances {
            if *v == venue && bal.reserved > Decimal::ZERO {
                old_reserved.insert(asset.clone(), bal.reserved);
            }
        }

        let keys_to_remove: Vec<(Venue, String)> = self
            .balances
            .keys()
            .filter(|(v, _)| *v == venue)
            .cloned()
            .collect();

        for key in keys_to_remove {
            self.balances.remove(&key);
        }

        for (asset, bal) in balances {
            let canonical = if self.chain_id == crate::core::types::ARBITRUM_CHAIN_ID {
                crate::core::assets::canonicalize_asset(&asset)
            } else {
                asset.to_uppercase()
            };
            let reserved = old_reserved
                .get(&canonical)
                .copied()
                .unwrap_or(Decimal::ZERO);
            self.balances.insert(
                (venue, canonical),
                VenueBalance {
                    free: bal.free,
                    locked: bal.locked,
                    reserved,
                },
            );
        }
        debug!(venue = %venue, "Updated CEX balances");
    }

    /// Replaces all balances for a wallet venue with fresh data (no locked amounts).
    pub fn update_from_wallet(&mut self, venue: Venue, balances: HashMap<String, Decimal>) {
        let mut old_reserved = HashMap::new();
        for ((v, asset), bal) in &self.balances {
            if *v == venue && bal.reserved > Decimal::ZERO {
                old_reserved.insert(asset.clone(), bal.reserved);
            }
        }

        let keys_to_remove: Vec<(Venue, String)> = self
            .balances
            .keys()
            .filter(|(v, _)| *v == venue)
            .cloned()
            .collect();

        for key in keys_to_remove {
            self.balances.remove(&key);
        }

        for (asset, amount) in balances {
            let reserved = old_reserved.get(&asset).copied().unwrap_or(Decimal::ZERO);
            self.balances.insert(
                (venue, asset),
                VenueBalance {
                    free: amount,
                    locked: Decimal::ZERO,
                    reserved,
                },
            );
        }
        debug!(venue = %venue, "Updated wallet balances");
    }

    /// Produces a portfolio snapshot with per-venue breakdowns and a USD total.
    pub fn snapshot(&self, prices: &HashMap<String, Decimal>) -> PortfolioSnapshot {
        let timestamp = chrono::Utc::now();
        let mut venues_map: HashMap<String, HashMap<String, NormalizedBalance>> = HashMap::new();
        let mut totals: HashMap<String, Decimal> = HashMap::new();
        let mut total_usd = Decimal::ZERO;

        for venue in &self.venues {
            let venue_key = venue.to_string();
            let mut venue_balances = HashMap::new();

            for ((v, asset), bal) in &self.balances {
                if *v == *venue {
                    let total = bal.free + bal.locked;
                    venue_balances.insert(
                        asset.clone(),
                        NormalizedBalance {
                            free: bal.free,
                            locked: bal.locked,
                            total,
                        },
                    );

                    *totals.entry(asset.clone()).or_insert(Decimal::ZERO) += total;
                }
            }

            venues_map.insert(venue_key, venue_balances);
        }

        for (asset, total) in &totals {
            if let Some(price) = prices.get(asset) {
                total_usd += total * price;
            }
        }

        PortfolioSnapshot {
            timestamp,
            venues: venues_map,
            totals,
            total_usd,
        }
    }

    /// Returns the free (unlocked) balance for an asset at a venue, if known.
    pub fn get_available(&self, venue: Venue, asset: &str) -> Option<Decimal> {
        self.balances
            .get(&(venue, asset.to_string()))
            .map(|b| b.free - b.reserved)
    }

    /// Returns the total (free + locked) balance for an asset at a venue, if known.
    pub fn get_total(&self, venue: Venue, asset: &str) -> Option<Decimal> {
        self.balances
            .get(&(venue, asset.to_string()))
            .map(|b| b.free + b.locked)
    }

    /// Internal helper for 'smart' wallet balance checks (e.g. ETH + WETH).
    fn get_wallet_effective_available(&self, asset: &str) -> Decimal {
        let mut total = Decimal::ZERO;
        for a in crate::core::assets::get_equivalent_assets(asset) {
            if let Some(bal) = self.balances.get(&(Venue::Wallet, a)) {
                total += bal.free - bal.reserved;
            }
        }
        total
    }

    /// Compares tracked balances against freshly-fetched actual balances.
    /// Returns a list of mismatches where the absolute difference exceeds
    /// `tolerance_pct` (expressed as a percentage, e.g. `1.0` = 1%).
    ///
    /// This is intended to be called after each balance sync to detect
    /// discrepancies caused by failed fills, partial fills, or external
    /// withdrawals.
    pub fn verify_balances(
        &self,
        venue: Venue,
        actual: &HashMap<String, Decimal>,
        tolerance_pct: Decimal,
    ) -> Vec<BalanceMismatch> {
        let mut mismatches = Vec::new();
        for (asset, actual_total) in actual {
            let tracked = self.get_total(venue, asset).unwrap_or_else(|| {
                warn!(
                    venue = %venue,
                    asset,
                    "tracked balance missing during verification; treating tracked total as 0"
                );
                Decimal::ZERO
            });
            if tracked == Decimal::ZERO && *actual_total == Decimal::ZERO {
                continue;
            }
            let diff = (tracked - actual_total).abs();
            let threshold = if tracked.abs() > Decimal::ZERO {
                tracked * tolerance_pct / Decimal::from(100)
            } else {
                // If tracked is zero but actual is not, any non-zero actual
                // is a mismatch.
                Decimal::ZERO
            };
            if diff > threshold {
                mismatches.push(BalanceMismatch {
                    venue,
                    asset: asset.clone(),
                    tracked,
                    actual: *actual_total,
                    diff,
                });
            }
        }
        mismatches
    }

    /// Checks whether an arb trade can be executed given available balances on both venues.
    pub fn can_execute(
        &self,
        buy_venue: Venue,
        buy_asset: &str,
        buy_amount: Decimal,
        sell_venue: Venue,
        sell_asset: &str,
        sell_amount: Decimal,
    ) -> CanExecuteResult {
        let buy_available = if buy_venue == Venue::Wallet {
            self.get_wallet_effective_available(buy_asset)
        } else {
            self.get_available(buy_venue, buy_asset).unwrap_or_else(|| {
                warn!(
                    venue = %buy_venue,
                    asset = buy_asset,
                    "available balance missing for buy leg; treating available as 0"
                );
                Decimal::ZERO
            })
        };

        let sell_available = if sell_venue == Venue::Wallet {
            self.get_wallet_effective_available(sell_asset)
        } else {
            self.get_available(sell_venue, sell_asset)
                .unwrap_or_else(|| {
                    warn!(
                        venue = %sell_venue,
                        asset = sell_asset,
                        "available balance missing for sell leg; treating available as 0"
                    );
                    Decimal::ZERO
                })
        };

        let buy_ok = buy_available >= buy_amount;
        let sell_ok = sell_available >= sell_amount;
        let can_execute = buy_ok && sell_ok;

        let reason = if !can_execute {
            if !buy_ok && !sell_ok {
                Some("Insufficient balance on both venues".into())
            } else if !buy_ok {
                Some(format!(
                    "Insufficient {} on {} (need {}, have {})",
                    buy_asset, buy_venue, buy_amount, buy_available
                ))
            } else {
                Some(format!(
                    "Insufficient {} on {} (need {}, have {})",
                    sell_asset, sell_venue, sell_amount, sell_available
                ))
            }
        } else {
            None
        };

        CanExecuteResult {
            can_execute,
            buy_venue_available: buy_available,
            buy_venue_needed: buy_amount,
            sell_venue_available: sell_available,
            sell_venue_needed: sell_amount,
            reason,
        }
    }

    /// Adjusts balances to reflect a completed trade (buy or sell) and its fee.
    #[allow(clippy::too_many_arguments)]
    pub fn record_trade(
        &mut self,
        venue: Venue,
        side: &str,
        base_asset: &str,
        quote_asset: &str,
        base_amount: Decimal,
        quote_amount: Decimal,
        fee: Decimal,
        fee_asset: &str,
    ) -> InventoryResult<()> {
        match side {
            "buy" => {
                self.adjust_balance(venue, base_asset, base_amount)?;
                self.adjust_balance(venue, quote_asset, -quote_amount)?;
            }
            "sell" => {
                self.adjust_balance(venue, base_asset, -base_amount)?;
                self.adjust_balance(venue, quote_asset, quote_amount)?;
            }
            _ => {
                return Err(InventoryError::InsufficientBalance(format!(
                    "unknown side: {side}"
                )));
            }
        }

        self.adjust_balance(venue, fee_asset, -fee)?;
        info!(
            venue = %venue,
            side,
            base_asset,
            base_amount = %base_amount,
            quote_asset,
            quote_amount = %quote_amount,
            fee = %fee,
            fee_asset,
            "Recorded trade"
        );
        Ok(())
    }

    fn adjust_balance(&mut self, venue: Venue, asset: &str, delta: Decimal) -> InventoryResult<()> {
        if venue == Venue::Wallet && delta < Decimal::ZERO {
            let mut remaining_to_subtract = -delta;
            // Get equivalents first as a list of static strings to avoid borrow issues
            let equivalents = crate::core::assets::get_equivalent_assets(asset);

            // First, try to subtract from the primary asset
            let primary_key = (venue, asset.to_string());
            if let Some(bal) = self.balances.get_mut(&primary_key) {
                let can_take = bal.free.min(remaining_to_subtract);
                bal.free -= can_take;
                remaining_to_subtract -= can_take;
            }

            // Then, spill over to equivalents if needed
            if remaining_to_subtract > Decimal::ZERO {
                for eq in equivalents {
                    if eq == asset {
                        continue;
                    }
                    let eq_key = (venue, eq.clone());
                    if let Some(bal) = self.balances.get_mut(&eq_key) {
                        let can_take = bal.free.min(remaining_to_subtract);
                        bal.free -= can_take;
                        remaining_to_subtract -= can_take;
                    }
                    if remaining_to_subtract <= Decimal::ZERO {
                        break;
                    }
                }
            }

            if remaining_to_subtract > Decimal::ZERO {
                return Err(InventoryError::NegativeBalance(format!(
                    "{} (and equivalents) on {} would go negative by {}",
                    asset, venue, remaining_to_subtract
                )));
            }
            return Ok(());
        }

        let key = (venue, asset.to_string());
        let bal = self.balances.entry(key).or_insert(VenueBalance {
            free: Decimal::ZERO,
            locked: Decimal::ZERO,
            reserved: Decimal::ZERO,
        });

        bal.free += delta;

        if bal.free < Decimal::ZERO {
            return Err(InventoryError::NegativeBalance(format!(
                "{} on {} would go negative: {}",
                asset, venue, bal.free
            )));
        }

        Ok(())
    }

    /// Computes per-venue distribution skew for a single asset.
    pub fn skew(&self, asset: &str) -> SkewResult {
        let mut venue_amounts: HashMap<String, Decimal> = HashMap::new();

        for venue in &self.venues {
            let total = self.get_total(*venue, asset).unwrap_or_else(|| {
                warn!(venue = %venue, asset, "No balance data loaded, treating total as zero");
                Decimal::ZERO
            });
            venue_amounts.insert(venue.to_string(), total);
        }

        let total: Decimal = venue_amounts.values().copied().sum();

        let num_venues = self.venues.len().max(1) as f64;
        let target_pct = 100.0 / num_venues;

        let mut venue_skews = HashMap::new();
        let mut max_deviation = 0.0f64;

        for venue in &self.venues {
            let amount = venue_amounts
                .get(&venue.to_string())
                .copied()
                .unwrap_or(Decimal::ZERO);
            let pct = if total > Decimal::ZERO {
                (amount / total * Decimal::from(100))
                    .to_f64()
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            let deviation_pct = pct - target_pct;
            if deviation_pct.abs() > max_deviation {
                max_deviation = deviation_pct.abs();
            }

            venue_skews.insert(
                venue.to_string(),
                VenueSkew {
                    amount,
                    pct,
                    deviation_pct,
                },
            );
        }

        let needs_rebalance = max_deviation > REBALANCE_DEVIATION_THRESHOLD_PCT;

        SkewResult {
            asset: asset.to_string(),
            total,
            venues: venue_skews,
            max_deviation_pct: max_deviation,
            needs_rebalance,
        }
    }

    /// Returns skew results for every tracked asset, sorted by asset name.
    pub fn get_skews(&self) -> Vec<SkewResult> {
        let mut assets = std::collections::HashSet::new();
        for (_, asset) in self.balances.keys() {
            assets.insert(asset.clone());
        }

        let mut results: Vec<SkewResult> = assets.iter().map(|a| self.skew(a)).collect();
        results.sort_by(|a, b| a.asset.cmp(&b.asset));
        results
    }

    /// Reserves a specific amount of an asset for an in-flight trade.
    pub fn reserve(&mut self, venue: Venue, asset: &str, amount: Decimal) -> InventoryResult<()> {
        let available = if venue == Venue::Wallet {
            self.get_wallet_effective_available(asset)
        } else {
            self.get_available(venue, asset).unwrap_or(Decimal::ZERO)
        };

        if available < amount {
            return Err(InventoryError::InsufficientBalance(format!(
                "Cannot reserve {} {}: only {} available",
                amount, asset, available
            )));
        }

        let key = (venue, asset.to_string());
        let bal = self.balances.entry(key).or_insert(VenueBalance {
            free: Decimal::ZERO,
            locked: Decimal::ZERO,
            reserved: Decimal::ZERO,
        });

        bal.reserved += amount;
        Ok(())
    }

    /// Releases a previously reserved amount of an asset.
    pub fn release(&mut self, venue: Venue, asset: &str, amount: Decimal) -> InventoryResult<()> {
        let key = (venue, asset.to_string());
        if let Some(bal) = self.balances.get_mut(&key) {
            if bal.reserved < amount {
                bal.reserved = Decimal::ZERO;
            } else {
                bal.reserved -= amount;
            }
            Ok(())
        } else {
            Err(InventoryError::InsufficientBalance(
                "Asset not found".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::MAINNET_CHAIN_ID;

    fn setup_tracker() -> InventoryTracker {
        let mut tracker =
            InventoryTracker::new(vec![Venue::Binance, Venue::Wallet], MAINNET_CHAIN_ID);

        let mut binance_bals = HashMap::new();
        binance_bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::from(5),
                locked: Decimal::ZERO,
                total: Decimal::from(5),
            },
        );
        binance_bals.insert(
            "USDT".into(),
            NormalizedBalance {
                free: Decimal::from(20000),
                locked: Decimal::from(500),
                total: Decimal::from(20500),
            },
        );
        tracker.update_from_cex(Venue::Binance, binance_bals);

        let mut wallet_bals = HashMap::new();
        wallet_bals.insert("ETH".into(), Decimal::from(5));
        wallet_bals.insert("USDT".into(), Decimal::from(15000));
        tracker.update_from_wallet(Venue::Wallet, wallet_bals);

        tracker
    }

    #[test]
    fn test_snapshot_aggregates_across_venues() {
        let tracker = setup_tracker();
        let prices = HashMap::from([
            ("ETH".into(), Decimal::from(2000)),
            ("USDT".into(), Decimal::ONE),
        ]);
        let snap = tracker.snapshot(&prices);
        assert_eq!(
            snap.totals.get("ETH").copied().unwrap_or(Decimal::ZERO),
            Decimal::from(10)
        );
        assert_eq!(
            snap.totals.get("USDT").copied().unwrap_or(Decimal::ZERO),
            Decimal::from(35500)
        );
    }

    #[test]
    fn test_can_execute_passes_when_sufficient() {
        let tracker = setup_tracker();
        let result = tracker.can_execute(
            Venue::Binance,
            "USDT",
            Decimal::from(4000),
            Venue::Wallet,
            "ETH",
            Decimal::from(2),
        );
        assert!(result.can_execute);
    }

    #[test]
    fn test_can_execute_fails_insufficient_buy() {
        let tracker = setup_tracker();
        let result = tracker.can_execute(
            Venue::Binance,
            "USDT",
            Decimal::from(50000),
            Venue::Wallet,
            "ETH",
            Decimal::from(2),
        );
        assert!(!result.can_execute);
        assert!(result.reason.is_some());
    }

    #[test]
    fn test_can_execute_fails_insufficient_sell() {
        let tracker = setup_tracker();
        let result = tracker.can_execute(
            Venue::Binance,
            "USDT",
            Decimal::from(4000),
            Venue::Wallet,
            "ETH",
            Decimal::from(100),
        );
        assert!(!result.can_execute);
    }

    #[test]
    fn test_record_trade_updates_balances() {
        let mut tracker = InventoryTracker::new(vec![Venue::Binance], MAINNET_CHAIN_ID);

        let mut bals = HashMap::new();
        bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::from(10),
                locked: Decimal::ZERO,
                total: Decimal::from(10),
            },
        );
        bals.insert(
            "USDT".into(),
            NormalizedBalance {
                free: Decimal::from(20000),
                locked: Decimal::ZERO,
                total: Decimal::from(20000),
            },
        );
        tracker.update_from_cex(Venue::Binance, bals);

        tracker
            .record_trade(
                Venue::Binance,
                "buy",
                "ETH",
                "USDT",
                Decimal::from(2),
                Decimal::from(4000),
                Decimal::from(4),
                "USDT",
            )
            .unwrap();

        assert_eq!(
            tracker.get_available(Venue::Binance, "ETH").unwrap(),
            Decimal::from(12)
        );
        assert_eq!(
            tracker.get_available(Venue::Binance, "USDT").unwrap(),
            Decimal::from(15996)
        );
    }

    #[test]
    fn test_skew_detects_imbalance() {
        let mut tracker =
            InventoryTracker::new(vec![Venue::Binance, Venue::Wallet], MAINNET_CHAIN_ID);

        let mut binance_bals = HashMap::new();
        binance_bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::from(1),
                locked: Decimal::ZERO,
                total: Decimal::from(1),
            },
        );
        tracker.update_from_cex(Venue::Binance, binance_bals);

        let mut wallet_bals = HashMap::new();
        wallet_bals.insert("ETH".into(), Decimal::from(9));
        tracker.update_from_wallet(Venue::Wallet, wallet_bals);

        let skew = tracker.skew("ETH");
        assert!(skew.needs_rebalance);
        assert!(skew.max_deviation_pct > 30.0);
    }

    #[test]
    fn test_skew_balanced() {
        let mut tracker =
            InventoryTracker::new(vec![Venue::Binance, Venue::Wallet], MAINNET_CHAIN_ID);

        let mut binance_bals = HashMap::new();
        binance_bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::from(5),
                locked: Decimal::ZERO,
                total: Decimal::from(5),
            },
        );
        tracker.update_from_cex(Venue::Binance, binance_bals);

        let mut wallet_bals = HashMap::new();
        wallet_bals.insert("ETH".into(), Decimal::from(5));
        tracker.update_from_wallet(Venue::Wallet, wallet_bals);

        let skew = tracker.skew("ETH");
        assert!(!skew.needs_rebalance);
        assert!(skew.max_deviation_pct < 1.0);
    }

    #[test]
    fn test_get_skews_returns_all_assets() {
        let tracker = setup_tracker();
        let skews = tracker.get_skews();
        assert_eq!(skews.len(), 2);
        let assets: Vec<&str> = skews.iter().map(|s| s.asset.as_str()).collect();
        assert!(assets.contains(&"ETH"));
        assert!(assets.contains(&"USDT"));
    }

    #[test]
    fn test_record_trade_unknown_side_returns_error() {
        let mut tracker = InventoryTracker::new(vec![Venue::Binance], MAINNET_CHAIN_ID);
        let mut bals = HashMap::new();
        bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::from(10),
                locked: Decimal::ZERO,
                total: Decimal::from(10),
            },
        );
        tracker.update_from_cex(Venue::Binance, bals);

        let result = tracker.record_trade(
            Venue::Binance,
            "short",
            "ETH",
            "USDT",
            Decimal::from(1),
            Decimal::from(2000),
            Decimal::ZERO,
            "USDT",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown side"));
    }

    #[test]
    fn test_adjust_balance_negative_returns_error() {
        let mut tracker = InventoryTracker::new(vec![Venue::Binance], MAINNET_CHAIN_ID);
        let mut bals = HashMap::new();
        bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::from(1),
                locked: Decimal::ZERO,
                total: Decimal::from(1),
            },
        );
        tracker.update_from_cex(Venue::Binance, bals);

        let result = tracker.record_trade(
            Venue::Binance,
            "sell",
            "ETH",
            "USDT",
            Decimal::from(5),
            Decimal::from(10000),
            Decimal::ZERO,
            "USDT",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("negative") || msg.contains("Negative"));
    }

    #[test]
    fn test_get_available_returns_none_for_unknown() {
        let tracker = InventoryTracker::new(vec![Venue::Binance], MAINNET_CHAIN_ID);
        assert!(tracker.get_available(Venue::Binance, "ETH").is_none());
    }

    #[test]
    fn test_get_total_returns_none_for_unknown() {
        let tracker = InventoryTracker::new(vec![Venue::Binance], MAINNET_CHAIN_ID);
        assert!(tracker.get_total(Venue::Binance, "ETH").is_none());
    }

    #[test]
    fn test_get_available_returns_some_for_known() {
        let tracker = setup_tracker();
        assert!(tracker.get_available(Venue::Binance, "ETH").is_some());
        assert_eq!(
            tracker.get_available(Venue::Binance, "ETH").unwrap(),
            Decimal::from(5)
        );
    }

    #[test]
    fn test_get_total_includes_locked() {
        let tracker = setup_tracker();
        let total = tracker.get_total(Venue::Binance, "USDT").unwrap();
        assert_eq!(total, Decimal::from(20500));
    }

    #[test]
    fn test_can_execute_no_balance_data_returns_zero_available() {
        let tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet], MAINNET_CHAIN_ID);
        let result = tracker.can_execute(
            Venue::Binance,
            "ETH",
            Decimal::from(1),
            Venue::Wallet,
            "ETH",
            Decimal::from(1),
        );
        assert!(!result.can_execute);
        assert!(result.reason.is_some());
        assert_eq!(result.buy_venue_available, Decimal::ZERO);
        assert_eq!(result.sell_venue_available, Decimal::ZERO);
    }

    #[test]
    fn test_can_execute_insufficient_on_both_venues() {
        let tracker = setup_tracker();
        let result = tracker.can_execute(
            Venue::Binance,
            "USDT",
            Decimal::from(50000),
            Venue::Wallet,
            "ETH",
            Decimal::from(100),
        );
        assert!(!result.can_execute);
        let reason = result.reason.unwrap();
        assert!(reason.contains("both venues"));
    }

    #[test]
    fn test_record_trade_sell_adjusts_correctly() {
        let mut tracker = InventoryTracker::new(vec![Venue::Binance], MAINNET_CHAIN_ID);
        let mut bals = HashMap::new();
        bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::from(10),
                locked: Decimal::ZERO,
                total: Decimal::from(10),
            },
        );
        bals.insert(
            "USDT".into(),
            NormalizedBalance {
                free: Decimal::ZERO,
                locked: Decimal::ZERO,
                total: Decimal::ZERO,
            },
        );
        tracker.update_from_cex(Venue::Binance, bals);

        tracker
            .record_trade(
                Venue::Binance,
                "sell",
                "ETH",
                "USDT",
                Decimal::from(2),
                Decimal::from(4000),
                Decimal::from(4),
                "USDT",
            )
            .unwrap();

        assert_eq!(
            tracker.get_available(Venue::Binance, "ETH").unwrap(),
            Decimal::from(8)
        );
        assert_eq!(
            tracker.get_available(Venue::Binance, "USDT").unwrap(),
            Decimal::from(3996)
        );
    }

    #[test]
    fn test_verify_balances_detects_mismatch() {
        let mut tracker = InventoryTracker::new(vec![Venue::Binance], MAINNET_CHAIN_ID);
        let mut bals = HashMap::new();
        bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::from(10),
                locked: Decimal::ZERO,
                total: Decimal::from(10),
            },
        );
        tracker.update_from_cex(Venue::Binance, bals);

        // Actual balance differs by more than 1% tolerance
        let mut actual = HashMap::new();
        actual.insert("ETH".into(), Decimal::from(9)); // 10% diff
        let mismatches = tracker.verify_balances(Venue::Binance, &actual, Decimal::from(1));
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].asset, "ETH");
        assert_eq!(mismatches[0].diff, Decimal::from(1));
    }

    #[test]
    fn test_verify_balances_within_tolerance() {
        let mut tracker = InventoryTracker::new(vec![Venue::Binance], MAINNET_CHAIN_ID);
        let mut bals = HashMap::new();
        bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::from(10),
                locked: Decimal::ZERO,
                total: Decimal::from(10),
            },
        );
        tracker.update_from_cex(Venue::Binance, bals);

        // Actual balance within 1% tolerance
        let mut actual = HashMap::new();
        actual.insert("ETH".into(), Decimal::from(10)); // exact match
        let mismatches = tracker.verify_balances(Venue::Binance, &actual, Decimal::from(1));
        assert!(mismatches.is_empty());
    }
}
