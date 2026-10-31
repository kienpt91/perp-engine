//! Domain tests: hostile and degenerate input. Every rejection must name the
//! right error and leave the state (modulo the applied-event counter)
//! untouched; the engine must stay usable and never halt on bad input.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod common;
use common::*;
use perp_domain::engine::Engine;
use perp_domain::error::Error;
use perp_domain::fixed::{Money, Price, Qty, MAX_AMOUNT, MAX_BALANCE, MAX_PRICE, MAX_QTY};
use perp_domain::rng::Rng;
use perp_domain::types::{
    CancelReason, EngineConfig, Event, EventBody, MarketConfig, MarketId, OrderKind, Output,
    PlaceOrder, Side, StpMode, Tif,
};

fn limit_order(
    ex: &mut Ex,
    account: u64,
    side: Side,
    price: u64,
    qty_raw: u64,
    tif: Tif,
) -> PlaceOrder {
    let mut p = ex.order(
        account,
        side,
        OrderKind::Limit {
            price: Price(price),
            tif,
        },
        1,
    );
    p.qty = Qty(qty_raw);
    p
}

fn funded() -> Ex {
    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3], 1_000_000);
    ex
}

fn assert_rejected_without_change(ex: &mut Ex, body: EventBody, err: Error) {
    let before = ex.engine.content_hash();
    assert_eq!(ex.apply(body), Err(err));
    assert_eq!(
        ex.engine.content_hash(),
        before,
        "{err:?} must not change state"
    );
    assert!(!ex.engine.is_halted());
}

// ---------------------------------------------------------------------------
// Prices and quantities
// ---------------------------------------------------------------------------

#[test]
fn limit_prices_out_of_range_are_rejected() {
    let mut ex = funded();
    for price in [
        0u64,
        1,
        9_999,
        50_000_000_001, // off tick
        MAX_PRICE + 10_000,
        MAX_PRICE + 1,
        u64::MAX,
        u64::MAX - 1,
    ] {
        let p = limit_order(&mut ex, 1, Side::Buy, price, 10, Tif::Gtc);
        assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::InvalidPrice);
    }
    ex.check();
}

#[test]
fn maximum_price_is_a_valid_resting_price() {
    let mut ex = funded();
    let p = limit_order(&mut ex, 1, Side::Sell, MAX_PRICE, 10, Tif::Gtc);
    let outs = ex.place(p).unwrap();
    assert!(has(
        &outs,
        |o| matches!(o, Output::Rested { price, .. } if price.0 == MAX_PRICE)
    ));
    ex.check();
}

#[test]
fn quantities_out_of_range_are_rejected() {
    let mut ex = funded();
    for q in [
        u64::MAX,
        MAX_QTY + 1,
        MAX_QTY,
        1_000_010, // max_order_qty + one lot
        15,        // off lot
        1,
    ] {
        let p = limit_order(&mut ex, 1, Side::Buy, px(40_000).0, q, Tif::Gtc);
        assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::InvalidQty);
    }
    ex.check();
}

#[test]
fn zero_quantity_and_zero_price_are_rejected() {
    let mut ex = funded();
    let p = limit_order(&mut ex, 1, Side::Buy, px(40_000).0, 0, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::ZeroQty);
    let p = limit_order(&mut ex, 1, Side::Buy, 0, 10, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::InvalidPrice);
}

#[test]
fn market_order_parameters_are_validated() {
    let mut ex = funded();
    let mut p = ex.order(
        1,
        Side::Buy,
        OrderKind::Market {
            max_slippage_bps: 5_001,
        },
        10,
    );
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::InvalidSlippage);
    p = ex.order(
        1,
        Side::Buy,
        OrderKind::Market {
            max_slippage_bps: u32::MAX,
        },
        10,
    );
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::InvalidSlippage);
    // valid slippage but an empty book
    p = ex.order(
        1,
        Side::Buy,
        OrderKind::Market {
            max_slippage_bps: 5_000,
        },
        10,
    );
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::NoLiquidity);
}

#[test]
fn orders_for_unknown_markets_and_accounts_are_rejected() {
    let mut ex = funded();
    let mut p = limit_order(&mut ex, 1, Side::Buy, px(40_000).0, 10, Tif::Gtc);
    p.market = u32::MAX;
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::UnknownMarket);
    let p = limit_order(&mut ex, u64::MAX, Side::Buy, px(40_000).0, 10, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::UnknownAccount);
}

#[test]
fn below_minimum_notional_is_rejected() {
    let mut ex = funded();
    // 0.001 base at 1 quote = 0.001 quote < 10
    let p = limit_order(&mut ex, 1, Side::Buy, px(1).0, 10, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::BelowMinNotional);
}

#[test]
fn client_order_id_extremes_and_duplicates() {
    let mut ex = funded();
    let mut p = limit_order(&mut ex, 1, Side::Buy, px(40_000).0, 10, Tif::Gtc);
    p.client_id = u64::MAX;
    ex.place(p).unwrap();
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::DuplicateClientOrderId);
    p.client_id = 0;
    ex.place(p).unwrap();
    // the same client id is fine for another account
    p.account = 2;
    p.client_id = u64::MAX;
    ex.place(p).unwrap();
    ex.check();
}

#[test]
fn cancel_of_a_foreign_order_is_rejected() {
    let mut ex = funded();
    let id = ex.gtc(1, Side::Buy, 40_000, 10);
    assert_rejected_without_change(
        &mut ex,
        EventBody::Cancel {
            account: 2,
            order_id: id,
        },
        Error::UnknownOrder,
    );
    assert_eq!(ex.engine.open_orders_of(1), vec![id]);
}

#[test]
fn replace_with_hostile_values_is_rejected() {
    let mut ex = funded();
    let id = ex.gtc(1, Side::Buy, 40_000, 10);
    let rep = |price: Option<Price>, qty: Option<Qty>| EventBody::Replace {
        account: 1,
        order_id: id,
        price,
        qty,
    };
    assert_rejected_without_change(
        &mut ex,
        rep(Some(Price(u64::MAX)), None),
        Error::InvalidPrice,
    );
    assert_rejected_without_change(&mut ex, rep(Some(Price(0)), None), Error::InvalidPrice);
    assert_rejected_without_change(
        &mut ex,
        rep(Some(Price(40_000_000_001)), None),
        Error::InvalidPrice,
    );
    assert_rejected_without_change(&mut ex, rep(None, Some(Qty(0))), Error::ZeroQty);
    assert_rejected_without_change(&mut ex, rep(None, Some(Qty(u64::MAX))), Error::InvalidQty);
    assert_rejected_without_change(&mut ex, rep(None, Some(Qty(15))), Error::InvalidQty);
    assert_rejected_without_change(
        &mut ex,
        EventBody::Replace {
            account: 1,
            order_id: u64::MAX,
            price: None,
            qty: None,
        },
        Error::UnknownOrder,
    );
    assert_rejected_without_change(
        &mut ex,
        EventBody::Replace {
            account: 999,
            order_id: id,
            price: None,
            qty: None,
        },
        Error::UnknownAccount,
    );
    ex.check();
}

// ---------------------------------------------------------------------------
// Money amounts
// ---------------------------------------------------------------------------

#[test]
fn zero_and_negative_deposits_are_rejected() {
    let mut ex = funded();
    for amount in [0i128, -1, -MAX_AMOUNT, i128::MIN] {
        assert_rejected_without_change(
            &mut ex,
            EventBody::Deposit {
                account: 1,
                amount: Money(amount),
            },
            Error::ZeroAmount,
        );
    }
}

#[test]
fn oversized_deposits_are_rejected() {
    let mut ex = funded();
    for amount in [MAX_AMOUNT + 1, i128::MAX, i128::MAX - 1] {
        assert_rejected_without_change(
            &mut ex,
            EventBody::Deposit {
                account: 1,
                amount: Money(amount),
            },
            Error::AmountTooLarge,
        );
    }
    // also for an account that does not exist yet: no account is created
    let before = ex.engine.account_ids();
    assert_eq!(
        ex.apply(EventBody::Deposit {
            account: 77,
            amount: Money(i128::MAX)
        }),
        Err(Error::AmountTooLarge)
    );
    assert_eq!(ex.engine.account_ids(), before);
}

#[test]
fn deposits_cannot_overflow_the_balance_cap() {
    let mut ex = Ex::new();
    let deposit = |ex: &mut Ex| {
        ex.apply(EventBody::Deposit {
            account: u64::MAX - 1,
            amount: Money(MAX_AMOUNT),
        })
    };
    for _ in 0..(MAX_BALANCE / MAX_AMOUNT) {
        deposit(&mut ex).unwrap();
    }
    assert_eq!(ex.engine.collateral(u64::MAX - 1), Some(Money(MAX_BALANCE)));
    let before = ex.engine.content_hash();
    assert_eq!(deposit(&mut ex), Err(Error::AmountTooLarge));
    assert_eq!(ex.engine.content_hash(), before);
    // a one-unit deposit overflows the cap too
    assert_eq!(
        ex.apply(EventBody::Deposit {
            account: u64::MAX - 1,
            amount: Money(1)
        }),
        Err(Error::AmountTooLarge)
    );
    // another account is unaffected by the first one's cap
    ex.deposit(5, 1);
    ex.check();
}

#[test]
fn deposit_to_the_largest_account_ids_works() {
    let mut ex = Ex::new();
    ex.deposit(u64::MAX - 1, 100_000);
    ex.deposit(u64::MAX, 100_000);
    ex.deposit(0, 30);
    assert_eq!(ex.engine.account_ids(), vec![0, u64::MAX - 1, u64::MAX]);
    ex.trade(u64::MAX, u64::MAX - 1, Side::Buy, 50_000, 100);
    assert_eq!(ex.size(u64::MAX), 1_000);
    assert_eq!(ex.size(u64::MAX - 1), -1_000);
    // account 0 with 30 quote cannot margin 0.1 base (5 000 notional)
    assert_eq!(
        ex.limit(0, Side::Buy, 50_000, 100, Tif::Ioc),
        Err(Error::MarginBreach)
    );
    ex.check();
}

#[test]
fn withdraw_extremes_are_rejected() {
    let mut ex = funded();
    for amount in [0i128, -1, i128::MIN] {
        assert_rejected_without_change(
            &mut ex,
            EventBody::Withdraw {
                account: 1,
                amount: Money(amount),
            },
            Error::ZeroAmount,
        );
    }
    for amount in [i128::MAX, usd(1_000_001).0, MAX_AMOUNT] {
        assert_rejected_without_change(
            &mut ex,
            EventBody::Withdraw {
                account: 1,
                amount: Money(amount),
            },
            Error::InsufficientCollateral,
        );
    }
    // unknown account with a valid amount; invalid amount wins over unknown account
    assert_rejected_without_change(
        &mut ex,
        EventBody::Withdraw {
            account: 99,
            amount: Money(1),
        },
        Error::UnknownAccount,
    );
    assert_rejected_without_change(
        &mut ex,
        EventBody::Withdraw {
            account: 99,
            amount: Money(i128::MIN),
        },
        Error::ZeroAmount,
    );
    // the exact balance can be withdrawn
    ex.apply(EventBody::Withdraw {
        account: 1,
        amount: usd(1_000_000),
    })
    .unwrap();
    assert_eq!(ex.collateral(1), 0);
    ex.check();
}

#[test]
fn insurance_funding_extremes_are_rejected() {
    let mut ex = funded();
    for amount in [0i128, -5, i128::MIN] {
        assert_rejected_without_change(
            &mut ex,
            EventBody::FundInsurance {
                amount: Money(amount),
            },
            Error::ZeroAmount,
        );
    }
    for amount in [MAX_AMOUNT + 1, i128::MAX] {
        assert_rejected_without_change(
            &mut ex,
            EventBody::FundInsurance {
                amount: Money(amount),
            },
            Error::AmountTooLarge,
        );
    }
    // the fund is capped like a balance
    for _ in 0..(MAX_BALANCE / MAX_AMOUNT) {
        ex.apply(EventBody::FundInsurance {
            amount: Money(MAX_AMOUNT),
        })
        .unwrap();
    }
    assert_rejected_without_change(
        &mut ex,
        EventBody::FundInsurance { amount: Money(1) },
        Error::AmountTooLarge,
    );
    ex.check();
}

// ---------------------------------------------------------------------------
// Oracle input
// ---------------------------------------------------------------------------

#[test]
fn hostile_oracle_prices_are_rejected_and_jumps_clamped() {
    let mut ex = funded();
    for price in [0u64, MAX_PRICE + 1, u64::MAX] {
        let ts = ex.ts + 1;
        assert_rejected_without_change(
            &mut ex,
            EventBody::Oracle {
                market: M,
                index_price: Price(price),
                source_ts: ts,
            },
            Error::OracleBadPrice,
        );
    }
    // a jump to the maximum is clamped to +5 %
    let ts = ex.ts + 1;
    let outs = ex
        .apply(EventBody::Oracle {
            market: M,
            index_price: Price(MAX_PRICE),
            source_ts: ts,
        })
        .unwrap();
    assert!(has(
        &outs,
        |o| matches!(o, Output::MarkUpdated { index, clamped: true, .. } if index.0 == 52_500_000_000)
    ));
    // ... and a collapse to one raw unit is clamped to -5 %
    let ts = ex.ts + 1;
    let outs = ex
        .apply(EventBody::Oracle {
            market: M,
            index_price: Price(1),
            source_ts: ts,
        })
        .unwrap();
    assert!(has(
        &outs,
        |o| matches!(o, Output::MarkUpdated { index, clamped: true, .. } if index.0 == 49_875_000_000)
    ));
    ex.check();
}

#[test]
fn oracle_source_time_extremes_are_rejected() {
    let mut ex = funded();
    ex.advance(100_000); // the helper's first oracle update is now old
    let ts = ex.ts;
    for (source_ts, err) in [
        (u64::MAX, Error::OracleRegression),
        (ts + 1_000, Error::OracleRegression),
        (0, Error::StaleOracle),
        (ts - 60_001, Error::StaleOracle),
    ] {
        assert_rejected_without_change(
            &mut ex,
            EventBody::Oracle {
                market: M,
                index_price: px(50_000),
                source_ts,
            },
            err,
        );
    }
    // a fresh update is accepted, replaying the same source time is not
    let ts = ex.ts + 1;
    ex.apply(EventBody::Oracle {
        market: M,
        index_price: px(50_000),
        source_ts: ts,
    })
    .unwrap();
    assert_rejected_without_change(
        &mut ex,
        EventBody::Oracle {
            market: M,
            index_price: px(50_000),
            source_ts: ts,
        },
        Error::OracleRegression,
    );
    // unknown market
    assert_eq!(
        ex.oracle_on_result(u32::MAX, 50_000),
        Err(Error::UnknownMarket)
    );
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

#[test]
fn time_regression_is_rejected_for_every_event_kind() {
    let mut ex = funded();
    ex.apply_at(1_000_000, EventBody::Tick).unwrap();
    let clock = ex.engine.clock();
    assert_eq!(clock, 1_000_000);
    let before = ex.engine.content_hash();
    let bodies = vec![
        EventBody::Tick,
        EventBody::Deposit {
            account: 1,
            amount: usd(1),
        },
        EventBody::Withdraw {
            account: 1,
            amount: usd(1),
        },
        EventBody::Cancel {
            account: 1,
            order_id: 1,
        },
        EventBody::CancelAll {
            account: 1,
            market: None,
        },
        EventBody::FundInsurance { amount: usd(1) },
        EventBody::Configure(EngineConfig::default()),
        EventBody::AddMarket(MarketConfig::example(9)),
        EventBody::Oracle {
            market: M,
            index_price: px(50_000),
            source_ts: clock,
        },
    ];
    for body in bodies {
        let e = Event {
            ts: clock - 1,
            body,
        };
        assert_eq!(ex.engine.apply(&e), Err(Error::TimeRegression), "{e:?}");
    }
    assert_eq!(ex.engine.content_hash(), before);
    assert_eq!(ex.engine.clock(), clock);
    // equal timestamps are fine
    ex.engine
        .apply(&Event {
            ts: clock,
            body: EventBody::Deposit {
                account: 1,
                amount: usd(1),
            },
        })
        .unwrap();
    ex.check();
}

#[test]
fn engine_stays_usable_at_the_largest_timestamp() {
    let mut ex = funded();
    ex.apply_at(
        u64::MAX,
        EventBody::Deposit {
            account: 1,
            amount: usd(5),
        },
    )
    .unwrap();
    assert_eq!(ex.engine.clock(), u64::MAX);
    // anything earlier is a regression
    let e = Event {
        ts: u64::MAX - 1,
        body: EventBody::Deposit {
            account: 1,
            amount: usd(5),
        },
    };
    assert_eq!(ex.engine.apply(&e), Err(Error::TimeRegression));
    // equal timestamps keep working
    ex.apply_at(
        u64::MAX,
        EventBody::Deposit {
            account: 2,
            amount: usd(5),
        },
    )
    .unwrap();
    ex.apply_at(
        u64::MAX,
        EventBody::Withdraw {
            account: 2,
            amount: usd(5),
        },
    )
    .unwrap();
    // the oracle is long stale: opening orders are refused, nothing halts
    let p = limit_order(&mut ex, 1, Side::Buy, px(40_000).0, 10, Tif::Gtc);
    assert_eq!(
        ex.engine.apply(&Event {
            ts: u64::MAX,
            body: EventBody::Place(p)
        }),
        Err(Error::StaleOracle)
    );
    assert!(!ex.engine.is_halted());
    ex.check();
}

#[test]
fn adding_a_market_at_the_largest_timestamp_is_rejected_cleanly() {
    let mut ex = funded();
    let before = ex.engine.content_hash();
    assert_eq!(
        ex.apply_at(u64::MAX, EventBody::AddMarket(MarketConfig::example(2))),
        Err(Error::Overflow)
    );
    assert!(
        !ex.engine.is_halted(),
        "overflow before any mutation is not fatal"
    );
    // the failed event did not move the clock
    assert_eq!(ex.engine.content_hash(), before);
}

#[test]
fn tick_and_oracle_at_a_huge_but_representable_time_work() {
    let mut ex = funded();
    ex.apply_at(1 << 62, EventBody::Tick).unwrap();
    assert!(!ex.engine.is_halted());
    ex.check();
}

#[test]
fn tick_at_the_largest_timestamp_does_not_halt_the_engine() {
    // The next funding boundary after u64::MAX is not representable.
    let mut ex = funded();
    let _ = ex.apply_at(u64::MAX, EventBody::Tick);
    assert!(
        !ex.engine.is_halted(),
        "a hostile timestamp must not stop the exchange: {:?}",
        ex.engine.halt_reason()
    );
}

#[test]
fn oracle_at_the_largest_timestamp_does_not_halt_the_engine() {
    let mut ex = funded();
    let _ = ex.apply_at(
        u64::MAX,
        EventBody::Oracle {
            market: M,
            index_price: px(50_000),
            source_ts: u64::MAX,
        },
    );
    assert!(!ex.engine.is_halted(), "{:?}", ex.engine.halt_reason());
}

// ---------------------------------------------------------------------------
// Configuration hostility
// ---------------------------------------------------------------------------

#[test]
fn events_before_configure_are_rejected() {
    let mut e = Engine::new();
    let ev = |body| Event { ts: 1, body };
    assert_eq!(
        e.apply(&ev(EventBody::Deposit {
            account: 1,
            amount: usd(1)
        })),
        Err(Error::NotConfigured)
    );
    assert_eq!(e.apply(&ev(EventBody::Tick)), Err(Error::NotConfigured));
    assert_eq!(
        e.apply(&ev(EventBody::AddMarket(MarketConfig::example(1)))),
        Err(Error::NotConfigured)
    );
    assert!(!e.is_halted());
    e.check_invariants().unwrap();
}

#[test]
fn hostile_engine_configurations_are_rejected() {
    let mut e = Engine::new();
    let try_cfg = |e: &mut Engine, c: EngineConfig| {
        e.apply(&Event {
            ts: 1,
            body: EventBody::Configure(c),
        })
    };
    let ok = EngineConfig::default();
    let bad = [
        EngineConfig {
            max_accounts: 0,
            ..ok.clone()
        },
        EngineConfig {
            max_accounts: u32::MAX,
            ..ok.clone()
        },
        EngineConfig {
            max_liquidations_per_event: 0,
            ..ok.clone()
        },
        EngineConfig {
            max_liquidations_per_event: u32::MAX,
            ..ok.clone()
        },
        EngineConfig {
            fee_tiers: vec![],
            ..ok.clone()
        },
        EngineConfig {
            fee_tiers: vec![
                perp_domain::types::FeeTier {
                    maker_ppm: 0,
                    taker_ppm: u32::MAX
                };
                3
            ],
            ..ok.clone()
        },
        EngineConfig {
            fee_tiers: vec![perp_domain::types::FeeTier {
                maker_ppm: i32::MIN,
                taker_ppm: 100,
            }],
            ..ok.clone()
        },
        EngineConfig {
            fee_tiers: vec![perp_domain::types::FeeTier {
                maker_ppm: -200,
                taker_ppm: 100,
            }],
            ..ok.clone()
        },
    ];
    for c in bad {
        assert!(matches!(try_cfg(&mut e, c), Err(Error::InvalidConfig(_))));
        assert!(e.config().is_none());
    }
    try_cfg(&mut e, ok.clone()).unwrap();
    assert_eq!(try_cfg(&mut e, ok), Err(Error::AlreadyConfigured));
}

#[test]
fn hostile_market_configurations_are_rejected() {
    let mut ex = Ex::bare();
    let base = MarketConfig::example(1);
    let bad = [
        MarketConfig {
            tick: 0,
            ..base.clone()
        },
        MarketConfig {
            tick: u64::MAX,
            ..base.clone()
        },
        MarketConfig {
            lot: 0,
            ..base.clone()
        },
        MarketConfig {
            lot: u64::MAX,
            ..base.clone()
        },
        MarketConfig {
            max_order_qty: 15,
            ..base.clone()
        },
        MarketConfig {
            max_order_qty: u64::MAX,
            ..base.clone()
        },
        MarketConfig {
            max_position: u64::MAX,
            ..base.clone()
        },
        MarketConfig {
            max_leverage: 0,
            ..base.clone()
        },
        MarketConfig {
            max_leverage: u32::MAX,
            ..base.clone()
        },
        MarketConfig {
            tiers: vec![],
            ..base.clone()
        },
        MarketConfig {
            liq_chunk_bps: 0,
            ..base.clone()
        },
        MarketConfig {
            liq_chunk_bps: u32::MAX,
            ..base.clone()
        },
        MarketConfig {
            liq_band_bps: 0,
            ..base.clone()
        },
        MarketConfig {
            oracle_max_change_bps: 0,
            ..base.clone()
        },
        MarketConfig {
            oracle_max_staleness_ms: 0,
            ..base.clone()
        },
        MarketConfig {
            funding_interval_ms: 0,
            ..base.clone()
        },
        MarketConfig {
            funding_cap_ppb: i64::MIN,
            ..base.clone()
        },
        MarketConfig {
            funding_interest_ppb: i64::MAX,
            ..base.clone()
        },
    ];
    for c in bad {
        let before = ex.engine.content_hash();
        assert!(matches!(
            ex.apply(EventBody::AddMarket(c)),
            Err(Error::InvalidConfig(_))
        ));
        assert_eq!(ex.engine.content_hash(), before);
    }
    ex.apply(EventBody::AddMarket(base.clone())).unwrap();
    assert_eq!(
        ex.apply(EventBody::AddMarket(base)),
        Err(Error::MarketExists)
    );
    ex.check();
}

#[test]
fn market_count_is_capped() {
    let mut ex = Ex::bare();
    for id in 1..=64u32 {
        ex.apply(EventBody::AddMarket(MarketConfig::example(id)))
            .unwrap();
    }
    assert_eq!(
        ex.apply(EventBody::AddMarket(MarketConfig::example(65))),
        Err(Error::TooManyMarkets)
    );
    assert_eq!(ex.engine.market_ids().len(), 64);
    ex.check();
}

#[test]
fn account_count_is_capped() {
    let mut ex = Ex::bare();
    ex.engine = Engine::new();
    ex.ts = 1_000;
    ex.apply(EventBody::Configure(EngineConfig {
        max_accounts: 3,
        ..EngineConfig::default()
    }))
    .unwrap();
    ex.fund(&[1, 2, 3], 10);
    assert_rejected_without_change(
        &mut ex,
        EventBody::Deposit {
            account: 4,
            amount: usd(10),
        },
        Error::TooManyAccounts,
    );
    // existing accounts can still deposit
    ex.deposit(2, 5);
    assert_eq!(ex.engine.account_ids(), vec![1, 2, 3]);
}

// ---------------------------------------------------------------------------
// Storms and limits
// ---------------------------------------------------------------------------

#[test]
fn cancels_of_unknown_orders_are_cheap_and_change_nothing() {
    let mut ex = funded();
    ex.gtc(1, Side::Buy, 40_000, 10);
    let before = ex.engine.content_hash();
    let applied = ex.engine.events_applied();
    let mut rng = Rng::new(5);
    for i in 0..12_000u64 {
        let order_id = match i % 4 {
            0 => rng.next_u64(),
            1 => u64::MAX - (i % 3),
            2 => 0,
            _ => 10_000 + i,
        };
        let account = if i % 5 == 0 { 424_242 } else { 1 + (i % 3) };
        let r = ex.apply(EventBody::Cancel { account, order_id });
        if account == 424_242 {
            assert_eq!(r, Err(Error::UnknownAccount));
        } else {
            assert_eq!(r, Err(Error::UnknownOrder));
        }
    }
    assert_eq!(ex.engine.content_hash(), before);
    assert_eq!(ex.engine.events_applied(), applied + 12_000);
    ex.check();
}

#[test]
fn place_and_cancel_storm_leaves_no_residue() {
    let mut ex = funded();
    let empty_len = {
        let mut w = perp_domain::codec::Writer::new_buf();
        ex.engine.encode_state(&mut w);
        w.len()
    };
    let mut last_id = 0;
    for i in 0..5_000u64 {
        let price = 40_000 + (i % 50);
        let id = ex.gtc(1 + (i % 3), Side::Buy, price, 10);
        assert!(id > last_id, "order ids never repeat");
        last_id = id;
        let outs = ex.cancel(1 + (i % 3), id).unwrap();
        assert!(has(
            &outs,
            |o| matches!(o, Output::Canceled { order_id, reason: CancelReason::User, .. } if *order_id == id)
        ));
    }
    for a in 1..=3 {
        assert!(ex.engine.open_orders_of(a).is_empty());
    }
    assert_eq!(ex.engine.summary().open_orders, 0);
    assert_eq!(ex.engine.book_view(M, 10).unwrap().bids.len(), 0);
    let mut w = perp_domain::codec::Writer::new_buf();
    ex.engine.encode_state(&mut w);
    assert_eq!(w.len(), empty_len, "state size is back to the empty size");
    ex.check();
}

#[test]
fn account_order_limit_applies_and_frees_up_after_cancel_all() {
    let mut ex = funded();
    let max = ex.engine.market_config(M).unwrap().max_orders_per_account;
    assert_eq!(max, 64);
    for i in 0..u64::from(max) {
        ex.gtc(1, Side::Buy, 40_000 + i, 10);
    }
    let p = limit_order(&mut ex, 1, Side::Buy, px(41_000).0, 10, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::TooManyOrders);
    // IOC does not rest and is not limited by the resting count
    let p = limit_order(&mut ex, 1, Side::Buy, px(41_000).0, 10, Tif::Ioc);
    let outs = ex.place(p).unwrap();
    assert!(has(&outs, |o| matches!(
        o,
        Output::Canceled {
            reason: CancelReason::Remainder,
            ..
        }
    )));
    // another account is unaffected
    ex.gtc(2, Side::Buy, 40_000, 10);
    // cancel everything and fill up again
    let outs = ex
        .apply(EventBody::CancelAll {
            account: 1,
            market: None,
        })
        .unwrap();
    assert_eq!(outs.len(), 64);
    assert!(ex.engine.open_orders_of(1).is_empty());
    for i in 0..u64::from(max) {
        ex.gtc(1, Side::Buy, 40_000 + i, 10);
    }
    assert_eq!(ex.engine.open_orders_of(1).len(), 64);
    ex.check();
}

#[test]
fn book_order_limit_is_enforced() {
    let cfg = MarketConfig {
        max_book_orders: 100,
        ..MarketConfig::example(M)
    };
    let mut ex = Ex::with_market(cfg);
    ex.fund(&[1, 2, 3], 1_000_000);
    for i in 0..100u64 {
        ex.gtc(1 + (i % 3), Side::Buy, 40_000 + i, 10);
    }
    let p = limit_order(&mut ex, 1, Side::Buy, px(41_000).0, 10, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::BookFull);
    ex.check();
}

#[test]
fn price_level_limit_is_enforced_on_replace() {
    let cfg = MarketConfig {
        max_levels_per_side: 2,
        ..MarketConfig::example(M)
    };
    let mut ex = Ex::with_market(cfg);
    ex.fund(&[1], 1_000_000);
    ex.gtc(1, Side::Buy, 40_000, 10);
    ex.gtc(1, Side::Buy, 39_000, 10);
    let id = ex.gtc(1, Side::Buy, 39_000, 10);
    // a third level is refused
    let ev = EventBody::Replace {
        account: 1,
        order_id: id,
        price: Some(px(38_000)),
        qty: None,
    };
    assert_rejected_without_change(&mut ex, ev, Error::BookFull);
    ex.check();
}

#[test]
fn replace_storm_keeps_exactly_one_order() {
    let mut ex = funded();
    let id = ex.gtc(1, Side::Buy, 40_000, 100);
    let mut priority_kept = 0u32;
    let mut priority_lost = 0u32;
    for i in 0..2_000u64 {
        // alternate: shrink in place, grow (loses priority), move price
        let (price, qty) = match i % 3 {
            0 => (None, Some(Qty(900))),
            1 => (None, Some(Qty(1_000))),
            _ => (Some(Price(px(40_000).0 + 10_000 * (i % 7))), None),
        };
        let outs = ex
            .apply(EventBody::Replace {
                account: 1,
                order_id: id,
                price,
                qty,
            })
            .unwrap();
        match outs.as_slice() {
            [Output::Replaced {
                order_id,
                kept_priority,
            }] => {
                assert_eq!(*order_id, id);
                if *kept_priority {
                    priority_kept += 1;
                } else {
                    priority_lost += 1;
                }
            }
            other => panic!("unexpected outputs {other:?}"),
        }
    }
    assert!(priority_kept > 0 && priority_lost > 0);
    assert_eq!(ex.engine.open_orders_of(1), vec![id]);
    assert_eq!(ex.engine.summary().open_orders, 1);
    ex.check();
}

#[test]
fn thousands_of_tiny_orders_are_swept_with_a_bounded_number_of_fills() {
    let mut ex = Ex::new();
    // 100 makers x 50 tiny asks of 0.001 base at one price level
    for a in 1..=100u64 {
        ex.deposit(a, 10_000);
    }
    for round in 0..50u64 {
        for a in 1..=100u64 {
            ex.gtc(a, Side::Sell, 50_100, 1);
            let _ = round;
        }
    }
    assert_eq!(ex.engine.summary().open_orders, 5_000);
    ex.deposit(500, 1_000_000);
    let outs = ex.limit(500, Side::Buy, 50_100, 5_000, Tif::Ioc).unwrap();
    // at most 256 fills per incoming order; the rest is cancelled as remainder
    assert_eq!(fills(&outs).len(), 256);
    assert_eq!(filled_qty(&outs), 2_560);
    assert!(has(
        &outs,
        |o| matches!(o, Output::Canceled { reason: CancelReason::Remainder, qty, .. } if qty.0 == 50_000 - 2_560)
    ));
    assert_eq!(ex.size(500), 2_560);
    assert_eq!(ex.engine.summary().open_orders, 5_000 - 256);
    ex.check();
}

#[test]
fn fill_or_kill_for_more_than_the_fill_cap_is_rejected() {
    let mut ex = Ex::new();
    for a in 1..=10u64 {
        ex.deposit(a, 10_000);
    }
    for _ in 0..30u64 {
        for a in 1..=10u64 {
            ex.gtc(a, Side::Sell, 50_100, 1);
        }
    }
    ex.deposit(500, 1_000_000);
    // 300 resting orders; filling 300 of them needs more than 256 fills
    let before = ex.engine.content_hash();
    assert_eq!(
        ex.limit(500, Side::Buy, 50_100, 300, Tif::Fok),
        Err(Error::FokNotFillable)
    );
    assert_eq!(ex.engine.content_hash(), before);
    // 256 fills are fine
    let outs = ex.limit(500, Side::Buy, 50_100, 256, Tif::Fok).unwrap();
    assert_eq!(fills(&outs).len(), 256);
    ex.check();
}

#[test]
fn many_rejected_events_never_change_the_content_hash() {
    let mut ex = funded();
    ex.gtc(1, Side::Buy, 40_000, 10);
    let content = ex.engine.content_hash();
    let applied = ex.engine.events_applied();
    let mut n = 0u64;
    for _ in 0..500 {
        let p1 = limit_order(&mut ex, 1, Side::Buy, 0, 10, Tif::Gtc);
        let p2 = limit_order(&mut ex, 1, Side::Buy, px(40_000).0, 0, Tif::Gtc);
        let p3 = limit_order(&mut ex, 99, Side::Buy, px(40_000).0, 10, Tif::Gtc);
        for (body, err) in [
            (EventBody::Place(p1), Error::InvalidPrice),
            (EventBody::Place(p2), Error::ZeroQty),
            (EventBody::Place(p3), Error::UnknownAccount),
            (
                EventBody::Cancel {
                    account: 1,
                    order_id: 888,
                },
                Error::UnknownOrder,
            ),
            (
                EventBody::Withdraw {
                    account: 1,
                    amount: usd(2_000_000),
                },
                Error::InsufficientCollateral,
            ),
            (
                EventBody::Deposit {
                    account: 1,
                    amount: Money(-3),
                },
                Error::ZeroAmount,
            ),
        ] {
            assert_eq!(ex.apply(body), Err(err));
            n += 1;
        }
    }
    assert_eq!(ex.engine.content_hash(), content);
    assert_eq!(ex.engine.events_applied(), applied + n);
    ex.check();
}

// ---------------------------------------------------------------------------
// Position limits near the maximum
// ---------------------------------------------------------------------------

#[test]
fn position_plus_resting_orders_may_not_exceed_the_position_limit() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000_000);
    for _ in 0..5 {
        ex.trade(1, 2, Side::Buy, 50_000, 99_000); // 99 base each
    }
    assert_eq!(ex.size(1), 4_950_000);
    assert_eq!(ex.size(2), -4_950_000);
    let max = ex.engine.market_config(M).unwrap().max_position;
    assert_eq!(max, 5_000_000);

    // 6 base would take the position to 501 > 500
    let p = limit_order(&mut ex, 1, Side::Buy, px(40_000).0, 60_000, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::PositionLimit);
    let p = limit_order(&mut ex, 1, Side::Buy, px(50_000).0, 60_000, Tif::Ioc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::PositionLimit);
    // exactly 5 base reaches the limit and may rest
    let id = ex.gtc(1, Side::Buy, 40_000, 5_000);
    // ... but then not one lot more, even on a different price
    let p = limit_order(&mut ex, 1, Side::Buy, px(39_000).0, 10, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::PositionLimit);
    // a replace that grows the order past the limit is rejected too
    assert_rejected_without_change(
        &mut ex,
        EventBody::Replace {
            account: 1,
            order_id: id,
            price: None,
            qty: Some(Qty(50_010)),
        },
        Error::PositionLimit,
    );
    // selling reduces exposure and is always possible
    let p = limit_order(&mut ex, 1, Side::Sell, px(60_000).0, 1_000_000, Tif::Gtc);
    ex.place(p).unwrap();
    // the short side has the mirrored limit
    let p = limit_order(&mut ex, 2, Side::Sell, px(60_000).0, 60_000, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::PositionLimit);
    ex.check();
}

#[test]
fn position_at_the_absolute_quantity_ceiling_is_accepted_but_not_exceeded() {
    let cfg = MarketConfig {
        max_order_qty: MAX_QTY,
        max_position: MAX_QTY,
        ..MarketConfig::example(M)
    };
    let mut ex = Ex::with_market(cfg);
    ex.fund(&[1, 2], 100_000_000_000_000);
    let mut sell = limit_order(&mut ex, 2, Side::Sell, px(50_000).0, MAX_QTY, Tif::Gtc);
    sell.client_id = 1;
    ex.place(sell).unwrap();
    let buy = limit_order(&mut ex, 1, Side::Buy, px(50_000).0, MAX_QTY, Tif::Ioc);
    ex.place(buy).unwrap();
    assert_eq!(ex.size(1), i64::try_from(MAX_QTY).unwrap());
    assert_eq!(ex.size(2), -i64::try_from(MAX_QTY).unwrap());
    ex.check();
    let p = limit_order(&mut ex, 1, Side::Buy, px(40_000).0, 10, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::PositionLimit);
    let p = limit_order(&mut ex, 1, Side::Buy, px(40_000).0, MAX_QTY + 10, Tif::Gtc);
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::InvalidQty);
    let outs = ex.oracle(51_000).unwrap();
    assert!(has(&outs, |o| matches!(o, Output::MarkUpdated { .. })));
    ex.check();
}

#[test]
fn reduce_only_orders_without_a_position_are_rejected() {
    let mut ex = funded();
    let mut p = limit_order(&mut ex, 1, Side::Sell, px(60_000).0, 10, Tif::Gtc);
    p.reduce_only = true;
    assert_rejected_without_change(&mut ex, EventBody::Place(p), Error::ReduceOnlyViolation);
}

// ---------------------------------------------------------------------------
// Fuzzing with garbage
// ---------------------------------------------------------------------------

const BAD_U64: [u64; 12] = [
    0,
    1,
    7,
    9,
    1 << 63,
    u64::MAX,
    u64::MAX - 1,
    MAX_QTY,
    MAX_QTY + 1,
    MAX_PRICE,
    MAX_PRICE + 1,
    1_000_010,
];
const BAD_I128: [i128; 9] = [
    0,
    -1,
    1,
    i128::MIN,
    i128::MAX,
    MAX_AMOUNT,
    MAX_AMOUNT + 1,
    -MAX_AMOUNT,
    MAX_BALANCE,
];

struct Fuzzer {
    rng: Rng,
    engine: Engine,
    ts: u64,
    client: u64,
    recent: Vec<u64>,
    ok: u64,
    rejected: u64,
    volatile: bool,
}

impl Fuzzer {
    fn new(seed: u64, volatile: bool) -> Fuzzer {
        let mut f = Fuzzer {
            rng: Rng::new(seed),
            engine: Engine::new(),
            ts: 1_000,
            client: 0,
            recent: Vec::new(),
            ok: 0,
            rejected: 0,
            volatile,
        };
        f.setup();
        f
    }

    fn step(&mut self, body: EventBody) -> Result<Vec<Output>, Error> {
        self.ts += 1;
        let ev = Event { ts: self.ts, body };
        self.engine.apply(&ev)
    }

    fn setup(&mut self) {
        self.step(EventBody::Configure(EngineConfig::default()))
            .unwrap();
        for id in 1..=2u32 {
            self.step(EventBody::AddMarket(MarketConfig::example(id)))
                .unwrap();
            let ts = self.ts + 1;
            self.step(EventBody::Oracle {
                market: id,
                index_price: px(50_000),
                source_ts: ts,
            })
            .unwrap();
        }
        self.step(EventBody::FundInsurance {
            amount: usd(100_000),
        })
        .unwrap();
        for a in 1..=45u64 {
            let amount = usd(i128::from(self.rng.range(2_000, 200_000)));
            self.step(EventBody::Deposit { account: a, amount })
                .unwrap();
        }
    }

    fn account(&mut self) -> u64 {
        if self.rng.chance(1, 30) {
            *self
                .rng
                .pick(&[0, 46, 1000, u64::MAX, u64::MAX - 1])
                .unwrap()
        } else {
            self.rng.range(1, 45)
        }
    }

    fn market(&mut self) -> MarketId {
        if self.rng.chance(1, 50) {
            *self.rng.pick(&[0, 3, u32::MAX]).unwrap()
        } else {
            self.rng.range(1, 2) as u32
        }
    }

    fn order_id(&mut self) -> u64 {
        if !self.recent.is_empty() && self.rng.chance(2, 3) {
            *self.rng.pick(&self.recent).unwrap()
        } else {
            *self.rng.pick(&BAD_U64).unwrap()
        }
    }

    fn price_near(&mut self, market: MarketId) -> u64 {
        if self.rng.chance(1, 12) {
            return *self.rng.pick(&BAD_U64).unwrap();
        }
        let mark = self
            .engine
            .mark_price(market)
            .map_or(50_000_000_000, |p| p.0);
        let span = mark / 25; // +- 4 %
        let p = mark - span + self.rng.below(2 * span + 1);
        let p = (p / 10_000).max(1) * 10_000;
        if self.rng.chance(1, 20) {
            p + 1 // off tick
        } else {
            p
        }
    }

    fn event(&mut self) -> EventBody {
        let kind = self.rng.below(100);
        match kind {
            0..=44 => {
                let market = self.market();
                let tif = *self
                    .rng
                    .pick(&[Tif::Gtc, Tif::Ioc, Tif::Fok, Tif::PostOnly])
                    .unwrap();
                let kind = if self.rng.chance(1, 4) {
                    let s = if self.rng.chance(1, 10) {
                        self.rng.below(7_000) as u32
                    } else {
                        self.rng.below(300) as u32
                    };
                    OrderKind::Market {
                        max_slippage_bps: s,
                    }
                } else {
                    OrderKind::Limit {
                        price: Price(self.price_near(market)),
                        tif,
                    }
                };
                let qty = if self.rng.chance(1, 12) {
                    *self.rng.pick(&BAD_U64).unwrap()
                } else if self.rng.chance(1, 40) {
                    self.rng.range(1, 100_000) * 10 // up to the max order size
                } else {
                    self.rng.range(1, 600) * 10
                };
                self.client += 1;
                let client_id = if self.rng.chance(1, 15) {
                    self.rng.below(64)
                } else {
                    self.client
                };
                EventBody::Place(PlaceOrder {
                    market,
                    account: self.account(),
                    side: if self.rng.chance(1, 2) {
                        Side::Buy
                    } else {
                        Side::Sell
                    },
                    kind,
                    qty: Qty(qty),
                    client_id,
                    reduce_only: self.rng.chance(1, 8),
                    stp: *self
                        .rng
                        .pick(&[
                            StpMode::CancelNewest,
                            StpMode::CancelOldest,
                            StpMode::CancelBoth,
                        ])
                        .unwrap(),
                })
            }
            45..=57 => EventBody::Cancel {
                account: self.account(),
                order_id: self.order_id(),
            },
            58..=62 => {
                let market = if self.rng.chance(1, 2) {
                    Some(self.market())
                } else {
                    None
                };
                EventBody::CancelAll {
                    account: self.account(),
                    market,
                }
            }
            63..=69 => {
                let m = self.market();
                let price = if self.rng.chance(1, 2) {
                    Some(Price(self.price_near(m)))
                } else {
                    None
                };
                let qty = if self.rng.chance(1, 2) {
                    Some(Qty(if self.rng.chance(1, 10) {
                        *self.rng.pick(&BAD_U64).unwrap()
                    } else {
                        self.rng.range(1, 600) * 10
                    }))
                } else {
                    None
                };
                EventBody::Replace {
                    account: self.account(),
                    order_id: self.order_id(),
                    price,
                    qty,
                }
            }
            70..=77 => {
                let amount = if self.rng.chance(1, 10) {
                    *self.rng.pick(&BAD_I128).unwrap()
                } else {
                    i128::from(self.rng.range(1, 50_000)) * 1_000_000
                };
                EventBody::Deposit {
                    account: self.account(),
                    amount: Money(amount),
                }
            }
            78..=83 => {
                let amount = if self.rng.chance(1, 10) {
                    *self.rng.pick(&BAD_I128).unwrap()
                } else {
                    i128::from(self.rng.range(1, 30_000)) * 1_000_000
                };
                EventBody::Withdraw {
                    account: self.account(),
                    amount: Money(amount),
                }
            }
            84..=93 => {
                let market = self.market();
                let cur = self
                    .engine
                    .index_price(market)
                    .map_or(50_000_000_000, |p| p.0);
                let max_bps = if self.volatile { 700 } else { 120 };
                let bps = self.rng.below(2 * max_bps + 1);
                let price = if self.rng.chance(1, 25) {
                    *self.rng.pick(&BAD_U64).unwrap()
                } else {
                    (u128::from(cur) * u128::from(10_000 - max_bps + bps) / 10_000) as u64
                };
                let source_ts = match self.rng.below(30) {
                    0 => 0,
                    1 => self.ts + 5,
                    2 => u64::MAX,
                    _ => self.ts + 1,
                };
                EventBody::Oracle {
                    market,
                    index_price: Price(price),
                    source_ts,
                }
            }
            94..=96 => EventBody::Tick,
            97 => EventBody::SetFeeTier {
                account: self.account(),
                tier: self.rng.below(10) as u8,
            },
            98 => EventBody::FundInsurance {
                amount: Money(if self.rng.chance(1, 4) {
                    *self.rng.pick(&BAD_I128).unwrap()
                } else {
                    i128::from(self.rng.range(1, 5_000)) * 1_000_000
                }),
            },
            _ => {
                if self.rng.chance(1, 2) {
                    EventBody::Configure(EngineConfig::default())
                } else {
                    let mut c = MarketConfig::example(self.rng.range(1, 5) as u32);
                    if self.rng.chance(1, 2) {
                        c.tick = *self.rng.pick(&BAD_U64).unwrap();
                    }
                    EventBody::AddMarket(c)
                }
            }
        }
    }

    fn run(&mut self, events: u64) {
        for i in 0..events {
            let body = self.event();
            // time: mostly forward by a few ms, sometimes a long pause, and
            // rarely an event stamped slightly in the past (a regression)
            let ev_ts = if self.rng.chance(1, 60) {
                self.engine.clock().saturating_sub(self.rng.range(1, 500))
            } else {
                if self.rng.chance(1, 400) {
                    self.ts += self.rng.range(30_000, 150_000);
                } else {
                    self.ts += self.rng.below(40);
                }
                self.ts
            };
            let ev = Event {
                ts: ev_ts,
                body: body.clone(),
            };
            let res = self.engine.apply(&ev);
            assert!(
                !self.engine.is_halted(),
                "halted at event {i} by {ev:?}: {:?} ({res:?})",
                self.engine.halt_reason()
            );
            match res {
                Ok(outs) => {
                    self.ok += 1;
                    for o in &outs {
                        if let Output::Accepted { order_id, .. } = o {
                            self.recent.push(*order_id);
                            if self.recent.len() > 64 {
                                self.recent.remove(0);
                            }
                        }
                    }
                }
                Err(e) => {
                    self.rejected += 1;
                    assert!(
                        !matches!(e, Error::Internal(_) | Error::Halted),
                        "internal error at event {i} by {ev:?}: {e:?}"
                    );
                }
            }
            if i % 5_000 == 4_999 {
                self.engine
                    .check_invariants()
                    .unwrap_or_else(|why| panic!("invariants broken after event {i}: {why}"));
            }
        }
        self.engine.check_invariants().unwrap();
    }
}

#[test]
fn fuzz_garbage_events_never_panic_halt_or_break_invariants() {
    let mut f = Fuzzer::new(0xA11CE, false);
    f.run(200_000);
    let s = f.engine.summary();
    assert!(!s.halted);
    assert!(f.ok > 20_000, "enough accepted events: {}", f.ok);
    assert!(
        f.rejected > 20_000,
        "enough rejected events: {}",
        f.rejected
    );
    assert!(s.fills > 500, "the fuzzer trades: {}", s.fills);
    assert!(s.accounts >= 45);
}

#[test]
fn fuzz_volatile_market_exercises_liquidations_without_halting() {
    let mut f = Fuzzer::new(0xBEEF, true);
    f.run(60_000);
    let s = f.engine.summary();
    assert!(!s.halted);
    assert!(s.fills > 200);
    // the money identity incl. bad debt holds whatever happened
    let e = &f.engine;
    assert_eq!(
        e.total_collateral().0 + e.insurance_fund().0 + e.fee_pool().0 - e.total_basis().0,
        e.net_deposits().0 + e.bad_debt().0
    );
}
