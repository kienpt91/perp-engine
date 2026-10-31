//! Fixed-point scales, newtypes and checked wide-integer helpers.
//!
//! There are no floating point numbers anywhere in the engine.
//!
//! | quantity | type    | raw unit                                   |
//! |----------|---------|--------------------------------------------|
//! | price    | [`Price`] (`u64`) | 1e-6 quote currency per 1 base unit |
//! | quantity | [`Qty`] (`u64`)   | 1e-4 base units                      |
//! | money    | [`Money`] (`i128`)| 1e-6 quote currency (micro-units)    |
//!
//! Notional (money) of `qty` at `price` is `price_raw * qty_raw / QTY_SCALE`.
//! Because the price scale equals the money scale, no further conversion is
//! needed. Rates are integers: basis points (1e-4) for margin and slippage,
//! parts per million (1e-6) for fees, parts per billion (1e-9) for funding.
//!
//! # Rounding rules
//!
//! Every division names its [`Round`] mode. The policy is *always round in the
//! exchange's favour*:
//!
//! * fees charged to a trader: [`Round::Ceil`]
//! * maker rebates paid to a trader: magnitude [`Round::Floor`]
//! * margin requirements: [`Round::Ceil`]
//! * position value for unrealised PnL: [`Round::Floor`] (signed, so gains
//!   shrink and losses grow)
//! * cost-basis released when a position shrinks: [`Round::Ceil`]

use crate::error::{Error, Result};

/// Raw price units per 1.0 quote currency.
pub const PRICE_SCALE: u64 = 1_000_000;
/// Raw quantity units per 1.0 base unit.
pub const QTY_SCALE: u64 = 10_000;
/// Raw money units per 1.0 quote currency.
pub const MONEY_SCALE: u64 = 1_000_000;
/// Basis points per 1.0.
pub const BPS_SCALE: u64 = 10_000;
/// Parts per million per 1.0 (fees).
pub const PPM_SCALE: u64 = 1_000_000;
/// Parts per billion per 1.0 (funding rates).
pub const RATE_SCALE: u64 = 1_000_000_000;

/// Largest accepted price (10 million quote currency per base unit).
pub const MAX_PRICE: u64 = 10_000_000_000_000;
/// Largest accepted order quantity and position size (1e8 base units).
pub const MAX_QTY: u64 = 1_000_000_000_000;
/// Largest accepted single money amount (deposit, withdrawal).
pub const MAX_AMOUNT: i128 = 1_000_000_000_000_000_000_000_000;
/// Largest accepted account collateral.
pub const MAX_BALANCE: i128 = 1_000_000_000_000_000_000_000_000_000;

/// Rounding direction of an integer division (towards -inf / +inf).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    /// Towards negative infinity.
    Floor,
    /// Towards positive infinity.
    Ceil,
}

/// Price in raw units (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(pub u64);

/// Quantity in raw units (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Qty(pub u64);

/// Signed amount of quote currency in raw units (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Money(pub i128);

impl Price {
    /// Validates `0 < raw <= MAX_PRICE`.
    pub fn new(raw: u64) -> Result<Price> {
        if raw == 0 || raw > MAX_PRICE {
            Err(Error::InvalidPrice)
        } else {
            Ok(Price(raw))
        }
    }
}

impl Qty {
    /// Validates `0 < raw <= MAX_QTY`.
    pub fn new(raw: u64) -> Result<Qty> {
        if raw == 0 {
            Err(Error::ZeroQty)
        } else if raw > MAX_QTY {
            Err(Error::InvalidQty)
        } else {
            Ok(Qty(raw))
        }
    }
}

impl Money {
    /// Zero.
    pub const ZERO: Money = Money(0);

    /// Checked addition.
    pub fn checked_add(self, other: Money) -> Result<Money> {
        self.0
            .checked_add(other.0)
            .map(Money)
            .ok_or(Error::Overflow)
    }

    /// Checked subtraction.
    pub fn checked_sub(self, other: Money) -> Result<Money> {
        self.0
            .checked_sub(other.0)
            .map(Money)
            .ok_or(Error::Overflow)
    }

    /// Checked negation.
    pub fn checked_neg(self) -> Result<Money> {
        self.0.checked_neg().map(Money).ok_or(Error::Overflow)
    }
}

/// `floor(a / b)` for signed integers; `b` must be non-zero.
pub fn floor_div(a: i128, b: i128) -> Result<i128> {
    if b == 0 {
        return Err(Error::DivisionByZero);
    }
    let q = a.checked_div(b).ok_or(Error::Overflow)?;
    let r = a.checked_rem(b).ok_or(Error::Overflow)?;
    if r != 0 && ((r < 0) != (b < 0)) {
        q.checked_sub(1).ok_or(Error::Overflow)
    } else {
        Ok(q)
    }
}

/// `ceil(a / b)` for signed integers; `b` must be non-zero.
pub fn ceil_div(a: i128, b: i128) -> Result<i128> {
    if b == 0 {
        return Err(Error::DivisionByZero);
    }
    let q = a.checked_div(b).ok_or(Error::Overflow)?;
    let r = a.checked_rem(b).ok_or(Error::Overflow)?;
    if r != 0 && ((r < 0) == (b < 0)) {
        q.checked_add(1).ok_or(Error::Overflow)
    } else {
        Ok(q)
    }
}

/// Division with an explicit rounding mode.
pub fn div_round(a: i128, b: i128, round: Round) -> Result<i128> {
    match round {
        Round::Floor => floor_div(a, b),
        Round::Ceil => ceil_div(a, b),
    }
}

/// `a * b / d` with a checked 128-bit intermediate and explicit rounding.
pub fn mul_div(a: i128, b: i128, d: i128, round: Round) -> Result<i128> {
    let p = a.checked_mul(b).ok_or(Error::Overflow)?;
    div_round(p, d, round)
}

/// Money value of `qty` at `price`: `price * qty / QTY_SCALE`.
pub fn notional(price: Price, qty: Qty, round: Round) -> Result<Money> {
    notional_raw(price.0, qty.0, round)
}

/// [`notional`] on raw integers.
pub fn notional_raw(price: u64, qty: u64, round: Round) -> Result<Money> {
    mul_div(
        i128::from(price),
        i128::from(qty),
        i128::from(QTY_SCALE),
        round,
    )
    .map(Money)
}

/// Signed position value: `size * price / QTY_SCALE`, rounded towards -inf.
///
/// Because the result is floored, long gains shrink and short gains shrink
/// too (a short's value is negative, flooring makes it more negative).
pub fn signed_value(size: i64, price: Price) -> Result<Money> {
    mul_div(
        i128::from(size),
        i128::from(price.0),
        i128::from(QTY_SCALE),
        Round::Floor,
    )
    .map(Money)
}

/// `amount * bps / 10_000` with explicit rounding (`amount >= 0` expected).
pub fn bps_of(amount: Money, bps: u32, round: Round) -> Result<Money> {
    mul_div(amount.0, i128::from(bps), i128::from(BPS_SCALE), round).map(Money)
}

/// `amount * ppm / 1_000_000` with explicit rounding.
pub fn ppm_of(amount: Money, ppm: u32, round: Round) -> Result<Money> {
    mul_div(amount.0, i128::from(ppm), i128::from(PPM_SCALE), round).map(Money)
}

/// Absolute value of a position size as `u64`.
pub fn abs_size(size: i64) -> u64 {
    size.unsigned_abs()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn floor_div_handles_signs() {
        assert_eq!(floor_div(7, 2).unwrap(), 3);
        assert_eq!(floor_div(-7, 2).unwrap(), -4);
        assert_eq!(floor_div(7, -2).unwrap(), -4);
        assert_eq!(floor_div(-7, -2).unwrap(), 3);
        assert_eq!(floor_div(-8, 2).unwrap(), -4);
    }

    #[test]
    fn ceil_div_handles_signs() {
        assert_eq!(ceil_div(7, 2).unwrap(), 4);
        assert_eq!(ceil_div(-7, 2).unwrap(), -3);
        assert_eq!(ceil_div(7, -2).unwrap(), -3);
        assert_eq!(ceil_div(-7, -2).unwrap(), 4);
        assert_eq!(ceil_div(8, 2).unwrap(), 4);
    }

    #[test]
    fn division_by_zero_is_an_error() {
        assert_eq!(floor_div(1, 0), Err(Error::DivisionByZero));
        assert_eq!(ceil_div(1, 0), Err(Error::DivisionByZero));
    }

    #[test]
    fn min_over_minus_one_is_overflow_not_panic() {
        assert_eq!(floor_div(i128::MIN, -1), Err(Error::Overflow));
        assert_eq!(ceil_div(i128::MIN, -1), Err(Error::Overflow));
    }

    #[test]
    fn mul_div_overflow_is_reported() {
        assert_eq!(mul_div(i128::MAX, 2, 1, Round::Floor), Err(Error::Overflow));
    }

    #[test]
    fn notional_scales_correctly() {
        // 1.0 base at 50_000.0 quote == 50_000.0 quote
        let n = notional(Price(50_000 * PRICE_SCALE), Qty(QTY_SCALE), Round::Floor).unwrap();
        assert_eq!(n, Money(50_000 * i128::from(MONEY_SCALE)));
    }

    #[test]
    fn notional_rounding_modes_differ_on_remainder() {
        let lo = notional(Price(3), Qty(1), Round::Floor).unwrap();
        let hi = notional(Price(3), Qty(1), Round::Ceil).unwrap();
        assert_eq!(lo, Money(0));
        assert_eq!(hi, Money(1));
    }

    #[test]
    fn notional_extreme_values_fit() {
        let n = notional(Price(MAX_PRICE), Qty(MAX_QTY), Round::Ceil).unwrap();
        assert!(n.0 > 0);
    }

    #[test]
    fn signed_value_floors_both_signs() {
        // 0.0001 * 3 raw price units = 0.0003 -> floor 0; short = -0.0003 -> floor -1
        assert_eq!(signed_value(1, Price(3)).unwrap(), Money(0));
        assert_eq!(signed_value(-1, Price(3)).unwrap(), Money(-1));
    }

    #[test]
    fn fee_rounds_up_and_rebate_rounds_down() {
        assert_eq!(ppm_of(Money(1), 1, Round::Ceil).unwrap(), Money(1));
        assert_eq!(ppm_of(Money(1), 1, Round::Floor).unwrap(), Money(0));
    }

    #[test]
    fn bps_of_rounds_as_requested() {
        assert_eq!(bps_of(Money(10_001), 1, Round::Floor).unwrap(), Money(1));
        assert_eq!(bps_of(Money(10_001), 1, Round::Ceil).unwrap(), Money(2));
    }

    #[test]
    fn constructors_validate_ranges() {
        assert_eq!(Price::new(0), Err(Error::InvalidPrice));
        assert_eq!(Price::new(MAX_PRICE + 1), Err(Error::InvalidPrice));
        assert!(Price::new(MAX_PRICE).is_ok());
        assert_eq!(Qty::new(0), Err(Error::ZeroQty));
        assert_eq!(Qty::new(MAX_QTY + 1), Err(Error::InvalidQty));
        assert!(Qty::new(1).is_ok());
    }

    #[test]
    fn money_checked_ops() {
        assert_eq!(Money(i128::MAX).checked_add(Money(1)), Err(Error::Overflow));
        assert_eq!(Money(i128::MIN).checked_sub(Money(1)), Err(Error::Overflow));
        assert_eq!(Money(i128::MIN).checked_neg(), Err(Error::Overflow));
        assert_eq!(Money(5).checked_sub(Money(7)).unwrap(), Money(-2));
    }

    #[test]
    fn abs_size_of_min_is_defined() {
        assert_eq!(abs_size(i64::MIN), 1u64 << 63);
        assert_eq!(abs_size(-5), 5);
    }

    #[test]
    fn no_overflow_bound_for_funding_numerators() {
        // size * price * rate must fit i128 at the documented caps
        let n = u128::from(MAX_QTY) * u128::from(MAX_PRICE) * u128::from(RATE_SCALE);
        assert!(n < i128::MAX as u128);
    }
}
