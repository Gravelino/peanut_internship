use chrono::Utc;
use rust_decimal::Decimal;

use crate::safety::killswitch::SafetyDecision;
use crate::strategy::Signal;

const DEFAULT_PRE_TRADE_MAX_SPREAD_BPS: u64 = 500;
const DEFAULT_PRE_TRADE_MAX_AGE_SECONDS: f64 = 5.0;

#[derive(Debug, Clone)]
pub struct PreTradeValidator {
    pub max_spread_bps: Decimal,
    pub max_age_seconds: f64,
}

impl Default for PreTradeValidator {
    fn default() -> Self {
        Self {
            max_spread_bps: Decimal::from(DEFAULT_PRE_TRADE_MAX_SPREAD_BPS),
            max_age_seconds: DEFAULT_PRE_TRADE_MAX_AGE_SECONDS,
        }
    }
}

impl PreTradeValidator {
    pub fn validate_signal(&self, signal: &Signal) -> SafetyDecision {
        if signal.cex_price <= Decimal::ZERO {
            return SafetyDecision::Blocked {
                reason: "Invalid CEX price".to_string(),
            };
        }
        if signal.dex_price <= Decimal::ZERO {
            return SafetyDecision::Blocked {
                reason: "Invalid DEX price".to_string(),
            };
        }
        if signal.size <= Decimal::ZERO {
            return SafetyDecision::Blocked {
                reason: "Invalid trade size".to_string(),
            };
        }
        if signal.spread_bps > self.max_spread_bps {
            return SafetyDecision::Blocked {
                reason: format!("Spread {}bps too high", signal.spread_bps),
            };
        }
        if Utc::now() >= signal.expiry {
            return SafetyDecision::Blocked {
                reason: "Signal expired".to_string(),
            };
        }
        if signal.age_seconds() > self.max_age_seconds {
            return SafetyDecision::Blocked {
                reason: format!("Signal too old: {:.1}s", signal.age_seconds()),
            };
        }
        if signal.expected_net_pnl <= Decimal::ZERO {
            return SafetyDecision::Blocked {
                reason: "Expected net PnL must be positive".to_string(),
            };
        }
        if !signal.inventory_ok {
            return SafetyDecision::Blocked {
                reason: "Inventory check failed".to_string(),
            };
        }
        if !signal.within_limits {
            return SafetyDecision::Blocked {
                reason: "Signal marked outside limits".to_string(),
            };
        }
        SafetyDecision::Allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::{Direction, SignalParams};
    use chrono::Duration;

    fn signal() -> Signal {
        let mut s = Signal::new(SignalParams {
            pair: "ETH/USDT".into(),
            direction: Direction::BuyCexSellDex,
            cex_price: Decimal::from(2000),
            dex_price: Decimal::from(2020),
            spread_bps: Decimal::from(100),
            size: Decimal::new(1, 2),
            notional_usd: Decimal::from(1000),
            expected_gross_pnl: Decimal::from(1),
            expected_fees: Decimal::new(1, 1),
            expected_net_pnl: Decimal::new(9, 1),
            ttl: Duration::seconds(5),
            inventory_ok: true,
            within_limits: true,
        });
        s.score = Decimal::from(80);
        s
    }

    #[test]
    fn rejects_zero_cex_price() {
        let mut s = signal();
        s.cex_price = Decimal::ZERO;
        assert!(!PreTradeValidator::default().validate_signal(&s).allowed());
    }

    #[test]
    fn rejects_zero_dex_price() {
        let mut s = signal();
        s.dex_price = Decimal::ZERO;
        assert!(!PreTradeValidator::default().validate_signal(&s).allowed());
    }

    #[test]
    fn rejects_zero_size() {
        let mut s = signal();
        s.size = Decimal::ZERO;
        assert!(!PreTradeValidator::default().validate_signal(&s).allowed());
    }

    #[test]
    fn rejects_absurd_spread() {
        let mut s = signal();
        s.spread_bps = Decimal::from(501);
        assert!(!PreTradeValidator::default().validate_signal(&s).allowed());
    }

    #[test]
    fn rejects_expired_signal() {
        let s = Signal::new(SignalParams {
            ttl: Duration::seconds(-1),
            pair: "ETH/USDT".into(),
            direction: Direction::BuyCexSellDex,
            cex_price: Decimal::from(2000),
            dex_price: Decimal::from(2020),
            spread_bps: Decimal::from(100),
            size: Decimal::new(1, 2),
            notional_usd: Decimal::from(1000),
            expected_gross_pnl: Decimal::from(1),
            expected_fees: Decimal::new(1, 1),
            expected_net_pnl: Decimal::new(9, 1),
            inventory_ok: true,
            within_limits: true,
        });
        assert!(!PreTradeValidator::default().validate_signal(&s).allowed());
    }

    #[test]
    fn accepts_normal_signal() {
        assert!(
            PreTradeValidator::default()
                .validate_signal(&signal())
                .allowed()
        );
    }
}
