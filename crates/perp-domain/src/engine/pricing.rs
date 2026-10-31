//! Oracle input handling: staleness, bounded change filter, mark price.
//!
//! # Rules
//!
//! * An update carries the index price and the time the source observed it
//!   (`source_ts`). It is rejected if `source_ts` is in the future, older than
//!   the staleness limit, or not newer than the previous accepted update.
//! * **Bounded change filter**: the accepted index never moves more than
//!   `oracle_max_change_bps` from the previous index in one update. A larger
//!   jump is clamped to the bound (the update is still accepted and flagged
//!   `clamped`), so a single bad print cannot move margin by more than the bound.
//! * **Mark price** = median of (index, book mid, last trade), where a missing
//!   input counts as the index, clamped to `max_mark_deviation_bps` around the
//!   index.
//! * Each accepted update also records a premium sample
//!   `(mid - index) / index` (zero when the book is one-sided) for funding.

use super::Engine;
use crate::error::{Error, Result};
use crate::fixed::{floor_div, mul_div, Price, Round, BPS_SCALE, MAX_PRICE, RATE_SCALE};
use crate::types::{MarketId, Millis, Output, Side};

fn bps_factor(plus: bool, bps: u32) -> Result<i128> {
    let base = i128::from(BPS_SCALE);
    let b = i128::from(bps);
    if plus {
        base.checked_add(b).ok_or(Error::Overflow)
    } else {
        base.checked_sub(b).ok_or(Error::Overflow)
    }
}

fn median3(a: u64, b: u64, c: u64) -> u64 {
    a.max(b).min(a.min(b).max(c))
}

impl Engine {
    pub(super) fn oracle(
        &mut self,
        ts: Millis,
        market: MarketId,
        index_price: Price,
        source_ts: Millis,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        let m = self.markets.get(&market).ok_or(Error::UnknownMarket)?;
        if index_price.0 == 0 || index_price.0 > MAX_PRICE {
            return Err(Error::OracleBadPrice);
        }
        if source_ts > ts {
            return Err(Error::OracleRegression);
        }
        if ts - source_ts > m.cfg.oracle_max_staleness_ms {
            return Err(Error::StaleOracle);
        }
        if m.index.is_some() && source_ts <= m.oracle_ts {
            return Err(Error::OracleRegression);
        }

        let (index, clamped) = match m.index {
            None => (index_price, false),
            Some(prev) => {
                let b = m.cfg.oracle_max_change_bps;
                let hi = mul_div(
                    i128::from(prev.0),
                    bps_factor(true, b)?,
                    i128::from(BPS_SCALE),
                    Round::Floor,
                )?;
                let lo = mul_div(
                    i128::from(prev.0),
                    bps_factor(false, b)?,
                    i128::from(BPS_SCALE),
                    Round::Ceil,
                )?;
                let want = i128::from(index_price.0);
                let got = want.clamp(lo, hi);
                let got = u64::try_from(got).map_err(|_| Error::Overflow)?;
                (Price(got.clamp(1, MAX_PRICE)), got != index_price.0)
            }
        };

        let mid = match (m.book.best(Side::Buy), m.book.best(Side::Sell)) {
            (Some(b), Some(a)) => {
                let sum = u128::from(b.0) + u128::from(a.0);
                Some(u64::try_from(sum / 2).map_err(|_| Error::Overflow)?)
            }
            _ => None,
        };
        let med = median3(
            index.0,
            mid.unwrap_or(index.0),
            m.last_trade.map_or(index.0, |p| p.0),
        );
        let dev = mul_div(
            i128::from(index.0),
            i128::from(m.cfg.max_mark_deviation_bps),
            i128::from(BPS_SCALE),
            Round::Ceil,
        )?;
        let dev = u64::try_from(dev).map_err(|_| Error::Overflow)?;
        let mark = Price(med.clamp(
            index.0.saturating_sub(dev).max(1),
            index.0.saturating_add(dev).min(MAX_PRICE),
        ));
        let premium = match mid {
            None => 0,
            Some(mid) => {
                let diff = i128::from(mid) - i128::from(index.0);
                let p = floor_div(
                    diff.checked_mul(i128::from(RATE_SCALE))
                        .ok_or(Error::Overflow)?,
                    i128::from(index.0),
                )?;
                p.clamp(-i128::from(RATE_SCALE), i128::from(RATE_SCALE))
            }
        };

        self.begin(ts);
        self.advance_funding(ts, out)?;
        let m = self.market_mut(market)?;
        m.index = Some(index);
        m.mark = Some(mark);
        m.oracle_ts = source_ts;
        m.premium_sum = m.premium_sum.checked_add(premium).ok_or(Error::Overflow)?;
        m.premium_count = m.premium_count.checked_add(1).ok_or(Error::Overflow)?;
        let holders: Vec<_> = m.holders.iter().copied().collect();
        self.pending_liq.extend(holders);
        out.push(Output::MarkUpdated {
            market,
            index,
            mark,
            clamped,
        });
        Ok(())
    }

    pub(super) fn tick(&mut self, ts: Millis, out: &mut Vec<Output>) -> Result<()> {
        self.begin(ts);
        self.advance_funding(ts, out)?;
        let all: Vec<_> = self
            .markets
            .values()
            .flat_map(|m| m.holders.iter().copied())
            .collect();
        self.pending_liq.extend(all);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::median3;

    #[test]
    fn median_of_three_for_all_orderings() {
        for (a, b, c) in [
            (1, 2, 3),
            (1, 3, 2),
            (2, 1, 3),
            (2, 3, 1),
            (3, 1, 2),
            (3, 2, 1),
        ] {
            assert_eq!(median3(a, b, c), 2);
        }
        assert_eq!(median3(5, 5, 1), 5);
        assert_eq!(median3(7, 7, 7), 7);
    }
}
