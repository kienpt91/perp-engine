//! Funding: premium-index based, clamped, settled exactly.
//!
//! # Rules
//!
//! * Each oracle update adds a premium sample (see `pricing`). When time passes
//!   a market's `next_funding_ts` (checked on `Oracle` and `Tick` events), the
//!   rate is `clamp(mean premium + interest, -cap, +cap)` in ppb per interval.
//!   Intervals that elapse without any event are not back-filled: the next
//!   boundary is the first multiple of the interval after the event time.
//! * Positive rate: longs pay shorts. A position pays or receives
//!   `|size| * mark / QTY_SCALE * |rate| / RATE_SCALE`.
//! * **Exactness.** Payers and receivers have the same total exact amount
//!   (position sizes sum to zero). The integer total transferred is the floor of
//!   that amount plus the carried fraction; the fractional rest is carried to the
//!   next interval (`funding_carry`, a numerator over `QTY_SCALE * RATE_SCALE`).
//!   The integer total is split among payers and, separately, among receivers
//!   with the largest-remainder method (ties by account id), so
//!   `sum(paid) == sum(received)` exactly and no money is created or lost.

use super::{Engine, Halt};
use crate::error::{Error, Result};
use crate::fixed::{floor_div, Money, QTY_SCALE, RATE_SCALE};
use crate::types::{AccountId, MarketId, Millis, Output};

/// Split `total` units among `shares` (account, numerator) proportionally to
/// the numerators using integer division by `d` plus largest remainder.
/// The returned amounts sum to exactly `total`.
pub(crate) fn distribute(
    shares: &[(AccountId, u128)],
    total: u128,
    d: u128,
) -> Result<Vec<(AccountId, u128)>> {
    if d == 0 {
        return Err(Error::DivisionByZero);
    }
    let base_sum = shares
        .iter()
        .try_fold(0u128, |acc, (_, n)| acc.checked_add(n / d))
        .ok_or(Error::Overflow)?;
    let leftover = total
        .checked_sub(base_sum)
        .ok_or(Error::Internal("funding split underflow"))?;
    if shares.is_empty() {
        return if leftover == 0 {
            Ok(Vec::new())
        } else {
            Err(Error::Internal("funding without parties"))
        };
    }
    let n = shares.len() as u128;
    let each = leftover / n;
    let extra = usize::try_from(leftover % n).map_err(|_| Error::Overflow)?;
    let mut order: Vec<usize> = (0..shares.len()).collect();
    order.sort_by_key(|i| {
        let (id, num) = shares.get(*i).copied().unwrap_or((0, 0));
        (std::cmp::Reverse(num % d), id)
    });
    let mut amounts: Vec<(AccountId, u128)> = shares
        .iter()
        .map(|(id, num)| (*id, num / d + each))
        .collect();
    for i in order.into_iter().take(extra) {
        if let Some(slot) = amounts.get_mut(i) {
            slot.1 += 1;
        }
    }
    Ok(amounts)
}

impl Engine {
    /// Settle funding for every market whose boundary has passed.
    pub(super) fn advance_funding(&mut self, ts: Millis, out: &mut Vec<Output>) -> Result<()> {
        let due: Vec<MarketId> = self
            .markets
            .iter()
            .filter(|(_, m)| ts >= m.next_funding_ts)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.settle_funding(id, ts, out)?;
        }
        Ok(())
    }

    fn settle_funding(&mut self, id: MarketId, ts: Millis, out: &mut Vec<Output>) -> Result<()> {
        let m = self.market_mut(id)?;
        let interval = m.cfg.funding_interval_ms;
        m.next_funding_ts = (ts / interval)
            .checked_add(1)
            .and_then(|n| n.checked_mul(interval))
            .ok_or(Error::Overflow)?;
        let mean = if m.premium_count > 0 {
            floor_div(m.premium_sum, i128::from(m.premium_count))?
        } else {
            0
        };
        m.premium_sum = 0;
        m.premium_count = 0;
        let Some(mark) = m.mark else {
            return Ok(());
        };
        let cap = i128::from(m.cfg.funding_cap_ppb);
        let rate = mean
            .saturating_add(i128::from(m.cfg.funding_interest_ppb))
            .clamp(-cap, cap);
        let rate = i64::try_from(rate).map_err(|_| Error::Overflow)?;
        m.last_funding_rate = rate;
        if rate == 0 {
            return Ok(());
        }

        let longs_pay = rate > 0;
        let rate_abs = u128::from(rate.unsigned_abs());
        let mut payers: Vec<(AccountId, u128)> = Vec::new();
        let mut receivers: Vec<(AccountId, u128)> = Vec::new();
        let (mut n_pay, mut n_recv) = (0u128, 0u128);
        let holders: Vec<AccountId> = m.holders.iter().copied().collect();
        for h in holders {
            let size = self.accounts.get(&h).map_or(0, |a| a.size_in(id));
            let num = u128::from(size.unsigned_abs())
                .checked_mul(u128::from(mark.0))
                .and_then(|v| v.checked_mul(rate_abs))
                .ok_or(Error::Overflow)?;
            if (size > 0) == longs_pay {
                n_pay = n_pay.checked_add(num).ok_or(Error::Overflow)?;
                payers.push((h, num));
            } else {
                n_recv = n_recv.checked_add(num).ok_or(Error::Overflow)?;
                receivers.push((h, num));
            }
        }
        if n_pay != n_recv {
            return Err(self.halt(Halt::Funding));
        }
        let d = u128::from(QTY_SCALE) * u128::from(RATE_SCALE);
        let carry = self.market_mut(id)?.funding_carry;
        let pool = n_pay.checked_add(carry).ok_or(Error::Overflow)?;
        let total = pool / d;
        let new_carry = pool % d;
        let pay = distribute(&payers, total, d)?;
        let recv = distribute(&receivers, total, d)?;
        for (acct, amt) in pay {
            let amt = i128::try_from(amt).map_err(|_| Error::Overflow)?;
            let a = self.acct_mut(acct)?;
            a.collateral = a.collateral.checked_sub(amt).ok_or(Error::Overflow)?;
            self.pending_liq.insert(acct);
        }
        for (acct, amt) in recv {
            let amt = i128::try_from(amt).map_err(|_| Error::Overflow)?;
            let a = self.acct_mut(acct)?;
            a.collateral = a.collateral.checked_add(amt).ok_or(Error::Overflow)?;
        }
        self.market_mut(id)?.funding_carry = new_carry;
        out.push(Output::FundingSettled {
            market: id,
            rate_ppb: rate,
            transferred: Money(i128::try_from(total).map_err(|_| Error::Overflow)?),
            carry: new_carry,
        });
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn distribution_sums_exactly_to_total() {
        let shares = vec![(1, 5u128), (2, 5), (3, 5)];
        let d = 10;
        // exact amounts 0.5 each, total 1 (floor of 1.5)
        let out = distribute(&shares, 1, d).unwrap();
        assert_eq!(out.iter().map(|(_, a)| *a).sum::<u128>(), 1);
        // ties broken by account id: lowest id gets the unit
        assert_eq!(out, vec![(1, 1), (2, 0), (3, 0)]);
    }

    #[test]
    fn largest_remainder_wins() {
        let shares = vec![(7, 14u128), (8, 19), (9, 11)];
        let out = distribute(&shares, 4, 10).unwrap();
        // bases 1,1,1; leftover 1 goes to the largest remainder (19 % 10 = 9)
        assert_eq!(out, vec![(7, 1), (8, 2), (9, 1)]);
    }

    #[test]
    fn leftover_larger_than_party_count_is_spread() {
        let shares = vec![(1, 0u128), (2, 0)];
        let out = distribute(&shares, 5, 10).unwrap();
        assert_eq!(out.iter().map(|(_, a)| *a).sum::<u128>(), 5);
    }

    #[test]
    fn no_parties_requires_zero_total() {
        assert!(distribute(&[], 0, 10).unwrap().is_empty());
        assert!(distribute(&[], 1, 10).is_err());
    }

    #[test]
    fn total_below_base_sum_is_an_error() {
        assert!(distribute(&[(1, 100)], 1, 10).is_err());
        assert_eq!(distribute(&[(1, 1)], 1, 0), Err(Error::DivisionByZero));
    }
}
