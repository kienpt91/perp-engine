//! Regression tests for four review findings. They state the CORRECT
//! behaviour and fail on an engine that has the problem.
//!
//! * MONEY-001: a fill at a price far from mark (colluding accounts) must not
//!   push a taker below zero equity or touch the insurance fund.
//! * MONEY-002: a liquidation payout covers the realised shortfall of the
//!   closed chunk only, never the unrealised loss of the open remainder.
//! * DOS-002: unfillable liquidation candidates must not starve a fillable one.
//! * DOS-003: a stale oracle in a tiny position must not shield large losses
//!   in a fresh market.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::*;
use perp_domain::engine::Engine;
use perp_domain::error::Error;
use perp_domain::fixed::{Money, Price, Qty, QTY_SCALE};
use perp_domain::rng::Rng;
use perp_domain::traffic::{TrafficConfig, TrafficGenerator, START_PRICE};
use perp_domain::types::{
    AccountId, EngineConfig, Event, EventBody, MarketConfig, MarketId, OrderKind, Output,
    PlaceOrder, Side, StpMode, Tif,
};

const MARK: u64 = 50_000;

/// Place a limit order in an arbitrary market.
fn place_in(
    ex: &mut Ex,
    market: MarketId,
    account: AccountId,
    side: Side,
    price: u64,
    milli: u64,
    tif: Tif,
) -> Result<Vec<Output>, Error> {
    let mut p = ex.order(
        account,
        side,
        OrderKind::Limit {
            price: px(price),
            tif,
        },
        milli,
    );
    p.market = market;
    ex.place(p)
}

/// Common state checks: not halted, no bad debt, no underwater account,
/// money identity, insurance fund untouched.
fn clean(ex: &Ex, fund0: i128, what: &str) {
    assert!(
        !ex.engine.is_halted(),
        "{what}: engine halted: {:?}",
        ex.engine.halt_reason()
    );
    assert_eq!(ex.engine.bad_debt().0, 0, "{what}: bad debt recorded");
    assert_eq!(
        ex.engine.insurance_fund().0,
        fund0,
        "{what}: insurance fund changed (was {fund0}, now {})",
        ex.engine.insurance_fund().0
    );
    let under = ex.engine.underwater_accounts();
    assert!(
        under.is_empty(),
        "{what}: accounts with negative equity: {under:?}"
    );
    ex.engine
        .check_invariants()
        .unwrap_or_else(|e| panic!("{what}: invariant violated: {e}"));
}

// ---------------------------------------------------------------------------
// MONEY-001: off-market fills
// ---------------------------------------------------------------------------

/// Initial margin at mark (5% bracket) for `q_milli` base units.
fn imr_at_mark(q_milli: u64) -> i128 {
    i128::from(MARK) * i128::from(q_milli) / 1_000 * 5 / 100
}

/// Collateral that passes the entry check of an order for `q_milli` at the
/// reference price `ref_px` (initial margin at mark + taker fee reserve).
fn honest_deposit(q_milli: u64, ref_px: u64) -> i128 {
    let n = i128::from(ref_px.max(MARK)) * i128::from(q_milli) / 1_000;
    imr_at_mark(q_milli) + n / 2_000 + 2
}

/// The colluding pair: B rests an order at `maker_price`, A (just above its
/// initial margin at mark) crosses it. Everything the attack could break is
/// asserted after every step. With `full`, B then closes against a sock puppet
/// at mark and withdraws.
fn run_attack(taker_side: Side, maker_price: u64, q_milli: u64, full: bool) {
    let mut ex = Ex::new();
    ex.apply(EventBody::FundInsurance { amount: usd(1_000) })
        .unwrap();
    let fund0 = ex.engine.insurance_fund().0;
    let (a, b, c) = (1, 2, 3);
    let dep_a = honest_deposit(q_milli, maker_price);
    ex.deposit(b, 200_000);
    ex.deposit(a, dep_a);
    ex.deposit(c, 200_000);
    let total_dep = 400_000 + dep_a;
    clean(&ex, fund0, "after deposits");

    // Rejection is an acceptable outcome of the fix, so errors are ignored.
    let _ = ex.limit(b, taker_side.opposite(), maker_price, q_milli, Tif::Gtc);
    clean(&ex, fund0, "after maker order");
    let _ = ex.limit(a, taker_side, maker_price, q_milli, Tif::Gtc);
    clean(&ex, fund0, "after taker order");

    let eq = ex.engine.equity(a).unwrap().0;
    assert!(eq >= 0, "taker equity negative: {eq}");
    if let Some(p) = ex.engine.position(a, M) {
        let mark = ex.engine.mark_price(M).unwrap().0;
        let n = i128::from(p.size.unsigned_abs()) * i128::from(mark) / i128::from(QTY_SCALE);
        let imr = n * 5 / 100;
        assert!(
            eq >= imr,
            "taker holds {} lots with equity {eq} below the initial margin {imr} it had to hold",
            p.size
        );
    }

    if full {
        if let Some(p) = ex.engine.position(b, M) {
            let close = if p.size > 0 { Side::Sell } else { Side::Buy };
            let n = p.size.unsigned_abs() / 10;
            let _ = ex.limit(c, close.opposite(), MARK, n, Tif::Gtc);
            let _ = ex.limit(b, close, MARK, n, Tif::Ioc);
            clean(&ex, fund0, "after B closed against C");
        }
        let mut withdrawn = 0i128;
        for acct in [a, b, c] {
            let col = ex.engine.collateral(acct).unwrap().0;
            if col > 0 {
                if ex
                    .apply(EventBody::Withdraw {
                        account: acct,
                        amount: Money(col),
                    })
                    .is_ok()
                {
                    withdrawn += col;
                }
                clean(&ex, fund0, "after withdrawal");
            }
        }
        let equities: i128 = [a, b, c]
            .iter()
            .map(|i| ex.engine.equity(*i).unwrap().0)
            .sum();
        assert!(
            equities + withdrawn <= usd(total_dep).0,
            "colluders extracted {} from {} deposited",
            equities + withdrawn,
            usd(total_dep).0
        );
    }
}

#[test]
fn money001_attack_ten_times_mark_buyer() {
    run_attack(Side::Buy, 500_000, 10_000, true);
}

#[test]
fn money001_buyer_fill_at_3x_mark() {
    run_attack(Side::Buy, 150_000, 10_000, false);
}

#[test]
fn money001_buyer_fill_at_2x_mark() {
    run_attack(Side::Buy, 100_000, 10_000, false);
}

#[test]
fn money001_buyer_fill_at_1_2x_mark() {
    run_attack(Side::Buy, 60_000, 10_000, false);
}

#[test]
fn money001_buyer_fill_at_1_05x_mark() {
    run_attack(Side::Buy, 52_500, 10_000, false);
}

#[test]
fn money001_mirror_seller_fill_at_0_1x_mark() {
    run_attack(Side::Sell, 5_000, 10_000, true);
}

#[test]
fn money001_mirror_seller_fill_at_0_5x_mark() {
    run_attack(Side::Sell, 25_000, 10_000, false);
}

#[test]
fn money001_mirror_seller_fill_at_0_95x_mark() {
    run_attack(Side::Sell, 47_500, 10_000, false);
}

/// Control: honest trade at mark is accepted and leaves everything clean.
#[test]
fn money001_control_trade_at_mark_is_accepted() {
    let mut ex = Ex::new();
    ex.apply(EventBody::FundInsurance { amount: usd(1_000) })
        .unwrap();
    let fund0 = ex.engine.insurance_fund().0;
    ex.deposit(2, 200_000);
    ex.deposit(1, honest_deposit(10_000, MARK));
    ex.gtc(2, Side::Sell, MARK, 10_000);
    let outs = ex.limit(1, Side::Buy, MARK, 10_000, Tif::Gtc).unwrap();
    assert_eq!(filled_qty(&outs), qty(10_000).0, "honest trade must fill");
    assert_eq!(ex.size(1), qty(10_000).0 as i64);
    clean(&ex, fund0, "honest trade");
}

/// Control: a fill 0.2% from mark with ample collateral is accepted.
#[test]
fn money001_control_fill_near_mark_is_accepted() {
    let mut ex = Ex::new();
    ex.apply(EventBody::FundInsurance { amount: usd(1_000) })
        .unwrap();
    let fund0 = ex.engine.insurance_fund().0;
    ex.deposit(2, 200_000);
    ex.deposit(1, 50_000);
    ex.gtc(2, Side::Sell, 50_100, 10_000);
    let outs = ex.limit(1, Side::Buy, 50_100, 10_000, Tif::Gtc).unwrap();
    assert_eq!(filled_qty(&outs), qty(10_000).0);
    clean(&ex, fund0, "near-mark trade");
}

// --- (c) seeded fuzz -------------------------------------------------------

fn fuzz_minimal_collateral(seed: u64) {
    let mut rng = Rng::new(seed);
    let mut ex = Ex::new();
    ex.apply(EventBody::FundInsurance { amount: usd(1_000) })
        .unwrap();
    for acct in 1..=6u64 {
        let d = i128::from(rng.range(100, 5_000));
        ex.deposit(acct, d);
    }
    let mut last_fund = ex.engine.insurance_fund().0;
    for i in 0..20_000u32 {
        let r = rng.below(100);
        let ctx = format!("seed {seed} event {i}");
        let res = if r < 60 {
            let acct = rng.range(1, 6);
            let side = if rng.chance(1, 2) {
                Side::Buy
            } else {
                Side::Sell
            };
            let mark = ex.engine.mark_price(M).unwrap().0;
            let p = (rng.range(mark / 5, mark * 5) / 10_000 * 10_000).max(10_000);
            let tif = if rng.chance(3, 5) { Tif::Gtc } else { Tif::Ioc };
            let order = PlaceOrder {
                market: M,
                account: acct,
                side,
                kind: OrderKind::Limit {
                    price: Price(p),
                    tif,
                },
                qty: Qty(rng.range(1, 3_000) * 10),
                client_id: ex.next_client(),
                reduce_only: false,
                stp: StpMode::CancelNewest,
            };
            ex.place(order)
        } else if r < 80 {
            let acct = rng.range(1, 6);
            let open = ex.engine.open_orders_of(acct);
            match rng.pick(&open) {
                Some(id) => ex.cancel(acct, *id),
                None => Ok(Vec::new()),
            }
        } else if r < 95 {
            let idx = ex.engine.index_price(M).unwrap().0 / 1_000_000;
            let step = (idx * rng.range(0, 30) / 10_000).max(1);
            let next = if rng.chance(1, 2) {
                idx + step
            } else {
                idx - step
            };
            ex.oracle_on_result(M, next)
        } else {
            ex.apply(EventBody::Tick)
        };
        let _ = res;
        let fund = ex.engine.insurance_fund().0;
        assert!(
            fund >= last_fund,
            "{ctx}: insurance fund decreased {last_fund} -> {fund}"
        );
        last_fund = fund;
        // `clean` also pins the fund to its previous value only when equal;
        // the fund may legitimately grow (liquidation fees), so re-base it.
        clean(&ex, fund, &ctx);
    }
}

#[test]
fn money001_fuzz_seed_1() {
    fuzz_minimal_collateral(1);
}

#[test]
fn money001_fuzz_seed_2() {
    fuzz_minimal_collateral(2);
}

#[test]
fn money001_fuzz_seed_3() {
    fuzz_minimal_collateral(0xDEAD_BEEF);
}

#[test]
fn money001_fuzz_seed_4() {
    fuzz_minimal_collateral(4_242);
}

// --- (d) market orders -----------------------------------------------------

/// A market order with the maximum slippage must be bounded by the mark, not
/// by the far-away best opposite price.
fn market_case(taker_side: Side, maker_price: u64) {
    let mut ex = Ex::new();
    ex.apply(EventBody::FundInsurance { amount: usd(1_000) })
        .unwrap();
    let fund0 = ex.engine.insurance_fund().0;
    ex.deposit(2, 200_000);
    ex.deposit(1, honest_deposit(1_000, maker_price) + 500);
    let _ = ex.limit(2, taker_side.opposite(), maker_price, 1_000, Tif::Gtc);
    let res = ex.market(1, taker_side, 1_000, 5_000);
    let mark = ex.engine.mark_price(M).unwrap().0;
    let lo = mark - mark / 2;
    let hi = mark + mark / 2;
    if let Ok(outs) = &res {
        for o in outs {
            if let Output::Fill { price, .. } = o {
                assert!(
                    (lo..=hi).contains(&price.0),
                    "market order executed at {} outside 5000 bps of mark {mark}",
                    price.0
                );
            }
        }
    }
    clean(&ex, fund0, "market order");
}

#[test]
fn money001_market_buy_not_filled_at_10x_mark() {
    market_case(Side::Buy, 500_000);
}

#[test]
fn money001_market_sell_not_filled_at_0_1x_mark() {
    market_case(Side::Sell, 5_000);
}

/// Control: a market order with a tight cap fills at an ask 0.2% above mark.
#[test]
fn money001_control_market_order_near_mark_fills() {
    let mut ex = Ex::new();
    ex.deposit(2, 200_000);
    ex.deposit(1, 10_000);
    ex.gtc(2, Side::Sell, 50_100, 1_000);
    let outs = ex.market(1, Side::Buy, 1_000, 100).unwrap();
    assert_eq!(filled_qty(&outs), qty(1_000).0);
    assert!(has(
        &outs,
        |o| matches!(o, Output::Fill { price, .. } if *price == px(50_100))
    ));
    ex.check();
}

// ---------------------------------------------------------------------------
// MONEY-002: insurance pays the realised shortfall only
// ---------------------------------------------------------------------------

const L: AccountId = 10;
const S: AccountId = 11;
const K1: AccountId = 12;
const K2: AccountId = 13;

/// A 10 base long opened at 50 000 with 25 251 collateral (5% of notional
/// plus the entry fee reserve), a generous insurance fund and, optionally,
/// bids that absorb the liquidation chunks: 2.5 base at 44 000 and 1.875 base
/// at 42 000.
fn ladder(with_bids: bool) -> Ex {
    let mut ex = Ex::new();
    ex.apply(EventBody::FundInsurance {
        amount: usd(500_000),
    })
    .unwrap();
    ex.fund(&[S, K1, K2], 200_000);
    ex.deposit(L, 25_251);
    ex.trade(L, S, Side::Buy, MARK, 10_000);
    if with_bids {
        ex.gtc(K1, Side::Buy, 44_000, 2_500);
        ex.gtc(K2, Side::Buy, 42_000, 1_875);
    }
    ex
}

/// Oracle path: each update is within 5% of the previous index.
const DROPS: [u64; 4] = [47_500, 45_126, 42_870, 40_727];

#[derive(Clone, Copy)]
struct Snap {
    coll: i128,
    fund: i128,
    size: i64,
    basis: i128,
}

fn snap(ex: &Ex, who: AccountId) -> Snap {
    let p = ex.engine.position(who, M).unwrap_or_default();
    Snap {
        coll: ex.engine.collateral(who).unwrap().0,
        fund: ex.engine.insurance_fund().0,
        size: p.size,
        basis: p.basis,
    }
}

fn paid_to(outs: &[Output], who: AccountId) -> i128 {
    outs.iter()
        .filter_map(|o| match o {
            Output::InsurancePaid { account, amount } if *account == who => Some(amount.0),
            _ => None,
        })
        .sum()
}

/// Shortfall the fund should have covered for a long `who` whose liquidation
/// fills are in `outs`: loss of the closed part beyond the collateral held.
fn realised_shortfall(before: Snap, outs: &[Output], who: AccountId) -> (i128, i128) {
    let (mut proceeds, mut closed, mut fees) = (0i128, 0u64, 0i128);
    for o in outs {
        if let Output::Fill {
            taker,
            price,
            qty,
            taker_fee,
            liquidation: true,
            ..
        } = o
        {
            if *taker == who {
                proceeds += i128::from(price.0) * i128::from(qty.0) / i128::from(QTY_SCALE);
                closed += qty.0;
                fees += taker_fee.0;
            }
        }
    }
    if closed == 0 {
        return (0, 0);
    }
    let abs = i128::from(before.size.unsigned_abs());
    let released = (before.basis * i128::from(closed) + abs - 1) / abs;
    let after_fills = before.coll + proceeds - released - fees;
    ((-after_fills).max(0), fees)
}

/// The rules of a payout, checked after one event for the long `who`.
fn check_payout(before: Snap, ex: &Ex, outs: &[Output], who: AccountId, what: &str) -> i128 {
    let paid = paid_to(outs, who);
    let (shortfall, fees) = realised_shortfall(before, outs, who);
    let now = snap(ex, who);
    assert!(now.coll >= 0, "{what}: collateral negative: {}", now.coll);
    assert!(
        (paid - shortfall).abs() <= 1_000,
        "{what}: fund paid {paid} but the realised shortfall of the closed chunk is {shortfall}"
    );
    if paid > 0 {
        assert_eq!(
            now.coll, 0,
            "{what}: collateral {} after a payout (unrealised loss was credited)",
            now.coll
        );
    }
    assert_eq!(
        now.fund,
        before.fund - paid + fees,
        "{what}: fund moved by {} but payout was {paid} and liquidation fees {fees}",
        now.fund - before.fund
    );
    paid
}

#[test]
fn money002_ladder_fund_pays_realised_shortfall_only() {
    let mut ex = ladder(true);
    let mut payouts = Vec::new();
    for (i, price) in DROPS[..3].iter().enumerate() {
        let before = snap(&ex, L);
        let outs = ex.oracle_on(M, *price);
        let what = format!("oracle step {} to {price}", i + 1);
        let paid = check_payout(before, &ex, &outs, L, &what);
        payouts.push(paid);
        assert!(ex.engine.bad_debt().0 == 0, "{what}: bad debt");
        ex.check();
    }
    // Step 2 sells 2.5 base at 44 000: loss 15 000 < collateral, no payout.
    assert_eq!(payouts[1], 0, "step 2 realised no shortfall: {payouts:?}");
    // Step 3 sells 1.875 base at 42 000: loss 15 000 > the 10 001 left.
    assert!(payouts[2] > 4_000 * 1_000_000, "step 3 payout: {payouts:?}");
    let s = snap(&ex, L);
    assert_eq!(s.coll, 0);
    assert!(s.size > 0, "position must still be open");
    assert!(
        ex.engine.equity(L).unwrap().0 < 0,
        "equity of the open leveraged position must stay negative, not be credited to zero"
    );
}

#[test]
fn money002_no_fills_means_no_payout_and_no_credit() {
    let mut ex = ladder(false);
    let c0 = ex.engine.collateral(L).unwrap().0;
    let fund0 = ex.engine.insurance_fund().0;
    for (i, price) in DROPS.iter().enumerate() {
        let outs = ex.oracle_on(M, *price);
        let what = format!("oracle step {} to {price}", i + 1);
        assert_eq!(paid_to(&outs, L), 0, "{what}: fund paid without any fill");
        assert_eq!(ex.engine.insurance_fund().0, fund0, "{what}: fund changed");
        assert_eq!(
            ex.engine.collateral(L).unwrap().0,
            c0,
            "{what}: collateral credited"
        );
        ex.check();
    }
    assert!(ex.engine.equity(L).unwrap().0 < 0);
    assert!(ex.size(L) > 0);
}

#[test]
fn money002_recovery_does_not_charge_the_fund_again() {
    let mut ex = ladder(true);
    for price in &DROPS[..3] {
        ex.oracle_on(M, *price);
    }
    let f3 = ex.engine.insurance_fund().0;
    // +2% twice, then idle ticks: no book liquidity is left, nothing is realised.
    for price in [43_800u64, 44_700] {
        let outs = ex.oracle_on(M, price);
        assert_eq!(paid_to(&outs, L), 0, "recovery to {price} charged the fund");
        assert_eq!(ex.engine.insurance_fund().0, f3, "fund moved at {price}");
        assert!(ex.engine.collateral(L).unwrap().0 >= 0);
    }
    for _ in 0..3 {
        let outs = ex.apply(EventBody::Tick).unwrap();
        assert_eq!(paid_to(&outs, L), 0);
        assert_eq!(ex.engine.insurance_fund().0, f3);
    }
    ex.check();
}

fn traffic_with_crashes(seed: u64) {
    let cfg = TrafficConfig {
        seed,
        accounts: 40,
        markets: 2,
        oracle_every: 20,
        tick_every: 40,
    };
    let mut gen = TrafficGenerator::new(cfg);
    let mut engine = Engine::new();
    for ev in gen.bootstrap() {
        engine.apply(&ev).expect("bootstrap event");
    }
    let mut rng = Rng::new(seed ^ 0x00C0_FFEE);
    // Degenerate accounts: minimal collateral, near-maximum leverage.
    for a in 1_001..=1_010u64 {
        let ts = engine.clock() + 1;
        let dep = Event {
            ts,
            body: EventBody::Deposit {
                account: a,
                amount: usd(1_000),
            },
        };
        engine.apply(&dep).expect("degenerate deposit");
    }
    let mut client = 9_000_000u64;
    let (mut crash_left, mut crash_market, mut crash_down) = (0u32, 1 as MarketId, true);
    for i in 0..10_000u32 {
        if i % 150 == 0 {
            crash_left = u32::try_from(rng.range(5, 12)).unwrap();
            crash_market = MarketId::try_from(rng.range(1, 2)).unwrap();
            crash_down = rng.chance(2, 3);
        }
        let ts = engine.clock() + 1;
        let ev = if rng.chance(1, 8) {
            client += 1;
            let market = MarketId::try_from(rng.range(1, 2)).unwrap();
            Event {
                ts,
                body: EventBody::Place(PlaceOrder {
                    market,
                    account: rng.range(1_001, 1_010),
                    side: if rng.chance(1, 2) {
                        Side::Buy
                    } else {
                        Side::Sell
                    },
                    kind: OrderKind::Market {
                        max_slippage_bps: 200,
                    },
                    qty: Qty(rng.range(100, 450) * 10),
                    client_id: client,
                    reduce_only: false,
                    stp: StpMode::CancelNewest,
                }),
            }
        } else if crash_left > 0 {
            crash_left -= 1;
            let idx = engine
                .index_price(crash_market)
                .map_or(START_PRICE, |p| p.0);
            let next = if crash_down {
                idx / 100 * 95
            } else {
                idx / 100 * 105
            };
            Event {
                ts,
                body: EventBody::Oracle {
                    market: crash_market,
                    index_price: Price(next.clamp(1_000_000, 1_000_000_000_000)),
                    source_ts: ts,
                },
            }
        } else {
            let mut ev = gen.next_event(&engine);
            ev.ts = ev.ts.max(ts);
            let t = ev.ts;
            if let EventBody::Oracle { source_ts, .. } = &mut ev.body {
                *source_ts = t;
            }
            ev
        };
        let outs = engine.apply(&ev).unwrap_or_default();
        let ctx = format!("seed {seed} event {i}");
        assert!(
            !engine.is_halted(),
            "{ctx}: halted: {:?}",
            engine.halt_reason()
        );
        engine
            .check_invariants()
            .unwrap_or_else(|e| panic!("{ctx}: invariant violated: {e}"));
        for acct in engine.account_ids() {
            if engine.is_pending_liquidation(acct) {
                continue;
            }
            let c = engine.collateral(acct).unwrap().0;
            if c < 0 {
                // Cross margin: collateral may be negative while unrealised
                // gains in another market keep the account solvent. All oracles
                // are fresh here, so there is no stale-position case.
                let holds = [1, 2].iter().any(|m| engine.position(acct, *m).is_some());
                let eq = engine.equity(acct).unwrap().0;
                assert!(
                    holds && eq >= 0,
                    "{ctx}: account {acct} collateral {c} < 0 with equity {eq} (positions: {holds})"
                );
            }
        }
        for (k, o) in outs.iter().enumerate() {
            let Output::InsurancePaid { account, amount } = o else {
                continue;
            };
            let later = outs[k + 1..].iter().any(|x| match x {
                Output::Fill { taker, maker, .. } => taker == account || maker == account,
                Output::Deleveraged {
                    bankrupt,
                    counterparty,
                    ..
                } => bankrupt == account || counterparty == account,
                _ => false,
            });
            if !later {
                let c = engine.collateral(*account).unwrap().0;
                assert_eq!(
                    c, 0,
                    "{ctx}: account {account} has collateral {c} after the fund paid {}",
                    amount.0
                );
            }
        }
    }
}

#[test]
fn money002_traffic_with_crashes_seed_1() {
    traffic_with_crashes(1);
}

#[test]
fn money002_traffic_with_crashes_seed_2() {
    traffic_with_crashes(2);
}

#[test]
fn money002_traffic_with_crashes_seed_3() {
    traffic_with_crashes(77);
}

// ---------------------------------------------------------------------------
// DOS-002: liquidation budget starvation
// ---------------------------------------------------------------------------

const VICTIM: AccountId = 500;

/// `decoys` low-id accounts hold small shorts in market 1 (no asks, so a
/// liquidation can never fill); the victim holds a long in market 2 with bids
/// available. Market 1 rises 5%, market 2 falls 5%.
fn dos_setup(decoys: u64, budget: u32) -> (Ex, i64) {
    let mut ex = Ex::bare_with(EngineConfig {
        max_liquidations_per_event: budget,
        ..EngineConfig::default()
    });
    ex.apply(EventBody::AddMarket(MarketConfig::example(1)))
        .unwrap();
    ex.apply(EventBody::AddMarket(MarketConfig::example(2)))
        .unwrap();
    ex.oracle_on(1, MARK);
    ex.oracle_on(2, MARK);
    ex.apply(EventBody::FundInsurance {
        amount: usd(100_000),
    })
    .unwrap();
    let (h, sv, bv) = (1_000, 1_001, 1_002);
    ex.fund(&[h, sv, bv], 2_000_000);
    ex.gtc(h, Side::Buy, MARK, 200 * decoys);
    for d in 1..=decoys {
        ex.deposit(d, 520);
        let outs = place_in(&mut ex, 1, d, Side::Sell, MARK, 200, Tif::Ioc).unwrap();
        assert_eq!(filled_qty(&outs), qty(200).0, "decoy {d} short");
    }
    ex.deposit(VICTIM, 25_300);
    place_in(&mut ex, 2, sv, Side::Sell, MARK, 10_000, Tif::Gtc).unwrap();
    let outs = place_in(&mut ex, 2, VICTIM, Side::Buy, MARK, 10_000, Tif::Ioc).unwrap();
    assert_eq!(filled_qty(&outs), qty(10_000).0, "victim long");
    // Inside the price band of the liquidation mark (47 500 +- 2.5%), so the
    // oracle sweep keeps it.
    place_in(&mut ex, 2, bv, Side::Buy, 48_000, 20_000, Tif::Gtc).unwrap();
    let initial = ex.engine.position(VICTIM, 2).unwrap().size;

    ex.oracle_on(1, 52_500);
    ex.oracle_on(2, 47_500);
    (ex, initial)
}

fn victim_started(outs: &[Output]) -> bool {
    has(
        outs,
        |o| matches!(o, Output::LiquidationStarted { account, .. } if *account == VICTIM),
    )
}

#[test]
fn dos002_decoys_cannot_delay_victim_beyond_queue_over_budget() {
    let budget = 8u32;
    let (mut ex, initial) = dos_setup(64, budget);
    // The oracle event that made the victim liquidatable already ran; count
    // what is queued now (the decoys are still pending).
    let pending = ex
        .engine
        .account_ids()
        .into_iter()
        .filter(|a| ex.engine.is_pending_liquidation(*a))
        .count() as u64;
    let bound = pending.div_ceil(u64::from(budget)) + 1;
    let mut started = ex.engine.position(VICTIM, 2).unwrap().size != initial;
    for _ in 0..bound {
        if started {
            break;
        }
        let outs = ex.apply(EventBody::Tick).unwrap();
        started = victim_started(&outs);
    }
    assert!(
        started,
        "victim not liquidated within {bound} ticks ({pending} pending, budget {budget})"
    );
    assert!(
        ex.engine.position(VICTIM, 2).unwrap().size < initial,
        "victim position did not shrink"
    );
    ex.check();
}

#[test]
fn dos002_victim_is_never_starved_forever() {
    let (mut ex, initial) = dos_setup(64, 8);
    let mut started = false;
    for _ in 0..40 {
        let outs = ex.apply(EventBody::Tick).unwrap();
        started |= victim_started(&outs);
    }
    assert!(started, "no LiquidationStarted for the victim in 40 ticks");
    assert!(ex.engine.position(VICTIM, 2).map_or(0, |p| p.size) < initial);
    assert!(!ex.engine.is_halted());
    ex.check();
}

/// Control: fewer decoys than the budget cannot starve anyone.
#[test]
fn dos002_control_few_decoys_victim_liquidated_at_once() {
    let (mut ex, initial) = dos_setup(4, 8);
    let mut started = ex.engine.position(VICTIM, 2).unwrap().size != initial;
    if !started {
        started = victim_started(&ex.apply(EventBody::Tick).unwrap());
    }
    assert!(started, "victim not liquidated with only 4 decoys");
    ex.check();
}

// ---------------------------------------------------------------------------
// DOS-003(a): a stale oracle must not shield other markets
// ---------------------------------------------------------------------------

const X: AccountId = 1;

/// X holds 10 base long in market 1 and 0.002 base long in market 2. After the
/// setup market 2's oracle goes stale (no updates for 65 s) while market 1
/// keeps updating at its initial price.
fn shield_setup() -> Ex {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(1)))
        .unwrap();
    ex.apply(EventBody::AddMarket(MarketConfig::example(2)))
        .unwrap();
    ex.oracle_on(1, MARK);
    ex.oracle_on(2, MARK);
    ex.apply(EventBody::FundInsurance {
        amount: usd(100_000),
    })
    .unwrap();
    ex.fund(&[2, 3, 4], 200_000);
    ex.deposit(X, 26_000);
    place_in(&mut ex, 1, 2, Side::Sell, MARK, 10_000, Tif::Gtc).unwrap();
    let o = place_in(&mut ex, 1, X, Side::Buy, MARK, 10_000, Tif::Ioc).unwrap();
    assert_eq!(filled_qty(&o), qty(10_000).0);
    place_in(&mut ex, 2, 3, Side::Sell, MARK, 2, Tif::Gtc).unwrap();
    let o = place_in(&mut ex, 2, X, Side::Buy, MARK, 2, Tif::Ioc).unwrap();
    assert_eq!(filled_qty(&o), qty(2).0);
    // Inside the price band of the post-drop mark (47 500 +- 2.5%).
    place_in(&mut ex, 1, 4, Side::Buy, 48_000, 20_000, Tif::Gtc).unwrap();
    for _ in 0..13 {
        ex.advance(5_000);
        ex.oracle_on(1, MARK);
    }
    ex
}

fn x_started(outs: &[Output]) -> bool {
    has(
        outs,
        |o| matches!(o, Output::LiquidationStarted { account, .. } if *account == X),
    )
}

#[test]
fn dos003_stale_tiny_position_does_not_shield_large_loss() {
    let mut ex = shield_setup();
    let initial = ex.engine.position(X, 1).unwrap().size;
    let mut started = x_started(&ex.oracle_on(1, 47_500));
    for _ in 0..3 {
        started |= x_started(&ex.apply(EventBody::Tick).unwrap());
    }
    assert!(
        started,
        "account below maintenance in the fresh market was never liquidated"
    );
    assert!(ex.engine.position(X, 1).unwrap().size < initial);
    assert!(!ex.engine.is_halted(), "{:?}", ex.engine.halt_reason());
    ex.check();
}

/// Converse: with the fresh part healthy the stale tiny position triggers
/// nothing and nothing breaks.
#[test]
fn dos003_control_healthy_fresh_part_is_left_alone() {
    let mut ex = shield_setup();
    let initial = ex.engine.position(X, 1).unwrap().size;
    let mut started = x_started(&ex.oracle_on(1, MARK));
    for _ in 0..3 {
        started |= x_started(&ex.apply(EventBody::Tick).unwrap());
    }
    assert!(!started, "healthy account was liquidated");
    assert_eq!(ex.engine.position(X, 1).unwrap().size, initial);
    assert!(!ex.engine.is_halted());
    ex.check();
}
