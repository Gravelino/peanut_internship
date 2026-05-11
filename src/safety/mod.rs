pub mod killswitch;
pub mod limits;
pub mod validator;

pub use crate::core::types::{
    ABSOLUTE_MAX_DAILY_LOSS, ABSOLUTE_MAX_TRADE_USD, ABSOLUTE_MAX_TRADES_PER_HOUR,
    ABSOLUTE_MIN_CAPITAL, DEFAULT_KILL_SWITCH_FILE,
};
pub use killswitch::{SafetyDecision, is_kill_switch_active, safety_check};
pub use limits::{RiskLimits, RiskManager};
pub use validator::PreTradeValidator;
