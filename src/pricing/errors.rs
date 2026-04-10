use thiserror::Error;

/// All errors that can occur inside the `pricing` module.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PricingError {
    /// The supplied token does not belong to this pair.
    #[error("token {0} is not part of this pair")]
    UnknownToken(String),

    /// A swap was requested with zero input.
    #[error("amount_in must be greater than zero")]
    ZeroAmountIn,

    /// A swap was requested for zero output.
    #[error("amount_out must be greater than zero")]
    ZeroAmountOut,

    /// The requested output exceeds available reserves.
    #[error("amount_out {amount_out} exceeds reserve {reserve}")]
    InsufficientLiquidity { amount_out: u128, reserve: u128 },

    /// fee_bps value is out of the valid range [0, 10_000).
    #[error("fee_bps {0} is invalid; must be in range [0, 10000)")]
    InvalidFeeBps(u32),

    /// On-chain fetch failed.
    #[error("chain call failed: {0}")]
    ChainCall(String),

    /// ABI decode failed.
    #[error("abi decode failed: {0}")]
    AbiDecode(String),

    /// Binary-search bound exceeded.
    #[error("no trade size satisfies the given impact constraint")]
    NoFeasibleSize,

    /// No valid route was found between tokens.
    #[error("no valid route found between tokens")]
    NoRouteExists,

    /// Integer arithmetic overflow occurred in AMM math.
    #[error("arithmetic overflow during {0}")]
    ArithmeticOverflow(&'static str),
}

/// Convenience alias used throughout the pricing module.
pub type PricingResult<T> = Result<T, PricingError>;
