//! Per-account state.

use crate::types::{MarketId, OrderId};
use std::collections::BTreeMap;

/// An open position in one market.
///
/// `basis` is the signed cost basis in money units: the sum of signed fill
/// notionals still attributed to the open size (positive for longs: money
/// paid; negative for shorts: money received). Using a money-denominated
/// basis instead of an entry *price* makes realised PnL exact: every fill's
/// notional is computed once and used for both counterparties.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Position {
    /// Signed size: positive long, negative short. Never zero when stored.
    pub size: i64,
    /// Signed cost basis (see type docs).
    pub basis: i128,
}

/// Total size of the account's live, non-reduce-only orders per side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Exposure {
    /// Sum of resting buy quantity.
    pub buy: u64,
    /// Sum of resting sell quantity.
    pub sell: u64,
}

/// A trading account with cross margin over all markets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    /// Collateral: deposits plus realised PnL, funding and fees. May be
    /// negative only transiently, while unrealised gains elsewhere back it.
    pub collateral: i128,
    /// Index into the fee schedule.
    pub fee_tier: u8,
    /// Open positions by market (only non-zero sizes).
    pub positions: BTreeMap<MarketId, Position>,
    // --- derived from the order books; rebuilt on snapshot load, not encoded ---
    /// Live resting orders: order id -> market.
    pub orders: BTreeMap<OrderId, MarketId>,
    /// Client id -> order id for live resting orders.
    pub client_ids: BTreeMap<u64, OrderId>,
    /// Resting exposure per market.
    pub exposure: BTreeMap<MarketId, Exposure>,
}

impl Account {
    /// Signed position size in `market` (0 if flat).
    pub fn size_in(&self, market: MarketId) -> i64 {
        self.positions.get(&market).map_or(0, |p| p.size)
    }

    /// Resting exposure in `market`.
    pub fn exposure_in(&self, market: MarketId) -> Exposure {
        self.exposure.get(&market).copied().unwrap_or_default()
    }
}
