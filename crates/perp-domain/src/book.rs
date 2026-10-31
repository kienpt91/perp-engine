//! Limit order book with price-time priority.
//!
//! Each side is a `BTreeMap<Price, Level>` (O(log n) in the number of price
//! levels). A level keeps its orders in a `BTreeMap<seq, OrderId>`, so the
//! first entry is always the oldest order and removing any order from the middle
//! of the queue is O(log n) as well. All iteration orders are deterministic
//! (ascending keys); no hash maps are used anywhere in engine state.

use crate::error::{Error, Result};
use crate::fixed::Price;
use crate::types::{AccountId, OrderId, Side, StpMode};
use crate::views::LevelView;
use std::collections::BTreeMap;

/// A resting order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    /// Engine-assigned id.
    pub id: OrderId,
    /// Owner.
    pub account: AccountId,
    /// Side.
    pub side: Side,
    /// Limit price.
    pub price: Price,
    /// Remaining quantity (never zero while resting).
    pub qty: u64,
    /// Time-priority sequence number: lower is older.
    pub seq: u64,
    /// Caller supplied id.
    pub client_id: u64,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Self-trade prevention mode.
    pub stp: StpMode,
}

/// All orders at one price, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Level {
    /// Sum of the remaining quantities.
    pub total: u64,
    /// `seq -> order id`, ascending seq is time priority.
    pub queue: BTreeMap<u64, OrderId>,
}

/// One market's order book.
#[derive(Debug, Clone, Default)]
pub struct Book {
    bids: BTreeMap<Price, Level>,
    asks: BTreeMap<Price, Level>,
    orders: BTreeMap<OrderId, Order>,
}

impl Book {
    /// Empty book.
    pub fn new() -> Book {
        Book::default()
    }

    fn side_map(&self, side: Side) -> &BTreeMap<Price, Level> {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn side_map_mut(&mut self, side: Side) -> &mut BTreeMap<Price, Level> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    /// Number of resting orders.
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    /// True if no orders rest.
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    /// Number of price levels on a side.
    pub fn level_count(&self, side: Side) -> usize {
        self.side_map(side).len()
    }

    /// True when a level exists at `price` on `side`.
    pub fn has_level(&self, side: Side, price: Price) -> bool {
        self.side_map(side).contains_key(&price)
    }

    /// Look up an order.
    pub fn get(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    /// All orders, ascending by id.
    pub fn orders(&self) -> impl Iterator<Item = &Order> {
        self.orders.values()
    }

    /// Best price on `side` (highest bid / lowest ask).
    pub fn best(&self, side: Side) -> Option<Price> {
        match side {
            Side::Buy => self.bids.keys().next_back().copied(),
            Side::Sell => self.asks.keys().next().copied(),
        }
    }

    /// Oldest order at the best level of `side`.
    pub fn front(&self, side: Side) -> Option<OrderId> {
        let level = match side {
            Side::Buy => self.bids.values().next_back(),
            Side::Sell => self.asks.values().next(),
        }?;
        level.queue.values().next().copied()
    }

    /// Levels of `side`, best first.
    pub fn levels(&self, side: Side) -> Box<dyn Iterator<Item = (Price, &Level)> + '_> {
        match side {
            Side::Buy => Box::new(self.bids.iter().rev().map(|(p, l)| (*p, l))),
            Side::Sell => Box::new(self.asks.iter().map(|(p, l)| (*p, l))),
        }
    }

    /// Aggregated view of the best `depth` levels of `side`.
    pub fn depth(&self, side: Side, depth: usize) -> Vec<LevelView> {
        self.levels(side)
            .take(depth)
            .map(|(price, l)| LevelView {
                price,
                qty: crate::fixed::Qty(l.total),
                orders: u32::try_from(l.queue.len()).unwrap_or(u32::MAX),
            })
            .collect()
    }

    /// Insert a new resting order at the back of its level's queue.
    pub fn insert(&mut self, order: Order) -> Result<()> {
        if order.qty == 0 {
            return Err(Error::Internal("zero quantity order inserted"));
        }
        if self.orders.contains_key(&order.id) {
            return Err(Error::Internal("duplicate order id"));
        }
        let level = self
            .side_map_mut(order.side)
            .entry(order.price)
            .or_default();
        if level.queue.insert(order.seq, order.id).is_some() {
            return Err(Error::Internal("duplicate order sequence"));
        }
        level.total = level.total.checked_add(order.qty).ok_or(Error::Overflow)?;
        self.orders.insert(order.id, order);
        Ok(())
    }

    /// Remove an order, returning it.
    pub fn remove(&mut self, id: OrderId) -> Option<Order> {
        let order = self.orders.remove(&id)?;
        let map = self.side_map_mut(order.side);
        let mut empty = false;
        if let Some(level) = map.get_mut(&order.price) {
            level.queue.remove(&order.seq);
            level.total = level.total.saturating_sub(order.qty);
            empty = level.queue.is_empty();
        }
        if empty {
            map.remove(&order.price);
        }
        Some(order)
    }

    /// Decrease the remaining quantity of an order in place (keeps priority).
    /// `new_qty` must be non-zero and below the current quantity.
    pub fn shrink(&mut self, id: OrderId, new_qty: u64) -> Result<()> {
        let order = self
            .orders
            .get_mut(&id)
            .ok_or(Error::Internal("shrink: unknown order"))?;
        if new_qty == 0 || new_qty > order.qty {
            return Err(Error::Internal("shrink: bad quantity"));
        }
        let delta = order.qty - new_qty;
        order.qty = new_qty;
        let (side, price) = (order.side, order.price);
        let level = self
            .side_map_mut(side)
            .get_mut(&price)
            .ok_or(Error::Internal("shrink: no level"))?;
        level.total = level
            .total
            .checked_sub(delta)
            .ok_or(Error::Internal("level total"))?;
        Ok(())
    }

    /// True if best bid >= best ask (must never happen).
    pub fn is_crossed(&self) -> bool {
        match (self.best(Side::Buy), self.best(Side::Sell)) {
            (Some(b), Some(a)) => b >= a,
            _ => false,
        }
    }

    /// Full internal consistency check (used by tests and snapshot loading).
    pub fn check(&self) -> std::result::Result<(), String> {
        let mut seen = 0usize;
        for (side, map) in [(Side::Buy, &self.bids), (Side::Sell, &self.asks)] {
            for (price, level) in map {
                if level.queue.is_empty() {
                    return Err(format!("empty level at {}", price.0));
                }
                let mut total = 0u64;
                for (seq, id) in &level.queue {
                    let o = self
                        .orders
                        .get(id)
                        .ok_or_else(|| format!("level refers to missing order {id}"))?;
                    if o.seq != *seq || o.price != *price || o.side != side {
                        return Err(format!("order {id} disagrees with its level"));
                    }
                    if o.qty == 0 {
                        return Err(format!("order {id} has zero qty"));
                    }
                    total = total.checked_add(o.qty).ok_or("level total overflow")?;
                    seen += 1;
                }
                if total != level.total {
                    return Err(format!("level {} total mismatch", price.0));
                }
            }
        }
        if seen != self.orders.len() {
            return Err("orders not present in any level".into());
        }
        if self.is_crossed() {
            return Err("book is crossed".into());
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn order(id: u64, side: Side, price: u64, qty: u64, seq: u64) -> Order {
        Order {
            id,
            account: 1,
            side,
            price: Price(price),
            qty,
            seq,
            client_id: id,
            reduce_only: false,
            stp: StpMode::CancelNewest,
        }
    }

    #[test]
    fn best_prices_and_priority() {
        let mut b = Book::new();
        b.insert(order(1, Side::Buy, 100, 5, 1)).unwrap();
        b.insert(order(2, Side::Buy, 101, 5, 2)).unwrap();
        b.insert(order(3, Side::Buy, 101, 7, 3)).unwrap();
        b.insert(order(4, Side::Sell, 105, 1, 4)).unwrap();
        b.insert(order(5, Side::Sell, 103, 1, 5)).unwrap();
        assert_eq!(b.best(Side::Buy), Some(Price(101)));
        assert_eq!(b.best(Side::Sell), Some(Price(103)));
        assert_eq!(
            b.front(Side::Buy),
            Some(2),
            "oldest order at best level first"
        );
        assert_eq!(b.front(Side::Sell), Some(5));
        b.check().unwrap();
    }

    #[test]
    fn remove_from_middle_keeps_fifo_and_prunes_levels() {
        let mut b = Book::new();
        for i in 0..5 {
            b.insert(order(i + 1, Side::Sell, 200, 1, i + 1)).unwrap();
        }
        assert_eq!(b.remove(3).unwrap().id, 3);
        let ids: Vec<_> = b
            .levels(Side::Sell)
            .next()
            .unwrap()
            .1
            .queue
            .values()
            .copied()
            .collect();
        assert_eq!(ids, vec![1, 2, 4, 5]);
        for id in [1, 2, 4, 5] {
            b.remove(id).unwrap();
        }
        assert_eq!(b.level_count(Side::Sell), 0);
        assert!(b.is_empty());
        assert!(b.remove(1).is_none());
    }

    #[test]
    fn shrink_keeps_priority_and_updates_total() {
        let mut b = Book::new();
        b.insert(order(1, Side::Buy, 100, 10, 1)).unwrap();
        b.insert(order(2, Side::Buy, 100, 10, 2)).unwrap();
        b.shrink(1, 4).unwrap();
        assert_eq!(b.front(Side::Buy), Some(1));
        assert_eq!(b.depth(Side::Buy, 1)[0].qty.0, 14);
        assert!(b.shrink(1, 0).is_err());
        assert!(b.shrink(1, 5).is_err());
        assert!(b.shrink(99, 1).is_err());
        b.check().unwrap();
    }

    #[test]
    fn duplicate_and_zero_inserts_are_errors() {
        let mut b = Book::new();
        b.insert(order(1, Side::Buy, 100, 1, 1)).unwrap();
        assert!(b.insert(order(1, Side::Buy, 101, 1, 2)).is_err());
        assert!(
            b.insert(order(2, Side::Buy, 100, 1, 1)).is_err(),
            "same seq at same level"
        );
        assert!(b.insert(order(3, Side::Buy, 100, 0, 3)).is_err());
    }

    #[test]
    fn depth_is_best_first() {
        let mut b = Book::new();
        for (i, p) in [100u64, 102, 101].iter().enumerate() {
            b.insert(order(i as u64 + 1, Side::Buy, *p, 1, i as u64 + 1))
                .unwrap();
            b.insert(order(i as u64 + 10, Side::Sell, *p + 10, 1, i as u64 + 10))
                .unwrap();
        }
        let bids: Vec<_> = b.depth(Side::Buy, 10).iter().map(|l| l.price.0).collect();
        let asks: Vec<_> = b.depth(Side::Sell, 2).iter().map(|l| l.price.0).collect();
        assert_eq!(bids, vec![102, 101, 100]);
        assert_eq!(asks, vec![110, 111]);
    }

    #[test]
    fn crossed_book_is_detected_by_check() {
        let mut b = Book::new();
        b.insert(order(1, Side::Buy, 100, 1, 1)).unwrap();
        b.insert(order(2, Side::Sell, 100, 1, 2)).unwrap();
        assert!(b.is_crossed());
        assert!(b.check().is_err());
    }

    #[test]
    fn total_overflow_is_an_error() {
        let mut b = Book::new();
        b.insert(order(1, Side::Buy, 100, u64::MAX, 1)).unwrap();
        assert_eq!(
            b.insert(order(2, Side::Buy, 100, 1, 2)),
            Err(Error::Overflow)
        );
    }

    #[test]
    fn empty_book_has_no_best() {
        let b = Book::new();
        assert_eq!(b.best(Side::Buy), None);
        assert_eq!(b.front(Side::Sell), None);
        assert!(!b.is_crossed());
        b.check().unwrap();
    }
}
