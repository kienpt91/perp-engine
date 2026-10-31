//! Synthetic order flow (domain service).
//!
//! A pure function of a seed and the engine's current state: it produces the
//! next [`Event`] for a population of simulated traders. It reads no clock and
//! no entropy source, so a run is reproducible from `(config, seed)`.
//!
//! Trader behaviours, assigned by `account_id % 5`:
//!
//! | id % 5 | behaviour       | what it does |
//! |--------|-----------------|--------------|
//! | 0      | market maker    | quotes post-only/GTC orders around the index, cancels and replaces them |
//! | 1      | noise taker     | small market and IOC orders on random sides |
//! | 2      | trend follower  | trades IOC in the direction of the recent index move |
//! | 3      | leveraged whale | rare large orders that use most of the account's margin |
//! | 4      | churner         | random cancels, replaces, reduce-only closes, deposits, withdrawals |
//!
//! A small share of events is deliberately invalid (zero quantity, off-tick
//! price, unknown account) to exercise rejection paths.

use crate::engine::Engine;
use crate::fixed::{Money, Price, Qty};
use crate::rng::Rng;
use crate::types::{
    AccountId, EngineConfig, Event, EventBody, MarketConfig, MarketId, Millis, OrderKind,
    PlaceOrder, Side, StpMode, Tif,
};

/// Parameters of a simulated population.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficConfig {
    /// Seed of the pseudo-random stream.
    pub seed: u64,
    /// Number of trader accounts (ids `1..=accounts`).
    pub accounts: u32,
    /// Number of markets (ids `1..=markets`).
    pub markets: u32,
    /// One oracle update per this many events (on average).
    pub oracle_every: u64,
    /// One tick per this many events (on average).
    pub tick_every: u64,
}

impl Default for TrafficConfig {
    fn default() -> Self {
        TrafficConfig {
            seed: 1,
            accounts: 200,
            markets: 1,
            oracle_every: 25,
            tick_every: 200,
        }
    }
}

/// Initial index price of every market (50 000.00 quote).
pub const START_PRICE: u64 = 50_000_000_000;

/// Generator state.
#[derive(Debug, Clone)]
pub struct TrafficGenerator {
    rng: Rng,
    cfg: TrafficConfig,
    ts: Millis,
    client_seq: u64,
    history: Vec<(MarketId, u64)>,
}

impl TrafficGenerator {
    /// New generator.
    pub fn new(cfg: TrafficConfig) -> TrafficGenerator {
        TrafficGenerator {
            rng: Rng::new(cfg.seed),
            cfg,
            ts: 1_000,
            client_seq: 1,
            history: Vec::new(),
        }
    }

    /// Market configuration used by the simulation (short funding interval so
    /// funding is exercised in short runs).
    pub fn market_config(id: MarketId) -> MarketConfig {
        MarketConfig {
            funding_interval_ms: 60_000,
            ..MarketConfig::example(id)
        }
    }

    /// Events that set the exchange up: configuration, markets, insurance fund,
    /// first oracle prices and one deposit per account.
    pub fn bootstrap(&mut self) -> Vec<Event> {
        let mut ev = Vec::new();
        let mut push = |g: &mut TrafficGenerator, body: EventBody| {
            g.ts += 1;
            ev.push(Event { ts: g.ts, body });
        };
        let config = EngineConfig {
            max_accounts: self.cfg.accounts.saturating_add(16),
            ..EngineConfig::default()
        };
        push(self, EventBody::Configure(config));
        for m in 1..=self.cfg.markets {
            push(self, EventBody::AddMarket(Self::market_config(m)));
        }
        push(
            self,
            EventBody::FundInsurance {
                amount: Money(50_000_000_000),
            },
        );
        for m in 1..=self.cfg.markets {
            let ts = self.ts + 1;
            push(
                self,
                EventBody::Oracle {
                    market: m,
                    index_price: Price(START_PRICE),
                    source_ts: ts,
                },
            );
            self.history.push((m, START_PRICE));
        }
        for a in 1..=u64::from(self.cfg.accounts) {
            let amount = Money(i128::from(self.rng.range(5_000, 500_000)) * 1_000_000);
            push(self, EventBody::Deposit { account: a, amount });
            if a % 3 == 0 {
                push(
                    self,
                    EventBody::SetFeeTier {
                        account: a,
                        tier: 1,
                    },
                );
            }
        }
        ev
    }

    fn fresh_client_id(&mut self) -> u64 {
        self.client_seq += 1;
        self.client_seq
    }

    fn index_of(&self, engine: &Engine, market: MarketId) -> u64 {
        engine.index_price(market).map_or(START_PRICE, |p| p.0)
    }

    fn round_tick(price: u64, tick: u64) -> u64 {
        (price / tick).max(1) * tick
    }

    /// Next event for the current engine state.
    pub fn next_event(&mut self, engine: &Engine) -> Event {
        self.ts += self.rng.range(1, 40);
        let ts = self.ts;
        let r = self.rng.below(1_000);
        let body = if self.rng.chance(1, self.cfg.oracle_every.max(1)) {
            self.oracle_event(engine)
        } else if self.rng.chance(1, self.cfg.tick_every.max(1)) {
            EventBody::Tick
        } else if r < 8 {
            self.invalid_event(engine)
        } else {
            self.trader_event(engine)
        };
        Event { ts, body }
    }

    fn pick_market(&mut self) -> MarketId {
        u32::try_from(self.rng.range(1, u64::from(self.cfg.markets.max(1)))).unwrap_or(1)
    }

    fn oracle_event(&mut self, engine: &Engine) -> EventBody {
        let market = self.pick_market();
        let cur = self.index_of(engine, market);
        let bps = if self.rng.chance(1, 25) {
            self.rng.range(200, 450) // jump
        } else {
            self.rng.range(0, 40)
        };
        let step = u64::try_from(u128::from(cur) * u128::from(bps) / 10_000).unwrap_or(0);
        let next = if self.rng.chance(1, 2) {
            cur.saturating_add(step)
        } else {
            cur.saturating_sub(step)
        };
        let next = next.clamp(1_000_000, 1_000_000_000_000);
        self.history.push((market, next));
        if self.history.len() > 64 {
            self.history.remove(0);
        }
        EventBody::Oracle {
            market,
            index_price: Price(next),
            source_ts: self.ts,
        }
    }

    fn invalid_event(&mut self, engine: &Engine) -> EventBody {
        let market = self.pick_market();
        let account = self.rng.range(1, u64::from(self.cfg.accounts.max(1)));
        let client_id = self.fresh_client_id();
        let idx = self.index_of(engine, market);
        match self.rng.below(4) {
            0 => EventBody::Place(PlaceOrder {
                market,
                account,
                side: Side::Buy,
                kind: OrderKind::Limit {
                    price: Price(idx),
                    tif: Tif::Gtc,
                },
                qty: Qty(0),
                client_id,
                reduce_only: false,
                stp: StpMode::CancelNewest,
            }),
            1 => EventBody::Place(PlaceOrder {
                market,
                account,
                side: Side::Sell,
                kind: OrderKind::Limit {
                    price: Price(idx + 1),
                    tif: Tif::Gtc,
                },
                qty: Qty(100),
                client_id,
                reduce_only: false,
                stp: StpMode::CancelNewest,
            }),
            2 => EventBody::Cancel {
                account,
                order_id: u64::MAX,
            },
            _ => EventBody::Withdraw {
                account: u64::MAX - 1,
                amount: Money(1),
            },
        }
    }

    fn trader_event(&mut self, engine: &Engine) -> EventBody {
        let account: AccountId = self.rng.range(1, u64::from(self.cfg.accounts.max(1)));
        let market = self.pick_market();
        match account % 5 {
            0 => self.maker(engine, account, market),
            1 => self.noise(engine, account, market),
            2 => self.trend(engine, account, market),
            3 => self.whale(engine, account, market),
            _ => self.churn(engine, account, market),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        account: AccountId,
        market: MarketId,
        side: Side,
        kind: OrderKind,
        lots: u64,
        reduce_only: bool,
        stp: StpMode,
    ) -> EventBody {
        let client_id = self.fresh_client_id();
        EventBody::Place(PlaceOrder {
            market,
            account,
            side,
            kind,
            qty: Qty(lots * 10),
            client_id,
            reduce_only,
            stp,
        })
    }

    fn random_side(&mut self) -> Side {
        if self.rng.chance(1, 2) {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    fn maker(&mut self, engine: &Engine, account: AccountId, market: MarketId) -> EventBody {
        let open = engine.open_orders_of(account);
        let idx = self.index_of(engine, market);
        let tick = engine.market_config(market).map_or(10_000, |c| c.tick);
        if !open.is_empty() && self.rng.chance(1, 3) {
            let id = open
                .get(self.rng.below(open.len() as u64) as usize)
                .copied()
                .unwrap_or(0);
            return match self.rng.below(3) {
                0 => EventBody::Cancel {
                    account,
                    order_id: id,
                },
                1 => EventBody::Replace {
                    account,
                    order_id: id,
                    price: engine.order(market, id).map(|o| {
                        let shift = self.rng.range(1, 5) * tick;
                        Price(if o.side == Side::Buy {
                            o.price.0.saturating_sub(shift).max(tick)
                        } else {
                            o.price.0 + shift
                        })
                    }),
                    qty: None,
                },
                _ => EventBody::Replace {
                    account,
                    order_id: id,
                    price: None,
                    qty: Some(Qty(self.rng.range(1, 40) * 10)),
                },
            };
        }
        let side = self.random_side();
        let offset = self.rng.range(1, 200) * (idx / 20_000 / tick).max(1) * tick;
        let price = match side {
            Side::Buy => Self::round_tick(idx.saturating_sub(offset), tick),
            Side::Sell => Self::round_tick(idx + offset, tick),
        };
        let tif = if self.rng.chance(1, 2) {
            Tif::PostOnly
        } else {
            Tif::Gtc
        };
        let lots = self.rng.range(1, 60);
        let stp = if self.rng.chance(1, 2) {
            StpMode::CancelOldest
        } else {
            StpMode::CancelNewest
        };
        self.place(
            account,
            market,
            side,
            OrderKind::Limit {
                price: Price(price),
                tif,
            },
            lots,
            false,
            stp,
        )
    }

    fn noise(&mut self, engine: &Engine, account: AccountId, market: MarketId) -> EventBody {
        let side = self.random_side();
        let lots = self.rng.range(1, 40);
        let idx = self.index_of(engine, market);
        let tick = engine.market_config(market).map_or(10_000, |c| c.tick);
        if self.rng.chance(1, 2) {
            self.place(
                account,
                market,
                side,
                OrderKind::Market {
                    max_slippage_bps: 50,
                },
                lots,
                false,
                StpMode::CancelNewest,
            )
        } else {
            let wiggle = idx / 2_000;
            let price = match side {
                Side::Buy => Self::round_tick(idx + wiggle, tick),
                Side::Sell => Self::round_tick(idx.saturating_sub(wiggle), tick),
            };
            let tif = if self.rng.chance(1, 4) {
                Tif::Fok
            } else {
                Tif::Ioc
            };
            self.place(
                account,
                market,
                side,
                OrderKind::Limit {
                    price: Price(price),
                    tif,
                },
                lots,
                false,
                StpMode::CancelBoth,
            )
        }
    }

    fn trend(&mut self, engine: &Engine, account: AccountId, market: MarketId) -> EventBody {
        let idx = self.index_of(engine, market);
        let older = self
            .history
            .iter()
            .filter(|(m, _)| *m == market)
            .map(|(_, p)| *p)
            .nth(0)
            .unwrap_or(idx);
        let side = if idx >= older { Side::Buy } else { Side::Sell };
        let tick = engine.market_config(market).map_or(10_000, |c| c.tick);
        let wiggle = idx / 250;
        let price = match side {
            Side::Buy => Self::round_tick(idx + wiggle, tick),
            Side::Sell => Self::round_tick(idx.saturating_sub(wiggle), tick),
        };
        let lots = self.rng.range(5, 80);
        self.place(
            account,
            market,
            side,
            OrderKind::Limit {
                price: Price(price),
                tif: Tif::Ioc,
            },
            lots,
            false,
            StpMode::CancelNewest,
        )
    }

    fn whale(&mut self, engine: &Engine, account: AccountId, market: MarketId) -> EventBody {
        if !self.rng.chance(1, 3) {
            return self.noise(engine, account, market);
        }
        let side = self.random_side();
        let idx = self.index_of(engine, market);
        // size so that notional is roughly 15x the account's collateral
        let collateral = engine.collateral(account).map_or(0, |c| c.0.max(0));
        let target = u128::try_from(collateral).unwrap_or(0) * 18;
        let qty_raw = target * 10_000 / u128::from(idx.max(1));
        let lots = u64::try_from(qty_raw / 10).unwrap_or(0).clamp(1, 20_000);
        let tick = engine.market_config(market).map_or(10_000, |c| c.tick);
        let wiggle = idx / 100;
        let price = match side {
            Side::Buy => Self::round_tick(idx + wiggle, tick),
            Side::Sell => Self::round_tick(idx.saturating_sub(wiggle), tick),
        };
        self.place(
            account,
            market,
            side,
            OrderKind::Limit {
                price: Price(price),
                tif: Tif::Ioc,
            },
            lots,
            false,
            StpMode::CancelNewest,
        )
    }

    fn churn(&mut self, engine: &Engine, account: AccountId, market: MarketId) -> EventBody {
        let pos = engine.position(account, market).map_or(0, |p| p.size);
        match self.rng.below(8) {
            0 if pos != 0 => {
                let side = if pos > 0 { Side::Sell } else { Side::Buy };
                let lots = (pos.unsigned_abs() / 10).max(1).min(self.rng.range(1, 200));
                self.place(
                    account,
                    market,
                    side,
                    OrderKind::Market {
                        max_slippage_bps: 100,
                    },
                    lots,
                    true,
                    StpMode::CancelNewest,
                )
            }
            1 => EventBody::CancelAll {
                account,
                market: if self.rng.chance(1, 2) {
                    Some(market)
                } else {
                    None
                },
            },
            2 => EventBody::Deposit {
                account,
                amount: Money(i128::from(self.rng.range(100, 20_000)) * 1_000_000),
            },
            3 => EventBody::Withdraw {
                account,
                amount: Money(i128::from(self.rng.range(10, 5_000)) * 1_000_000),
            },
            _ => self.maker(engine, account, market),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn run(seed: u64, n: usize) -> (Engine, Vec<Event>) {
        let mut g = TrafficGenerator::new(TrafficConfig {
            seed,
            accounts: 40,
            ..TrafficConfig::default()
        });
        let mut e = Engine::new();
        let mut log = Vec::new();
        for ev in g.bootstrap() {
            e.apply(&ev).unwrap();
            log.push(ev);
        }
        for _ in 0..n {
            let ev = g.next_event(&e);
            let _ = e.apply(&ev);
            log.push(ev);
        }
        (e, log)
    }

    #[test]
    fn same_seed_gives_identical_log_and_state() {
        let (a, la) = run(5, 3_000);
        let (b, lb) = run(5, 3_000);
        assert_eq!(la, lb);
        assert_eq!(a.state_hash(), b.state_hash());
    }

    #[test]
    fn different_seeds_give_different_flow() {
        let (_, la) = run(1, 500);
        let (_, lb) = run(2, 500);
        assert_ne!(la, lb);
    }

    #[test]
    fn bootstrap_configures_and_funds_everything() {
        let mut g = TrafficGenerator::new(TrafficConfig {
            accounts: 10,
            markets: 2,
            ..TrafficConfig::default()
        });
        let mut e = Engine::new();
        for ev in g.bootstrap() {
            e.apply(&ev).unwrap();
        }
        assert_eq!(e.market_ids(), vec![1, 2]);
        assert_eq!(e.account_ids().len(), 10);
        assert!(e.mark_price(1).is_some() && e.mark_price(2).is_some());
        e.check_invariants().unwrap();
    }

    #[test]
    fn timestamps_never_go_backwards() {
        let (_, log) = run(9, 2_000);
        assert!(log.windows(2).all(|w| w[0].ts <= w[1].ts));
    }

    #[test]
    fn flow_trades_and_keeps_invariants() {
        let (e, _) = run(3, 20_000);
        e.check_invariants().unwrap();
        assert!(
            e.summary().fills > 100,
            "the simulated flow should produce trades"
        );
        assert!(!e.is_halted());
    }
}
