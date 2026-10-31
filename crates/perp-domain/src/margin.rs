//! Tiered margin schedule.
//!
//! The schedule is *marginal* (like tax brackets): only the slice of a
//! position's notional that falls inside a bracket is charged that bracket's
//! rate, so the requirement is continuous in the notional. Every slice is
//! rounded up. The initial requirement is never below `notional / max_leverage`
//! (rounded up).

use crate::error::{Error, Result};
use crate::fixed::{bps_of, div_round, Money, Round};
use crate::types::MarketConfig;

/// Which requirement to compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginKind {
    /// Required to open or increase a position, and to withdraw.
    Initial,
    /// Below this equity the account is liquidated.
    Maintenance,
}

/// Margin required for a position with absolute notional `notional`.
pub fn required_margin(cfg: &MarketConfig, notional: Money, kind: MarginKind) -> Result<Money> {
    if notional.0 < 0 {
        return Err(Error::Internal("negative notional"));
    }
    let mut total = Money::ZERO;
    let mut lo = 0i128;
    for tier in &cfg.tiers {
        if notional.0 <= lo {
            break;
        }
        let hi = tier.up_to.0.min(notional.0);
        let slice = Money(hi.checked_sub(lo).ok_or(Error::Overflow)?);
        let rate = match kind {
            MarginKind::Initial => tier.imr_bps,
            MarginKind::Maintenance => tier.mmr_bps,
        };
        total = total.checked_add(bps_of(slice, rate, Round::Ceil)?)?;
        lo = tier.up_to.0;
    }
    if kind == MarginKind::Initial {
        let floor_by_leverage =
            div_round(notional.0, i128::from(cfg.max_leverage.max(1)), Round::Ceil)?;
        if floor_by_leverage > total.0 {
            total = Money(floor_by_leverage);
        }
    }
    Ok(total)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn cfg() -> MarketConfig {
        MarketConfig::example(1)
    }

    #[test]
    fn first_bracket_uses_first_rate() {
        let c = cfg();
        // 100_000_000 notional at 5% initial, 2.5% maintenance
        assert_eq!(
            required_margin(&c, Money(100_000_000), MarginKind::Initial).unwrap(),
            Money(5_000_000)
        );
        assert_eq!(
            required_margin(&c, Money(100_000_000), MarginKind::Maintenance).unwrap(),
            Money(2_500_000)
        );
    }

    #[test]
    fn brackets_are_marginal_and_continuous() {
        let c = cfg();
        let edge = c.tiers[0].up_to.0;
        let at = required_margin(&c, Money(edge), MarginKind::Initial)
            .unwrap()
            .0;
        let above = required_margin(&c, Money(edge + 1), MarginKind::Initial)
            .unwrap()
            .0;
        assert!(
            above >= at && above - at <= 1,
            "no cliff at the bracket edge: {at} -> {above}"
        );
        // 100 above the edge costs 10% of 100 = 10 on top
        let ten = required_margin(&c, Money(edge + 100), MarginKind::Initial)
            .unwrap()
            .0;
        assert_eq!(ten, at + 10);
    }

    #[test]
    fn requirement_is_monotone() {
        let c = cfg();
        let mut prev = 0;
        for n in (0..2_000_000_000_000i128).step_by(97_000_000_011) {
            let m = required_margin(&c, Money(n), MarginKind::Initial)
                .unwrap()
                .0;
            assert!(m >= prev);
            prev = m;
        }
    }

    #[test]
    fn rounds_up() {
        let c = cfg();
        // 5% of 1 is 0.05 -> rounds up to 1
        assert_eq!(
            required_margin(&c, Money(1), MarginKind::Maintenance).unwrap(),
            Money(1)
        );
        assert_eq!(
            required_margin(&c, Money(0), MarginKind::Initial).unwrap(),
            Money(0)
        );
    }

    #[test]
    fn leverage_floor_applies_to_initial_only() {
        let mut c = cfg();
        c.max_leverage = 10; // 10% > first tier's 5%
        let n = Money(100_000_000);
        assert_eq!(
            required_margin(&c, n, MarginKind::Initial).unwrap(),
            Money(10_000_000)
        );
        assert_eq!(
            required_margin(&c, n, MarginKind::Maintenance).unwrap(),
            Money(2_500_000)
        );
    }

    #[test]
    fn maintenance_below_initial() {
        let c = cfg();
        for n in [
            1i128,
            1_000,
            10_000_000_000,
            9_000_000_000_000,
            1_000_000_000_000_000,
        ] {
            let i = required_margin(&c, Money(n), MarginKind::Initial).unwrap();
            let m = required_margin(&c, Money(n), MarginKind::Maintenance).unwrap();
            assert!(m <= i, "n={n}");
        }
    }

    #[test]
    fn negative_notional_is_an_error() {
        assert!(required_margin(&cfg(), Money(-1), MarginKind::Initial).is_err());
    }

    #[test]
    fn huge_notional_does_not_overflow() {
        let r = required_margin(&cfg(), Money(10_i128.pow(30)), MarginKind::Initial).unwrap();
        assert!(r.0 > 0);
    }
}
