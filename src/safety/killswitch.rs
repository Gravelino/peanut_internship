use std::path::Path;

use rust_decimal::Decimal;

use crate::core::types::{
    ABSOLUTE_MAX_DAILY_LOSS, ABSOLUTE_MAX_TRADE_USD, ABSOLUTE_MAX_TRADES_PER_HOUR,
    ABSOLUTE_MIN_CAPITAL,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyDecision {
    Allowed,
    Blocked { reason: String },
}

impl SafetyDecision {
    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::Allowed => "OK",
            Self::Blocked { reason } => reason,
        }
    }
}

pub fn is_kill_switch_active(path: impl AsRef<Path>) -> bool {
    path.as_ref().exists()
}

pub fn safety_check(
    trade_usd: Decimal,
    daily_pnl: Decimal,
    total_capital: Decimal,
    trades_this_hour: u32,
) -> SafetyDecision {
    if trade_usd > Decimal::from(ABSOLUTE_MAX_TRADE_USD) {
        return SafetyDecision::Blocked {
            reason: format!("Trade ${trade_usd:.2} exceeds absolute max ${ABSOLUTE_MAX_TRADE_USD}"),
        };
    }
    if daily_pnl <= -Decimal::from(ABSOLUTE_MAX_DAILY_LOSS) {
        return SafetyDecision::Blocked {
            reason: "Absolute daily loss limit reached".to_string(),
        };
    }
    if total_capital < Decimal::from(ABSOLUTE_MIN_CAPITAL) {
        return SafetyDecision::Blocked {
            reason: format!("Capital ${total_capital:.2} below minimum ${ABSOLUTE_MIN_CAPITAL}"),
        };
    }
    if trades_this_hour >= ABSOLUTE_MAX_TRADES_PER_HOUR {
        return SafetyDecision::Blocked {
            reason: "Absolute hourly trade limit reached".to_string(),
        };
    }
    SafetyDecision::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn kill_switch_inactive_when_file_missing() {
        let path = std::env::temp_dir().join(format!(
            "arb_bot_missing_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!is_kill_switch_active(&path));
    }

    #[test]
    fn kill_switch_active_when_file_exists() {
        let path = std::env::temp_dir().join(format!(
            "arb_bot_kill_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, b"stop").unwrap();
        assert!(is_kill_switch_active(&path));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn safety_check_blocks_absolute_trade_size() {
        let decision = safety_check(Decimal::from(26), Decimal::ZERO, Decimal::from(100), 0);
        assert!(!decision.allowed());
    }

    #[test]
    fn safety_check_blocks_absolute_daily_loss() {
        let decision = safety_check(Decimal::from(5), Decimal::from(-20), Decimal::from(100), 0);
        assert!(!decision.allowed());
    }

    #[test]
    fn safety_check_blocks_low_capital() {
        let decision = safety_check(Decimal::from(5), Decimal::ZERO, Decimal::from(49), 0);
        assert!(!decision.allowed());
    }
}
