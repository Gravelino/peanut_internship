use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::safety::killswitch::{ABSOLUTE_MIN_CAPITAL, SafetyDecision, safety_check};
use crate::strategy::Signal;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_trade_usd: Decimal,
    pub max_trade_pct: Decimal,
    pub max_position_per_token: Decimal,
    pub max_open_positions: u32,
    pub max_loss_per_trade: Decimal,
    pub max_daily_loss: Decimal,
    pub max_drawdown_pct: Decimal,
    pub max_trades_per_hour: u32,
    pub consecutive_loss_limit: u32,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_trade_usd: Decimal::from(5),
            max_trade_pct: Decimal::new(20, 2),
            max_position_per_token: Decimal::from(30),
            max_open_positions: 1,
            max_loss_per_trade: Decimal::from(5),
            max_daily_loss: Decimal::from(10),
            max_drawdown_pct: Decimal::new(20, 2),
            max_trades_per_hour: 20,
            consecutive_loss_limit: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RiskManager {
    limits: RiskLimits,
    initial_capital: Decimal,
    peak_capital: Decimal,
    current_capital: Decimal,
    daily_pnl: Decimal,
    trades_this_hour: u32,
    hour_started_at: DateTime<Utc>,
    consecutive_losses: u32,
    open_positions: u32,
}

impl RiskManager {
    pub fn new(limits: RiskLimits, initial_capital: Decimal) -> Self {
        Self {
            limits,
            initial_capital,
            peak_capital: initial_capital,
            current_capital: initial_capital,
            daily_pnl: Decimal::ZERO,
            trades_this_hour: 0,
            hour_started_at: Utc::now(),
            consecutive_losses: 0,
            open_positions: 0,
        }
    }

    pub fn check_pre_trade(&mut self, signal: &Signal) -> SafetyDecision {
        self.reset_hour_if_needed();
        let trade_value = signal.notional_usd;

        if trade_value > self.limits.max_trade_usd {
            return SafetyDecision::Blocked {
                reason: format!(
                    "Trade ${trade_value:.2} exceeds max ${:.2}",
                    self.limits.max_trade_usd
                ),
            };
        }

        if trade_value > self.current_capital * self.limits.max_trade_pct {
            return SafetyDecision::Blocked {
                reason: format!(
                    "Trade ${trade_value:.2} exceeds {:.0}% of capital",
                    self.limits.max_trade_pct * Decimal::from(100)
                ),
            };
        }

        if self.open_positions >= self.limits.max_open_positions {
            return SafetyDecision::Blocked {
                reason: format!("Open position limit ({}) reached", self.open_positions),
            };
        }

        if self.daily_pnl <= -self.limits.max_daily_loss {
            return SafetyDecision::Blocked {
                reason: format!("Daily loss limit reached: ${:.2}", self.daily_pnl),
            };
        }

        let drawdown = self.drawdown_pct();
        if drawdown >= self.limits.max_drawdown_pct {
            return SafetyDecision::Blocked {
                reason: format!(
                    "Drawdown {:.1}% exceeds limit",
                    drawdown * Decimal::from(100)
                ),
            };
        }

        if self.consecutive_losses >= self.limits.consecutive_loss_limit {
            return SafetyDecision::Blocked {
                reason: format!(
                    "Consecutive loss limit ({}) reached",
                    self.consecutive_losses
                ),
            };
        }

        if self.trades_this_hour >= self.limits.max_trades_per_hour {
            return SafetyDecision::Blocked {
                reason: "Hourly trade limit reached".to_string(),
            };
        }

        safety_check(
            trade_value,
            self.daily_pnl,
            self.current_capital,
            self.trades_this_hour,
        )
    }

    pub fn record_trade(&mut self, pnl: Decimal) {
        self.daily_pnl += pnl;
        self.current_capital += pnl;
        if self.current_capital > self.peak_capital {
            self.peak_capital = self.current_capital;
        }
        self.trades_this_hour += 1;
        if pnl < Decimal::ZERO {
            self.consecutive_losses += 1;
        } else {
            self.consecutive_losses = 0;
        }
    }

    pub fn reset_daily(&mut self) {
        self.daily_pnl = Decimal::ZERO;
        self.trades_this_hour = 0;
        self.consecutive_losses = 0;
        self.hour_started_at = Utc::now();
    }

    pub fn drawdown_pct(&self) -> Decimal {
        if self.peak_capital <= Decimal::ZERO {
            Decimal::ZERO
        } else {
            (self.peak_capital - self.current_capital) / self.peak_capital
        }
    }

    pub fn current_capital(&self) -> Decimal {
        self.current_capital
    }

    pub fn initial_capital(&self) -> Decimal {
        self.initial_capital
    }

    pub fn daily_pnl(&self) -> Decimal {
        self.daily_pnl
    }

    pub fn trades_this_hour(&self) -> u32 {
        self.trades_this_hour
    }

    pub fn consecutive_losses(&self) -> u32 {
        self.consecutive_losses
    }

    pub fn limits(&self) -> &RiskLimits {
        &self.limits
    }

    /// Returns `true` when tracked capital has fallen below the absolute
    /// minimum safety threshold (`ABSOLUTE_MIN_CAPITAL`). The caller should
    /// halt the bot via `HaltCoordinator` when this fires.
    pub fn is_below_absolute_min_capital(&self) -> bool {
        self.current_capital < Decimal::from(ABSOLUTE_MIN_CAPITAL)
    }

    fn reset_hour_if_needed(&mut self) {
        if Utc::now() - self.hour_started_at >= Duration::hours(1) {
            self.trades_this_hour = 0;
            self.hour_started_at = Utc::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::{Direction, SignalParams};

    fn signal(size: Decimal, cex_price: Decimal) -> Signal {
        let mut s = Signal::new(SignalParams {
            pair: "ETH/USDT".into(),
            direction: Direction::BuyCexSellDex,
            cex_price,
            dex_price: cex_price + Decimal::from(20),
            spread_bps: Decimal::from(100),
            size,
            expected_gross_pnl: Decimal::from(20),
            expected_fees: Decimal::from(1),
            expected_net_pnl: Decimal::from(19),
            ttl: Duration::seconds(5),
            inventory_ok: true,
            within_limits: true,
        });
        s.score = Decimal::from(80);
        s
    }

    #[test]
    fn blocks_max_trade_usd() {
        let mut manager = RiskManager::new(RiskLimits::default(), Decimal::from(100));
        let decision = manager.check_pre_trade(&signal(Decimal::ONE, Decimal::from(6)));
        assert!(!decision.allowed());
    }

    #[test]
    fn blocks_max_trade_pct() {
        let limits = RiskLimits {
            max_trade_usd: Decimal::from(100),
            ..RiskLimits::default()
        };
        let mut manager = RiskManager::new(limits, Decimal::from(100));
        let decision = manager.check_pre_trade(&signal(Decimal::ONE, Decimal::from(21)));
        assert!(!decision.allowed());
    }

    #[test]
    fn blocks_daily_loss() {
        let mut manager = RiskManager::new(RiskLimits::default(), Decimal::from(100));
        manager.record_trade(Decimal::from(-10));
        let decision = manager.check_pre_trade(&signal(Decimal::ONE, Decimal::from(5)));
        assert!(!decision.allowed());
    }

    #[test]
    fn blocks_drawdown() {
        let limits = RiskLimits {
            max_daily_loss: Decimal::from(100),
            ..RiskLimits::default()
        };
        let mut manager = RiskManager::new(limits, Decimal::from(100));
        manager.record_trade(Decimal::from(-20));
        let decision = manager.check_pre_trade(&signal(Decimal::ONE, Decimal::from(5)));
        assert!(!decision.allowed());
    }

    #[test]
    fn blocks_consecutive_losses() {
        let limits = RiskLimits {
            max_daily_loss: Decimal::from(100),
            max_drawdown_pct: Decimal::ONE,
            ..RiskLimits::default()
        };
        let mut manager = RiskManager::new(limits, Decimal::from(100));
        manager.record_trade(Decimal::from(-1));
        manager.record_trade(Decimal::from(-1));
        manager.record_trade(Decimal::from(-1));
        let decision = manager.check_pre_trade(&signal(Decimal::ONE, Decimal::from(5)));
        assert!(!decision.allowed());
    }

    #[test]
    fn blocks_hourly_trade_limit() {
        let limits = RiskLimits {
            max_trades_per_hour: 2,
            ..RiskLimits::default()
        };
        let mut manager = RiskManager::new(limits, Decimal::from(100));
        manager.record_trade(Decimal::ONE);
        manager.record_trade(Decimal::ONE);
        let decision = manager.check_pre_trade(&signal(Decimal::ONE, Decimal::from(5)));
        assert!(!decision.allowed());
    }

    #[test]
    fn updates_capital_after_profit() {
        let mut manager = RiskManager::new(RiskLimits::default(), Decimal::from(100));
        manager.record_trade(Decimal::from(3));
        assert_eq!(manager.current_capital(), Decimal::from(103));
    }

    #[test]
    fn updates_consecutive_losses_after_loss() {
        let mut manager = RiskManager::new(RiskLimits::default(), Decimal::from(100));
        manager.record_trade(Decimal::from(-1));
        assert_eq!(manager.consecutive_losses(), 1);
    }

    #[test]
    fn resets_consecutive_losses_after_profit() {
        let mut manager = RiskManager::new(RiskLimits::default(), Decimal::from(100));
        manager.record_trade(Decimal::from(-1));
        manager.record_trade(Decimal::from(1));
        assert_eq!(manager.consecutive_losses(), 0);
    }

    #[test]
    fn detects_capital_below_absolute_min() {
        // Start with capital just above threshold, then take a loss that
        // drops it below $50 (ABSOLUTE_MIN_CAPITAL).
        let mut manager = RiskManager::new(RiskLimits::default(), Decimal::from(55));
        assert!(!manager.is_below_absolute_min_capital());
        manager.record_trade(Decimal::from(-10)); // capital = 45
        assert!(manager.is_below_absolute_min_capital());
    }
}
