//! Equity, margin checks, liquidation, insurance fund and auto-deleveraging.
//!
//! # Liquidation rules (documented behaviour)
//!
//! An account is liquidatable when its equity at mark is below its summed
//! maintenance margin requirement. It is checked after every fill that touches
//! it and after every oracle update and tick. Checks are queued in
//! `pending_liq`; at most `max_liquidations_per_event` liquidation steps run per
//! event and the rest wait for the next event. Liquidation is skipped while the
//! oracle of a position's market is stale.
//!
//! One step, for the account's lowest-numbered market with a position:
//!
//! 1. all resting orders of the account are cancelled;
//! 2. an immediate-or-cancel, reduce-only order for `liq_chunk_bps` of the
//!    position (rounded up to the lot, at most the whole position) is matched
//!    against the book;
//! 3. its limit price is the *liquidation price*: mark moved by `liq_band_bps`
//!    against the position, but never worse than the bankruptcy price (the price
//!    at which the account's remaining equity would reach zero, kept clear of
//!    fee and rounding dust);
//! 4. fills pay the liquidation fee (`liq_fee_ppm`, capped by what the account
//!    has left) to the insurance fund.
//!
//! If equity is already negative, book liquidation would lose more than the
//! account has. The insurance fund must then cover the deficit plus the worst
//! case band slippage; if it can, the chunk is sold into the book and the fund
//! pays the resulting shortfall. If it cannot, the fund pays what it has and the
//! whole position is closed by auto-deleveraging at the bankruptcy price against
//! opposite positions, ranked by profit ratio (highest first), then position
//! size, then account id. `bad_debt` records any loss that nobody could cover.

use super::orders::{band_price, Taker};
use super::{Engine, Halt, MAX_FILLS_PER_MATCH};
use crate::account::Account;
use crate::error::{Error, Result};
use crate::fixed::{
    bps_of, ceil_div, floor_div, notional, notional_raw, ppm_of, signed_value, Money, Price, Qty,
    Round, MAX_PRICE, QTY_SCALE,
};
use crate::margin::{required_margin, MarginKind};
use crate::types::{AccountId, CancelReason, MarketId, Millis, OrderId, Output, Side, StpMode};
use std::cmp::Reverse;
use std::collections::BTreeSet;

impl Engine {
    /// Operator kill switch: stop the engine. Later events return
    /// [`Error::Halted`]; the flag is part of the state hash and snapshots.
    pub fn emergency_halt(&mut self) {
        if self.halted.is_none() {
            self.halted = Some(Halt::Invariant);
        }
    }

    /// Mark price of a market, optionally requiring a fresh oracle.
    pub(super) fn mark_for(&self, market: MarketId, now: Millis, fresh: bool) -> Result<Price> {
        let m = self.markets.get(&market).ok_or(Error::UnknownMarket)?;
        let mark = m.mark.ok_or(Error::NoMarkPrice)?;
        if fresh && now.saturating_sub(m.oracle_ts) > m.cfg.oracle_max_staleness_ms {
            return Err(Error::StaleOracle);
        }
        Ok(mark)
    }

    /// Collateral plus unrealised PnL of all positions at mark.
    pub(super) fn equity_of(&self, acct: &Account, now: Millis, fresh: bool) -> Result<i128> {
        let mut equity = acct.collateral;
        for (market, pos) in &acct.positions {
            let mark = self.mark_for(*market, now, fresh)?;
            let value = signed_value(pos.size, mark)?;
            let upnl = value.0.checked_sub(pos.basis).ok_or(Error::Overflow)?;
            equity = equity.checked_add(upnl).ok_or(Error::Overflow)?;
        }
        Ok(equity)
    }

    /// Summed margin requirement over the account's markets. With
    /// `with_orders`, resting orders count at their worst case; `override_worst`
    /// replaces the worst-case size of one market (used for hypothetical orders).
    pub(super) fn margin_requirement(
        &self,
        acct: &Account,
        kind: MarginKind,
        with_orders: bool,
        now: Millis,
        fresh: bool,
        override_worst: Option<(MarketId, u64)>,
    ) -> Result<i128> {
        let mut ids: BTreeSet<MarketId> = acct.positions.keys().copied().collect();
        if with_orders {
            ids.extend(acct.exposure.keys().copied());
        }
        if let Some((m, _)) = override_worst {
            ids.insert(m);
        }
        let mut total = 0i128;
        for id in ids {
            let m = self.markets.get(&id).ok_or(Error::UnknownMarket)?;
            let worst = match override_worst {
                Some((om, w)) if om == id => w,
                _ if with_orders => {
                    let e = acct.exposure_in(id);
                    super::orders::worst_abs(i128::from(acct.size_in(id)), e.buy, e.sell)?
                }
                _ => acct.size_in(id).unsigned_abs(),
            };
            if worst == 0 {
                continue;
            }
            let mark = self.mark_for(id, now, fresh)?;
            let n = notional(mark, Qty(worst), Round::Ceil)?;
            total = total
                .checked_add(required_margin(&m.cfg, n, kind)?.0)
                .ok_or(Error::Overflow)?;
        }
        Ok(total)
    }

    /// Is the account below maintenance margin (and evaluable: fresh prices)?
    pub(super) fn is_liquidatable(&self, id: AccountId, now: Millis) -> Result<bool> {
        let Some(acct) = self.accounts.get(&id) else {
            return Ok(false);
        };
        if acct.positions.is_empty() {
            return Ok(false);
        }
        let equity = match self.equity_of(acct, now, true) {
            Ok(e) => e,
            Err(Error::StaleOracle | Error::NoMarkPrice) => return Ok(false),
            Err(e) => return Err(e),
        };
        let required =
            self.margin_requirement(acct, MarginKind::Maintenance, false, now, true, None)?;
        Ok(equity < required)
    }

    /// Process queued maintenance checks within the per-event budget.
    pub(super) fn run_liquidations(&mut self, out: &mut Vec<Output>) -> Result<()> {
        let budget = self
            .config
            .as_ref()
            .map_or(0, |c| c.max_liquidations_per_event);
        let now = self.clock;
        let mut steps = 0u32;
        while let Some(id) = self.pending_liq.pop_first() {
            let flat = self
                .accounts
                .get(&id)
                .is_none_or(|a| a.positions.is_empty());
            if flat {
                self.cover_shortfall(id, out)?;
                continue;
            }
            if !self.is_liquidatable(id, now)? {
                continue;
            }
            if steps >= budget {
                self.pending_liq.insert(id);
                break;
            }
            steps += 1;
            if self.liquidation_step(id, now, out)? {
                self.pending_liq.insert(id);
            }
        }
        Ok(())
    }

    /// Pay `amount` of an account's deficit: insurance fund first, then
    /// (loudly) bad debt.
    fn cover_deficit(&mut self, id: AccountId, amount: i128, out: &mut Vec<Output>) -> Result<()> {
        if amount <= 0 {
            return Ok(());
        }
        let pay = amount.min(self.insurance);
        self.insurance = self.insurance.checked_sub(pay).ok_or(Error::Overflow)?;
        let uncovered = amount - pay;
        self.bad_debt = self
            .bad_debt
            .checked_add(uncovered)
            .ok_or(Error::Overflow)?;
        let acct = self.acct_mut(id)?;
        acct.collateral = acct.collateral.checked_add(amount).ok_or(Error::Overflow)?;
        if pay > 0 {
            out.push(Output::InsurancePaid {
                account: id,
                amount: Money(pay),
            });
        }
        Ok(())
    }

    /// A flat account with negative collateral is a loss nobody owns: cover it.
    fn cover_shortfall(&mut self, id: AccountId, out: &mut Vec<Output>) -> Result<()> {
        let collateral = self.accounts.get(&id).map_or(0, |a| a.collateral);
        if collateral < 0 {
            self.cover_deficit(id, -collateral, out)?;
        }
        Ok(())
    }

    /// One liquidation step. Returns whether any position was reduced.
    fn liquidation_step(
        &mut self,
        id: AccountId,
        now: Millis,
        out: &mut Vec<Output>,
    ) -> Result<bool> {
        let acct = self
            .accounts
            .get(&id)
            .ok_or(Error::Internal("liquidating unknown account"))?;
        let (market, pos) = acct
            .positions
            .iter()
            .next()
            .map(|(m, p)| (*m, *p))
            .ok_or(Error::Internal("liquidating a flat account"))?;
        let equity = self.equity_of(acct, now, true)?;
        let mark = self.mark_for(market, now, true)?;
        let (chunk_bps, band_bps, liq_fee_ppm, lot) = {
            let c = &self.markets.get(&market).ok_or(Error::UnknownMarket)?.cfg;
            (c.liq_chunk_bps, c.liq_band_bps, c.liq_fee_ppm, c.lot)
        };
        out.push(Output::LiquidationStarted {
            account: id,
            equity: Money(equity),
        });
        self.liq_steps = self.liq_steps.saturating_add(1);

        let resting: Vec<(OrderId, MarketId)> = self
            .accounts
            .get(&id)
            .map(|a| a.orders.iter().map(|(o, m)| (*o, *m)).collect())
            .unwrap_or_default();
        for (order_id, m) in resting {
            let o = self.unrest(m, order_id)?;
            out.push(Output::Canceled {
                order_id: o.id,
                account: id,
                qty: Qty(o.qty),
                reason: CancelReason::Liquidation,
            });
        }

        let abs = pos.size.unsigned_abs();
        let long = pos.size > 0;
        let side = if long { Side::Sell } else { Side::Buy };
        let chunk = {
            let raw = ceil_div(i128::from(abs) * i128::from(chunk_bps), 10_000)?;
            let raw = u64::try_from(raw).map_err(|_| Error::Overflow)?;
            let lots = raw.div_ceil(lot).max(1);
            lots.checked_mul(lot).ok_or(Error::Overflow)?.min(abs)
        };
        let band_px = band_price(mark, band_bps, side)?;
        // Rounding dust: each fill may lose one unit on each side.
        let dust = i128::try_from(2 * MAX_FILLS_PER_MATCH).map_err(|_| Error::Overflow)?;

        let (limit, fee_ppm) = if equity >= 0 {
            let fee_reserve = ppm_of(
                notional(mark, Qty(abs), Round::Ceil)?,
                liq_fee_ppm,
                Round::Ceil,
            )?;
            let loss_budget = (equity - dust - fee_reserve.0).max(0);
            let drop = floor_div(
                loss_budget.saturating_mul(i128::from(QTY_SCALE)),
                i128::from(abs),
            )?;
            let drop = u64::try_from(drop).unwrap_or(u64::MAX);
            let limit = if long {
                Price(band_px.0.max(mark.0.saturating_sub(drop).max(1)))
            } else {
                Price(band_px.0.min(mark.0.saturating_add(drop).min(MAX_PRICE)))
            };
            (limit, liq_fee_ppm)
        } else {
            let deficit = -equity;
            let slip = bps_of(
                notional(mark, Qty(chunk), Round::Ceil)?,
                band_bps,
                Round::Ceil,
            )?;
            let allowance = deficit.saturating_add(slip.0).saturating_add(dust);
            if self.insurance < allowance {
                return self.deleverage(id, market, now, deficit, out);
            }
            (band_px, 0)
        };

        let taker_id = self.next_order_id;
        self.next_order_id = taker_id.checked_add(1).ok_or(Error::Overflow)?;
        let mut taker = Taker {
            id: taker_id,
            account: id,
            side,
            limit,
            remaining: chunk,
            reduce_only: true,
            stp: StpMode::CancelNewest,
            liquidation: true,
            liq_fee_ppm: fee_ppm,
        };
        let res = self.match_taker(market, &mut taker, out)?;
        if equity < 0 {
            let acct = self
                .accounts
                .get(&id)
                .ok_or(Error::Internal("account vanished"))?;
            let after = self.equity_of(acct, now, false)?;
            if after < 0 {
                self.cover_deficit(id, -after, out)?;
            }
        }
        if self
            .accounts
            .get(&id)
            .is_some_and(|a| a.positions.is_empty())
        {
            self.cover_shortfall(id, out)?;
        }
        Ok(res.filled > 0)
    }

    /// Insurance fund pays what it can, then the whole position is closed
    /// against opposite positions at the bankruptcy price.
    fn deleverage(
        &mut self,
        id: AccountId,
        market: MarketId,
        now: Millis,
        deficit: i128,
        out: &mut Vec<Output>,
    ) -> Result<bool> {
        let pay = deficit.min(self.insurance);
        if pay > 0 {
            self.insurance -= pay;
            let acct = self.acct_mut(id)?;
            acct.collateral = acct.collateral.checked_add(pay).ok_or(Error::Overflow)?;
            out.push(Output::InsurancePaid {
                account: id,
                amount: Money(pay),
            });
        }
        let remaining_deficit = deficit - pay;
        let pos = self
            .accounts
            .get(&id)
            .and_then(|a| a.positions.get(&market).copied())
            .ok_or(Error::Internal("deleverage without position"))?;
        let abs = pos.size.unsigned_abs();
        let long = pos.size > 0;
        let mark = self.mark_for(market, now, true)?;
        let per_unit = ceil_div(
            remaining_deficit.saturating_mul(i128::from(QTY_SCALE)),
            i128::from(abs),
        )?;
        let per_unit = u64::try_from(per_unit).unwrap_or(u64::MAX);
        let price = if long {
            Price(mark.0.saturating_add(per_unit).min(MAX_PRICE))
        } else {
            Price(mark.0.saturating_sub(per_unit).max(1))
        };
        let bankrupt_side = if long { Side::Sell } else { Side::Buy };
        let round = if long { Round::Ceil } else { Round::Floor };

        let mut left = abs;
        for cp in self.adl_ranking(market, !long, mark)? {
            if left == 0 {
                break;
            }
            let cp_abs = self
                .accounts
                .get(&cp)
                .map_or(0, |a| a.size_in(market).unsigned_abs());
            let q = cp_abs.min(left);
            let n = notional_raw(price.0, q, round)?;
            self.apply_position(id, market, bankrupt_side, q, price, n)?;
            self.apply_position(cp, market, bankrupt_side.opposite(), q, price, n)?;
            self.adl_fills = self.adl_fills.saturating_add(1);
            self.mark_dirty(cp);
            out.push(Output::Deleveraged {
                market,
                bankrupt: id,
                counterparty: cp,
                price,
                qty: Qty(q),
            });
            left -= q;
        }
        if left > 0 {
            return Err(self.halt(Halt::Adl));
        }
        if self
            .accounts
            .get(&id)
            .is_some_and(|a| a.positions.is_empty())
        {
            self.cover_shortfall(id, out)?;
        }
        Ok(true)
    }

    /// Counterparties for auto-deleveraging, best candidate first: highest
    /// profit ratio, then largest position, then lowest account id.
    fn adl_ranking(
        &self,
        market: MarketId,
        want_long: bool,
        mark: Price,
    ) -> Result<Vec<AccountId>> {
        let m = self.markets.get(&market).ok_or(Error::UnknownMarket)?;
        let mut ranked: Vec<(Reverse<i128>, Reverse<u64>, AccountId)> = Vec::new();
        for h in &m.holders {
            let Some(p) = self.accounts.get(h).and_then(|a| a.positions.get(&market)) else {
                continue;
            };
            if (p.size > 0) != want_long {
                continue;
            }
            let abs = p.size.unsigned_abs();
            let upnl = signed_value(p.size, mark)?
                .0
                .checked_sub(p.basis)
                .ok_or(Error::Overflow)?;
            let value = notional(mark, Qty(abs), Round::Floor)?.0.max(1);
            let ratio = floor_div(upnl.saturating_mul(1_000_000), value)?;
            ranked.push((Reverse(ratio), Reverse(abs), *h));
        }
        ranked.sort();
        Ok(ranked.into_iter().map(|(_, _, id)| id).collect())
    }
}
