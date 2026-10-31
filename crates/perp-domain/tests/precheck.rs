//! Domain tests: `Engine::precheck` (cheap pre-log rejection) against
//! `Engine::apply`, and the wire format / validation of
//! `MarketConfig::price_band_bps`.
//!
//! Contract under test: `precheck(e) == Err(x)` implies `apply(e) == Err(x)`
//! without any state change; `precheck(e) == Ok(())` promises nothing.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod common;
use common::*;

use perp_domain::codec::{from_bytes, to_bytes};
use perp_domain::engine::Engine;
use perp_domain::error::Error;
use perp_domain::fixed::{Money, Price, Qty, MAX_AMOUNT};
use perp_domain::rng::Rng;
use perp_domain::traffic::{TrafficConfig, TrafficGenerator};
use perp_domain::types::{
    EngineConfig, Event, EventBody, MarketConfig, OrderKind, Output, PlaceOrder, Side, StpMode, Tif,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn event(ts: u64, body: EventBody) -> Event {
    Event { ts, body }
}

/// `precheck` rejects with `expected`, and `apply` agrees without any change.
fn assert_rejected_early(engine: &Engine, ts: u64, body: EventBody, expected: Error) {
    let ev = event(ts, body);
    let hash = engine.content_hash();
    assert_eq!(engine.precheck(&ev), Err(expected), "precheck of {ev:?}");
    assert_eq!(engine.content_hash(), hash, "precheck is pure");
    let mut clone = engine.clone();
    assert_eq!(clone.apply(&ev), Err(expected), "apply of {ev:?}");
    assert_eq!(clone.content_hash(), hash, "rejected apply changes nothing");
    assert_eq!(
        clone.is_halted(),
        engine.is_halted(),
        "a rejection is not a halt"
    );
}

/// `precheck` passes but `apply` rejects with `expected` (state-dependent rule).
fn assert_late_rejection(engine: &Engine, ts: u64, body: EventBody, expected: Error) {
    let ev = event(ts, body);
    assert_eq!(engine.precheck(&ev), Ok(()), "precheck of {ev:?}");
    let hash = engine.content_hash();
    let mut clone = engine.clone();
    assert_eq!(clone.apply(&ev), Err(expected), "apply of {ev:?}");
    assert_eq!(clone.content_hash(), hash, "rejected apply changes nothing");
    assert!(!clone.is_halted());
}

/// `precheck` passes and `apply` accepts.
fn assert_accepted(engine: &Engine, ts: u64, body: EventBody) -> Vec<Output> {
    let ev = event(ts, body);
    assert_eq!(engine.precheck(&ev), Ok(()), "precheck of {ev:?}");
    let mut clone = engine.clone();
    clone.apply(&ev).unwrap_or_else(|e| panic!("{ev:?}: {e:?}"))
}

fn limit_body(
    ex: &mut Ex,
    account: u64,
    side: Side,
    price: Price,
    milli: u64,
    tif: Tif,
) -> EventBody {
    EventBody::Place(ex.order(account, side, OrderKind::Limit { price, tif }, milli))
}

fn market_body(ex: &mut Ex, account: u64, side: Side, milli: u64) -> EventBody {
    EventBody::Place(ex.order(
        account,
        side,
        OrderKind::Market {
            max_slippage_bps: 100,
        },
        milli,
    ))
}

/// One representative event of every `EventBody` variant (all fields valid
/// except what the caller tweaks), used to check rules that apply to all.
fn every_variant(ex: &mut Ex) -> Vec<EventBody> {
    vec![
        EventBody::Configure(EngineConfig::default()),
        EventBody::AddMarket(MarketConfig::example(9)),
        EventBody::SetFeeTier {
            account: 1,
            tier: 1,
        },
        EventBody::Deposit {
            account: 1,
            amount: usd(1),
        },
        EventBody::Withdraw {
            account: 1,
            amount: usd(1),
        },
        EventBody::FundInsurance { amount: usd(1) },
        limit_body(ex, 1, Side::Buy, px(49_000), 1_000, Tif::Gtc),
        EventBody::Cancel {
            account: 1,
            order_id: 1,
        },
        EventBody::CancelAll {
            account: 1,
            market: None,
        },
        EventBody::Replace {
            account: 1,
            order_id: 1,
            price: None,
            qty: None,
        },
        EventBody::Oracle {
            market: M,
            index_price: px(50_000),
            source_ts: 1,
        },
        EventBody::Tick,
    ]
}

// ---------------------------------------------------------------------------
// Global gates: halted, time, configuration
// ---------------------------------------------------------------------------

#[test]
fn halted_engine_rejects_every_variant_at_precheck() {
    let mut ex = Ex::new();
    ex.fund(&[1], 10_000);
    let bodies = every_variant(&mut ex);
    ex.engine.emergency_halt();
    let ts = ex.ts + 1;
    for body in bodies {
        assert_rejected_early(&ex.engine, ts, body, Error::Halted);
    }
    // halting wins over a time regression
    assert_rejected_early(&ex.engine, 0, EventBody::Tick, Error::Halted);
}

#[test]
fn time_regression_is_rejected_at_precheck_for_every_variant() {
    let mut ex = Ex::new();
    ex.fund(&[1], 10_000);
    let clock = ex.engine.clock();
    assert!(clock > 0);
    for body in every_variant(&mut ex) {
        assert_rejected_early(&ex.engine, clock - 1, body, Error::TimeRegression);
    }
}

#[test]
fn equal_timestamp_is_not_a_time_regression() {
    let ex = Ex::new();
    let clock = ex.engine.clock();
    assert_eq!(ex.engine.precheck(&event(clock, EventBody::Tick)), Ok(()));
    assert_accepted(&ex.engine, clock, EventBody::Tick);
}

#[test]
fn unconfigured_engine_rejects_everything_but_configure() {
    let mut ex = Ex::new();
    let bodies = every_variant(&mut ex);
    let fresh = Engine::new();
    for body in bodies {
        if matches!(body, EventBody::Configure(_)) {
            assert_eq!(fresh.precheck(&event(5, body)), Ok(()));
        } else {
            assert_rejected_early(&fresh, 5, body, Error::NotConfigured);
        }
    }
}

#[test]
fn configure_twice_is_rejected_at_precheck() {
    let ex = Ex::bare();
    assert_rejected_early(
        &ex.engine,
        ex.ts + 1,
        EventBody::Configure(EngineConfig::default()),
        Error::AlreadyConfigured,
    );
    // even a different, invalid configuration reports AlreadyConfigured first
    let bad = EngineConfig {
        max_accounts: 0,
        ..EngineConfig::default()
    };
    assert_rejected_early(
        &ex.engine,
        ex.ts + 1,
        EventBody::Configure(bad),
        Error::AlreadyConfigured,
    );
}

#[test]
fn configure_validation_is_left_to_apply() {
    let fresh = Engine::new();
    let bad = EngineConfig {
        max_accounts: 0,
        ..EngineConfig::default()
    };
    assert_late_rejection(
        &fresh,
        1,
        EventBody::Configure(bad),
        Error::InvalidConfig("max_accounts"),
    );
    assert_eq!(
        assert_accepted(&fresh, 1, EventBody::Configure(EngineConfig::default())),
        vec![Output::Configured]
    );
}

// ---------------------------------------------------------------------------
// Per-variant early rejections
// ---------------------------------------------------------------------------

#[test]
fn add_market_rejections_at_precheck() {
    let mut ex = Ex::new();
    let ts = ex.ts + 1;
    // duplicate id
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::AddMarket(MarketConfig::example(M)),
        Error::MarketExists,
    );
    // invalid config, including the new price band rule
    let mut cfg = MarketConfig::example(2);
    cfg.tick = 0;
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::AddMarket(cfg),
        Error::InvalidConfig("tick"),
    );
    let mut cfg = MarketConfig::example(2);
    cfg.price_band_bps = 0;
    let want = cfg.validate().unwrap_err();
    assert!(matches!(want, Error::InvalidConfig(_)));
    assert_rejected_early(&ex.engine, ts, EventBody::AddMarket(cfg), want);
    // valid new market passes both
    assert_accepted(
        &ex.engine,
        ts,
        EventBody::AddMarket(MarketConfig::example(2)),
    );

    // table full
    for id in 2..=64 {
        ex.apply(EventBody::AddMarket(MarketConfig::example(id)))
            .unwrap();
    }
    let ts = ex.ts + 1;
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::AddMarket(MarketConfig::example(65)),
        Error::TooManyMarkets,
    );
}

#[test]
fn set_fee_tier_rejections_at_precheck() {
    let mut ex = Ex::new();
    ex.deposit(1, 10);
    let ts = ex.ts + 1;
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::SetFeeTier {
            account: 1,
            tier: 2,
        },
        Error::UnknownFeeTier,
    );
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::SetFeeTier {
            account: 7,
            tier: 1,
        },
        Error::UnknownAccount,
    );
    let outs = assert_accepted(
        &ex.engine,
        ts,
        EventBody::SetFeeTier {
            account: 1,
            tier: 1,
        },
    );
    assert_eq!(
        outs,
        vec![Output::FeeTierSet {
            account: 1,
            tier: 1
        }]
    );
}

#[test]
fn deposit_and_fund_insurance_amount_rules_at_precheck() {
    let mut ex = Ex::new();
    ex.deposit(1, 10);
    let ts = ex.ts + 1;
    for amount in [Money(0), Money(-1), Money(i128::MIN)] {
        assert_rejected_early(
            &ex.engine,
            ts,
            EventBody::Deposit { account: 1, amount },
            Error::ZeroAmount,
        );
        assert_rejected_early(
            &ex.engine,
            ts,
            EventBody::FundInsurance { amount },
            Error::ZeroAmount,
        );
    }
    for amount in [Money(MAX_AMOUNT + 1), Money(i128::MAX)] {
        assert_rejected_early(
            &ex.engine,
            ts,
            EventBody::Deposit { account: 1, amount },
            Error::AmountTooLarge,
        );
        assert_rejected_early(
            &ex.engine,
            ts,
            EventBody::FundInsurance { amount },
            Error::AmountTooLarge,
        );
    }
    assert_accepted(
        &ex.engine,
        ts,
        EventBody::FundInsurance {
            amount: Money(MAX_AMOUNT),
        },
    );
}

#[test]
fn deposit_to_an_unknown_account_passes_and_creates_it() {
    let ex = Ex::new();
    let outs = assert_accepted(
        &ex.engine,
        ex.ts + 1,
        EventBody::Deposit {
            account: 123_456,
            amount: usd(5),
        },
    );
    assert_eq!(
        outs,
        vec![Output::Deposited {
            account: 123_456,
            amount: usd(5),
            balance: usd(5)
        }]
    );
}

#[test]
fn deposit_over_the_account_cap_is_only_caught_by_apply() {
    let mut e = Engine::new();
    e.apply(&event(
        1,
        EventBody::Configure(EngineConfig {
            max_accounts: 1,
            ..EngineConfig::default()
        }),
    ))
    .unwrap();
    e.apply(&event(
        2,
        EventBody::Deposit {
            account: 1,
            amount: usd(1),
        },
    ))
    .unwrap();
    assert_late_rejection(
        &e,
        3,
        EventBody::Deposit {
            account: 2,
            amount: usd(1),
        },
        Error::TooManyAccounts,
    );
}

#[test]
fn withdraw_rejections_at_precheck() {
    let mut ex = Ex::new();
    ex.deposit(1, 100);
    let ts = ex.ts + 1;
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::Withdraw {
            account: 1,
            amount: Money(0),
        },
        Error::ZeroAmount,
    );
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::Withdraw {
            account: 9,
            amount: usd(1),
        },
        Error::UnknownAccount,
    );
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::Withdraw {
            account: 1,
            amount: Money(usd(100).0 + 1),
        },
        Error::InsufficientCollateral,
    );
    let outs = assert_accepted(
        &ex.engine,
        ts,
        EventBody::Withdraw {
            account: 1,
            amount: usd(100),
        },
    );
    assert_eq!(
        outs,
        vec![Output::Withdrawn {
            account: 1,
            amount: usd(100),
            balance: usd(0)
        }]
    );
}

#[test]
fn withdraw_that_breaches_margin_or_meets_a_stale_oracle_is_caught_by_apply() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 10_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    let collateral = ex.engine.collateral(1).unwrap();
    let ts = ex.ts + 1;
    assert_late_rejection(
        &ex.engine,
        ts,
        EventBody::Withdraw {
            account: 1,
            amount: collateral,
        },
        Error::MarginBreach,
    );
    // a withdrawal within the free margin is fine
    assert_accepted(
        &ex.engine,
        ts,
        EventBody::Withdraw {
            account: 1,
            amount: usd(100),
        },
    );
    // a stale oracle is not a lookup either
    assert_late_rejection(
        &ex.engine,
        ts + 100_000,
        EventBody::Withdraw {
            account: 1,
            amount: usd(1),
        },
        Error::StaleOracle,
    );
}

#[test]
fn place_rejections_at_precheck() {
    let mut ex = Ex::new();
    ex.fund(&[1], 100_000);
    let ts = ex.ts + 1;
    let base = |ex: &mut Ex| {
        ex.order(
            1,
            Side::Buy,
            OrderKind::Limit {
                price: px(49_000),
                tif: Tif::Gtc,
            },
            1_000,
        )
    };
    let mut p = base(&mut ex);
    p.market = 9;
    assert_rejected_early(&ex.engine, ts, EventBody::Place(p), Error::UnknownMarket);
    let mut p = base(&mut ex);
    p.account = 99;
    assert_rejected_early(&ex.engine, ts, EventBody::Place(p), Error::UnknownAccount);
    let mut p = base(&mut ex);
    p.qty = Qty(0);
    assert_rejected_early(&ex.engine, ts, EventBody::Place(p), Error::ZeroQty);
    let mut p = base(&mut ex);
    p.qty = Qty(15); // lot is 10 raw
    assert_rejected_early(&ex.engine, ts, EventBody::Place(p), Error::InvalidQty);
    let mut p = base(&mut ex);
    p.qty = Qty(1_000_010); // above max_order_qty (1 000 000 raw)
    assert_rejected_early(&ex.engine, ts, EventBody::Place(p), Error::InvalidQty);
    let mut p = base(&mut ex);
    p.qty = Qty(u64::MAX);
    assert_rejected_early(&ex.engine, ts, EventBody::Place(p), Error::InvalidQty);
    // an unknown market wins over a bad quantity (lookups first)
    let mut p = base(&mut ex);
    p.market = 9;
    p.qty = Qty(0);
    assert_rejected_early(&ex.engine, ts, EventBody::Place(p), Error::UnknownMarket);
    // the same order with valid fields is accepted
    let p = base(&mut ex);
    let outs = assert_accepted(&ex.engine, ts, EventBody::Place(p));
    assert!(has(&outs, |o| matches!(o, Output::Rested { .. })));
}

#[test]
fn place_state_dependent_rejections_pass_precheck() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.deposit(3, 100); // too poor for a 1.0 position
    ex.gtc(2, Side::Sell, 50_100, 1_000);
    let ts = ex.ts + 1;

    // tick size
    let b = limit_body(
        &mut ex,
        1,
        Side::Buy,
        Price(49_000_000_001),
        1_000,
        Tif::Gtc,
    );
    assert_late_rejection(&ex.engine, ts, b, Error::InvalidPrice);
    let b = limit_body(&mut ex, 1, Side::Buy, Price(0), 1_000, Tif::Gtc);
    assert_late_rejection(&ex.engine, ts, b, Error::InvalidPrice);
    // price band: buy above floor(50 000 * 1.025) = 51 250
    let b = limit_body(&mut ex, 1, Side::Buy, px(51_251), 1_000, Tif::Gtc);
    assert_late_rejection(&ex.engine, ts, b, Error::PriceOutsideBand);
    let b = limit_body(&mut ex, 1, Side::Sell, px(48_749), 1_000, Tif::Gtc);
    assert_late_rejection(&ex.engine, ts, b, Error::PriceOutsideBand);
    // minimum notional (0.001 base at 1.00 quote)
    let b = limit_body(&mut ex, 1, Side::Buy, px(1), 1, Tif::Gtc);
    assert_late_rejection(&ex.engine, ts, b, Error::BelowMinNotional);
    // initial margin
    let b = limit_body(&mut ex, 3, Side::Buy, px(50_000), 1_000, Tif::Gtc);
    assert_late_rejection(&ex.engine, ts, b, Error::MarginBreach);
    // post-only that would cross the resting ask
    let b = limit_body(&mut ex, 1, Side::Buy, px(50_100), 1_000, Tif::PostOnly);
    assert_late_rejection(&ex.engine, ts, b, Error::WouldCross);
    // fill-or-kill larger than the liquidity
    let b = limit_body(&mut ex, 1, Side::Buy, px(50_100), 2_000, Tif::Fok);
    assert_late_rejection(&ex.engine, ts, b, Error::FokNotFillable);
    // market order with nothing on the other side
    let b = market_body(&mut ex, 1, Side::Sell, 1_000);
    assert_late_rejection(&ex.engine, ts, b, Error::NoLiquidity);
    // stale oracle (the only update was long before ts)
    let b = limit_body(&mut ex, 1, Side::Buy, px(49_000), 1_000, Tif::Gtc);
    assert_late_rejection(&ex.engine, ts + 60_001, b, Error::StaleOracle);
    // the accepted counterparts
    let b = limit_body(&mut ex, 1, Side::Buy, px(51_250), 1_000, Tif::Gtc);
    assert_accepted(&ex.engine, ts, b);
    let b = limit_body(&mut ex, 1, Side::Buy, px(50_100), 1_000, Tif::Fok);
    assert_accepted(&ex.engine, ts, b);
}

#[test]
fn order_without_a_mark_price_passes_precheck_and_fails_in_apply() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    ex.deposit(1, 1_000);
    let ts = ex.ts + 1;
    let b = limit_body(&mut ex, 1, Side::Buy, px(50_000), 1, Tif::Gtc);
    assert_late_rejection(&ex.engine, ts, b, Error::NoMarkPrice);
}

#[test]
fn cancel_cancel_all_and_replace_rejections_at_precheck() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    let mine = ex.gtc(1, Side::Buy, 49_000, 1_000);
    let theirs = ex.gtc(2, Side::Buy, 49_000, 1_000);
    let ts = ex.ts + 1;
    // unknown order, someone else's order, unknown account
    for order_id in [999, theirs] {
        assert_rejected_early(
            &ex.engine,
            ts,
            EventBody::Cancel {
                account: 1,
                order_id,
            },
            Error::UnknownOrder,
        );
        assert_rejected_early(
            &ex.engine,
            ts,
            EventBody::Replace {
                account: 1,
                order_id,
                price: None,
                qty: None,
            },
            Error::UnknownOrder,
        );
    }
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::Cancel {
            account: 9,
            order_id: mine,
        },
        Error::UnknownAccount,
    );
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::Replace {
            account: 9,
            order_id: mine,
            price: None,
            qty: None,
        },
        Error::UnknownAccount,
    );
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::CancelAll {
            account: 9,
            market: None,
        },
        Error::UnknownAccount,
    );
    // accepted counterparts
    let outs = assert_accepted(
        &ex.engine,
        ts,
        EventBody::Cancel {
            account: 1,
            order_id: mine,
        },
    );
    assert!(has(
        &outs,
        |o| matches!(o, Output::Canceled { order_id, .. } if *order_id == mine)
    ));
    assert_accepted(
        &ex.engine,
        ts,
        EventBody::CancelAll {
            account: 1,
            market: Some(M),
        },
    );
    assert_accepted(
        &ex.engine,
        ts,
        EventBody::Replace {
            account: 1,
            order_id: mine,
            price: Some(px(48_000)),
            qty: None,
        },
    );
}

#[test]
fn replace_outside_the_band_passes_precheck_and_fails_in_apply() {
    let mut ex = Ex::new();
    ex.fund(&[1], 100_000);
    let id = ex.gtc(1, Side::Buy, 49_000, 1_000);
    let ts = ex.ts + 1;
    assert_late_rejection(
        &ex.engine,
        ts,
        EventBody::Replace {
            account: 1,
            order_id: id,
            price: Some(px(52_000)),
            qty: None,
        },
        Error::PriceOutsideBand,
    );
    assert_late_rejection(
        &ex.engine,
        ts,
        EventBody::Replace {
            account: 1,
            order_id: id,
            price: Some(Price(49_000_000_001)),
            qty: None,
        },
        Error::InvalidPrice,
    );
}

#[test]
fn oracle_rejections_at_precheck_and_in_apply() {
    let ex = Ex::new();
    let ts = ex.ts + 100_000;
    assert_rejected_early(
        &ex.engine,
        ts,
        EventBody::Oracle {
            market: 9,
            index_price: px(50_000),
            source_ts: ts,
        },
        Error::UnknownMarket,
    );
    // everything about the update itself is judged by apply
    let last = ex.ts; // source_ts of the bootstrap update
    for (at, price, source_ts, want) in [
        (ts, Price(0), ts, Error::OracleBadPrice),
        (ts, px(50_000), ts + 1, Error::OracleRegression), // observed in the future
        (ts, px(50_000), ts - 60_001, Error::StaleOracle),
        (last + 10, px(50_000), last, Error::OracleRegression), // not newer than the last
    ] {
        assert_late_rejection(
            &ex.engine,
            at,
            EventBody::Oracle {
                market: M,
                index_price: price,
                source_ts,
            },
            want,
        );
    }
    let outs = assert_accepted(
        &ex.engine,
        ts,
        EventBody::Oracle {
            market: M,
            index_price: px(50_100),
            source_ts: ts,
        },
    );
    assert!(has(&outs, |o| matches!(o, Output::MarkUpdated { .. })));
    ex.check();
}

#[test]
fn tick_always_passes_precheck_on_a_live_engine() {
    let ex = Ex::new();
    let outs = assert_accepted(&ex.engine, ex.ts + 1, EventBody::Tick);
    assert!(outs.is_empty());
}

#[test]
fn precheck_does_not_touch_state_or_counters() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 10_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    let full = ex.engine.state_hash();
    let applied = ex.engine.events_applied();
    for body in every_variant(&mut ex) {
        let _ = ex.engine.precheck(&event(ex.ts + 1, body));
    }
    assert_eq!(ex.engine.state_hash(), full, "not even the counter moves");
    assert_eq!(ex.engine.events_applied(), applied);
}

// ---------------------------------------------------------------------------
// Seeded property: precheck Err(e) => apply Err(e) and no change
// ---------------------------------------------------------------------------

/// Hand-made hostile events with extreme values.
fn garbage(rng: &mut Rng, ts: u64, accounts: u64) -> Event {
    let account = match rng.below(4) {
        0 => 0,
        1 => u64::MAX,
        2 => accounts + 1 + rng.below(10),
        _ => 1 + rng.below(accounts),
    };
    let market = match rng.below(4) {
        0 => 0,
        1 => u32::MAX,
        2 => 2,
        _ => 1,
    };
    let money = [
        Money(0),
        Money(-1),
        Money(1),
        Money(i128::MIN),
        Money(i128::MAX),
        Money(MAX_AMOUNT),
        Money(MAX_AMOUNT + 1),
        usd(1_000),
    ];
    let amount = *rng.pick(&money).unwrap();
    let prices = [
        0,
        1,
        10_000,
        49_999_999_999,
        50_000_000_000,
        u64::MAX,
        10_000_000_000_000,
        10_000_000_010_000,
    ];
    let price = Price(*rng.pick(&prices).unwrap());
    let qtys = [0, 1, 10, 15, 1_000, 1_000_000, 1_000_010, u64::MAX];
    let q = Qty(*rng.pick(&qtys).unwrap());
    let side = if rng.chance(1, 2) {
        Side::Buy
    } else {
        Side::Sell
    };
    let tifs = [Tif::Gtc, Tif::Ioc, Tif::Fok, Tif::PostOnly];
    let tif = *rng.pick(&tifs).unwrap();
    let order_id = match rng.below(3) {
        0 => 0,
        1 => u64::MAX,
        _ => 1 + rng.below(50_000),
    };
    let body = match rng.below(14) {
        0 => EventBody::Configure(EngineConfig::default()),
        1 => {
            let mut cfg = MarketConfig::example(market);
            match rng.below(5) {
                0 => cfg.tick = 0,
                1 => cfg.price_band_bps = u32::MAX,
                2 => cfg.tiers.clear(),
                3 => cfg.funding_interval_ms = 0,
                _ => {}
            }
            EventBody::AddMarket(cfg)
        }
        2 => EventBody::SetFeeTier {
            account,
            tier: *rng.pick(&[0u8, 1, 2, 255]).unwrap(),
        },
        3 => EventBody::Deposit { account, amount },
        4 => EventBody::Withdraw { account, amount },
        5 => EventBody::FundInsurance { amount },
        6 | 7 => EventBody::Place(PlaceOrder {
            market,
            account,
            side,
            kind: if rng.chance(1, 4) {
                OrderKind::Market {
                    max_slippage_bps: *rng.pick(&[0u32, 100, 5_000, 5_001, u32::MAX]).unwrap(),
                }
            } else {
                OrderKind::Limit { price, tif }
            },
            qty: q,
            client_id: rng.next_u64(),
            reduce_only: rng.chance(1, 4),
            stp: StpMode::CancelNewest,
        }),
        8 => EventBody::Cancel { account, order_id },
        9 => EventBody::CancelAll {
            account,
            market: if rng.chance(1, 2) { Some(market) } else { None },
        },
        10 => EventBody::Replace {
            account,
            order_id,
            price: if rng.chance(1, 2) { Some(price) } else { None },
            qty: if rng.chance(1, 2) { Some(q) } else { None },
        },
        11 | 12 => EventBody::Oracle {
            market,
            index_price: price,
            source_ts: *rng.pick(&[0, 1, ts, ts + 1, u64::MAX]).unwrap(),
        },
        _ => EventBody::Tick,
    };
    // mostly the generator's own time; sometimes the distant past
    let ts = if rng.chance(1, 20) { 0 } else { ts };
    Event { ts, body }
}

#[test]
fn property_precheck_rejection_implies_identical_apply_rejection() {
    let mut gen = TrafficGenerator::new(TrafficConfig {
        seed: 20_240_607,
        accounts: 40,
        markets: 2,
        oracle_every: 20,
        tick_every: 100,
    });
    let mut rng = Rng::new(0xC0FF_EE00);
    let mut engine = Engine::new();
    let mut early = 0u64; // precheck rejected
    let mut late = 0u64; // precheck passed, apply rejected
    let mut accepted = 0u64;
    let mut seen_codes = std::collections::BTreeSet::new();

    let bootstrap = gen.bootstrap();
    let total = 100_000usize;
    let mut step = 0usize;
    let mut pending = bootstrap.into_iter();
    while step < total {
        let ev = match pending.next() {
            Some(e) => e,
            None => {
                let e = gen.next_event(&engine);
                if rng.chance(1, 3) {
                    garbage(&mut rng, e.ts, 40)
                } else {
                    e
                }
            }
        };
        step += 1;
        match engine.precheck(&ev) {
            Err(expected) => {
                early += 1;
                seen_codes.insert(expected.code());
                // hashing the whole state is expensive: check it on every 20th
                let hashed = early.is_multiple_of(20);
                let hash = if hashed {
                    Some(engine.content_hash())
                } else {
                    None
                };
                assert_eq!(engine.apply(&ev), Err(expected), "step {step}: {ev:?}");
                if let Some(h) = hash {
                    assert_eq!(engine.content_hash(), h, "step {step}: {ev:?}");
                }
            }
            Ok(()) => match engine.apply(&ev) {
                Ok(_) => accepted += 1,
                Err(_) => late += 1,
            },
        }
        assert!(!engine.is_halted(), "step {step}: engine halted on {ev:?}");
    }
    engine.check_invariants().unwrap();
    // the property is not vacuous
    assert!(early > 5_000, "early rejections: {early}");
    assert!(late > 1_000, "late rejections: {late}");
    assert!(accepted > 20_000, "accepted: {accepted}");
    for code in [
        "unknown_account",
        "unknown_order",
        "zero_amount",
        "amount_too_large",
        "invalid_qty",
        "zero_qty",
        "time_regression",
        "unknown_market",
        "insufficient_collateral",
        "already_configured",
        "market_exists",
        "invalid_config",
        "unknown_fee_tier",
    ] {
        assert!(seen_codes.contains(code), "never exercised: {code}");
    }
}

// ---------------------------------------------------------------------------
// price_band_bps: wire format and validation
// ---------------------------------------------------------------------------

fn add_market_bytes(cfg: &MarketConfig) -> Vec<u8> {
    to_bytes(&EventBody::AddMarket(cfg.clone()))
}

#[test]
fn price_band_bps_round_trips_through_add_market_encoding() {
    for band in [1, 100, 249, 250, 251, 1_000, u32::MAX] {
        let mut cfg = MarketConfig::example(3);
        cfg.price_band_bps = band;
        let body = EventBody::AddMarket(cfg.clone());
        let bytes = to_bytes(&body);
        let back: EventBody = from_bytes(&bytes).unwrap();
        assert_eq!(back, body, "band {band}");
        let EventBody::AddMarket(c) = back else {
            panic!("variant");
        };
        assert_eq!(c.price_band_bps, band);
        // and as a whole log record
        let ev = Event {
            ts: 77,
            body: EventBody::AddMarket(cfg),
        };
        assert_eq!(from_bytes::<Event>(&to_bytes(&ev)).unwrap(), ev);
    }
}

#[test]
fn price_band_bps_sits_right_after_max_mark_deviation_bps_on_the_wire() {
    let mut a = MarketConfig::example(3);
    let mut b = a.clone();
    a.price_band_bps = 0x0102_0304;
    b.price_band_bps = 0x1112_1314;
    let (ba, bb) = (add_market_bytes(&a), add_market_bytes(&b));
    assert_eq!(ba.len(), bb.len());
    let diff: Vec<usize> = (0..ba.len()).filter(|i| ba[*i] != bb[*i]).collect();
    assert_eq!(diff.len(), 4, "a u32 field, little endian: {diff:?}");
    let at = diff[0];
    assert_eq!(&ba[at..at + 4], &0x0102_0304u32.to_le_bytes());
    // the tail is funding_interval_ms (u64), funding_cap_ppb (i64), funding_interest_ppb (i64)
    assert_eq!(ba.len() - (at + 4), 24);
    // the field before it is max_mark_deviation_bps
    assert_eq!(
        &ba[at - 4..at],
        &a.max_mark_deviation_bps.to_le_bytes(),
        "max_mark_deviation_bps precedes the band"
    );
}

#[test]
fn truncated_or_padded_add_market_bytes_are_rejected() {
    let bytes = add_market_bytes(&MarketConfig::example(3));
    for n in 0..bytes.len() {
        assert!(
            from_bytes::<EventBody>(&bytes[..n]).is_err(),
            "prefix of {n} bytes decoded"
        );
    }
    let mut padded = bytes.clone();
    padded.push(0);
    assert!(from_bytes::<EventBody>(&padded).is_err(), "trailing byte");
    assert!(from_bytes::<EventBody>(&bytes).is_ok());
}

#[test]
fn price_band_must_be_between_one_and_the_first_tier_maintenance_rate() {
    // example: first tier mmr is 250 bps
    let try_band = |band: u32| {
        let mut cfg = MarketConfig::example(3);
        cfg.price_band_bps = band;
        cfg.validate()
    };
    assert_eq!(try_band(1), Ok(()));
    assert_eq!(try_band(250), Ok(()));
    let msg = "price_band_bps must be 1..=first tier mmr_bps";
    assert_eq!(try_band(0), Err(Error::InvalidConfig(msg)));
    assert_eq!(try_band(251), Err(Error::InvalidConfig(msg)));
    assert_eq!(try_band(u32::MAX), Err(Error::InvalidConfig(msg)));

    // the upper limit follows the first tier's maintenance rate
    let mut cfg = MarketConfig::example(3);
    cfg.tiers[0].mmr_bps = 100;
    cfg.price_band_bps = 100;
    assert_eq!(cfg.validate(), Ok(()));
    cfg.price_band_bps = 101;
    assert_eq!(cfg.validate(), Err(Error::InvalidConfig(msg)));
    cfg.tiers[0].mmr_bps = 400;
    assert_eq!(cfg.validate(), Ok(()));
}

#[test]
fn decoding_does_not_validate_but_add_market_rejects_a_bad_band() {
    let mut cfg = MarketConfig::example(3);
    cfg.price_band_bps = 0;
    let body: EventBody = from_bytes(&add_market_bytes(&cfg)).unwrap();
    let ex = Ex::bare();
    let msg = "price_band_bps must be 1..=first tier mmr_bps";
    assert_rejected_early(&ex.engine, ex.ts + 1, body, Error::InvalidConfig(msg));
}

#[test]
fn decoded_price_band_drives_the_order_band_to_the_exact_tick() {
    let mut cfg = MarketConfig::example(M);
    cfg.price_band_bps = 100; // 1%
    let EventBody::AddMarket(cfg) =
        from_bytes::<EventBody>(&to_bytes(&EventBody::AddMarket(cfg))).unwrap()
    else {
        panic!("variant");
    };
    assert_eq!(cfg.price_band_bps, 100);
    let mut ex = Ex::with_market(cfg);
    ex.fund(&[1], 1_000_000);
    let ts = ex.ts + 1;
    // mark 50 000: buys up to floor(50 000 * 1.01) = 50 500.00, sells down to 49 500.00
    let b = limit_body(&mut ex, 1, Side::Buy, px(50_500), 1_000, Tif::Gtc);
    assert_accepted(&ex.engine, ts, b);
    let b = limit_body(
        &mut ex,
        1,
        Side::Buy,
        Price(50_500_010_000),
        1_000,
        Tif::Gtc,
    );
    assert_late_rejection(&ex.engine, ts, b, Error::PriceOutsideBand);
    let b = limit_body(&mut ex, 1, Side::Sell, px(49_500), 1_000, Tif::Gtc);
    assert_accepted(&ex.engine, ts, b);
    let b = limit_body(
        &mut ex,
        1,
        Side::Sell,
        Price(49_499_990_000),
        1_000,
        Tif::Gtc,
    );
    assert_late_rejection(&ex.engine, ts, b, Error::PriceOutsideBand);
}
