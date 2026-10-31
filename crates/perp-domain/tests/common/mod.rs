//! Shared helpers for the domain tests: a tiny harness around [`Engine`] that
//! stamps events with increasing timestamps and offers readable builders.
//!
//! Units used by the helpers: prices in whole quote units (`px(50_000)` is
//! 50 000.00), quantities in milli-base units (`qty(1_000)` is 1.0 base),
//! money in whole quote units (`usd(100)`).
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use perp_domain::engine::Engine;
use perp_domain::error::Error;
use perp_domain::fixed::{Money, Price, Qty};
use perp_domain::types::{
    AccountId, EngineConfig, Event, EventBody, MarketConfig, MarketId, OrderId, OrderKind, Output,
    PlaceOrder, Side, StpMode, Tif,
};

/// Whole quote units to money.
pub fn usd(n: i128) -> Money {
    Money(n * 1_000_000)
}

/// Whole quote units to price.
pub fn px(n: u64) -> Price {
    Price(n * 1_000_000)
}

/// Milli-base units to quantity (1 lot = 10 raw = 1 milli).
pub fn qty(milli: u64) -> Qty {
    Qty(milli * 10)
}

pub const M: MarketId = 1;

/// Engine plus a clock.
pub struct Ex {
    pub engine: Engine,
    pub ts: u64,
    client: u64,
}

impl Ex {
    /// Configured engine, one market (id 1, example parameters) and a fresh
    /// oracle price of 50 000.
    pub fn new() -> Ex {
        Ex::with_market(MarketConfig::example(M))
    }

    pub fn with_market(cfg: MarketConfig) -> Ex {
        let mut ex = Ex::bare();
        let id = cfg.id;
        ex.apply(EventBody::AddMarket(cfg)).unwrap();
        ex.oracle_on(id, 50_000);
        ex
    }

    /// Configured engine without markets.
    pub fn bare() -> Ex {
        let mut ex = Ex {
            engine: Engine::new(),
            ts: 1_000,
            client: 0,
        };
        ex.apply(EventBody::Configure(EngineConfig::default()))
            .unwrap();
        ex
    }

    pub fn apply(&mut self, body: EventBody) -> Result<Vec<Output>, Error> {
        self.ts += 1;
        self.engine.apply(&Event { ts: self.ts, body })
    }

    pub fn apply_at(&mut self, ts: u64, body: EventBody) -> Result<Vec<Output>, Error> {
        self.ts = ts;
        self.engine.apply(&Event { ts, body })
    }

    pub fn advance(&mut self, ms: u64) {
        self.ts += ms;
    }

    pub fn deposit(&mut self, account: AccountId, quote: i128) {
        self.apply(EventBody::Deposit {
            account,
            amount: usd(quote),
        })
        .unwrap();
    }

    /// Fund `accounts` with `quote` each.
    pub fn fund(&mut self, accounts: &[AccountId], quote: i128) {
        for a in accounts {
            self.deposit(*a, quote);
        }
    }

    pub fn oracle(&mut self, price: u64) -> Result<Vec<Output>, Error> {
        self.oracle_on_result(M, price)
    }

    pub fn oracle_on(&mut self, market: MarketId, price: u64) -> Vec<Output> {
        self.oracle_on_result(market, price).unwrap()
    }

    pub fn oracle_on_result(&mut self, market: MarketId, price: u64) -> Result<Vec<Output>, Error> {
        self.ts += 1;
        let ts = self.ts;
        self.engine.apply(&Event {
            ts,
            body: EventBody::Oracle {
                market,
                index_price: px(price),
                source_ts: ts,
            },
        })
    }

    pub fn next_client(&mut self) -> u64 {
        self.client += 1;
        self.client
    }

    pub fn place(&mut self, p: PlaceOrder) -> Result<Vec<Output>, Error> {
        self.apply(EventBody::Place(p))
    }

    pub fn order(
        &mut self,
        account: AccountId,
        side: Side,
        kind: OrderKind,
        milli: u64,
    ) -> PlaceOrder {
        PlaceOrder {
            market: M,
            account,
            side,
            kind,
            qty: qty(milli),
            client_id: self.next_client(),
            reduce_only: false,
            stp: StpMode::CancelNewest,
        }
    }

    pub fn limit(
        &mut self,
        account: AccountId,
        side: Side,
        price: u64,
        milli: u64,
        tif: Tif,
    ) -> Result<Vec<Output>, Error> {
        let p = self.order(
            account,
            side,
            OrderKind::Limit {
                price: px(price),
                tif,
            },
            milli,
        );
        self.place(p)
    }

    pub fn gtc(&mut self, account: AccountId, side: Side, price: u64, milli: u64) -> OrderId {
        let outs = self.limit(account, side, price, milli, Tif::Gtc).unwrap();
        accepted_id(&outs)
    }

    pub fn market(
        &mut self,
        account: AccountId,
        side: Side,
        milli: u64,
        slippage_bps: u32,
    ) -> Result<Vec<Output>, Error> {
        let p = self.order(
            account,
            side,
            OrderKind::Market {
                max_slippage_bps: slippage_bps,
            },
            milli,
        );
        self.place(p)
    }

    /// Open a position of `milli` for `taker` against `maker` at `price`:
    /// `maker` rests on the opposite side and `taker` crosses it.
    pub fn trade(
        &mut self,
        taker: AccountId,
        maker: AccountId,
        side: Side,
        price: u64,
        milli: u64,
    ) {
        self.gtc(maker, side.opposite(), price, milli);
        let outs = self.limit(taker, side, price, milli, Tif::Ioc).unwrap();
        assert!(
            !fills(&outs).is_empty(),
            "trade helper expected a fill: {outs:?}"
        );
    }

    pub fn cancel(&mut self, account: AccountId, order_id: OrderId) -> Result<Vec<Output>, Error> {
        self.apply(EventBody::Cancel { account, order_id })
    }

    pub fn size(&self, account: AccountId) -> i64 {
        self.engine.position(account, M).map_or(0, |p| p.size)
    }

    pub fn collateral(&self, account: AccountId) -> i128 {
        self.engine.collateral(account).unwrap().0
    }

    pub fn check(&self) {
        self.engine.check_invariants().unwrap();
    }
}

pub fn accepted_id(outs: &[Output]) -> OrderId {
    outs.iter()
        .find_map(|o| match o {
            Output::Accepted { order_id, .. } => Some(*order_id),
            _ => None,
        })
        .expect("no Accepted output")
}

pub fn fills(outs: &[Output]) -> Vec<&Output> {
    outs.iter()
        .filter(|o| matches!(o, Output::Fill { .. }))
        .collect()
}

pub fn filled_qty(outs: &[Output]) -> u64 {
    outs.iter()
        .map(|o| match o {
            Output::Fill { qty, .. } => qty.0,
            _ => 0,
        })
        .sum()
}

pub fn has(outs: &[Output], f: impl Fn(&Output) -> bool) -> bool {
    outs.iter().any(f)
}

impl Ex {
    /// Like [`Ex::bare`] but with a caller-chosen engine configuration.
    pub fn bare_with(cfg: EngineConfig) -> Ex {
        let mut ex = Ex {
            engine: Engine::new(),
            ts: 1_000,
            client: 0,
        };
        ex.apply(EventBody::Configure(cfg)).unwrap();
        ex
    }
}
