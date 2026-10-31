//! Canonical state encoding (snapshots and state hash) and invariant checks.
//!
//! The encoding contains only primary data. Derived indexes (per-account order
//! registries and exposure, per-market holder sets, level totals) are rebuilt on
//! decode, and the result is checked with [`Engine::check_invariants`], so a
//! hostile snapshot cannot produce an inconsistent engine. Collections are
//! written in ascending key order and decoding requires strictly ascending keys,
//! which makes the encoding unique.

use super::{Engine, Halt, MarketState};
use crate::account::{Account, Exposure, Position};
use crate::book::{Book, Order};
use crate::codec::{Decode, DecodeError, Encode, Reader, Writer};
use crate::fixed::{Price, MAX_PRICE, MAX_QTY};
use crate::types::{AccountId, EngineConfig, MarketConfig, MarketId, Side, StpMode, MAX_MARKETS};
use std::collections::{BTreeMap, BTreeSet};

const STATE_VERSION: u8 = 1;
const MIN_ORDER_BYTES: usize = 51;
const MIN_ACCOUNT_BYTES: usize = 8 + 16 + 1 + 4;
const MIN_MARKET_BYTES: usize = 150;

fn opt_price(w: &mut Writer, p: Option<Price>) {
    match p {
        Some(p) => {
            w.bool(true);
            w.u64(p.0);
        }
        None => w.bool(false),
    }
}

fn read_opt_price(r: &mut Reader<'_>) -> Result<Option<Price>, DecodeError> {
    if !r.bool()? {
        return Ok(None);
    }
    let v = r.u64()?;
    if v == 0 || v > MAX_PRICE {
        return Err(DecodeError("price out of range"));
    }
    Ok(Some(Price(v)))
}

fn encode_order(o: &Order, w: &mut Writer) {
    w.u64(o.id);
    w.u64(o.account);
    o.side.encode(w);
    w.u64(o.price.0);
    w.u64(o.qty);
    w.u64(o.seq);
    w.u64(o.client_id);
    w.bool(o.reduce_only);
    o.stp.encode(w);
}

fn decode_order(r: &mut Reader<'_>) -> Result<Order, DecodeError> {
    Ok(Order {
        id: r.u64()?,
        account: r.u64()?,
        side: Side::decode(r)?,
        price: Price(r.u64()?),
        qty: r.u64()?,
        seq: r.u64()?,
        client_id: r.u64()?,
        reduce_only: r.bool()?,
        stp: StpMode::decode(r)?,
    })
}

impl Engine {
    /// Write the canonical encoding of the whole state.
    pub fn encode_state(&self, w: &mut Writer) {
        self.encode_with_counter(w, self.events_applied);
    }

    /// SHA-256 of the state with the applied-event counter zeroed. A rejected
    /// event leaves this unchanged; the full [`Engine::state_hash`] always moves.
    pub fn content_hash(&self) -> crate::sha256::Digest {
        let mut w = Writer::new_hash();
        self.encode_with_counter(&mut w, 0);
        w.finish_hash()
    }

    fn encode_with_counter(&self, w: &mut Writer, events_applied: u64) {
        w.u8(STATE_VERSION);
        match &self.config {
            Some(c) => {
                w.bool(true);
                c.encode(w);
            }
            None => w.bool(false),
        }
        w.u64(self.clock);
        w.u64(events_applied);
        w.u64(self.next_order_id);
        w.u64(self.next_seq);
        w.i128(self.insurance);
        w.i128(self.fee_pool);
        w.i128(self.net_deposits);
        w.i128(self.bad_debt);
        w.u8(self.halted.map_or(0, |h| h as u8));
        w.u64(self.fills);
        w.u64(self.liq_steps);
        w.u64(self.adl_fills);
        w.count(self.pending_liq.len());
        for id in &self.pending_liq {
            w.u64(*id);
        }
        w.count(self.markets.len());
        for (id, m) in &self.markets {
            debug_assert_eq!(*id, m.cfg.id);
            m.cfg.encode(w);
            opt_price(w, m.index);
            opt_price(w, m.mark);
            w.u64(m.oracle_ts);
            opt_price(w, m.last_trade);
            w.i128(m.premium_sum);
            w.u64(m.premium_count);
            w.u64(m.next_funding_ts);
            w.u128(m.funding_carry);
            w.i64(m.last_funding_rate);
            w.count(m.book.len());
            for o in m.book.orders() {
                encode_order(o, w);
            }
        }
        w.count(self.accounts.len());
        for (id, a) in &self.accounts {
            w.u64(*id);
            w.i128(a.collateral);
            w.u8(a.fee_tier);
            w.count(a.positions.len());
            for (m, p) in &a.positions {
                w.u32(*m);
                w.i64(p.size);
                w.i128(p.basis);
            }
        }
    }

    /// Strictly decode a state produced by [`Engine::encode_state`], rebuild the
    /// derived indexes and verify every invariant.
    pub fn decode_state(r: &mut Reader<'_>) -> Result<Engine, DecodeError> {
        if r.u8()? != STATE_VERSION {
            return Err(DecodeError("unsupported state version"));
        }
        let mut e = Engine::new();
        if r.bool()? {
            let c = EngineConfig::decode(r)?;
            c.validate()
                .map_err(|_| DecodeError("invalid engine config"))?;
            e.config = Some(c);
        }
        e.clock = r.u64()?;
        e.events_applied = r.u64()?;
        e.next_order_id = r.u64()?;
        e.next_seq = r.u64()?;
        e.insurance = r.i128()?;
        e.fee_pool = r.i128()?;
        e.net_deposits = r.i128()?;
        e.bad_debt = r.i128()?;
        e.halted = match r.u8()? {
            0 => None,
            v => Some(Halt::from_u8(v).ok_or(DecodeError("invalid halt code"))?),
        };
        e.fills = r.u64()?;
        e.liq_steps = r.u64()?;
        e.adl_fills = r.u64()?;

        let max_accounts = e.config.as_ref().map_or(0, |c| c.max_accounts as usize);
        let max_markets = if e.config.is_some() { MAX_MARKETS } else { 0 };

        let n = r.count(max_accounts, 8)?;
        let mut last: Option<AccountId> = None;
        for _ in 0..n {
            let id = r.u64()?;
            if last.is_some_and(|l| l >= id) {
                return Err(DecodeError("pending liquidations not ascending"));
            }
            last = Some(id);
            e.pending_liq.insert(id);
        }

        let n = r.count(max_markets, MIN_MARKET_BYTES)?;
        let mut last_market: Option<MarketId> = None;
        let mut pending_orders: Vec<(MarketId, Order)> = Vec::new();
        for _ in 0..n {
            let cfg = MarketConfig::decode(r)?;
            cfg.validate()
                .map_err(|_| DecodeError("invalid market config"))?;
            if last_market.is_some_and(|l| l >= cfg.id) {
                return Err(DecodeError("markets not ascending"));
            }
            last_market = Some(cfg.id);
            let index = read_opt_price(r)?;
            let mark = read_opt_price(r)?;
            let oracle_ts = r.u64()?;
            let last_trade = read_opt_price(r)?;
            let premium_sum = r.i128()?;
            let premium_count = r.u64()?;
            let next_funding_ts = r.u64()?;
            let funding_carry = r.u128()?;
            let last_funding_rate = r.i64()?;
            let order_count = r.count(cfg.max_book_orders as usize, MIN_ORDER_BYTES)?;
            let mut prev: Option<u64> = None;
            for _ in 0..order_count {
                let o = decode_order(r)?;
                if prev.is_some_and(|p| p >= o.id) {
                    return Err(DecodeError("orders not ascending"));
                }
                prev = Some(o.id);
                pending_orders.push((cfg.id, o));
            }
            e.markets.insert(
                cfg.id,
                MarketState {
                    cfg,
                    book: Book::new(),
                    index,
                    mark,
                    oracle_ts,
                    last_trade,
                    premium_sum,
                    premium_count,
                    next_funding_ts,
                    funding_carry,
                    last_funding_rate,
                    holders: BTreeSet::new(),
                },
            );
        }

        let n = r.count(max_accounts, MIN_ACCOUNT_BYTES)?;
        let mut last_acct: Option<AccountId> = None;
        for _ in 0..n {
            let id = r.u64()?;
            if last_acct.is_some_and(|l| l >= id) {
                return Err(DecodeError("accounts not ascending"));
            }
            last_acct = Some(id);
            let collateral = r.i128()?;
            let fee_tier = r.u8()?;
            let np = r.count(MAX_MARKETS, 28)?;
            let mut positions = BTreeMap::new();
            let mut last_pm: Option<MarketId> = None;
            for _ in 0..np {
                let m = r.u32()?;
                if last_pm.is_some_and(|l| l >= m) {
                    return Err(DecodeError("positions not ascending"));
                }
                last_pm = Some(m);
                let size = r.i64()?;
                let basis = r.i128()?;
                if size == 0 || size.unsigned_abs() > MAX_QTY {
                    return Err(DecodeError("position size out of range"));
                }
                let ms = e
                    .markets
                    .get_mut(&m)
                    .ok_or(DecodeError("position in unknown market"))?;
                ms.holders.insert(id);
                positions.insert(m, Position { size, basis });
            }
            e.accounts.insert(
                id,
                Account {
                    collateral,
                    fee_tier,
                    positions,
                    ..Account::default()
                },
            );
        }
        r.finish()?;

        for (market, o) in pending_orders {
            if o.qty == 0 || o.qty > MAX_QTY || o.price.0 == 0 || o.price.0 > MAX_PRICE {
                return Err(DecodeError("order out of range"));
            }
            let acct = e
                .accounts
                .get_mut(&o.account)
                .ok_or(DecodeError("order of unknown account"))?;
            if acct.client_ids.insert(o.client_id, o.id).is_some() {
                return Err(DecodeError("duplicate client order id"));
            }
            acct.orders.insert(o.id, market);
            if !o.reduce_only {
                let ex = acct.exposure.entry(market).or_default();
                let slot = if o.side == Side::Buy {
                    &mut ex.buy
                } else {
                    &mut ex.sell
                };
                *slot = slot
                    .checked_add(o.qty)
                    .ok_or(DecodeError("exposure overflow"))?;
            }
            let ms = e
                .markets
                .get_mut(&market)
                .ok_or(DecodeError("order in unknown market"))?;
            ms.book
                .insert(o)
                .map_err(|_| DecodeError("inconsistent order book"))?;
        }
        e.check_invariants()
            .map_err(|_| DecodeError("state invariant violated"))?;
        Ok(e)
    }

    /// Verify every structural and monetary invariant. `Err` carries a
    /// description of the first violation.
    pub fn check_invariants(&self) -> Result<(), String> {
        let Some(cfg) = &self.config else {
            if !self.markets.is_empty() || !self.accounts.is_empty() {
                return Err("state without configuration".into());
            }
            return Ok(());
        };
        if self.accounts.len() > cfg.max_accounts as usize {
            return Err("too many accounts".into());
        }
        if self.insurance < 0 || self.fee_pool < 0 || self.bad_debt < 0 {
            return Err("negative insurance, fee pool or bad debt".into());
        }
        let max_id = self.next_order_id;
        let mut order_total = 0usize;
        for (id, m) in &self.markets {
            if *id != m.cfg.id {
                return Err("market key mismatch".into());
            }
            m.book.check().map_err(|e| format!("market {id}: {e}"))?;
            if m.book.len() > m.cfg.max_book_orders as usize {
                return Err(format!("market {id}: book over its order limit"));
            }
            if m.book.level_count(Side::Buy) > m.cfg.max_levels_per_side as usize
                || m.book.level_count(Side::Sell) > m.cfg.max_levels_per_side as usize
            {
                return Err(format!("market {id}: book over its level limit"));
            }
            for o in m.book.orders() {
                order_total += 1;
                if o.id >= max_id || o.seq >= self.next_seq {
                    return Err(format!("order {} beyond the id counters", o.id));
                }
                if o.price.0 % m.cfg.tick != 0 || o.qty % m.cfg.lot != 0 {
                    return Err(format!("order {} off tick or lot", o.id));
                }
                let a = self
                    .accounts
                    .get(&o.account)
                    .ok_or("order of unknown account")?;
                if a.orders.get(&o.id) != Some(id) || a.client_ids.get(&o.client_id) != Some(&o.id)
                {
                    return Err(format!("account registry disagrees about order {}", o.id));
                }
            }
            let holders: BTreeSet<AccountId> = self
                .accounts
                .iter()
                .filter(|(_, a)| a.positions.contains_key(id))
                .map(|(i, _)| *i)
                .collect();
            if holders != m.holders {
                return Err(format!("market {id}: holder index out of date"));
            }
            if self.position_sum(*id) != 0 {
                return Err(format!("market {id}: positions do not sum to zero"));
            }
            if m.index.is_some() != m.mark.is_some() {
                return Err(format!("market {id}: index and mark must be set together"));
            }
        }
        let mut sum_collateral: i128 = 0;
        let mut sum_basis: i128 = 0;
        let mut registered = 0usize;
        for (id, a) in &self.accounts {
            if usize::from(a.fee_tier) >= cfg.fee_tiers.len() {
                return Err(format!("account {id}: fee tier out of range"));
            }
            if a.orders.len() != a.client_ids.len() {
                return Err(format!("account {id}: order registries differ in size"));
            }
            registered += a.orders.len();
            let mut exposure: BTreeMap<MarketId, Exposure> = BTreeMap::new();
            for (oid, m) in &a.orders {
                let o = self
                    .markets
                    .get(m)
                    .and_then(|ms| ms.book.get(*oid))
                    .ok_or_else(|| format!("account {id}: order {oid} not on a book"))?;
                if o.account != *id {
                    return Err(format!("order {oid} owner mismatch"));
                }
                if !o.reduce_only {
                    let e = exposure.entry(*m).or_default();
                    let slot = if o.side == Side::Buy {
                        &mut e.buy
                    } else {
                        &mut e.sell
                    };
                    *slot = slot.checked_add(o.qty).ok_or("exposure overflow")?;
                }
                let limit = self
                    .markets
                    .get(m)
                    .map_or(0, |ms| ms.cfg.max_orders_per_account);
                if a.orders.len() > limit as usize {
                    return Err(format!("account {id}: too many orders"));
                }
            }
            if exposure != a.exposure {
                return Err(format!("account {id}: exposure index out of date"));
            }
            for (m, p) in &a.positions {
                if p.size == 0 {
                    return Err(format!("account {id}: zero-size position stored"));
                }
                let ms = self.markets.get(m).ok_or("position in unknown market")?;
                if p.size.unsigned_abs() > ms.cfg.max_position.max(MAX_QTY) {
                    return Err(format!("account {id}: position beyond limits"));
                }
                if ms.mark.is_none() {
                    return Err(format!("account {id}: position without mark price"));
                }
                sum_basis = sum_basis.checked_add(p.basis).ok_or("basis overflow")?;
            }
            sum_collateral = sum_collateral
                .checked_add(a.collateral)
                .ok_or("collateral overflow")?;
        }
        if registered != order_total {
            return Err("orders on books and in account registries differ".into());
        }
        if !self
            .pending_liq
            .iter()
            .all(|id| self.accounts.contains_key(id))
        {
            return Err("pending liquidation of unknown account".into());
        }
        let lhs = sum_collateral
            .checked_add(self.insurance)
            .and_then(|v| v.checked_add(self.fee_pool))
            .and_then(|v| v.checked_sub(sum_basis))
            .ok_or("money identity overflow")?;
        let rhs = self
            .net_deposits
            .checked_add(self.bad_debt)
            .ok_or("money identity overflow")?;
        if lhs != rhs {
            return Err(format!("money identity broken: {lhs} != {rhs}"));
        }
        Ok(())
    }
}
