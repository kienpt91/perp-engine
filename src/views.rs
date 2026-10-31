//! Read-only snapshots of engine state for dumps and queries.

use crate::fixed::{Money, Price, Qty};
use crate::types::{AccountId, MarketId, Millis};

/// One aggregated price level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelView {
    /// Price.
    pub price: Price,
    /// Total resting quantity.
    pub qty: Qty,
    /// Number of resting orders.
    pub orders: u32,
}

/// Top of book: bids best (highest) first, asks best (lowest) first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookView {
    /// Market.
    pub market: MarketId,
    /// Bid levels, best first.
    pub bids: Vec<LevelView>,
    /// Ask levels, best first.
    pub asks: Vec<LevelView>,
}

/// One open position of an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionView {
    /// Market.
    pub market: MarketId,
    /// Signed size: positive long, negative short.
    pub size: i64,
    /// Average entry price (cost basis / size), if the position is open.
    pub entry_price: Option<Price>,
    /// Mark price used, if any.
    pub mark: Option<Price>,
    /// Unrealised PnL at mark.
    pub unrealized: Money,
}

/// Account state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountView {
    /// Account id.
    pub id: AccountId,
    /// Collateral balance (realised PnL, fees and funding included).
    pub collateral: Money,
    /// Collateral plus unrealised PnL.
    pub equity: Money,
    /// Initial margin requirement of the positions and open orders.
    pub initial_margin: Money,
    /// Maintenance margin requirement of the positions.
    pub maintenance_margin: Money,
    /// Live resting orders.
    pub open_orders: u32,
    /// Fee tier.
    pub fee_tier: u8,
    /// Positions, ordered by market id.
    pub positions: Vec<PositionView>,
}

/// Engine-wide counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Events applied (accepted or rejected).
    pub events_applied: u64,
    /// Engine clock.
    pub clock: Millis,
    /// Accounts.
    pub accounts: u32,
    /// Markets.
    pub markets: u32,
    /// Resting orders over all books.
    pub open_orders: u64,
    /// Total collateral over all accounts.
    pub total_collateral: Money,
    /// Insurance fund balance.
    pub insurance_fund: Money,
    /// Fee pool balance (fees collected minus rebates paid).
    pub fee_pool: Money,
    /// Number of accounts with an open position.
    pub accounts_with_positions: u32,
    /// Liquidation steps executed so far.
    pub liquidation_steps: u64,
    /// Auto-deleveraging fills executed so far.
    pub adl_fills: u64,
    /// Fills executed so far.
    pub fills: u64,
    /// True if the engine stopped after an internal inconsistency.
    pub halted: bool,
}
