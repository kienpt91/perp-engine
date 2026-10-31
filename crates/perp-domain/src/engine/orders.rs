//! Order entry, cancellation, replacement and the matching loop.

use super::{Engine, MAX_FILLS_PER_MATCH};
use crate::account::{Account, Exposure};
use crate::book::Order;
use crate::error::{Error, Result};
use crate::fixed::{
    ceil_div, mul_div, notional, notional_raw, ppm_of, Money, Price, Qty, Round, BPS_SCALE,
    MAX_PRICE, QTY_SCALE,
};
use crate::margin::MarginKind;
use crate::types::{
    AccountId, CancelReason, FeeTier, MarketId, Millis, OrderId, OrderKind, Output, PlaceOrder,
    Side, StpMode, Tif,
};
use std::collections::BTreeMap;

/// An incoming order while it is being matched.
pub(super) struct Taker {
    pub id: OrderId,
    pub account: AccountId,
    pub side: Side,
    pub limit: Price,
    pub remaining: u64,
    pub reduce_only: bool,
    pub stp: StpMode,
    pub liquidation: bool,
    pub liq_fee_ppm: u32,
}

/// Result of a matching run.
pub(super) struct MatchResult {
    pub filled: u64,
    /// Why matching stopped early, if it did.
    pub stop: Option<CancelReason>,
}

/// Worst-case absolute position if every resting order on one side filled.
pub(super) fn worst_abs(pos: i128, buy: u64, sell: u64) -> Result<u64> {
    let up = pos
        .checked_add(i128::from(buy))
        .ok_or(Error::Overflow)?
        .unsigned_abs();
    let down = pos
        .checked_sub(i128::from(sell))
        .ok_or(Error::Overflow)?
        .unsigned_abs();
    u64::try_from(up.max(down)).map_err(|_| Error::Overflow)
}

impl Engine {
    pub(super) fn fee_tier_of(&self, account: AccountId) -> Result<FeeTier> {
        let tier = self.accounts.get(&account).map_or(0, |a| a.fee_tier);
        self.config
            .as_ref()
            .and_then(|c| c.fee_tiers.get(usize::from(tier)).copied())
            .ok_or(Error::Internal("fee tier missing"))
    }

    /// Quantity of `account`'s position that an order on `side` may reduce.
    pub(super) fn reducible(&self, account: AccountId, market: MarketId, side: Side) -> u64 {
        let pos = self.accounts.get(&account).map_or(0, |a| a.size_in(market));
        match side {
            Side::Sell if pos > 0 => pos.unsigned_abs(),
            Side::Buy if pos < 0 => pos.unsigned_abs(),
            _ => 0,
        }
    }

    fn exposure_change(
        &mut self,
        account: AccountId,
        market: MarketId,
        side: Side,
        qty: u64,
        add: bool,
    ) -> Result<()> {
        let acct = self.acct_mut(account)?;
        let e = acct.exposure.entry(market).or_default();
        let slot = match side {
            Side::Buy => &mut e.buy,
            Side::Sell => &mut e.sell,
        };
        *slot = if add {
            slot.checked_add(qty).ok_or(Error::Overflow)?
        } else {
            slot.checked_sub(qty)
                .ok_or(Error::Internal("exposure underflow"))?
        };
        if *e == Exposure::default() {
            acct.exposure.remove(&market);
        }
        Ok(())
    }

    /// Put an order on the book and register it with its account.
    pub(super) fn rest_order(&mut self, market: MarketId, order: Order) -> Result<()> {
        let (account, side, qty, id, client_id, reduce_only) = (
            order.account,
            order.side,
            order.qty,
            order.id,
            order.client_id,
            order.reduce_only,
        );
        self.market_mut(market)?.book.insert(order)?;
        let acct = self.acct_mut(account)?;
        acct.orders.insert(id, market);
        acct.client_ids.insert(client_id, id);
        if !reduce_only {
            self.exposure_change(account, market, side, qty, true)?;
        }
        Ok(())
    }

    /// Remove an order from the book and from its account's registries.
    pub(super) fn unrest(&mut self, market: MarketId, id: OrderId) -> Result<Order> {
        let order = self
            .market_mut(market)?
            .book
            .remove(id)
            .ok_or(Error::Internal("unrest: order not on book"))?;
        let acct = self.acct_mut(order.account)?;
        acct.orders.remove(&id);
        acct.client_ids.remove(&order.client_id);
        if !order.reduce_only {
            self.exposure_change(order.account, market, order.side, order.qty, false)?;
        }
        Ok(order)
    }

    fn shrink_resting(&mut self, market: MarketId, order: &Order, new_qty: u64) -> Result<()> {
        self.market_mut(market)?.book.shrink(order.id, new_qty)?;
        if !order.reduce_only {
            let delta = order.qty - new_qty;
            self.exposure_change(order.account, market, order.side, delta, false)?;
        }
        Ok(())
    }

    pub(super) fn cancel_resting(
        &mut self,
        market: MarketId,
        id: OrderId,
        reason: CancelReason,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        let o = self.unrest(market, id)?;
        out.push(Output::Canceled {
            order_id: o.id,
            account: o.account,
            qty: Qty(o.qty),
            reason,
        });
        Ok(())
    }

    /// Margin check for an order (or a replace) that raises resting exposure.
    ///
    /// The check is skipped when the worst-case position does not grow, so an
    /// under-margined account can still place orders that only reduce risk.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn check_new_exposure(
        &self,
        acct: &Account,
        market: MarketId,
        side: Side,
        add: u64,
        remove: u64,
        now: Millis,
        ref_price: Price,
        adverse: i128,
    ) -> Result<()> {
        let m = self.markets.get(&market).ok_or(Error::UnknownMarket)?;
        let pos = i128::from(acct.size_in(market));
        let exp = acct.exposure_in(market);
        let before = worst_abs(pos, exp.buy, exp.sell)?;
        let (mut buy, mut sell) = (exp.buy, exp.sell);
        match side {
            Side::Buy => {
                buy = buy
                    .saturating_sub(remove)
                    .checked_add(add)
                    .ok_or(Error::Overflow)?
            }
            Side::Sell => {
                sell = sell
                    .saturating_sub(remove)
                    .checked_add(add)
                    .ok_or(Error::Overflow)?;
            }
        }
        let after = worst_abs(pos, buy, sell)?;
        if after > m.cfg.max_position {
            return Err(Error::PositionLimit);
        }
        if adverse > 0 {
            self.check_equity_floor(acct, now, adverse, true)?;
        }
        if after <= before {
            return Ok(());
        }
        let mark = self.mark_for(market, now, true)?;
        let equity = self.equity_of(acct, now, true)?;
        let required = self.margin_requirement(
            acct,
            MarginKind::Initial,
            true,
            now,
            true,
            Some((market, after)),
        )?;
        let taker_ppm = self.fee_tier_of_account(acct)?.taker_ppm;
        let ref_n = notional(ref_price.max(mark), Qty(add), Round::Ceil)?;
        let reserve = ppm_of(ref_n, taker_ppm, Round::Ceil)?;
        // The order may execute away from the mark: its execution-price loss
        // (`adverse`, see `price_impact`) is reserved next to the fee.
        let reserved = reserve.0.checked_add(adverse).ok_or(Error::Overflow)?;
        if equity.checked_sub(reserved).ok_or(Error::Overflow)? < required {
            return Err(Error::MarginBreach);
        }
        Ok(())
    }

    /// Reject an order whose worst-case execution-price loss (`adverse`) would
    /// leave the account with negative equity: a fill must never create a
    /// deficit that the insurance fund would have to absorb.
    pub(super) fn check_equity_floor(
        &self,
        acct: &Account,
        now: Millis,
        adverse: i128,
        fresh: bool,
    ) -> Result<()> {
        let equity = self.equity_of(acct, now, fresh)?;
        if equity.checked_sub(adverse).ok_or(Error::Overflow)? < 0 {
            return Err(Error::MarginBreach);
        }
        Ok(())
    }

    /// Dry run of the matching loop for a taker: the net equity the taker would
    /// lose by trading at the resting prices instead of at the mark (a positive
    /// number is a loss, rounded against the taker; gains offset losses), and
    /// the quantity that would fill. Reduce-only makers are assumed to fill in
    /// full, which can only overstate the loss.
    #[allow(clippy::too_many_arguments)]
    fn price_impact(
        &self,
        market: MarketId,
        account: AccountId,
        side: Side,
        limit: Price,
        qty: u64,
        stp: StpMode,
        mark: Price,
    ) -> Result<(i128, u64)> {
        let m = self.markets.get(&market).ok_or(Error::UnknownMarket)?;
        let mut need = qty;
        let mut fills = 0usize;
        let mut loss = 0i128;
        'levels: for (price, level) in m.book.levels(side.opposite()) {
            if !crosses(side, price, limit) {
                break;
            }
            for id in level.queue.values() {
                let o = m
                    .book
                    .get(*id)
                    .ok_or(Error::Internal("level refers to missing order"))?;
                if o.account == account {
                    if stp == StpMode::CancelOldest {
                        continue;
                    }
                    break 'levels;
                }
                let take = o.qty.min(need);
                loss = loss
                    .checked_add(price_loss(side, price, mark, take)?)
                    .ok_or(Error::Overflow)?;
                need -= take;
                fills += 1;
                if need == 0 || fills >= MAX_FILLS_PER_MATCH {
                    break 'levels;
                }
            }
        }
        Ok((loss, qty - need))
    }

    fn fee_tier_of_account(&self, acct: &Account) -> Result<FeeTier> {
        self.config
            .as_ref()
            .and_then(|c| c.fee_tiers.get(usize::from(acct.fee_tier)).copied())
            .ok_or(Error::Internal("fee tier missing"))
    }

    /// Cancel resting orders that the new mark has made more aggressive than the
    /// price band allows (bids above `mark * (1 + band)`, asks below
    /// `mark * (1 - band)`), so that no fill executes outside the band.
    pub(super) fn sweep_price_band(
        &mut self,
        market: MarketId,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        let m = self.markets.get(&market).ok_or(Error::UnknownMarket)?;
        let Some(mark) = m.mark else {
            return Ok(());
        };
        let hi = band_price(mark, m.cfg.price_band_bps, Side::Buy)?;
        let lo = band_price(mark, m.cfg.price_band_bps, Side::Sell)?;
        let mut ids = m.book.aggressive_ids(Side::Buy, hi);
        ids.extend(m.book.aggressive_ids(Side::Sell, lo));
        for id in ids {
            self.cancel_resting(market, id, CancelReason::PriceBand, out)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Place
    // -----------------------------------------------------------------------

    pub(super) fn place(
        &mut self,
        ts: Millis,
        p: &PlaceOrder,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        let m = self.markets.get(&p.market).ok_or(Error::UnknownMarket)?;
        let acct = self.accounts.get(&p.account).ok_or(Error::UnknownAccount)?;
        let cfg = &m.cfg;
        if p.qty.0 == 0 {
            return Err(Error::ZeroQty);
        }
        if p.qty.0 > cfg.max_order_qty || !p.qty.0.is_multiple_of(cfg.lot) {
            return Err(Error::InvalidQty);
        }
        let opp = p.side.opposite();
        let (limit, ref_price, rests, post_only, fok, mark) = match p.kind {
            OrderKind::Limit { price, tif } => {
                if price.0 == 0 || price.0 > MAX_PRICE || !price.0.is_multiple_of(cfg.tick) {
                    return Err(Error::InvalidPrice);
                }
                let mark = m.mark.ok_or(Error::NoMarkPrice)?;
                if beyond_band(p.side, price, mark, cfg.price_band_bps)? {
                    return Err(Error::PriceOutsideBand);
                }
                (
                    price,
                    price,
                    matches!(tif, Tif::Gtc | Tif::PostOnly),
                    tif == Tif::PostOnly,
                    tif == Tif::Fok,
                    mark,
                )
            }
            OrderKind::Market { max_slippage_bps } => {
                if max_slippage_bps > 5_000 {
                    return Err(Error::InvalidSlippage);
                }
                m.book.best(opp).ok_or(Error::NoLiquidity)?;
                let mark = m.mark.ok_or(Error::NoMarkPrice)?;
                // The protection price is measured from the MARK, never from the
                // best opposite price (a far-away best quote would otherwise be
                // acceptable), and never wider than the market's price band.
                let slip = max_slippage_bps.min(cfg.price_band_bps);
                (
                    band_price(mark, slip, p.side)?,
                    mark,
                    false,
                    false,
                    false,
                    mark,
                )
            }
        };
        let stale = ts.saturating_sub(m.oracle_ts) > cfg.oracle_max_staleness_ms;
        if stale && !p.reduce_only {
            return Err(Error::StaleOracle);
        }
        if acct.client_ids.contains_key(&p.client_id) {
            return Err(Error::DuplicateClientOrderId);
        }
        if rests {
            if acct.orders.len() >= cfg.max_orders_per_account as usize {
                return Err(Error::TooManyOrders);
            }
            if m.book.len() >= cfg.max_book_orders as usize {
                return Err(Error::BookFull);
            }
        }
        let mut qty = p.qty.0;
        if p.reduce_only {
            let allowed = self.reducible(p.account, p.market, p.side);
            if allowed == 0 {
                return Err(Error::ReduceOnlyViolation);
            }
            qty = qty.min(allowed);
        } else if notional_raw(ref_price.0, qty, Round::Floor)? < cfg.min_notional {
            return Err(Error::BelowMinNotional);
        }
        let (impact, est_fill) =
            self.price_impact(p.market, p.account, p.side, limit, qty, p.stp, mark)?;
        let rest_qty = if rests { qty - est_fill } else { 0 };
        let adverse = impact
            .max(0)
            .checked_add(price_loss(p.side, limit, mark, rest_qty)?.max(0))
            .ok_or(Error::Overflow)?;
        if p.reduce_only {
            self.check_equity_floor(acct, ts, adverse, !stale)?;
        } else {
            self.check_new_exposure(acct, p.market, p.side, qty, 0, ts, ref_price, adverse)?;
        }
        if post_only
            && m.book
                .best(opp)
                .is_some_and(|best| crosses(p.side, best, limit))
        {
            return Err(Error::WouldCross);
        }
        if fok && !self.fok_fillable(p.market, p.account, p.side, limit, qty, p.stp)? {
            return Err(Error::FokNotFillable);
        }

        self.begin(ts);
        let id = self.next_order_id;
        self.next_order_id = id.checked_add(1).ok_or(Error::Overflow)?;
        out.push(Output::Accepted {
            order_id: id,
            account: p.account,
            market: p.market,
            client_id: p.client_id,
        });
        let mut taker = Taker {
            id,
            account: p.account,
            side: p.side,
            limit,
            remaining: qty,
            reduce_only: p.reduce_only,
            stp: p.stp,
            liquidation: false,
            liq_fee_ppm: 0,
        };
        let res = self.match_taker(p.market, &mut taker, out)?;
        if fok && taker.remaining > 0 {
            return Err(Error::Internal("fill-or-kill order left a remainder"));
        }
        if taker.remaining > 0 {
            self.finish_remainder(p, &taker, rests, res.stop, out)?;
        }
        Ok(())
    }

    /// Rest or cancel what is left of an order after matching.
    fn finish_remainder(
        &mut self,
        p: &PlaceOrder,
        taker: &Taker,
        rests: bool,
        stop: Option<CancelReason>,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        if rests && stop.is_none() {
            let price = taker.limit;
            let m = self
                .markets
                .get(&p.market)
                .ok_or(Error::Internal("market vanished"))?;
            let acct = self
                .accounts
                .get(&p.account)
                .ok_or(Error::Internal("account vanished"))?;
            let has_room = m.book.len() < m.cfg.max_book_orders as usize
                && (m.book.has_level(p.side, price)
                    || m.book.level_count(p.side) < m.cfg.max_levels_per_side as usize)
                && acct.orders.len() < m.cfg.max_orders_per_account as usize;
            if has_room {
                let seq = self.next_seq;
                self.next_seq = seq.checked_add(1).ok_or(Error::Overflow)?;
                self.rest_order(
                    p.market,
                    Order {
                        id: taker.id,
                        account: p.account,
                        side: p.side,
                        price,
                        qty: taker.remaining,
                        seq,
                        client_id: p.client_id,
                        reduce_only: p.reduce_only,
                        stp: p.stp,
                    },
                )?;
                out.push(Output::Rested {
                    order_id: taker.id,
                    qty: Qty(taker.remaining),
                    price,
                });
                return Ok(());
            }
            out.push(Output::Canceled {
                order_id: taker.id,
                account: p.account,
                qty: Qty(taker.remaining),
                reason: CancelReason::BookFull,
            });
            return Ok(());
        }
        out.push(Output::Canceled {
            order_id: taker.id,
            account: p.account,
            qty: Qty(taker.remaining),
            reason: stop.unwrap_or(CancelReason::Remainder),
        });
        Ok(())
    }

    /// Would a fill-or-kill order be filled completely? Mirrors the matching
    /// loop (self-trade handling, reduce-only makers, fill cap) without
    /// changing anything.
    fn fok_fillable(
        &self,
        market: MarketId,
        account: AccountId,
        side: Side,
        limit: Price,
        qty: u64,
        stp: StpMode,
    ) -> Result<bool> {
        let m = self.markets.get(&market).ok_or(Error::UnknownMarket)?;
        let mut need = qty;
        let mut fills = 0usize;
        let mut consumed: BTreeMap<AccountId, u64> = BTreeMap::new();
        for (price, level) in m.book.levels(side.opposite()) {
            if !crosses(side, price, limit) {
                return Ok(false);
            }
            for id in level.queue.values() {
                let o = m
                    .book
                    .get(*id)
                    .ok_or(Error::Internal("level refers to missing order"))?;
                let mut avail = o.qty;
                if o.account == account {
                    if stp == StpMode::CancelOldest {
                        continue;
                    }
                    return Ok(false);
                }
                if o.reduce_only {
                    let used = consumed.get(&o.account).copied().unwrap_or(0);
                    let allowed = self
                        .reducible(o.account, market, o.side)
                        .saturating_sub(used);
                    if allowed == 0 {
                        continue;
                    }
                    avail = avail.min(allowed);
                }
                let take = avail.min(need);
                if o.reduce_only {
                    *consumed.entry(o.account).or_insert(0) += take;
                }
                need -= take;
                fills += 1;
                if need == 0 {
                    return Ok(fills <= MAX_FILLS_PER_MATCH);
                }
                if fills >= MAX_FILLS_PER_MATCH {
                    return Ok(false);
                }
            }
        }
        Ok(false)
    }

    // -----------------------------------------------------------------------
    // Matching
    // -----------------------------------------------------------------------

    /// Match an incoming order against the book. Fills execute at the resting
    /// (maker) price.
    pub(super) fn match_taker(
        &mut self,
        market: MarketId,
        t: &mut Taker,
        out: &mut Vec<Output>,
    ) -> Result<MatchResult> {
        let mut filled = 0u64;
        let mut fills = 0usize;
        let mut stop = None;
        let opp = t.side.opposite();
        while t.remaining > 0 {
            if fills >= MAX_FILLS_PER_MATCH {
                stop = Some(CancelReason::Remainder);
                break;
            }
            let front = {
                let m = self
                    .markets
                    .get(&market)
                    .ok_or(Error::Internal("market vanished"))?;
                match (m.book.best(opp), m.book.front(opp)) {
                    (Some(best), Some(front)) if crosses(t.side, best, t.limit) => front,
                    _ => break,
                }
            };
            let maker = self
                .markets
                .get(&market)
                .and_then(|m| m.book.get(front))
                .cloned()
                .ok_or(Error::Internal("front order missing"))?;

            if maker.account == t.account {
                match t.stp {
                    StpMode::CancelNewest => {
                        stop = Some(CancelReason::SelfTrade);
                        break;
                    }
                    StpMode::CancelOldest => {
                        self.cancel_resting(market, maker.id, CancelReason::SelfTrade, out)?;
                        continue;
                    }
                    StpMode::CancelBoth => {
                        self.cancel_resting(market, maker.id, CancelReason::SelfTrade, out)?;
                        stop = Some(CancelReason::SelfTrade);
                        break;
                    }
                }
            }

            let mut qty = t.remaining.min(maker.qty);
            if maker.reduce_only {
                let allowed = self.reducible(maker.account, market, maker.side);
                if allowed == 0 {
                    self.cancel_resting(market, maker.id, CancelReason::ReduceOnly, out)?;
                    continue;
                }
                qty = qty.min(allowed);
            }
            if t.reduce_only {
                let allowed = self.reducible(t.account, market, t.side);
                if allowed == 0 {
                    stop = Some(CancelReason::ReduceOnly);
                    break;
                }
                qty = qty.min(allowed);
            }

            self.execute_fill(market, t, &maker, qty, out)?;
            t.remaining -= qty;
            filled += qty;
            fills += 1;
        }
        Ok(MatchResult { filled, stop })
    }

    fn execute_fill(
        &mut self,
        market: MarketId,
        t: &Taker,
        maker: &Order,
        qty: u64,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        let price = maker.price;
        let n = notional_raw(price.0, qty, Round::Floor)?;
        let maker_tier = self.fee_tier_of(maker.account)?;
        let mut maker_fee = if maker_tier.maker_ppm >= 0 {
            ppm_of(n, maker_tier.maker_ppm.unsigned_abs(), Round::Ceil)?.0
        } else {
            -ppm_of(n, maker_tier.maker_ppm.unsigned_abs(), Round::Floor)?.0
        };
        if t.liquidation && maker_fee < 0 {
            maker_fee = 0; // no rebates on liquidation flow
        }
        let taker_ppm = if t.liquidation {
            t.liq_fee_ppm
        } else {
            self.fee_tier_of(t.account)?.taker_ppm
        };
        let mut taker_fee = ppm_of(n, taker_ppm, Round::Ceil)?.0;

        self.apply_position(t.account, market, t.side, qty, price, n)?;
        self.apply_position(maker.account, market, maker.side, qty, price, n)?;

        if t.liquidation {
            // A liquidated account never pays more than it has left.
            let have = self
                .accounts
                .get(&t.account)
                .map_or(0, |a| a.collateral.max(0));
            taker_fee = taker_fee.min(have);
        }
        self.charge_fee(t.account, taker_fee, t.liquidation)?;
        self.charge_fee(maker.account, maker_fee, false)?;

        if qty == maker.qty {
            self.unrest(market, maker.id)?;
        } else {
            self.shrink_resting(market, maker, maker.qty - qty)?;
        }
        self.market_mut(market)?.last_trade = Some(price);
        self.fills = self.fills.saturating_add(1);
        self.mark_dirty(t.account);
        self.mark_dirty(maker.account);
        out.push(Output::Fill {
            market,
            taker_order: t.id,
            maker_order: maker.id,
            taker: t.account,
            maker: maker.account,
            price,
            qty: Qty(qty),
            taker_side: t.side,
            taker_fee: Money(taker_fee),
            maker_fee: Money(maker_fee),
            liquidation: t.liquidation,
        });
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Cancel / replace
    // -----------------------------------------------------------------------

    pub(super) fn cancel(
        &mut self,
        ts: Millis,
        account: AccountId,
        order_id: OrderId,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        let acct = self.accounts.get(&account).ok_or(Error::UnknownAccount)?;
        let market = *acct.orders.get(&order_id).ok_or(Error::UnknownOrder)?;
        self.begin(ts);
        self.cancel_resting(market, order_id, CancelReason::User, out)
    }

    pub(super) fn cancel_all(
        &mut self,
        ts: Millis,
        account: AccountId,
        market: Option<MarketId>,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        let acct = self.accounts.get(&account).ok_or(Error::UnknownAccount)?;
        let targets: Vec<(OrderId, MarketId)> = acct
            .orders
            .iter()
            .filter(|(_, m)| market.is_none_or(|want| want == **m))
            .map(|(id, m)| (*id, *m))
            .collect();
        self.begin(ts);
        for (id, m) in targets {
            self.cancel_resting(m, id, CancelReason::User, out)?;
        }
        Ok(())
    }

    /// Cancel-replace. Priority is kept only when the price is unchanged and
    /// the quantity does not grow; otherwise the order moves to the back of its
    /// level. A replace never matches: a new price that would cross is rejected.
    pub(super) fn replace(
        &mut self,
        ts: Millis,
        account: AccountId,
        order_id: OrderId,
        price: Option<Price>,
        qty: Option<Qty>,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        let acct = self.accounts.get(&account).ok_or(Error::UnknownAccount)?;
        let market = *acct.orders.get(&order_id).ok_or(Error::UnknownOrder)?;
        let m = self
            .markets
            .get(&market)
            .ok_or(Error::Internal("market vanished"))?;
        let old = m
            .book
            .get(order_id)
            .cloned()
            .ok_or(Error::Internal("order missing"))?;
        let new_price = price.unwrap_or(old.price);
        let new_qty = qty.map_or(old.qty, |q| q.0);
        let cfg = &m.cfg;
        if new_price.0 == 0 || new_price.0 > MAX_PRICE || !new_price.0.is_multiple_of(cfg.tick) {
            return Err(Error::InvalidPrice);
        }
        if new_qty == 0 {
            return Err(Error::ZeroQty);
        }
        if new_qty > cfg.max_order_qty || !new_qty.is_multiple_of(cfg.lot) {
            return Err(Error::InvalidQty);
        }
        if m.book
            .best(old.side.opposite())
            .is_some_and(|b| crosses(old.side, b, new_price))
        {
            return Err(Error::WouldCross);
        }
        let mark = m.mark.ok_or(Error::NoMarkPrice)?;
        if new_price != old.price && beyond_band(old.side, new_price, mark, cfg.price_band_bps)? {
            return Err(Error::PriceOutsideBand);
        }
        let keep = new_price == old.price && new_qty <= old.qty;
        if old.reduce_only {
            if new_qty > self.reducible(account, market, old.side) {
                return Err(Error::ReduceOnlyViolation);
            }
        } else {
            if notional_raw(new_price.0, new_qty, Round::Floor)? < cfg.min_notional {
                return Err(Error::BelowMinNotional);
            }
            if new_qty > old.qty || new_price != old.price {
                if ts.saturating_sub(m.oracle_ts) > cfg.oracle_max_staleness_ms {
                    return Err(Error::StaleOracle);
                }
                let adverse = price_loss(old.side, new_price, mark, new_qty)?.max(0);
                self.check_new_exposure(
                    acct, market, old.side, new_qty, old.qty, ts, new_price, adverse,
                )?;
            }
        }
        if !keep
            && !m.book.has_level(old.side, new_price)
            && m.book.level_count(old.side) >= cfg.max_levels_per_side as usize
        {
            return Err(Error::BookFull);
        }

        self.begin(ts);
        if new_price == old.price && new_qty == old.qty {
            // nothing to change
        } else if keep {
            self.shrink_resting(market, &old, new_qty)?;
        } else {
            self.unrest(market, order_id)?;
            let seq = self.next_seq;
            self.next_seq = seq.checked_add(1).ok_or(Error::Overflow)?;
            self.rest_order(
                market,
                Order {
                    price: new_price,
                    qty: new_qty,
                    seq,
                    ..old
                },
            )?;
        }
        out.push(Output::Replaced {
            order_id,
            kept_priority: keep,
        });
        Ok(())
    }
}

/// Does a resting price `best` satisfy a taker's limit?
pub(super) fn crosses(taker_side: Side, best: Price, limit: Price) -> bool {
    match taker_side {
        Side::Buy => best <= limit,
        Side::Sell => best >= limit,
    }
}

/// Equity lost by trading `qty` at `price` instead of at `mark`, as a taker on
/// `side`: positive for a buy above the mark or a sell below it. Rounded up, so
/// a loss is never understated and a gain never overstated.
pub(super) fn price_loss(side: Side, price: Price, mark: Price, qty: u64) -> Result<i128> {
    let diff = match side {
        Side::Buy => i128::from(price.0) - i128::from(mark.0),
        Side::Sell => i128::from(mark.0) - i128::from(price.0),
    };
    let n = diff.checked_mul(i128::from(qty)).ok_or(Error::Overflow)?;
    ceil_div(n, i128::from(QTY_SCALE))
}

/// Is `price` more aggressive than the band around `mark` allows for an order
/// on `side` (a buy above `mark * (1 + band)` or a sell below `mark * (1 - band)`)?
pub(super) fn beyond_band(side: Side, price: Price, mark: Price, band_bps: u32) -> Result<bool> {
    let edge = band_price(mark, band_bps, side)?;
    Ok(match side {
        Side::Buy => price > edge,
        Side::Sell => price < edge,
    })
}

/// Price `band_bps` away from `mark`, rounded so that the band is never wider
/// than requested (down for sells, up for buys).
pub(super) fn band_price(mark: Price, band_bps: u32, side: Side) -> Result<Price> {
    let factor = match side {
        Side::Buy => u32::try_from(BPS_SCALE)
            .map_err(|_| Error::Overflow)?
            .saturating_add(band_bps),
        Side::Sell => u32::try_from(BPS_SCALE)
            .map_err(|_| Error::Overflow)?
            .saturating_sub(band_bps),
    };
    let round = if side == Side::Buy {
        Round::Floor
    } else {
        Round::Ceil
    };
    let v = mul_div(
        i128::from(mark.0),
        i128::from(factor),
        i128::from(BPS_SCALE),
        round,
    )?;
    let v = u64::try_from(v).map_err(|_| Error::Overflow)?;
    Ok(Price(v.clamp(1, MAX_PRICE)))
}
