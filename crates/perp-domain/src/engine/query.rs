//! Read-only queries and views.

use super::Engine;
use crate::account::Position;
use crate::book::Order;
use crate::fixed::{Money, Price, Round, QTY_SCALE};
use crate::margin::MarginKind;
use crate::sha256::Digest;
use crate::types::{AccountId, EngineConfig, MarketConfig, MarketId, Millis, OrderId, Side};
use crate::views::{AccountView, BookView, PositionView, Summary};

impl Engine {
    /// Number of events applied so far, accepted or rejected.
    pub fn events_applied(&self) -> u64 {
        self.events_applied
    }

    /// Timestamp of the last accepted event.
    pub fn clock(&self) -> Millis {
        self.clock
    }

    /// True after a fail-stop or [`Engine::emergency_halt`].
    pub fn is_halted(&self) -> bool {
        self.halted.is_some()
    }

    /// Why the engine halted, if it did.
    pub fn halt_reason(&self) -> Option<&'static str> {
        self.halted.map(|h| h.reason())
    }

    /// The configuration, once the `Configure` event has been applied.
    pub fn config(&self) -> Option<&EngineConfig> {
        self.config.as_ref()
    }

    /// Registered market ids, ascending.
    pub fn market_ids(&self) -> Vec<MarketId> {
        self.markets.keys().copied().collect()
    }

    /// Configuration of a market.
    pub fn market_config(&self, id: MarketId) -> Option<&MarketConfig> {
        self.markets.get(&id).map(|m| &m.cfg)
    }

    /// Current mark price of a market.
    pub fn mark_price(&self, id: MarketId) -> Option<Price> {
        self.markets.get(&id).and_then(|m| m.mark)
    }

    /// Current (filtered) index price of a market.
    pub fn index_price(&self, id: MarketId) -> Option<Price> {
        self.markets.get(&id).and_then(|m| m.index)
    }

    /// Best bid or ask of a market.
    pub fn best_price(&self, id: MarketId, side: Side) -> Option<Price> {
        self.markets.get(&id).and_then(|m| m.book.best(side))
    }

    /// Account ids, ascending.
    pub fn account_ids(&self) -> Vec<AccountId> {
        self.accounts.keys().copied().collect()
    }

    /// Collateral of an account.
    pub fn collateral(&self, id: AccountId) -> Option<Money> {
        self.accounts.get(&id).map(|a| Money(a.collateral))
    }

    /// Position of an account in a market.
    pub fn position(&self, id: AccountId, market: MarketId) -> Option<Position> {
        self.accounts
            .get(&id)
            .and_then(|a| a.positions.get(&market).copied())
    }

    /// Live resting order ids of an account, ascending.
    pub fn open_orders_of(&self, id: AccountId) -> Vec<OrderId> {
        self.accounts
            .get(&id)
            .map(|a| a.orders.keys().copied().collect())
            .unwrap_or_default()
    }

    /// A resting order.
    pub fn order(&self, market: MarketId, id: OrderId) -> Option<Order> {
        self.markets
            .get(&market)
            .and_then(|m| m.book.get(id))
            .cloned()
    }

    /// Equity of an account at the current marks, ignoring staleness.
    pub fn equity(&self, id: AccountId) -> Option<Money> {
        let a = self.accounts.get(&id)?;
        self.equity_of(a, self.clock, false).ok().map(Money)
    }

    /// Insurance fund balance.
    pub fn insurance_fund(&self) -> Money {
        Money(self.insurance)
    }

    /// Fees collected minus rebates paid.
    pub fn fee_pool(&self) -> Money {
        Money(self.fee_pool)
    }

    /// Deposits minus withdrawals plus insurance funding (money that entered
    /// the system).
    pub fn net_deposits(&self) -> Money {
        Money(self.net_deposits)
    }

    /// Losses nobody could cover. Non-zero means the venue is insolvent by
    /// this amount.
    pub fn bad_debt(&self) -> Money {
        Money(self.bad_debt)
    }

    /// Sum of all account collateral.
    pub fn total_collateral(&self) -> Money {
        Money(self.accounts.values().map(|a| a.collateral).sum())
    }

    /// Sum of all position cost bases.
    pub fn total_basis(&self) -> Money {
        Money(
            self.accounts
                .values()
                .flat_map(|a| a.positions.values())
                .map(|p| p.basis)
                .sum(),
        )
    }

    /// Sum of signed position sizes in a market (always zero).
    pub fn position_sum(&self, market: MarketId) -> i128 {
        self.accounts
            .values()
            .filter_map(|a| a.positions.get(&market))
            .map(|p| i128::from(p.size))
            .sum()
    }

    /// Funding fraction carried into the next interval, and the last rate.
    pub fn funding_state(&self, market: MarketId) -> Option<(u128, i64)> {
        self.markets
            .get(&market)
            .map(|m| (m.funding_carry, m.last_funding_rate))
    }

    /// Is a maintenance check queued for the account?
    pub fn is_pending_liquidation(&self, id: AccountId) -> bool {
        self.pending_liq.contains(&id)
    }

    /// Accounts holding positions whose equity at mark is negative.
    pub fn underwater_accounts(&self) -> Vec<AccountId> {
        self.accounts
            .iter()
            .filter(|(_, a)| !a.positions.is_empty())
            .filter(|(_, a)| self.equity_of(a, self.clock, false).is_ok_and(|e| e < 0))
            .map(|(id, _)| *id)
            .collect()
    }

    /// SHA-256 over the canonical encoding of the whole state.
    pub fn state_hash(&self) -> Digest {
        let mut w = crate::codec::Writer::new_hash();
        self.encode_state(&mut w);
        w.finish_hash()
    }

    /// Top of book.
    pub fn book_view(&self, market: MarketId, depth: usize) -> Option<BookView> {
        let m = self.markets.get(&market)?;
        Some(BookView {
            market,
            bids: m.book.depth(Side::Buy, depth),
            asks: m.book.depth(Side::Sell, depth),
        })
    }

    /// Account state with margin figures at the current marks.
    pub fn account_view(&self, id: AccountId) -> Option<AccountView> {
        let a = self.accounts.get(&id)?;
        let equity = self.equity_of(a, self.clock, false).unwrap_or(a.collateral);
        let initial = self
            .margin_requirement(a, MarginKind::Initial, true, self.clock, false, None)
            .unwrap_or(0);
        let maintenance = self
            .margin_requirement(a, MarginKind::Maintenance, false, self.clock, false, None)
            .unwrap_or(0);
        let positions = a
            .positions
            .iter()
            .map(|(market, p)| {
                let mark = self.markets.get(market).and_then(|m| m.mark);
                let unrealized = mark
                    .and_then(|mk| crate::fixed::signed_value(p.size, mk).ok())
                    .map_or(0, |v| v.0.saturating_sub(p.basis));
                let entry_price = crate::fixed::mul_div(
                    p.basis.abs(),
                    i128::from(QTY_SCALE),
                    i128::from(p.size.unsigned_abs()),
                    Round::Floor,
                )
                .ok()
                .and_then(|v| u64::try_from(v).ok())
                .map(Price);
                PositionView {
                    market: *market,
                    size: p.size,
                    entry_price,
                    mark,
                    unrealized: Money(unrealized),
                }
            })
            .collect();
        Some(AccountView {
            id,
            collateral: Money(a.collateral),
            equity: Money(equity),
            initial_margin: Money(initial),
            maintenance_margin: Money(maintenance),
            open_orders: u32::try_from(a.orders.len()).unwrap_or(u32::MAX),
            fee_tier: a.fee_tier,
            positions,
        })
    }

    /// Engine-wide counters.
    pub fn summary(&self) -> Summary {
        Summary {
            events_applied: self.events_applied,
            clock: self.clock,
            accounts: u32::try_from(self.accounts.len()).unwrap_or(u32::MAX),
            markets: u32::try_from(self.markets.len()).unwrap_or(u32::MAX),
            open_orders: self.markets.values().map(|m| m.book.len() as u64).sum(),
            total_collateral: self.total_collateral(),
            insurance_fund: self.insurance_fund(),
            fee_pool: self.fee_pool(),
            bad_debt: self.bad_debt(),
            accounts_with_positions: u32::try_from(
                self.accounts
                    .values()
                    .filter(|a| !a.positions.is_empty())
                    .count(),
            )
            .unwrap_or(u32::MAX),
            liquidation_steps: self.liq_steps,
            adl_fills: self.adl_fills,
            fills: self.fills,
            halted: self.halted.is_some(),
        }
    }
}
