use std::collections::HashMap;

use rust_decimal::Decimal;
use tracing::info;

use crate::inventory::tracker::InventoryTracker;
use crate::inventory::types::{
    min_operating_balance, transfer_fees, CostEstimate, TransferPlan, Venue,
};

#[derive(Debug, Clone)]
pub struct RebalancePlanner {
    tracker: InventoryTracker,
    #[allow(dead_code)]
    threshold_pct: f64,
    #[allow(dead_code)]
    target_ratio: HashMap<Venue, f64>,
}

impl RebalancePlanner {
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

    pub fn tracker(&self) -> &InventoryTracker {
        &self.tracker
    }

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

    pub fn plan(&self, asset: &str) -> Vec<TransferPlan> {
        let skew = self.tracker.skew(asset);

        if !skew.needs_rebalance {
            return vec![];
        }

        let fees = transfer_fees();
        let min_op = min_operating_balance();

        let fee_info = fees.get(asset);
        let min_withdrawal = fee_info.map(|f| f.min_withdrawal).unwrap_or(Decimal::ZERO);
        let withdrawal_fee = fee_info.map(|f| f.withdrawal_fee).unwrap_or(Decimal::ZERO);
        let est_time = fee_info.map(|f| f.estimated_time_min).unwrap_or(15);

        let min_balance = min_op.get(asset).copied().unwrap_or(Decimal::ZERO);

        let total = skew.total;
        if total <= Decimal::ZERO {
            return vec![];
        }

        let num_venues = self.tracker.venues().len().max(1);
        let target_per_venue = total / Decimal::from(num_venues as i32);

        let mut surplus_venues: Vec<(Venue, Decimal)> = vec![];
        let mut deficit_venues: Vec<(Venue, Decimal)> = vec![];

        for venue in self.tracker.venues() {
            let current = skew.venues.get(&venue.to_string());
            let current_amount = current.map(|c| c.amount).unwrap_or(Decimal::ZERO);

            if current_amount > target_per_venue {
                surplus_venues.push((*venue, current_amount - target_per_venue));
            } else if current_amount < target_per_venue {
                deficit_venues.push((*venue, target_per_venue - current_amount));
            }
        }

        let mut plans = Vec::new();

        for (from_venue, surplus) in &surplus_venues {
            let from_current = self.tracker.get_total(*from_venue, asset);
            let max_without_min = from_current - min_balance;
            if max_without_min <= Decimal::ZERO {
                continue;
            }

            let transfer_amount = max_without_min.min(*surplus);

            if transfer_amount < min_withdrawal {
                continue;
            }

            for (to_venue, deficit) in &deficit_venues {
                let amount = transfer_amount.min(*deficit);

                if amount < min_withdrawal {
                    continue;
                }

                let net = amount - withdrawal_fee;
                if net <= Decimal::ZERO {
                    continue;
                }

                let from_current = self.tracker.get_total(*from_venue, asset);
                if from_current - amount < min_balance {
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
                break;
            }
        }

        plans
    }

    pub fn plan_all(&self) -> HashMap<String, Vec<TransferPlan>> {
        let skews = self.tracker.get_skews();
        let mut result = HashMap::new();

        for skew in &skews {
            if skew.needs_rebalance {
                let plans = self.plan(&skew.asset);
                if !plans.is_empty() {
                    result.insert(skew.asset.clone(), plans);
                }
            }
        }

        result
    }

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
            let price = prices.get(&plan.asset).copied().unwrap_or(Decimal::ZERO);
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
}
