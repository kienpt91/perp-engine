//! The exchange aggregate.
//!
//! [`Engine`] is a deterministic state machine: `apply(event) -> outputs`. It
//! owns every entity (markets, books, accounts, insurance fund) and is the only
//! thing allowed to change them, so all invariants are enforced in one place.
//!
//! # Event handling contract
//!
//! * Every handler validates first and mutates second. A rejected event returns
//!   `Err` and changes nothing except the applied-event counter.
//! * An error *after* mutation started can only be an arithmetic or internal
//!   inconsistency. The engine then **halts** (fail-stop): the flag is part of
//!   the state hash, so replay halts at exactly the same event, and later
//!   events return [`Error::Halted`] without touching state.
//! * Time comes only from `Event::ts` and never goes backwards. Funding and
//!   deferred liquidations are processed on `Oracle` and `Tick` events.
//! * All collections are `BTreeMap`/`BTreeSet`: iteration order is a function of
//!   the keys, never of memory addresses or hashing seeds.
//!
//! # Money accounting
//!
//! For every account, `collateral` holds deposits plus realised PnL, funding and
//! fees. Positions carry a signed money cost basis (see [`Position`]). Trades
//! move `collateral - basis` by exactly zero in total, so
//!
//! ```text
//! sum(collateral) + insurance + fee_pool - sum(basis) == net_deposits + bad_debt
//! ```
//!
//! holds exactly after every event ([`Engine::check_invariants`] verifies it).
//! `bad_debt` is money that does not exist: it is only ever created when the
//! insurance fund and auto-deleveraging could not cover a loss, and it is
//! reported rather than hidden.

mod funding;
mod orders;
mod pricing;
mod query;
mod risk;
mod settle;
mod state;

use crate::account::Account;
use crate::book::Book;
use crate::error::{Error, Result};
use crate::fixed::{Money, Price, MAX_AMOUNT, MAX_BALANCE};
use crate::margin::MarginKind;
use crate::types::{
    AccountId, EngineConfig, Event, EventBody, MarketConfig, MarketId, Millis, Output, MAX_MARKETS,
};
use std::collections::{BTreeMap, BTreeSet};

/// Largest number of fills one incoming order may produce.
pub const MAX_FILLS_PER_MATCH: usize = 256;

/// Why the engine stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Halt {
    Arithmetic = 1,
    Invariant = 2,
    Funding = 3,
    Adl = 4,
}

impl Halt {
    pub(crate) fn from_u8(v: u8) -> Option<Halt> {
        match v {
            1 => Some(Halt::Arithmetic),
            2 => Some(Halt::Invariant),
            3 => Some(Halt::Funding),
            4 => Some(Halt::Adl),
            _ => None,
        }
    }

    /// Short description for operators.
    pub fn reason(self) -> &'static str {
        match self {
            Halt::Arithmetic => "arithmetic overflow while mutating state",
            Halt::Invariant => "internal invariant violated",
            Halt::Funding => "funding sides out of balance",
            Halt::Adl => "auto-deleveraging found no counterparty",
        }
    }
}

/// Per-market state: configuration, book, prices and funding bookkeeping.
#[derive(Debug, Clone)]
pub(crate) struct MarketState {
    pub cfg: MarketConfig,
    pub book: Book,
    pub index: Option<Price>,
    pub mark: Option<Price>,
    /// `source_ts` of the last accepted oracle update (0 when none).
    pub oracle_ts: Millis,
    pub last_trade: Option<Price>,
    pub premium_sum: i128,
    pub premium_count: u64,
    pub next_funding_ts: Millis,
    /// Numerator (over `QTY_SCALE * RATE_SCALE`) of the funding fraction that
    /// could not be assigned to a whole money unit yet.
    pub funding_carry: u128,
    pub last_funding_rate: i64,
    // derived: accounts holding a non-zero position
    pub holders: BTreeSet<AccountId>,
}

/// The exchange aggregate. See the module docs.
#[derive(Debug, Clone)]
pub struct Engine {
    config: Option<EngineConfig>,
    markets: BTreeMap<MarketId, MarketState>,
    accounts: BTreeMap<AccountId, Account>,
    clock: Millis,
    events_applied: u64,
    next_order_id: u64,
    next_seq: u64,
    insurance: i128,
    fee_pool: i128,
    net_deposits: i128,
    bad_debt: i128,
    pending_liq: BTreeSet<AccountId>,
    halted: Option<Halt>,
    fills: u64,
    liq_steps: u64,
    adl_fills: u64,
    /// Transient: set once a handler has started mutating state.
    mutating: bool,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

impl Engine {
    /// An empty, unconfigured engine. The first event must be `Configure`.
    pub fn new() -> Engine {
        Engine {
            config: None,
            markets: BTreeMap::new(),
            accounts: BTreeMap::new(),
            clock: 0,
            events_applied: 0,
            next_order_id: 1,
            next_seq: 1,
            insurance: 0,
            fee_pool: 0,
            net_deposits: 0,
            bad_debt: 0,
            pending_liq: BTreeSet::new(),
            halted: None,
            fills: 0,
            liq_steps: 0,
            adl_fills: 0,
            mutating: false,
        }
    }

    /// Apply one event. Deterministic: the same state and event always give the
    /// same result. See the module docs for the rejection and halt contract.
    pub fn apply(&mut self, event: &Event) -> Result<Vec<Output>> {
        if self.halted.is_some() {
            return Err(Error::Halted);
        }
        self.events_applied = self.events_applied.saturating_add(1);
        self.mutating = false;
        if event.ts < self.clock {
            return Err(Error::TimeRegression);
        }
        let mut out = Vec::new();
        match self.dispatch(event, &mut out) {
            Ok(()) => Ok(out),
            Err(e) => {
                if self.mutating {
                    self.halted = Some(match e {
                        Error::Overflow | Error::DivisionByZero => Halt::Arithmetic,
                        _ => Halt::Invariant,
                    });
                }
                Err(e)
            }
        }
    }

    fn halt(&mut self, why: Halt) -> Error {
        self.halted = Some(why);
        Error::Internal(why.reason())
    }

    /// Marks the point of no return of a handler.
    fn begin(&mut self, ts: Millis) {
        self.mutating = true;
        self.clock = ts;
    }

    fn dispatch(&mut self, event: &Event, out: &mut Vec<Output>) -> Result<()> {
        let ts = event.ts;
        if let EventBody::Configure(c) = &event.body {
            return self.configure(ts, c, out);
        }
        if self.config.is_none() {
            return Err(Error::NotConfigured);
        }
        match &event.body {
            EventBody::Configure(_) => {}
            EventBody::AddMarket(m) => self.add_market(ts, m, out)?,
            EventBody::SetFeeTier { account, tier } => {
                self.set_fee_tier(ts, *account, *tier, out)?
            }
            EventBody::Deposit { account, amount } => self.deposit(ts, *account, *amount, out)?,
            EventBody::Withdraw { account, amount } => self.withdraw(ts, *account, *amount, out)?,
            EventBody::FundInsurance { amount } => self.fund_insurance(ts, *amount, out)?,
            EventBody::Place(p) => self.place(ts, p, out)?,
            EventBody::Cancel { account, order_id } => self.cancel(ts, *account, *order_id, out)?,
            EventBody::CancelAll { account, market } => {
                self.cancel_all(ts, *account, *market, out)?;
            }
            EventBody::Replace {
                account,
                order_id,
                price,
                qty,
            } => {
                self.replace(ts, *account, *order_id, *price, *qty, out)?;
            }
            EventBody::Oracle {
                market,
                index_price,
                source_ts,
            } => {
                self.oracle(ts, *market, *index_price, *source_ts, out)?;
            }
            EventBody::Tick => self.tick(ts, out)?,
        }
        self.run_liquidations(out)
    }

    fn configure(&mut self, ts: Millis, c: &EngineConfig, out: &mut Vec<Output>) -> Result<()> {
        if self.config.is_some() {
            return Err(Error::AlreadyConfigured);
        }
        c.validate()?;
        self.begin(ts);
        self.config = Some(c.clone());
        out.push(Output::Configured);
        Ok(())
    }

    fn add_market(&mut self, ts: Millis, m: &MarketConfig, out: &mut Vec<Output>) -> Result<()> {
        m.validate()?;
        if self.markets.contains_key(&m.id) {
            return Err(Error::MarketExists);
        }
        if self.markets.len() >= MAX_MARKETS {
            return Err(Error::TooManyMarkets);
        }
        let next_funding_ts = (ts / m.funding_interval_ms)
            .checked_add(1)
            .and_then(|n| n.checked_mul(m.funding_interval_ms))
            .ok_or(Error::Overflow)?;
        self.begin(ts);
        self.markets.insert(
            m.id,
            MarketState {
                cfg: m.clone(),
                book: Book::new(),
                index: None,
                mark: None,
                oracle_ts: 0,
                last_trade: None,
                premium_sum: 0,
                premium_count: 0,
                next_funding_ts,
                funding_carry: 0,
                last_funding_rate: 0,
                holders: BTreeSet::new(),
            },
        );
        out.push(Output::MarketAdded { market: m.id });
        Ok(())
    }

    fn set_fee_tier(
        &mut self,
        ts: Millis,
        account: AccountId,
        tier: u8,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        let tiers = self.config.as_ref().map_or(0, |c| c.fee_tiers.len());
        if usize::from(tier) >= tiers {
            return Err(Error::UnknownFeeTier);
        }
        if !self.accounts.contains_key(&account) {
            return Err(Error::UnknownAccount);
        }
        self.begin(ts);
        self.acct_mut(account)?.fee_tier = tier;
        out.push(Output::FeeTierSet { account, tier });
        Ok(())
    }

    fn deposit(
        &mut self,
        ts: Millis,
        account: AccountId,
        amount: Money,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        if amount.0 <= 0 {
            return Err(Error::ZeroAmount);
        }
        if amount.0 > MAX_AMOUNT {
            return Err(Error::AmountTooLarge);
        }
        let max_accounts = self.config.as_ref().map_or(0, |c| c.max_accounts);
        let existing = self.accounts.get(&account).map(|a| a.collateral);
        if existing.is_none() && self.accounts.len() >= max_accounts as usize {
            return Err(Error::TooManyAccounts);
        }
        let balance = existing
            .unwrap_or(0)
            .checked_add(amount.0)
            .ok_or(Error::AmountTooLarge)?;
        if balance > MAX_BALANCE {
            return Err(Error::AmountTooLarge);
        }
        let net = self
            .net_deposits
            .checked_add(amount.0)
            .ok_or(Error::AmountTooLarge)?;
        self.begin(ts);
        self.net_deposits = net;
        self.accounts.entry(account).or_default().collateral = balance;
        out.push(Output::Deposited {
            account,
            amount,
            balance: Money(balance),
        });
        Ok(())
    }

    fn withdraw(
        &mut self,
        ts: Millis,
        account: AccountId,
        amount: Money,
        out: &mut Vec<Output>,
    ) -> Result<()> {
        if amount.0 <= 0 {
            return Err(Error::ZeroAmount);
        }
        let acct = self.accounts.get(&account).ok_or(Error::UnknownAccount)?;
        if amount.0 > acct.collateral {
            return Err(Error::InsufficientCollateral);
        }
        if !acct.positions.is_empty() || !acct.orders.is_empty() {
            let equity = self.equity_of(acct, ts, true)?;
            let required =
                self.margin_requirement(acct, MarginKind::Initial, true, ts, true, None)?;
            let after = equity.checked_sub(amount.0).ok_or(Error::Overflow)?;
            if after < required {
                return Err(Error::MarginBreach);
            }
        }
        self.begin(ts);
        let acct = self.acct_mut(account)?;
        acct.collateral = acct
            .collateral
            .checked_sub(amount.0)
            .ok_or(Error::Overflow)?;
        let balance = acct.collateral;
        self.net_deposits = self
            .net_deposits
            .checked_sub(amount.0)
            .ok_or(Error::Overflow)?;
        out.push(Output::Withdrawn {
            account,
            amount,
            balance: Money(balance),
        });
        Ok(())
    }

    fn fund_insurance(&mut self, ts: Millis, amount: Money, out: &mut Vec<Output>) -> Result<()> {
        if amount.0 <= 0 {
            return Err(Error::ZeroAmount);
        }
        if amount.0 > MAX_AMOUNT {
            return Err(Error::AmountTooLarge);
        }
        let fund = self
            .insurance
            .checked_add(amount.0)
            .ok_or(Error::AmountTooLarge)?;
        if fund > MAX_BALANCE {
            return Err(Error::AmountTooLarge);
        }
        let net = self
            .net_deposits
            .checked_add(amount.0)
            .ok_or(Error::AmountTooLarge)?;
        self.begin(ts);
        self.insurance = fund;
        self.net_deposits = net;
        out.push(Output::InsuranceFunded { amount });
        Ok(())
    }

    fn acct_mut(&mut self, id: AccountId) -> Result<&mut Account> {
        self.accounts
            .get_mut(&id)
            .ok_or(Error::Internal("account vanished"))
    }

    fn market_mut(&mut self, id: MarketId) -> Result<&mut MarketState> {
        self.markets
            .get_mut(&id)
            .ok_or(Error::Internal("market vanished"))
    }
}
