//! Engine error type. Every variant is `Copy` and carries no allocation so that
//! rejecting hostile input is cheap.

use std::fmt;

/// Why an event was rejected, or why the engine stopped.
///
/// A rejected event leaves the engine state unchanged except for the
/// applied-event counter (see [`crate::engine::Engine::apply`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Checked arithmetic overflowed.
    Overflow,
    /// Division by zero in fixed-point math.
    DivisionByZero,
    /// The engine has not received its `Configure` event yet.
    NotConfigured,
    /// `Configure` was sent twice.
    AlreadyConfigured,
    /// A configuration value is out of range or inconsistent.
    InvalidConfig(&'static str),
    /// Market id not known.
    UnknownMarket,
    /// Market id already registered.
    MarketExists,
    /// Too many markets.
    TooManyMarkets,
    /// Account does not exist.
    UnknownAccount,
    /// Too many accounts.
    TooManyAccounts,
    /// Order id not found (or not owned by the caller).
    UnknownOrder,
    /// Client order id already used by a live order of the account.
    DuplicateClientOrderId,
    /// Account already has the maximum number of live orders.
    TooManyOrders,
    /// Order book is full (orders or price levels).
    BookFull,
    /// Amount must be strictly positive.
    ZeroAmount,
    /// Amount or balance beyond the supported range.
    AmountTooLarge,
    /// Quantity is zero.
    ZeroQty,
    /// Quantity is not a multiple of the lot size, or above the maximum.
    InvalidQty,
    /// Price is zero, above the maximum, or not a multiple of the tick size.
    InvalidPrice,
    /// Order notional is below the market minimum.
    BelowMinNotional,
    /// Slippage parameter out of range.
    InvalidSlippage,
    /// Limit price is further from the mark than the market's price band allows.
    PriceOutsideBand,
    /// Post-only order would have matched.
    WouldCross,
    /// Fill-or-kill order cannot be filled completely.
    FokNotFillable,
    /// Market order found nothing to trade against inside its slippage limit.
    NoLiquidity,
    /// Reduce-only order would not reduce the position.
    ReduceOnlyViolation,
    /// Order would breach the initial margin requirement.
    MarginBreach,
    /// Order or fill would exceed the maximum position.
    PositionLimit,
    /// Withdrawal larger than the account collateral.
    InsufficientCollateral,
    /// No mark price has been received for the market yet.
    NoMarkPrice,
    /// Mark price is older than the staleness limit.
    StaleOracle,
    /// Oracle update is older than the previous one or already stale.
    OracleRegression,
    /// Oracle price is zero or out of range.
    OracleBadPrice,
    /// Event timestamp is earlier than the engine clock.
    TimeRegression,
    /// Fee tier index does not exist.
    UnknownFeeTier,
    /// The engine stopped after detecting an internal inconsistency.
    Halted,
    /// Internal consistency check failed; the engine halts (fail-stop).
    Internal(&'static str),
}

impl Error {
    /// Stable short code, used by the CLI and the TCP server.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Overflow => "overflow",
            Error::DivisionByZero => "division_by_zero",
            Error::NotConfigured => "not_configured",
            Error::AlreadyConfigured => "already_configured",
            Error::InvalidConfig(_) => "invalid_config",
            Error::UnknownMarket => "unknown_market",
            Error::MarketExists => "market_exists",
            Error::TooManyMarkets => "too_many_markets",
            Error::UnknownAccount => "unknown_account",
            Error::TooManyAccounts => "too_many_accounts",
            Error::UnknownOrder => "unknown_order",
            Error::DuplicateClientOrderId => "duplicate_client_order_id",
            Error::TooManyOrders => "too_many_orders",
            Error::BookFull => "book_full",
            Error::ZeroAmount => "zero_amount",
            Error::AmountTooLarge => "amount_too_large",
            Error::ZeroQty => "zero_qty",
            Error::InvalidQty => "invalid_qty",
            Error::InvalidPrice => "invalid_price",
            Error::BelowMinNotional => "below_min_notional",
            Error::InvalidSlippage => "invalid_slippage",
            Error::PriceOutsideBand => "price_outside_band",
            Error::WouldCross => "would_cross",
            Error::FokNotFillable => "fok_not_fillable",
            Error::NoLiquidity => "no_liquidity",
            Error::ReduceOnlyViolation => "reduce_only_violation",
            Error::MarginBreach => "margin_breach",
            Error::PositionLimit => "position_limit",
            Error::InsufficientCollateral => "insufficient_collateral",
            Error::NoMarkPrice => "no_mark_price",
            Error::StaleOracle => "stale_oracle",
            Error::OracleRegression => "oracle_regression",
            Error::OracleBadPrice => "oracle_bad_price",
            Error::TimeRegression => "time_regression",
            Error::UnknownFeeTier => "unknown_fee_tier",
            Error::Halted => "halted",
            Error::Internal(_) => "internal",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidConfig(why) | Error::Internal(why) => {
                write!(f, "{}: {}", self.code(), why)
            }
            _ => f.write_str(self.code()),
        }
    }
}

impl std::error::Error for Error {}

/// Result alias used across the engine.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn all() -> Vec<Error> {
        vec![
            Error::Overflow,
            Error::DivisionByZero,
            Error::NotConfigured,
            Error::AlreadyConfigured,
            Error::InvalidConfig("x"),
            Error::UnknownMarket,
            Error::MarketExists,
            Error::TooManyMarkets,
            Error::UnknownAccount,
            Error::TooManyAccounts,
            Error::UnknownOrder,
            Error::DuplicateClientOrderId,
            Error::TooManyOrders,
            Error::BookFull,
            Error::ZeroAmount,
            Error::AmountTooLarge,
            Error::ZeroQty,
            Error::InvalidQty,
            Error::InvalidPrice,
            Error::BelowMinNotional,
            Error::InvalidSlippage,
            Error::PriceOutsideBand,
            Error::WouldCross,
            Error::FokNotFillable,
            Error::NoLiquidity,
            Error::ReduceOnlyViolation,
            Error::MarginBreach,
            Error::PositionLimit,
            Error::InsufficientCollateral,
            Error::NoMarkPrice,
            Error::StaleOracle,
            Error::OracleRegression,
            Error::OracleBadPrice,
            Error::TimeRegression,
            Error::UnknownFeeTier,
            Error::Halted,
            Error::Internal("y"),
        ]
    }

    #[test]
    fn every_error_has_a_unique_snake_case_code() {
        let codes: BTreeSet<&str> = all().iter().map(Error::code).collect();
        assert_eq!(codes.len(), all().len());
        assert!(codes
            .iter()
            .all(|c| c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')));
    }

    #[test]
    fn display_is_the_code_and_adds_detail_only_where_there_is_one() {
        assert_eq!(Error::MarginBreach.to_string(), "margin_breach");
        assert_eq!(
            Error::InvalidConfig("tick").to_string(),
            "invalid_config: tick"
        );
        assert_eq!(Error::Internal("boom").to_string(), "internal: boom");
    }
}
