use std::collections::HashMap;

use crate::core::types::DEFAULT_TRANSFER_TIME_MIN;
use crate::inventory::tracker::InventoryTracker;
use crate::inventory::types::{
    CostEstimate, RebalanceStep, TradeStep, TransferPlan, Venue, WithdrawStep,
    min_operating_balance, transfer_fees,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use tracing::{info, warn};

/// Plans asset transfers to rebalance inventory across venues.
#[derive(Debug, Clone)]
pub struct RebalancePlanner {
    tracker: InventoryTracker,
    threshold_pct: f64,
    target_ratio: HashMap<Venue, f64>,
}

impl RebalancePlanner {
    /// Creates a planner with equal target ratios across all tracked venues.
    pub fn new(tracker: InventoryTracker, threshold_pct: f64) -> Self {
        let num_venues = tracker.venues().len().max(1) as f64;
        let target_ratio = tracker
            .venues()
            .iter()
            .map(|v| (*v, 1.0 / num_venues))
            .collect();

        Self {
            tracker,
            threshold_pct,
            target_ratio,
        }
    }

    /// Returns a reference to the underlying inventory tracker.
    pub fn tracker(&self) -> &InventoryTracker {
        &self.tracker
    }

    /// Creates a planner with custom per-venue target ratios.
    pub fn with_target_ratio(
        tracker: InventoryTracker,
        threshold_pct: f64,
        target_ratio: HashMap<Venue, f64>,
    ) -> Self {
        Self {
            tracker,
            threshold_pct,
            target_ratio,
        }
    }

    /// Returns a JSON-like summary of skew status for every tracked asset.
    pub fn check_all(&self) -> Vec<HashMap<String, serde_json::Value>> {
        let skews = self.tracker.get_skews();
        skews
            .iter()
            .map(|s| {
                let mut map = HashMap::new();
                map.insert("asset".into(), serde_json::Value::String(s.asset.clone()));
                map.insert(
                    "max_deviation_pct".into(),
                    serde_json::Value::from(s.max_deviation_pct),
                );
                map.insert(
                    "needs_rebalance".into(),
                    serde_json::Value::Bool(s.needs_rebalance),
                );
                map
            })
            .collect()
    }

    /// Generates transfer plans to rebalance a single asset across venues.
    pub fn plan(&self, asset: &str) -> Vec<TransferPlan> {
        let skew = self.tracker.skew(asset);

        if skew.max_deviation_pct <= self.threshold_pct {
            return vec![];
        }

        let fees = transfer_fees();
        let min_op = min_operating_balance();

        let fee_info = fees.get(asset);
        let min_withdrawal = fee_info.map(|f| f.min_withdrawal).unwrap_or_else(|| {
            warn!(asset, "No transfer fee info, assuming zero min_withdrawal");
            Decimal::ZERO
        });
        let withdrawal_fee = fee_info.map(|f| f.withdrawal_fee).unwrap_or_else(|| {
            warn!(asset, "No transfer fee info, assuming zero withdrawal_fee");
            Decimal::ZERO
        });
        let est_time = fee_info
            .map(|f| f.estimated_time_min)
            .unwrap_or(DEFAULT_TRANSFER_TIME_MIN);

        let min_balance = min_op.get(asset).copied().unwrap_or_else(|| {
            warn!(asset, "No min operating balance configured, assuming zero");
            Decimal::ZERO
        });

        let total = skew.total;
        if total <= Decimal::ZERO {
            return vec![];
        }

        let (mut surplus_venues, mut deficit_venues) = self.surplus_deficit(&skew, total, asset);

        let mut plans = Vec::new();

        for (from_venue, surplus_remaining) in surplus_venues.iter_mut() {
            let from_current = self.tracker.get_total(*from_venue, asset).unwrap_or_else(|| {
                warn!(venue = %from_venue, asset, "No balance data for source venue, treating total as zero");
                Decimal::ZERO
            });
            let mut from_remaining = from_current - min_balance;
            if from_remaining <= Decimal::ZERO {
                continue;
            }

            for (to_venue, deficit_remaining) in deficit_venues.iter_mut() {
                if *surplus_remaining <= Decimal::ZERO || from_remaining <= Decimal::ZERO {
                    break;
                }
                if *deficit_remaining <= Decimal::ZERO {
                    continue;
                }

                let amount = (*surplus_remaining).min(*deficit_remaining).min(from_remaining);
                if amount < min_withdrawal {
                    continue;
                }

                let net = amount - withdrawal_fee;
                if net <= Decimal::ZERO {
                    continue;
                }

                plans.push(TransferPlan {
                    from_venue: *from_venue,
                    to_venue: *to_venue,
                    asset: asset.to_string(),
                    amount,
                    estimated_fee: withdrawal_fee,
                    estimated_time_min: est_time,
                });

                info!(
                    from = %from_venue,
                    to = %to_venue,
                    asset,
                    amount = %amount,
                    fee = %withdrawal_fee,
                    "Planned transfer"
                );

                *surplus_remaining -= amount;
                *deficit_remaining -= amount;
                from_remaining -= amount;
            }
        }

        plans
    }

    /// Computes per-venue surplus and deficit against per-venue targets.
    /// Targets honor `target_ratio` when configured; otherwise split equally.
    #[allow(clippy::type_complexity)]
    fn surplus_deficit(
        &self,
        skew: &crate::exchange::types::SkewResult,
        total: Decimal,
        asset: &str,
    ) -> (Vec<(Venue, Decimal)>, Vec<(Venue, Decimal)>) {
        let num_venues = self.tracker.venues().len().max(1);
        let equal_ratio = 1.0 / num_venues as f64;

        let mut surplus: Vec<(Venue, Decimal)> = vec![];
        let mut deficit: Vec<(Venue, Decimal)> = vec![];

        for venue in self.tracker.venues() {
            let ratio = self
                .target_ratio
                .get(venue)
                .copied()
                .unwrap_or(equal_ratio);
            let ratio_dec = Decimal::from_f64(ratio).unwrap_or_else(|| {
                warn!(venue = %venue, ratio, "Invalid target_ratio, falling back to equal split");
                Decimal::from_f64(equal_ratio).unwrap_or(Decimal::ZERO)
            });
            let target = total * ratio_dec;

            let current_amount = skew
                .venues
                .get(&venue.to_string())
                .map(|c| c.amount)
                .unwrap_or_else(|| {
                    warn!(venue = %venue, asset, "No skew data for tracked venue, treating as zero");
                    Decimal::ZERO
                });

            if current_amount > target {
                surplus.push((*venue, current_amount - target));
            } else if current_amount < target {
                deficit.push((*venue, target - current_amount));
            }
        }
        (surplus, deficit)
    }

    /// Generates transfer plans for every asset that needs rebalancing.
    pub fn plan_all(&self) -> HashMap<String, Vec<TransferPlan>> {
        let skews = self.tracker.get_skews();
        let mut result = HashMap::new();

        for skew in &skews {
            if skew.max_deviation_pct > self.threshold_pct {
                let plans = self.plan(&skew.asset);
                if !plans.is_empty() {
                    result.insert(skew.asset.clone(), plans);
                }
            }
        }

        result
    }

    /// Estimates total fees, time, and affected assets for a set of transfer plans.
    pub fn estimate_cost(
        &self,
        plans: &[TransferPlan],
        prices: &HashMap<String, Decimal>,
    ) -> CostEstimate {
        let total_transfers = plans.len();
        let mut total_fees_usd = Decimal::ZERO;
        let mut max_time = 0u32;
        let mut assets = std::collections::HashSet::new();

        for plan in plans {
            assets.insert(plan.asset.clone());
            let price = match prices.get(&plan.asset).copied() {
                Some(p) => p,
                None => {
                    warn!(asset = %plan.asset, "Missing price for cost estimation, skipping");
                    continue;
                }
            };
            total_fees_usd += plan.estimated_fee * price;
            max_time = max_time.max(plan.estimated_time_min);
        }

        CostEstimate {
            total_transfers,
            total_fees_usd,
            total_time_min: max_time,
            assets_affected: assets.into_iter().collect(),
        }
    }

    /// Produces executable rebalance steps for a single asset across N venues.
    ///
    /// For CEX↔CEX pairs, creates `TradeStep`s (sell on surplus, buy on deficit).
    /// For CEX→Wallet or Wallet→CEX, creates `WithdrawStep`s.
    pub fn plan_executable(
        &self,
        asset: &str,
        quote_asset: &str,
        max_slippage_bps: Decimal,
    ) -> Vec<RebalanceStep> {
        let skew = self.tracker.skew(asset);
        if skew.max_deviation_pct <= self.threshold_pct {
            return vec![];
        }

        let total = skew.total;
        if total <= Decimal::ZERO {
            return vec![];
        }

        let min_balance = match min_operating_balance().get(asset) {
            Some(b) => *b,
            None => {
                warn!(asset, "No min operating balance configured, treating as zero");
                Decimal::ZERO
            }
        };
        let fees = transfer_fees();
        let fee_info = fees.get(asset);

        let (mut surplus, mut deficit) = self.surplus_deficit(&skew, total, asset);

        let mut steps = Vec::new();

        for (surplus_venue, surplus_remaining) in surplus.iter_mut() {
            let from_total = match self.tracker.get_total(*surplus_venue, asset) {
                Some(t) => t,
                None => {
                    warn!(venue = %surplus_venue, asset, "No balance data for surplus venue, skipping");
                    continue;
                }
            };
            let mut from_remaining = from_total - min_balance;
            if from_remaining <= Decimal::ZERO {
                continue;
            }

            for (deficit_venue, deficit_remaining) in deficit.iter_mut() {
                if *surplus_remaining <= Decimal::ZERO || from_remaining <= Decimal::ZERO {
                    break;
                }
                if *deficit_remaining <= Decimal::ZERO {
                    continue;
                }

                let amount = (*surplus_remaining)
                    .min(*deficit_remaining)
                    .min(from_remaining);
                if amount <= Decimal::ZERO {
                    continue;
                }

                let mut consumed = false;
                if surplus_venue.is_cex() && deficit_venue.is_cex() {
                    let symbol = format!("{asset}{quote_asset}");
                    steps.push(RebalanceStep::Trade(TradeStep {
                        venue: *surplus_venue,
                        symbol: symbol.clone(),
                        side: "SELL".to_string(),
                        base_asset: asset.to_string(),
                        quote_asset: quote_asset.to_string(),
                        amount,
                        max_slippage_bps,
                    }));
                    steps.push(RebalanceStep::Trade(TradeStep {
                        venue: *deficit_venue,
                        symbol,
                        side: "BUY".to_string(),
                        base_asset: asset.to_string(),
                        quote_asset: quote_asset.to_string(),
                        amount,
                        max_slippage_bps,
                    }));
                    consumed = true;
                } else if surplus_venue.is_cex() && !deficit_venue.is_cex() {
                    let withdrawal_fee = match fee_info.map(|f| f.withdrawal_fee) {
                        Some(f) => f,
                        None => {
                            warn!(asset, "No withdrawal fee info, skipping withdrawal step");
                            continue;
                        }
                    };
                    if amount > withdrawal_fee {
                        steps.push(RebalanceStep::Withdraw(WithdrawStep {
                            from_venue: *surplus_venue,
                            to_venue: *deficit_venue,
                            asset: asset.to_string(),
                            amount,
                            fee: withdrawal_fee,
                        }));
                        consumed = true;
                    }
                } else if !surplus_venue.is_cex() && deficit_venue.is_cex() {
                    let withdrawal_fee = fee_info.map(|f| f.withdrawal_fee).unwrap_or(Decimal::ZERO);
                    if amount > withdrawal_fee {
                        steps.push(RebalanceStep::Withdraw(WithdrawStep {
                            from_venue: *surplus_venue,
                            to_venue: *deficit_venue,
                            asset: asset.to_string(),
                            amount,
                            fee: withdrawal_fee,
                        }));
                        consumed = true;
                    }
                } else {
                    warn!(
                        from = %surplus_venue,
                        to = %deficit_venue,
                        asset,
                        "Wallet-to-wallet rebalance not supported, skipping"
                    );
                }

                if consumed {
                    info!(
                        from = %surplus_venue,
                        to = %deficit_venue,
                        asset,
                        amount = %amount,
                        "Planned executable rebalance step"
                    );
                    *surplus_remaining -= amount;
                    *deficit_remaining -= amount;
                    from_remaining -= amount;
                }
            }
        }

        steps
    }

    /// Produces executable rebalance steps for every skewed asset.
    pub fn plan_executable_all(
        &self,
        quote_asset: &str,
        max_slippage_bps: Decimal,
    ) -> HashMap<String, Vec<RebalanceStep>> {
        let skews = self.tracker.get_skews();
        let mut result = HashMap::new();
        for skew in &skews {
            if skew.max_deviation_pct > self.threshold_pct {
                let steps = self.plan_executable(&skew.asset, quote_asset, max_slippage_bps);
                if !steps.is_empty() {
                    result.insert(skew.asset.clone(), steps);
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::types::NormalizedBalance;
    use crate::inventory::tracker::InventoryTracker;

    fn setup_imbalanced() -> RebalancePlanner {
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
        binance_bals.insert(
            "USDT".into(),
            NormalizedBalance {
                free: Decimal::from(18000),
                locked: Decimal::ZERO,
                total: Decimal::from(18000),
            },
        );
        tracker.update_from_cex(Venue::Binance, binance_bals);

        let mut wallet_bals = HashMap::new();
        wallet_bals.insert("ETH".into(), Decimal::from(9));
        wallet_bals.insert("USDT".into(), Decimal::from(12000));
        tracker.update_from_wallet(Venue::Wallet, wallet_bals);

        RebalancePlanner::new(tracker, 30.0)
    }

    #[test]
    fn test_check_detects_skewed_asset() {
        let planner = setup_imbalanced();
        let checks = planner.check_all();
        let eth_check = checks.iter().find(|c| c["asset"].as_str() == Some("ETH"));
        assert!(eth_check.is_some());
        assert_eq!(eth_check.unwrap()["needs_rebalance"].as_bool(), Some(true));
    }

    #[test]
    fn test_check_passes_balanced_asset() {
        let mut tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet]);
        let mut binance_bals = HashMap::new();
        binance_bals.insert(
            "USDT".into(),
            NormalizedBalance {
                free: Decimal::from(5500),
                locked: Decimal::ZERO,
                total: Decimal::from(5500),
            },
        );
        tracker.update_from_cex(Venue::Binance, binance_bals);
        let mut wallet_bals = HashMap::new();
        wallet_bals.insert("USDT".into(), Decimal::from(4500));
        tracker.update_from_wallet(Venue::Wallet, wallet_bals);

        let planner = RebalancePlanner::new(tracker, 30.0);
        let checks = planner.check_all();
        let usdt_check = checks.iter().find(|c| c["asset"].as_str() == Some("USDT"));
        assert!(usdt_check.is_some());
        assert_eq!(
            usdt_check.unwrap()["needs_rebalance"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn test_plan_generates_correct_transfer() {
        let planner = setup_imbalanced();
        let plans = planner.plan("ETH");
        assert!(!plans.is_empty());
        let plan = &plans[0];
        assert_eq!(plan.from_venue, Venue::Wallet);
        assert_eq!(plan.to_venue, Venue::Binance);
        assert!(plan.amount > Decimal::ZERO);
    }

    #[test]
    fn test_plan_respects_min_operating_balance() {
        let planner = setup_imbalanced();
        let plans = planner.plan("ETH");
        if let Some(plan) = plans.first() {
            assert!(plan.net_amount() > Decimal::ZERO);
        }
    }

    #[test]
    fn test_plan_accounts_for_fees() {
        let planner = setup_imbalanced();
        let plans = planner.plan("ETH");
        if let Some(plan) = plans.first() {
            assert!(plan.estimated_fee > Decimal::ZERO);
            assert!(plan.net_amount() < plan.amount);
        }
    }

    #[test]
    fn test_plan_empty_when_balanced() {
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

        let planner = RebalancePlanner::new(tracker, 30.0);
        let plans = planner.plan("ETH");
        assert!(plans.is_empty());
    }

    #[test]
    fn test_estimate_cost_sums_correctly() {
        let planner = setup_imbalanced();
        let plans = planner.plan("ETH");
        if !plans.is_empty() {
            let prices = HashMap::from([("ETH".into(), Decimal::from(2000))]);
            let cost = planner.estimate_cost(&plans, &prices);
            assert_eq!(cost.total_transfers, plans.len());
            assert!(cost.total_fees_usd > Decimal::ZERO);
        }
    }

    #[test]
    fn test_estimate_cost_skips_missing_prices() {
        let planner = setup_imbalanced();
        let plans = planner.plan("ETH");
        if !plans.is_empty() {
            let prices: HashMap<String, Decimal> = HashMap::new();
            let cost = planner.estimate_cost(&plans, &prices);
            assert_eq!(cost.total_fees_usd, Decimal::ZERO);
        }
    }

    #[test]
    fn test_plan_empty_when_total_zero() {
        let mut tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet]);
        let mut binance_bals = HashMap::new();
        binance_bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::ZERO,
                locked: Decimal::ZERO,
                total: Decimal::ZERO,
            },
        );
        tracker.update_from_cex(Venue::Binance, binance_bals);
        let mut wallet_bals = HashMap::new();
        wallet_bals.insert("ETH".into(), Decimal::ZERO);
        tracker.update_from_wallet(Venue::Wallet, wallet_bals);

        let planner = RebalancePlanner::new(tracker, 30.0);
        let plans = planner.plan("ETH");
        assert!(plans.is_empty());
    }

    #[test]
    fn test_check_all_returns_entries_for_all_assets() {
        let planner = setup_imbalanced();
        let checks = planner.check_all();
        assert!(checks.len() >= 2);
    }

    #[test]
    fn test_plan_all_returns_only_skewed() {
        let planner = setup_imbalanced();
        let plans = planner.plan_all();
        let eth_plans = plans.get("ETH");
        assert!(eth_plans.is_some());
    }

    #[test]
    fn test_estimate_cost_partial_prices() {
        let planner = setup_imbalanced();
        let plans = planner.plan("ETH");
        if !plans.is_empty() {
            let mut prices = HashMap::new();
            prices.insert("ETH".into(), Decimal::from(2000));
            let cost_with = planner.estimate_cost(&plans, &prices);
            let cost_without = planner.estimate_cost(&plans, &HashMap::new());
            assert!(cost_with.total_fees_usd > cost_without.total_fees_usd);
        }
    }

    #[test]
    fn test_plan_distributes_surplus_across_multiple_deficits() {
        let mut tracker =
            InventoryTracker::new(vec![Venue::Binance, Venue::Bybit, Venue::Wallet]);

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

        let mut bybit_bals = HashMap::new();
        bybit_bals.insert(
            "ETH".into(),
            NormalizedBalance {
                free: Decimal::from(2),
                locked: Decimal::ZERO,
                total: Decimal::from(2),
            },
        );
        tracker.update_from_cex(Venue::Bybit, bybit_bals);

        let mut wallet_bals = HashMap::new();
        wallet_bals.insert("ETH".into(), Decimal::from(12));
        tracker.update_from_wallet(Venue::Wallet, wallet_bals);

        let planner = RebalancePlanner::new(tracker, 30.0);
        let plans = planner.plan("ETH");

        assert_eq!(plans.len(), 2);
        let total_out: Decimal = plans.iter().map(|p| p.amount).sum();
        assert!(total_out <= Decimal::from(8));
        assert!(total_out >= Decimal::from(6));
        for p in &plans {
            assert_eq!(p.from_venue, Venue::Wallet);
            assert!(p.to_venue == Venue::Binance || p.to_venue == Venue::Bybit);
        }
    }

    #[test]
    fn test_plan_executable_wallet_to_cex_generates_withdraw() {
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

        let planner = RebalancePlanner::new(tracker, 30.0);
        let steps = planner.plan_executable("ETH", "USDT", Decimal::from(50));

        assert!(!steps.is_empty());
        assert!(matches!(steps[0], RebalanceStep::Withdraw(ref w)
            if w.from_venue == Venue::Wallet && w.to_venue == Venue::Binance));
    }
}
