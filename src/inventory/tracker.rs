use std::collections::HashMap;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use tracing::{debug, info};

use crate::exchange::types::{
    CanExecuteResult, NormalizedBalance, PortfolioSnapshot, SkewResult, VenueSkew,
};
use crate::inventory::errors::{InventoryError, InventoryResult};
use crate::inventory::types::Venue;

#[derive(Debug, Clone)]
struct VenueBalance {
    free: Decimal,
    locked: Decimal,
}

#[derive(Debug, Clone)]
pub struct InventoryTracker {
    balances: HashMap<(Venue, String), VenueBalance>,
    venues: Vec<Venue>,
}

impl InventoryTracker {
    pub fn new(venues: Vec<Venue>) -> Self {
        Self {
            balances: HashMap::new(),
            venues,
        }
    }

    pub fn venues(&self) -> &[Venue] {
        &self.venues
    }

    pub fn update_from_cex(&mut self, venue: Venue, balances: HashMap<String, NormalizedBalance>) {
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
            self.balances.insert(
                (venue, asset),
                VenueBalance {
                    free: bal.free,
                    locked: bal.locked,
                },
            );
        }
        debug!(venue = %venue, "Updated CEX balances");
    }

    pub fn update_from_wallet(&mut self, venue: Venue, balances: HashMap<String, Decimal>) {
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
            self.balances.insert(
                (venue, asset),
                VenueBalance {
                    free: amount,
                    locked: Decimal::ZERO,
                },
            );
        }
        debug!(venue = %venue, "Updated wallet balances");
    }

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

    pub fn get_available(&self, venue: Venue, asset: &str) -> Decimal {
        self.balances
            .get(&(venue, asset.to_string()))
            .map(|b| b.free)
            .unwrap_or(Decimal::ZERO)
    }

    pub fn get_total(&self, venue: Venue, asset: &str) -> Decimal {
        self.balances
            .get(&(venue, asset.to_string()))
            .map(|b| b.free + b.locked)
            .unwrap_or(Decimal::ZERO)
    }

    pub fn can_execute(
        &self,
        buy_venue: Venue,
        buy_asset: &str,
        buy_amount: Decimal,
        sell_venue: Venue,
        sell_asset: &str,
        sell_amount: Decimal,
    ) -> CanExecuteResult {
        let buy_available = self.get_available(buy_venue, buy_asset);
        let sell_available = self.get_available(sell_venue, sell_asset);

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
        let key = (venue, asset.to_string());
        let bal = self.balances.entry(key).or_insert(VenueBalance {
            free: Decimal::ZERO,
            locked: Decimal::ZERO,
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

    pub fn skew(&self, asset: &str) -> SkewResult {
        let mut venue_amounts: HashMap<String, Decimal> = HashMap::new();

        for venue in &self.venues {
            let total = self.get_total(*venue, asset);
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

        let needs_rebalance = max_deviation > 30.0;

        SkewResult {
            asset: asset.to_string(),
            total,
            venues: venue_skews,
            max_deviation_pct: max_deviation,
            needs_rebalance,
        }
    }

    pub fn get_skews(&self) -> Vec<SkewResult> {
        let mut assets = std::collections::HashSet::new();
        for (_, asset) in self.balances.keys() {
            assets.insert(asset.clone());
        }

        let mut results: Vec<SkewResult> = assets.iter().map(|a| self.skew(a)).collect();
        results.sort_by(|a, b| a.asset.cmp(&b.asset));
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_tracker() -> InventoryTracker {
        let mut tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet]);

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
        let mut tracker = InventoryTracker::new(vec![Venue::Binance]);

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
            tracker.get_available(Venue::Binance, "ETH"),
            Decimal::from(12)
        );
        assert_eq!(
            tracker.get_available(Venue::Binance, "USDT"),
            Decimal::from(15996)
        );
    }

    #[test]
    fn test_skew_detects_imbalance() {
        let mut tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet]);

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
        let mut tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet]);

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
}
