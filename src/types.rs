//! Public data types: configuration, input events, output events.
//!
//! Everything here has a canonical binary encoding ([`Encode`]/[`Decode`])
//! used by the write-ahead log and snapshots.

use crate::codec::{Decode, DecodeError, Encode, Reader, Writer};
use crate::error::{Error, Result};
use crate::fixed::{
    Money, Price, Qty, BPS_SCALE, MAX_AMOUNT, MAX_PRICE, MAX_QTY, PPM_SCALE, RATE_SCALE,
};

/// External account identifier chosen by the caller.
pub type AccountId = u64;
/// Engine-assigned order identifier (monotone, never reused).
pub type OrderId = u64;
/// Market identifier.
pub type MarketId = u32;
/// Milliseconds since an arbitrary epoch supplied by the event stream.
pub type Millis = u64;

/// Maximum number of markets.
pub const MAX_MARKETS: usize = 64;
/// Maximum number of margin tiers per market.
pub const MAX_TIERS: usize = 8;
/// Maximum number of fee tiers.
pub const MAX_FEE_TIERS: usize = 8;
/// Hard ceiling on `EngineConfig::max_accounts`.
pub const MAX_ACCOUNTS_LIMIT: u32 = 1_000_000;

/// Order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Side {
    /// Buy / long.
    Buy,
    /// Sell / short.
    Sell,
}

impl Side {
    /// The other side.
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    /// `+1` for buys, `-1` for sells.
    pub fn sign(self) -> i64 {
        match self {
            Side::Buy => 1,
            Side::Sell => -1,
        }
    }
}

/// Time in force of a limit order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tif {
    /// Good till cancelled: unfilled remainder rests on the book.
    Gtc,
    /// Immediate or cancel: unfilled remainder is cancelled.
    Ioc,
    /// Fill or kill: all or nothing, nothing rests.
    Fok,
    /// Post only: rejected if it would match on entry.
    PostOnly,
}

/// Self-trade prevention mode, chosen per order.
///
/// A self trade is a match between two orders of the same account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StpMode {
    /// Cancel the incoming (newest) order's remainder; the resting order stays.
    CancelNewest,
    /// Cancel the resting (oldest) order and keep matching the incoming one.
    CancelOldest,
    /// Cancel both.
    CancelBoth,
}

/// Order type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    /// Limit order at `price` with a time in force.
    Limit {
        /// Limit price (must be a multiple of the tick size).
        price: Price,
        /// Time in force.
        tif: Tif,
    },
    /// Market order. It is bounded by a protection price that lies
    /// `max_slippage_bps` away from the best opposite price at submission;
    /// any remainder is cancelled.
    Market {
        /// Maximum slippage from the best opposite price, in basis points.
        max_slippage_bps: u32,
    },
}

/// A new order request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceOrder {
    /// Market.
    pub market: MarketId,
    /// Owning account.
    pub account: AccountId,
    /// Side.
    pub side: Side,
    /// Type.
    pub kind: OrderKind,
    /// Quantity.
    pub qty: Qty,
    /// Caller-chosen id, unique among the account's live orders.
    pub client_id: u64,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Self-trade prevention.
    pub stp: StpMode,
}

/// Fee schedule entry. Negative `maker_ppm` is a rebate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeTier {
    /// Maker fee in ppm of notional; negative means rebate.
    pub maker_ppm: i32,
    /// Taker fee in ppm of notional.
    pub taker_ppm: u32,
}

/// Global engine configuration (the first event of every log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    /// Maximum number of accounts.
    pub max_accounts: u32,
    /// Liquidation steps performed per event; the rest is deferred.
    pub max_liquidations_per_event: u32,
    /// Fee tiers; account fee tier 0 is the default.
    pub fee_tiers: Vec<FeeTier>,
}

impl EngineConfig {
    /// Validate ranges.
    pub fn validate(&self) -> Result<()> {
        if self.max_accounts == 0 || self.max_accounts > MAX_ACCOUNTS_LIMIT {
            return Err(Error::InvalidConfig("max_accounts"));
        }
        if self.max_liquidations_per_event == 0 || self.max_liquidations_per_event > 10_000 {
            return Err(Error::InvalidConfig("max_liquidations_per_event"));
        }
        if self.fee_tiers.is_empty() || self.fee_tiers.len() > MAX_FEE_TIERS {
            return Err(Error::InvalidConfig("fee_tiers count"));
        }
        for t in &self.fee_tiers {
            if u64::from(t.taker_ppm) > PPM_SCALE / 10 || t.maker_ppm.unsigned_abs() > 100_000 {
                return Err(Error::InvalidConfig("fee tier range"));
            }
            if t.maker_ppm < 0 && t.maker_ppm.unsigned_abs() > t.taker_ppm {
                return Err(Error::InvalidConfig("rebate exceeds taker fee"));
            }
        }
        Ok(())
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            max_accounts: 10_000,
            max_liquidations_per_event: 64,
            fee_tiers: vec![
                FeeTier {
                    maker_ppm: 200,
                    taker_ppm: 500,
                },
                FeeTier {
                    maker_ppm: -50,
                    taker_ppm: 300,
                },
            ],
        }
    }
}

/// One bracket of the margin schedule. Brackets are marginal: only the part of
/// a position's notional inside the bracket is charged at the bracket's rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginTier {
    /// Upper bound of the bracket (position notional, money units). The last
    /// tier must use [`MarginTier::UNBOUNDED`].
    pub up_to: Money,
    /// Initial margin rate in basis points.
    pub imr_bps: u32,
    /// Maintenance margin rate in basis points.
    pub mmr_bps: u32,
}

impl MarginTier {
    /// Upper bound used by the last bracket.
    pub const UNBOUNDED: Money = Money(i128::MAX);
}

/// Per-market parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketConfig {
    /// Market id.
    pub id: MarketId,
    /// Price tick (raw price units).
    pub tick: u64,
    /// Quantity lot (raw quantity units).
    pub lot: u64,
    /// Minimum order notional.
    pub min_notional: Money,
    /// Maximum quantity of one order.
    pub max_order_qty: u64,
    /// Maximum absolute position per account.
    pub max_position: u64,
    /// Maximum leverage: the initial margin of a position is at least
    /// `notional / max_leverage`, whatever the tier schedule says.
    pub max_leverage: u32,
    /// Marginal margin brackets, ascending.
    pub tiers: Vec<MarginTier>,
    /// Live orders per account.
    pub max_orders_per_account: u32,
    /// Live orders per book (both sides).
    pub max_book_orders: u32,
    /// Price levels per side.
    pub max_levels_per_side: u32,
    /// Share of a position closed per liquidation step, in bps.
    pub liq_chunk_bps: u32,
    /// Liquidation IOC price band around mark, in bps.
    pub liq_band_bps: u32,
    /// Fee on liquidation fills, ppm of notional (goes to the insurance fund).
    pub liq_fee_ppm: u32,
    /// Largest accepted per-update move of the index price, in bps.
    pub oracle_max_change_bps: u32,
    /// Oracle staleness limit in milliseconds.
    pub oracle_max_staleness_ms: u64,
    /// Largest distance of mark from index, in bps.
    pub max_mark_deviation_bps: u32,
    /// Funding interval in milliseconds.
    pub funding_interval_ms: u64,
    /// Funding rate clamp per interval, in ppb (1e-9).
    pub funding_cap_ppb: i64,
    /// Constant interest component added to the premium, ppb per interval.
    pub funding_interest_ppb: i64,
}

impl MarketConfig {
    /// A reasonable default configuration for tests and the simulator.
    pub fn example(id: MarketId) -> MarketConfig {
        MarketConfig {
            id,
            tick: 10_000,                    // 0.01 quote
            lot: 10,                         // 0.001 base
            min_notional: Money(10_000_000), // 10 quote
            max_order_qty: 1_000_000,        // 100 base
            max_position: 5_000_000,
            max_leverage: 20,
            tiers: vec![
                MarginTier {
                    up_to: Money(500_000_000_000),
                    imr_bps: 500,
                    mmr_bps: 250,
                },
                MarginTier {
                    up_to: Money(5_000_000_000_000),
                    imr_bps: 1000,
                    mmr_bps: 500,
                },
                MarginTier {
                    up_to: MarginTier::UNBOUNDED,
                    imr_bps: 2000,
                    mmr_bps: 1000,
                },
            ],
            max_orders_per_account: 64,
            max_book_orders: 20_000,
            max_levels_per_side: 2_000,
            liq_chunk_bps: 2500,
            liq_band_bps: 500,
            liq_fee_ppm: 2_000,
            oracle_max_change_bps: 500,
            oracle_max_staleness_ms: 60_000,
            max_mark_deviation_bps: 300,
            funding_interval_ms: 3_600_000,
            funding_cap_ppb: 5_000_000,
            funding_interest_ppb: 100_000,
        }
    }

    /// Validate ranges and consistency.
    pub fn validate(&self) -> Result<()> {
        let bad = |why: &'static str| Err(Error::InvalidConfig(why));
        if self.tick == 0 || self.tick > MAX_PRICE {
            return bad("tick");
        }
        if self.lot == 0 || self.lot > MAX_QTY {
            return bad("lot");
        }
        if self.min_notional.0 < 0 || self.min_notional.0 > MAX_AMOUNT {
            return bad("min_notional");
        }
        if self.max_order_qty < self.lot
            || self.max_order_qty > MAX_QTY
            || self.max_order_qty % self.lot != 0
        {
            return bad("max_order_qty");
        }
        if self.max_position < self.max_order_qty || self.max_position > MAX_QTY {
            return bad("max_position");
        }
        if self.max_leverage == 0 || self.max_leverage > 1000 {
            return bad("max_leverage");
        }
        if self.tiers.is_empty() || self.tiers.len() > MAX_TIERS {
            return bad("tiers count");
        }
        let mut prev_up_to = 0i128;
        let mut prev_imr = 0u32;
        for (i, t) in self.tiers.iter().enumerate() {
            if t.up_to.0 <= prev_up_to {
                return bad("tiers not ascending");
            }
            let last = i + 1 == self.tiers.len();
            if last != (t.up_to == MarginTier::UNBOUNDED) {
                return bad("last tier must be unbounded");
            }
            if t.mmr_bps == 0 || t.mmr_bps >= t.imr_bps || u64::from(t.imr_bps) > BPS_SCALE {
                return bad("tier rates");
            }
            if t.imr_bps < prev_imr {
                return bad("tier imr decreasing");
            }
            prev_up_to = t.up_to.0;
            prev_imr = t.imr_bps;
        }
        // (no extra consistency rule: max_leverage is applied on top of the tiers)
        if self.max_orders_per_account == 0 || self.max_orders_per_account > 10_000 {
            return bad("max_orders_per_account");
        }
        if self.max_book_orders == 0 || self.max_book_orders > 5_000_000 {
            return bad("max_book_orders");
        }
        if self.max_levels_per_side == 0 || self.max_levels_per_side > 1_000_000 {
            return bad("max_levels_per_side");
        }
        if self.liq_chunk_bps == 0 || u64::from(self.liq_chunk_bps) > BPS_SCALE {
            return bad("liq_chunk_bps");
        }
        if self.liq_band_bps == 0 || self.liq_band_bps > 5_000 {
            return bad("liq_band_bps");
        }
        if u64::from(self.liq_fee_ppm) > PPM_SCALE / 10 {
            return bad("liq_fee_ppm");
        }
        if self.oracle_max_change_bps == 0 || self.oracle_max_change_bps > 5_000 {
            return bad("oracle_max_change_bps");
        }
        if self.oracle_max_staleness_ms == 0 || self.oracle_max_staleness_ms > 86_400_000 {
            return bad("oracle_max_staleness_ms");
        }
        if self.max_mark_deviation_bps == 0 || self.max_mark_deviation_bps > 5_000 {
            return bad("max_mark_deviation_bps");
        }
        if self.funding_interval_ms < 1_000 || self.funding_interval_ms > 86_400_000 {
            return bad("funding_interval_ms");
        }
        let cap = self.funding_cap_ppb;
        if cap < 0 || cap.unsigned_abs() > RATE_SCALE / 10 {
            return bad("funding_cap_ppb");
        }
        if self.funding_interest_ppb.unsigned_abs() > cap.unsigned_abs() {
            return bad("funding_interest_ppb");
        }
        Ok(())
    }

    /// Initial margin rate of the first bracket, in bps.
    pub fn base_imr_bps(&self) -> u32 {
        self.tiers.first().map_or(BPS_SCALE as u32, |t| t.imr_bps)
    }
}

/// Input events. The engine is a pure function of the ordered event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventBody {
    /// Global configuration; must be the first event, exactly once.
    Configure(EngineConfig),
    /// Register a market.
    AddMarket(MarketConfig),
    /// Assign an account to a fee tier (account must exist).
    SetFeeTier {
        /// Account.
        account: AccountId,
        /// Fee tier index.
        tier: u8,
    },
    /// Credit collateral to an account (created on first deposit).
    Deposit {
        /// Account.
        account: AccountId,
        /// Amount, strictly positive.
        amount: Money,
    },
    /// Debit collateral; must keep initial margin satisfied.
    Withdraw {
        /// Account.
        account: AccountId,
        /// Amount, strictly positive.
        amount: Money,
    },
    /// Add money to the insurance fund from outside the system.
    FundInsurance {
        /// Amount, strictly positive.
        amount: Money,
    },
    /// Place an order.
    Place(PlaceOrder),
    /// Cancel one order.
    Cancel {
        /// Caller.
        account: AccountId,
        /// Order to cancel.
        order_id: OrderId,
    },
    /// Cancel all live orders of an account, optionally for one market.
    CancelAll {
        /// Caller.
        account: AccountId,
        /// Restrict to a market.
        market: Option<MarketId>,
    },
    /// Cancel-replace. Priority is kept only when the price is unchanged and
    /// the quantity does not grow.
    Replace {
        /// Caller.
        account: AccountId,
        /// Order to replace.
        order_id: OrderId,
        /// New price (`None` keeps the current price).
        price: Option<Price>,
        /// New total remaining quantity (`None` keeps it).
        qty: Option<Qty>,
    },
    /// Oracle input: new index price observed at `source_ts`.
    Oracle {
        /// Market.
        market: MarketId,
        /// Index price.
        index_price: Price,
        /// When the source observed the price.
        source_ts: Millis,
    },
    /// Advance the clock (drives funding and deferred liquidations).
    Tick,
}

/// A totally ordered log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Timestamp assigned by the sequencer; non-decreasing.
    pub ts: Millis,
    /// Payload.
    pub body: EventBody,
}

/// Why an order left the book unfilled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// Requested by the owner.
    User,
    /// IOC or market order remainder.
    Remainder,
    /// Self-trade prevention.
    SelfTrade,
    /// Reduce-only order no longer reduces anything.
    ReduceOnly,
    /// Remainder could not rest because the book is full.
    BookFull,
    /// Cancelled because the account is being liquidated.
    Liquidation,
}

/// Output events produced by [`crate::engine::Engine::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Order accepted and assigned an id.
    Accepted {
        /// Assigned id.
        order_id: OrderId,
        /// Owner.
        account: AccountId,
        /// Market.
        market: MarketId,
        /// Client id.
        client_id: u64,
    },
    /// Order rests on the book.
    Rested {
        /// Order.
        order_id: OrderId,
        /// Remaining quantity.
        qty: Qty,
        /// Resting price.
        price: Price,
    },
    /// A trade.
    Fill {
        /// Market.
        market: MarketId,
        /// Taker order.
        taker_order: OrderId,
        /// Maker order (0 for auto-deleveraging fills).
        maker_order: OrderId,
        /// Taker account.
        taker: AccountId,
        /// Maker account.
        maker: AccountId,
        /// Execution price (maker price).
        price: Price,
        /// Executed quantity.
        qty: Qty,
        /// Taker side.
        taker_side: Side,
        /// Fee charged to the taker.
        taker_fee: Money,
        /// Fee charged to the maker; negative is a rebate.
        maker_fee: Money,
        /// True when the taker was being liquidated.
        liquidation: bool,
    },
    /// An order was removed without (fully) filling.
    Canceled {
        /// Order.
        order_id: OrderId,
        /// Owner.
        account: AccountId,
        /// Quantity that was still open.
        qty: Qty,
        /// Why.
        reason: CancelReason,
    },
    /// An order was replaced.
    Replaced {
        /// Order.
        order_id: OrderId,
        /// True when time priority was kept.
        kept_priority: bool,
    },
    /// Collateral credited.
    Deposited {
        /// Account.
        account: AccountId,
        /// Amount.
        amount: Money,
        /// Balance after.
        balance: Money,
    },
    /// Collateral debited.
    Withdrawn {
        /// Account.
        account: AccountId,
        /// Amount.
        amount: Money,
        /// Balance after.
        balance: Money,
    },
    /// Oracle input accepted.
    MarkUpdated {
        /// Market.
        market: MarketId,
        /// Index price after filtering.
        index: Price,
        /// Mark price.
        mark: Price,
        /// True when the bounded-change filter clamped the update.
        clamped: bool,
    },
    /// Funding settled for a market.
    FundingSettled {
        /// Market.
        market: MarketId,
        /// Rate applied (ppb of notional per interval; positive: longs pay).
        rate_ppb: i64,
        /// Total amount moved from payers to receivers.
        transferred: Money,
        /// Fractional remainder carried into the next interval (numerator
        /// over the funding denominator).
        carry: u128,
    },
    /// Liquidation of an account started.
    LiquidationStarted {
        /// Account.
        account: AccountId,
        /// Equity at mark when it was triggered.
        equity: Money,
    },
    /// The insurance fund paid a shortfall.
    InsurancePaid {
        /// Account that was covered.
        account: AccountId,
        /// Amount paid.
        amount: Money,
    },
    /// Auto-deleveraging: part of a bankrupt position was closed against a
    /// counterparty at the bankruptcy price.
    Deleveraged {
        /// Market.
        market: MarketId,
        /// Bankrupt account.
        bankrupt: AccountId,
        /// Counterparty that was reduced.
        counterparty: AccountId,
        /// Price used.
        price: Price,
        /// Quantity.
        qty: Qty,
    },
    /// Insurance fund credited from outside.
    InsuranceFunded {
        /// Amount.
        amount: Money,
    },
    /// Fee tier changed.
    FeeTierSet {
        /// Account.
        account: AccountId,
        /// Tier.
        tier: u8,
    },
    /// Market registered.
    MarketAdded {
        /// Market.
        market: MarketId,
    },
    /// Engine configured.
    Configured,
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

impl Encode for Side {
    fn encode(&self, w: &mut Writer) {
        w.u8(match self {
            Side::Buy => 0,
            Side::Sell => 1,
        });
    }
}

impl Decode for Side {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        match r.u8()? {
            0 => Ok(Side::Buy),
            1 => Ok(Side::Sell),
            _ => Err(DecodeError("invalid side")),
        }
    }
}

impl Encode for Tif {
    fn encode(&self, w: &mut Writer) {
        w.u8(match self {
            Tif::Gtc => 0,
            Tif::Ioc => 1,
            Tif::Fok => 2,
            Tif::PostOnly => 3,
        });
    }
}

impl Decode for Tif {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        match r.u8()? {
            0 => Ok(Tif::Gtc),
            1 => Ok(Tif::Ioc),
            2 => Ok(Tif::Fok),
            3 => Ok(Tif::PostOnly),
            _ => Err(DecodeError("invalid tif")),
        }
    }
}

impl Encode for StpMode {
    fn encode(&self, w: &mut Writer) {
        w.u8(match self {
            StpMode::CancelNewest => 0,
            StpMode::CancelOldest => 1,
            StpMode::CancelBoth => 2,
        });
    }
}

impl Decode for StpMode {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        match r.u8()? {
            0 => Ok(StpMode::CancelNewest),
            1 => Ok(StpMode::CancelOldest),
            2 => Ok(StpMode::CancelBoth),
            _ => Err(DecodeError("invalid stp mode")),
        }
    }
}

impl Encode for OrderKind {
    fn encode(&self, w: &mut Writer) {
        match self {
            OrderKind::Limit { price, tif } => {
                w.u8(0);
                w.u64(price.0);
                tif.encode(w);
            }
            OrderKind::Market { max_slippage_bps } => {
                w.u8(1);
                w.u32(*max_slippage_bps);
            }
        }
    }
}

impl Decode for OrderKind {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        match r.u8()? {
            0 => Ok(OrderKind::Limit {
                price: Price(r.u64()?),
                tif: Tif::decode(r)?,
            }),
            1 => Ok(OrderKind::Market {
                max_slippage_bps: r.u32()?,
            }),
            _ => Err(DecodeError("invalid order kind")),
        }
    }
}

impl Encode for PlaceOrder {
    fn encode(&self, w: &mut Writer) {
        w.u32(self.market);
        w.u64(self.account);
        self.side.encode(w);
        self.kind.encode(w);
        w.u64(self.qty.0);
        w.u64(self.client_id);
        w.bool(self.reduce_only);
        self.stp.encode(w);
    }
}

impl Decode for PlaceOrder {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        Ok(PlaceOrder {
            market: r.u32()?,
            account: r.u64()?,
            side: Side::decode(r)?,
            kind: OrderKind::decode(r)?,
            qty: Qty(r.u64()?),
            client_id: r.u64()?,
            reduce_only: r.bool()?,
            stp: StpMode::decode(r)?,
        })
    }
}

impl Encode for FeeTier {
    fn encode(&self, w: &mut Writer) {
        w.i32(self.maker_ppm);
        w.u32(self.taker_ppm);
    }
}

impl Decode for FeeTier {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        Ok(FeeTier {
            maker_ppm: r.i32()?,
            taker_ppm: r.u32()?,
        })
    }
}

impl Encode for EngineConfig {
    fn encode(&self, w: &mut Writer) {
        w.u32(self.max_accounts);
        w.u32(self.max_liquidations_per_event);
        w.count(self.fee_tiers.len());
        for t in &self.fee_tiers {
            t.encode(w);
        }
    }
}

impl Decode for EngineConfig {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        let max_accounts = r.u32()?;
        let max_liquidations_per_event = r.u32()?;
        let n = r.count(MAX_FEE_TIERS, 8)?;
        let mut fee_tiers = Vec::with_capacity(n);
        for _ in 0..n {
            fee_tiers.push(FeeTier::decode(r)?);
        }
        Ok(EngineConfig {
            max_accounts,
            max_liquidations_per_event,
            fee_tiers,
        })
    }
}

impl Encode for MarginTier {
    fn encode(&self, w: &mut Writer) {
        w.i128(self.up_to.0);
        w.u32(self.imr_bps);
        w.u32(self.mmr_bps);
    }
}

impl Decode for MarginTier {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        Ok(MarginTier {
            up_to: Money(r.i128()?),
            imr_bps: r.u32()?,
            mmr_bps: r.u32()?,
        })
    }
}

impl Encode for MarketConfig {
    fn encode(&self, w: &mut Writer) {
        w.u32(self.id);
        w.u64(self.tick);
        w.u64(self.lot);
        w.i128(self.min_notional.0);
        w.u64(self.max_order_qty);
        w.u64(self.max_position);
        w.u32(self.max_leverage);
        w.count(self.tiers.len());
        for t in &self.tiers {
            t.encode(w);
        }
        w.u32(self.max_orders_per_account);
        w.u32(self.max_book_orders);
        w.u32(self.max_levels_per_side);
        w.u32(self.liq_chunk_bps);
        w.u32(self.liq_band_bps);
        w.u32(self.liq_fee_ppm);
        w.u32(self.oracle_max_change_bps);
        w.u64(self.oracle_max_staleness_ms);
        w.u32(self.max_mark_deviation_bps);
        w.u64(self.funding_interval_ms);
        w.i64(self.funding_cap_ppb);
        w.i64(self.funding_interest_ppb);
    }
}

impl Decode for MarketConfig {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        let id = r.u32()?;
        let tick = r.u64()?;
        let lot = r.u64()?;
        let min_notional = Money(r.i128()?);
        let max_order_qty = r.u64()?;
        let max_position = r.u64()?;
        let max_leverage = r.u32()?;
        let n = r.count(MAX_TIERS, 24)?;
        let mut tiers = Vec::with_capacity(n);
        for _ in 0..n {
            tiers.push(MarginTier::decode(r)?);
        }
        Ok(MarketConfig {
            id,
            tick,
            lot,
            min_notional,
            max_order_qty,
            max_position,
            max_leverage,
            tiers,
            max_orders_per_account: r.u32()?,
            max_book_orders: r.u32()?,
            max_levels_per_side: r.u32()?,
            liq_chunk_bps: r.u32()?,
            liq_band_bps: r.u32()?,
            liq_fee_ppm: r.u32()?,
            oracle_max_change_bps: r.u32()?,
            oracle_max_staleness_ms: r.u64()?,
            max_mark_deviation_bps: r.u32()?,
            funding_interval_ms: r.u64()?,
            funding_cap_ppb: r.i64()?,
            funding_interest_ppb: r.i64()?,
        })
    }
}

impl Encode for EventBody {
    fn encode(&self, w: &mut Writer) {
        match self {
            EventBody::Configure(c) => {
                w.u8(0);
                c.encode(w);
            }
            EventBody::AddMarket(m) => {
                w.u8(1);
                m.encode(w);
            }
            EventBody::SetFeeTier { account, tier } => {
                w.u8(2);
                w.u64(*account);
                w.u8(*tier);
            }
            EventBody::Deposit { account, amount } => {
                w.u8(3);
                w.u64(*account);
                w.i128(amount.0);
            }
            EventBody::Withdraw { account, amount } => {
                w.u8(4);
                w.u64(*account);
                w.i128(amount.0);
            }
            EventBody::FundInsurance { amount } => {
                w.u8(5);
                w.i128(amount.0);
            }
            EventBody::Place(p) => {
                w.u8(6);
                p.encode(w);
            }
            EventBody::Cancel { account, order_id } => {
                w.u8(7);
                w.u64(*account);
                w.u64(*order_id);
            }
            EventBody::CancelAll { account, market } => {
                w.u8(8);
                w.u64(*account);
                match market {
                    Some(m) => {
                        w.bool(true);
                        w.u32(*m);
                    }
                    None => w.bool(false),
                }
            }
            EventBody::Replace {
                account,
                order_id,
                price,
                qty,
            } => {
                w.u8(9);
                w.u64(*account);
                w.u64(*order_id);
                match price {
                    Some(p) => {
                        w.bool(true);
                        w.u64(p.0);
                    }
                    None => w.bool(false),
                }
                match qty {
                    Some(q) => {
                        w.bool(true);
                        w.u64(q.0);
                    }
                    None => w.bool(false),
                }
            }
            EventBody::Oracle {
                market,
                index_price,
                source_ts,
            } => {
                w.u8(10);
                w.u32(*market);
                w.u64(index_price.0);
                w.u64(*source_ts);
            }
            EventBody::Tick => w.u8(11),
        }
    }
}

impl Decode for EventBody {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        Ok(match r.u8()? {
            0 => EventBody::Configure(EngineConfig::decode(r)?),
            1 => EventBody::AddMarket(MarketConfig::decode(r)?),
            2 => EventBody::SetFeeTier {
                account: r.u64()?,
                tier: r.u8()?,
            },
            3 => EventBody::Deposit {
                account: r.u64()?,
                amount: Money(r.i128()?),
            },
            4 => EventBody::Withdraw {
                account: r.u64()?,
                amount: Money(r.i128()?),
            },
            5 => EventBody::FundInsurance {
                amount: Money(r.i128()?),
            },
            6 => EventBody::Place(PlaceOrder::decode(r)?),
            7 => EventBody::Cancel {
                account: r.u64()?,
                order_id: r.u64()?,
            },
            8 => {
                let account = r.u64()?;
                let market = if r.bool()? { Some(r.u32()?) } else { None };
                EventBody::CancelAll { account, market }
            }
            9 => {
                let account = r.u64()?;
                let order_id = r.u64()?;
                let price = if r.bool()? {
                    Some(Price(r.u64()?))
                } else {
                    None
                };
                let qty = if r.bool()? { Some(Qty(r.u64()?)) } else { None };
                EventBody::Replace {
                    account,
                    order_id,
                    price,
                    qty,
                }
            }
            10 => EventBody::Oracle {
                market: r.u32()?,
                index_price: Price(r.u64()?),
                source_ts: r.u64()?,
            },
            11 => EventBody::Tick,
            _ => return Err(DecodeError("invalid event tag")),
        })
    }
}

impl Encode for Event {
    fn encode(&self, w: &mut Writer) {
        w.u64(self.ts);
        self.body.encode(w);
    }
}

impl Decode for Event {
    fn decode(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        Ok(Event {
            ts: r.u64()?,
            body: EventBody::decode(r)?,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::codec::{from_bytes, to_bytes};

    fn sample_events() -> Vec<Event> {
        let place = PlaceOrder {
            market: 1,
            account: 9,
            side: Side::Sell,
            kind: OrderKind::Limit {
                price: Price(123_450_000),
                tif: Tif::PostOnly,
            },
            qty: Qty(1000),
            client_id: 77,
            reduce_only: true,
            stp: StpMode::CancelBoth,
        };
        vec![
            Event {
                ts: 1,
                body: EventBody::Configure(EngineConfig::default()),
            },
            Event {
                ts: 2,
                body: EventBody::AddMarket(MarketConfig::example(3)),
            },
            Event {
                ts: 3,
                body: EventBody::SetFeeTier {
                    account: 5,
                    tier: 1,
                },
            },
            Event {
                ts: 4,
                body: EventBody::Deposit {
                    account: 5,
                    amount: Money(1_000_000),
                },
            },
            Event {
                ts: 5,
                body: EventBody::Withdraw {
                    account: 5,
                    amount: Money(5),
                },
            },
            Event {
                ts: 6,
                body: EventBody::FundInsurance { amount: Money(10) },
            },
            Event {
                ts: 7,
                body: EventBody::Place(place),
            },
            Event {
                ts: 7,
                body: EventBody::Place(PlaceOrder {
                    kind: OrderKind::Market {
                        max_slippage_bps: 50,
                    },
                    ..place
                }),
            },
            Event {
                ts: 8,
                body: EventBody::Cancel {
                    account: 5,
                    order_id: 11,
                },
            },
            Event {
                ts: 8,
                body: EventBody::CancelAll {
                    account: 5,
                    market: None,
                },
            },
            Event {
                ts: 8,
                body: EventBody::CancelAll {
                    account: 5,
                    market: Some(3),
                },
            },
            Event {
                ts: 9,
                body: EventBody::Replace {
                    account: 5,
                    order_id: 4,
                    price: Some(Price(5)),
                    qty: None,
                },
            },
            Event {
                ts: 9,
                body: EventBody::Replace {
                    account: 5,
                    order_id: 4,
                    price: None,
                    qty: Some(Qty(6)),
                },
            },
            Event {
                ts: 10,
                body: EventBody::Oracle {
                    market: 3,
                    index_price: Price(77),
                    source_ts: 9,
                },
            },
            Event {
                ts: 11,
                body: EventBody::Tick,
            },
        ]
    }

    #[test]
    fn every_event_variant_round_trips() {
        for e in sample_events() {
            let bytes = to_bytes(&e);
            let back: Event = from_bytes(&bytes).unwrap();
            assert_eq!(back, e);
        }
    }

    #[test]
    fn every_truncation_of_every_event_is_rejected() {
        for e in sample_events() {
            let bytes = to_bytes(&e);
            for n in 0..bytes.len() {
                assert!(from_bytes::<Event>(&bytes[..n]).is_err());
            }
        }
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        for e in sample_events() {
            let mut bytes = to_bytes(&e);
            bytes.push(0);
            assert!(from_bytes::<Event>(&bytes).is_err());
        }
    }

    #[test]
    fn unknown_tags_are_rejected() {
        let mut bytes = to_bytes(&Event {
            ts: 0,
            body: EventBody::Tick,
        });
        bytes[8] = 200;
        assert!(from_bytes::<Event>(&bytes).is_err());
    }

    #[test]
    fn default_configs_validate() {
        EngineConfig::default().validate().unwrap();
        MarketConfig::example(0).validate().unwrap();
    }

    #[test]
    fn engine_config_rejects_bad_values() {
        let mut c = EngineConfig::default();
        c.max_accounts = 0;
        assert!(c.validate().is_err());
        let mut c = EngineConfig::default();
        c.fee_tiers.clear();
        assert!(c.validate().is_err());
        let mut c = EngineConfig::default();
        c.fee_tiers[0] = FeeTier {
            maker_ppm: -600,
            taker_ppm: 500,
        };
        assert!(c.validate().is_err(), "rebate larger than taker fee");
        let mut c = EngineConfig::default();
        c.fee_tiers[0] = FeeTier {
            maker_ppm: 0,
            taker_ppm: 900_000,
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn market_config_rejects_bad_values() {
        let ok = MarketConfig::example(1);
        let mut m = ok.clone();
        m.tick = 0;
        assert!(m.validate().is_err());
        let mut m = ok.clone();
        m.lot = 0;
        assert!(m.validate().is_err());
        let mut m = ok.clone();
        m.max_order_qty = m.lot + 1;
        assert!(m.validate().is_err());
        let mut m = ok.clone();
        m.tiers.clear();
        assert!(m.validate().is_err());
        let mut m = ok.clone();
        m.tiers[0].mmr_bps = m.tiers[0].imr_bps;
        assert!(m.validate().is_err());
        let mut m = ok.clone();
        m.max_leverage = 0;
        assert!(m.validate().is_err());
        let mut m = ok.clone();
        m.tiers.swap(0, 1);
        assert!(m.validate().is_err());
        let mut m = ok.clone();
        m.tiers.pop();
        assert!(m.validate().is_err(), "last tier must be unbounded");
        let mut m = ok.clone();
        m.funding_interest_ppb = m.funding_cap_ppb + 1;
        assert!(m.validate().is_err());
        let mut m = ok;
        m.funding_interval_ms = 1;
        assert!(m.validate().is_err());
    }

    #[test]
    fn side_helpers() {
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.sign(), -1);
        assert_eq!(Side::Buy.sign(), 1);
    }
}
