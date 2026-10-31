//! Domain tests: fairness and partial-oracle rules of the liquidation queue.
//!
//! * the queue is served round-robin from a persistent cursor (DOS-002);
//! * an account is assessed on its markets with a fresh mark only, the step
//!   works on the lowest fresh market, and the insurance fund pays nothing while
//!   a stale position remains (DOS-003);
//! * auto-deleveraging pays `min(deficit of the position, fund)` and closes the
//!   position of that market only;
//! * over a seeded wild-oracle run the fund only ever pays realised shortfalls.
//!
//! Scenario numbers are derived by hand from the documented rules in
//! `engine/risk.rs`; the usual setup is: account 1 is the victim, account 2
//! ("SELLER") takes the other side of its trades, account 3 ("BIDDER") rests
//! the bids that provide liquidation liquidity.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod common;
use common::*;
use perp_domain::codec::{to_bytes, Reader, Writer};
use perp_domain::engine::Engine;
use perp_domain::fixed::{Money, Price};
use perp_domain::rng::Rng;
use perp_domain::traffic::{TrafficConfig, TrafficGenerator};
use perp_domain::types::{
    EngineConfig, Event, EventBody, MarketConfig, OrderKind, Output, Side, Tif,
};

const VICTIM: u64 = 1;
const SELLER: u64 = 2;
const BIDDER: u64 = 3;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn enc(e: &Engine) -> Vec<u8> {
    let mut w = Writer::new_buf();
    e.encode_state(&mut w);
    w.into_bytes()
}

fn dec(bytes: &[u8]) -> Result<Engine, perp_domain::codec::DecodeError> {
    let mut r = Reader::new(bytes);
    let e = Engine::decode_state(&mut r)?;
    r.finish()?;
    Ok(e)
}

/// Position of the pending-liquidation count: version, config flag, config,
/// then clock, counter, two id counters, four i128 balances, halt code and
/// three u64 statistics (121 bytes).
fn pending_count_at(e: &Engine) -> usize {
    2 + to_bytes(e.config().unwrap()).len() + 121
}

/// Offset of the round-robin cursor: it follows the pending id list.
fn cursor_offset(e: &Engine, bytes: &[u8]) -> usize {
    let at = pending_count_at(e);
    let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    at + 4 + 8 * n
}

fn cursor_of(e: &Engine) -> u64 {
    let bytes = enc(e);
    let at = cursor_offset(e, &bytes);
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn started(outs: &[Output]) -> Vec<u64> {
    outs.iter()
        .filter_map(|o| match o {
            Output::LiquidationStarted { account, .. } => Some(*account),
            _ => None,
        })
        .collect()
}

fn started_equity(outs: &[Output], account: u64) -> Vec<i128> {
    outs.iter()
        .filter_map(|o| match o {
            Output::LiquidationStarted { account: a, equity } if *a == account => Some(equity.0),
            _ => None,
        })
        .collect()
}

/// `(market, price, qty)` of liquidation fills.
fn liq_fills(outs: &[Output]) -> Vec<(u32, u64, u64)> {
    outs.iter()
        .filter_map(|o| match o {
            Output::Fill {
                market,
                price,
                qty,
                liquidation: true,
                ..
            } => Some((*market, price.0, qty.0)),
            _ => None,
        })
        .collect()
}

fn paid(outs: &[Output]) -> Vec<(u64, i128)> {
    outs.iter()
        .filter_map(|o| match o {
            Output::InsurancePaid { account, amount } => Some((*account, amount.0)),
            _ => None,
        })
        .collect()
}

fn adl(outs: &[Output]) -> Vec<(u32, u64, u64, u64, u64)> {
    outs.iter()
        .filter_map(|o| match o {
            Output::Deleveraged {
                market,
                bankrupt,
                counterparty,
                price,
                qty,
            } => Some((*market, *bankrupt, *counterparty, price.0, qty.0)),
            _ => None,
        })
        .collect()
}

fn identity(ex: &Ex) {
    let e = &ex.engine;
    assert_eq!(
        e.total_collateral().0 + e.insurance_fund().0 + e.fee_pool().0 - e.total_basis().0,
        e.net_deposits().0 + e.bad_debt().0,
        "money identity"
    );
    ex.check();
}

fn oracle_raw_on(ex: &mut Ex, market: u32, raw: u64) -> Vec<Output> {
    let source_ts = ex.ts + 1;
    ex.apply(EventBody::Oracle {
        market,
        index_price: Price(raw),
        source_ts,
    })
    .unwrap()
}

fn tick(ex: &mut Ex) -> Vec<Output> {
    ex.apply(EventBody::Tick).unwrap()
}

/// Limit order in an arbitrary market.
fn limit_in(
    ex: &mut Ex,
    market: u32,
    account: u64,
    side: Side,
    price: u64,
    milli: u64,
    tif: Tif,
) -> Vec<Output> {
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
    ex.place(p).unwrap()
}

/// `taker` crosses a resting order of `maker`: `taker` trades `side`.
fn trade_in(ex: &mut Ex, market: u32, taker: u64, maker: u64, side: Side, price: u64, milli: u64) {
    limit_in(ex, market, maker, side.opposite(), price, milli, Tif::Gtc);
    let outs = limit_in(ex, market, taker, side, price, milli, Tif::Ioc);
    assert!(
        outs.iter().any(|o| matches!(o, Output::Fill { .. })),
        "trade helper expected a fill"
    );
}

/// Engine with `max_liquidations_per_event = budget`, one market at 50 000.
fn budget_engine(budget: u32) -> Ex {
    let mut ex = Ex::bare();
    ex.engine = Engine::new();
    ex.ts = 1_000;
    ex.apply(EventBody::Configure(EngineConfig {
        max_liquidations_per_event: budget,
        ..EngineConfig::default()
    }))
    .unwrap();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    ex.oracle_on(M, 50_000);
    ex
}

/// `victims` are long 1.0 at 50 000 against SELLER with 2975 collateral;
/// BIDDER rests 10 base of bids at 47 300 (inside the liquidation limit price
/// once the mark is 47 500, which puts every victim at equity 475).
fn crowd(budget: u32, victims: &[u64]) -> Ex {
    let mut ex = budget_engine(budget);
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    for v in victims {
        ex.deposit(*v, 3_000);
        ex.trade(*v, SELLER, Side::Buy, 50_000, 1_000);
    }
    ex.gtc(BIDDER, Side::Buy, 47_300, 10_000);
    ex
}

fn two_markets() -> Ex {
    let mut ex = Ex::new();
    ex.apply(EventBody::AddMarket(MarketConfig::example(2)))
        .unwrap();
    ex.oracle_on(2, 50_000);
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    ex
}

// ---------------------------------------------------------------------------
// round-robin cursor
// ---------------------------------------------------------------------------

#[test]
fn queue_is_served_round_robin_not_lowest_id_first() {
    let mut ex = crowd(1, &[11, 12, 13]);
    // every victim stays liquidatable for several chunks, so lowest-id-first
    // would serve 11 again and again
    let mut served = Vec::new();
    served.push(started(&ex.oracle(47_500).unwrap()));
    for _ in 0..5 {
        served.push(started(&tick(&mut ex)));
    }
    assert_eq!(
        served,
        vec![vec![11], vec![12], vec![13], vec![11], vec![12], vec![13]]
    );
    assert_eq!(ex.engine.summary().liquidation_steps, 6);
    ex.check();
}

#[test]
fn cursor_wraps_around_to_the_lowest_pending_account() {
    let mut ex = crowd(1, &[11, 12, 13]);
    // the setup trades already wrapped the cursor: it sits behind account 2
    assert_eq!(cursor_of(&ex.engine), 3);
    ex.oracle(47_500).unwrap(); // serves 11
    assert_eq!(cursor_of(&ex.engine), 12);
    assert!(
        ex.engine.is_pending_liquidation(SELLER),
        "behind the cursor"
    );
    tick(&mut ex); // serves 12
    assert_eq!(cursor_of(&ex.engine), 13);
    // serves 13, finds nobody at or after 14, wraps to the lowest ids (2 and 3,
    // healthy and dropped), then runs into the budget at 11 and parks there
    assert_eq!(started(&tick(&mut ex)), vec![13]);
    assert_eq!(cursor_of(&ex.engine), 11);
    assert!(!ex.engine.is_pending_liquidation(SELLER));
    assert!(!ex.engine.is_pending_liquidation(BIDDER));
    assert_eq!(started(&tick(&mut ex)), vec![11]);
    ex.check();
}

#[test]
fn budget_exhaustion_requeues_the_unserved_account_at_the_cursor() {
    let mut ex = crowd(1, &[11, 12, 13]);
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(started(&outs), vec![11]);
    // 11 was reduced (and is still underwater): queued again; 12 and 13 never
    // got their step this event and stay queued; the cursor parks at 12.
    // Healthy accounts behind the cursor (2: SELLER, 3: BIDDER, which became a
    // holder by being hit) stay queued until the cursor wraps around to them.
    for v in [11, 12, 13] {
        assert!(ex.engine.is_pending_liquidation(v), "account {v}");
    }
    assert!(ex.engine.is_pending_liquidation(SELLER));
    assert!(ex.engine.is_pending_liquidation(BIDDER));
    assert_eq!(cursor_of(&ex.engine), 12);
    assert_eq!(ex.engine.position(12, M).unwrap().size, 10_000);
    assert_eq!(ex.engine.position(13, M).unwrap().size, 10_000);
    assert!(ex.engine.position(11, M).unwrap().size < 10_000);
    ex.check();
}

#[test]
fn every_pending_account_is_served_within_ceil_pending_over_budget_events() {
    // 5 liquidatable accounts, budget 2: ceil(5 / 2) = 3 events
    let victims = [11, 12, 13, 14, 15];
    let mut ex = crowd(2, &victims);
    let mut seen = std::collections::BTreeSet::new();
    let mut per_event = Vec::new();
    for i in 0..3 {
        let outs = if i == 0 {
            ex.oracle(47_500).unwrap()
        } else {
            tick(&mut ex)
        };
        let s = started(&outs);
        assert!(s.len() <= 2, "budget of 2 steps per event");
        seen.extend(s.iter().copied());
        per_event.push(s);
    }
    assert_eq!(
        per_event,
        vec![vec![11, 12], vec![13, 14], vec![15, 11]],
        "round-robin order"
    );
    assert_eq!(seen, victims.iter().copied().collect());
    ex.check();
}

#[test]
fn stuck_accounts_cannot_starve_a_serviceable_account() {
    // Accounts 11 and 12 hold market-1 positions and nobody quotes market 1:
    // their steps use budget without progress. Account 13 trades market 2,
    // which has bids. With one step per event a lowest-id-first queue would
    // never reach 13.
    let mut ex = two_markets();
    for v in [11u64, 12] {
        ex.deposit(v, 3_000);
        trade_in(&mut ex, 1, v, SELLER, Side::Buy, 50_000, 1_000);
    }
    ex.deposit(13, 3_000);
    trade_in(&mut ex, 2, 13, SELLER, Side::Buy, 50_000, 1_000);
    limit_in(&mut ex, 2, BIDDER, Side::Buy, 47_300, 10_000, Tif::Gtc);
    ex.oracle_on(1, 47_500);
    ex.oracle_on(2, 47_500);
    for _ in 0..6 {
        tick(&mut ex);
    }
    assert!(ex.engine.position(13, 2).unwrap().size < 10_000);
    assert_eq!(ex.engine.position(11, 1).unwrap().size, 10_000);
    ex.check();
}

#[test]
fn cursor_survives_a_snapshot_round_trip_and_decides_the_next_victim() {
    let mut ex = crowd(1, &[11, 12, 13]);
    ex.oracle(47_500).unwrap();
    assert_eq!(cursor_of(&ex.engine), 12);
    let bytes = enc(&ex.engine);
    let mut restored = dec(&bytes).unwrap();
    assert_eq!(cursor_of(&restored), 12);
    assert_eq!(restored.state_hash(), ex.engine.state_hash());
    let ev = Event {
        ts: ex.ts + 1,
        body: EventBody::Tick,
    };
    let live_outs = ex.engine.apply(&ev).unwrap();
    let restored_outs = restored.apply(&ev).unwrap();
    assert_eq!(live_outs, restored_outs);
    assert_eq!(started(&restored_outs), vec![12], "the cursor was kept");
    assert_eq!(restored.state_hash(), ex.engine.state_hash());
    assert_eq!(cursor_of(&restored), 13);
}

#[test]
fn cursor_is_part_of_the_state_hash_and_of_behaviour() {
    let mut ex = crowd(1, &[11, 12, 13]);
    ex.oracle(47_500).unwrap();
    let bytes = enc(&ex.engine);
    let at = cursor_offset(&ex.engine, &bytes);
    let mut patched = bytes.clone();
    patched[at..at + 8].copy_from_slice(&13u64.to_le_bytes());
    // any cursor value is a valid snapshot, but it is a different state ...
    let mut other = dec(&patched).unwrap();
    other.check_invariants().unwrap();
    assert_ne!(other.state_hash(), ex.engine.state_hash());
    assert_ne!(other.content_hash(), ex.engine.content_hash());
    // ... that serves a different account next
    let ev = Event {
        ts: ex.ts + 1,
        body: EventBody::Tick,
    };
    assert_eq!(started(&other.apply(&ev).unwrap()), vec![13]);
    assert_eq!(started(&ex.engine.apply(&ev).unwrap()), vec![12]);
    // re-encoding the patched state reproduces the patched bytes
    assert_eq!(enc(&dec(&patched).unwrap()), patched);
}

// ---------------------------------------------------------------------------
// partial staleness: assess the fresh part only
// ---------------------------------------------------------------------------

/// Victim: long 1.0 in market 1 bought at 49 000 (+1000 at the stale mark of
/// 50 000) and long 1.0 in market 2 at 50 000; deposit 5 300 -> collateral
/// 5250.5. Market 1 then goes stale (nobody updates it for 61 s) while market 2
/// is refreshed. BIDDER rests 3 base at 45 000 in market 2.
fn stale_market_one() -> Ex {
    let mut ex = two_markets();
    ex.deposit(VICTIM, 5_300);
    trade_in(&mut ex, 1, VICTIM, SELLER, Side::Buy, 49_000, 1_000);
    trade_in(&mut ex, 2, VICTIM, SELLER, Side::Buy, 50_000, 1_000);
    assert_eq!(ex.collateral(VICTIM), 5_250_500_000);
    limit_in(&mut ex, 2, BIDDER, Side::Buy, 45_000, 3_000, Tif::Gtc);
    ex.advance(61_000);
    ex.oracle_on(2, 50_000); // refreshes market 2 only
    ex
}

#[test]
fn a_stale_market_neither_shields_nor_blocks_the_fresh_part() {
    let mut ex = stale_market_one();
    // fresh part: equity 5250.5 - 2500 = 2750.5 >= maintenance 1187.5
    let outs = ex.oracle_on(2, 47_500);
    assert!(started(&outs).is_empty());
    // fresh part: equity 5250.5 - 4875 = 375.5 < maintenance 1128.125. The +1000
    // of the stale market is not counted (nor its 1250 of maintenance).
    let outs = ex.oracle_on(2, 45_125);
    assert_eq!(started_equity(&outs, VICTIM)[0], 375_500_000);
    let fills = liq_fills(&outs);
    assert!(!fills.is_empty());
    assert!(fills.iter().all(|f| f.0 == 2), "only the fresh market");
    // solvent limit: mark - (375.5 - dust - 0.2 % * 45 125) = 44 839.750512
    assert_eq!((fills[0].1, fills[0].2), (px(45_000).0, 2_500));
    // the stale position is untouched
    assert_eq!(ex.engine.position(VICTIM, 1).unwrap().size, 10_000);
    assert!(paid(&outs).is_empty());
    identity(&ex);
}

#[test]
fn account_view_still_values_a_stale_market_at_its_last_mark() {
    // The informational equity includes the stale market (+1000 at its last
    // mark); the liquidation decision above does not.
    let mut ex = stale_market_one();
    ex.oracle_on(2, 47_500);
    let v = ex.engine.account_view(VICTIM).unwrap();
    assert_eq!(v.equity, Money(3_750_500_000));
    ex.oracle_on(2, 45_125);
    assert_eq!(ex.engine.position(VICTIM, 1).unwrap().size, 10_000);
}

#[test]
fn step_works_on_the_lowest_fresh_market_not_the_lowest_market() {
    // positions in markets 1 (stale), 2 and 3 (fresh); deposit 8 000
    let mut ex = two_markets();
    ex.apply(EventBody::AddMarket(MarketConfig::example(3)))
        .unwrap();
    ex.oracle_on(3, 50_000);
    ex.deposit(VICTIM, 8_000);
    for m in [1, 2, 3] {
        trade_in(&mut ex, m, VICTIM, SELLER, Side::Buy, 50_000, 1_000);
        limit_in(&mut ex, m, BIDDER, Side::Buy, 45_000, 3_000, Tif::Gtc);
    }
    ex.advance(61_000);
    ex.oracle_on(2, 50_000);
    ex.oracle_on(3, 50_000);
    // market 2 falls to 45 125, market 3 to 47 500: fresh equity
    // 7925 - 4875 - 2500 = 550 < maintenance 2315.625
    ex.oracle_on(3, 47_500);
    ex.oracle_on(2, 47_500);
    let outs = ex.oracle_on(2, 45_125);
    assert_eq!(started_equity(&outs, VICTIM)[0], 550_000_000);
    let fills = liq_fills(&outs);
    assert_eq!((fills[0].0, fills[0].2), (2, 2_500), "market 2 first");
    assert!(fills.iter().all(|f| f.0 != 1), "never the stale market");
    assert_eq!(ex.engine.position(VICTIM, 1).unwrap().size, 10_000);
    identity(&ex);
}

#[test]
fn fully_stale_account_is_not_liquidated_and_not_queued() {
    let mut ex = two_markets();
    ex.deposit(VICTIM, 3_000);
    trade_in(&mut ex, 1, VICTIM, SELLER, Side::Buy, 50_000, 1_000);
    limit_in(&mut ex, 1, BIDDER, Side::Buy, 45_000, 3_000, Tif::Gtc);
    ex.oracle_on(1, 47_500); // equity 475: stuck (bid below limit)
    ex.advance(61_000);
    ex.oracle_on(2, 50_000); // market 1 goes stale
    let outs = tick(&mut ex);
    assert!(started(&outs).is_empty());
    assert!(!ex.engine.is_pending_liquidation(VICTIM));
    ex.check();
}

#[test]
fn no_insurance_payout_while_a_stale_position_remains() {
    // victim long 1.0 in market 1 (bid 49 500 rests there) and in market 2;
    // collateral 5250. Market 1 goes stale; market 2 is driven to 42 868.75
    // (equity -1881.25) with no bids, then 2.0 base of bids appear at 43 000.
    let mut ex = two_markets();
    ex.apply(EventBody::FundInsurance {
        amount: usd(10_000),
    })
    .unwrap();
    ex.deposit(VICTIM, 5_300);
    trade_in(&mut ex, 1, VICTIM, SELLER, Side::Buy, 50_000, 1_000);
    trade_in(&mut ex, 2, VICTIM, SELLER, Side::Buy, 50_000, 1_000);
    ex.deposit(4, 1_000_000);
    limit_in(&mut ex, 1, 4, Side::Buy, 49_500, 3_000, Tif::Gtc);
    ex.advance(61_000);
    ex.oracle_on(2, 50_000);
    ex.oracle_on(2, 47_500);
    ex.oracle_on(2, 45_125);
    let outs = oracle_raw_on(&mut ex, 2, 42_868_750_000);
    assert_eq!(started_equity(&outs, VICTIM)[0], -1_881_250_000);
    assert!(liq_fills(&outs).is_empty(), "no bids yet");
    limit_in(&mut ex, 2, BIDDER, Side::Buy, 43_000, 2_000, Tif::Gtc);
    let outs = tick(&mut ex);
    // the whole market-2 position is sold at 43 000 (-7000 on 5250 collateral)
    let fills = liq_fills(&outs);
    assert!(fills.iter().all(|f| f.0 == 2 && f.1 == px(43_000).0));
    assert_eq!(fills.iter().map(|f| f.2).sum::<u64>(), 10_000);
    assert_eq!(ex.engine.position(VICTIM, 2), None);
    // collateral is negative, but the stale market-1 position is still open:
    // its PnL is unknown, so the fund pays nothing
    assert_eq!(ex.collateral(VICTIM), usd(-1_750).0);
    assert!(paid(&outs).is_empty());
    assert_eq!(ex.engine.insurance_fund(), usd(10_000));
    assert_eq!(ex.engine.bad_debt(), Money(0));
    assert_eq!(ex.engine.position(VICTIM, 1).unwrap().size, 10_000);
    identity(&ex);

    // market 1 is refreshed: the account is insolvent again, the next chunk
    // (2500 raw sold at 49 500: -125) takes the collateral to -1875 and the
    // fund now pays exactly that realised shortfall
    let outs = ex.oracle_on(1, 50_000);
    assert_eq!(started_equity(&outs, VICTIM)[0], usd(-1_750).0);
    let fills = liq_fills(&outs);
    assert_eq!(
        (fills[0].0, fills[0].1, fills[0].2),
        (1, px(49_500).0, 2_500)
    );
    assert_eq!(paid(&outs)[0], (VICTIM, usd(1_875).0));
    assert_eq!(
        ex.engine.bad_debt(),
        Money(0),
        "covered by the fund, not bad debt"
    );
    identity(&ex);
}

// ---------------------------------------------------------------------------
// ADL: min(deficit of the position, fund), one market only
// ---------------------------------------------------------------------------

/// Victim long 1.0 in market 1 (entry 50 000) and short 1.0 in market 2 (entry
/// 50 000); deposit 5 300 -> collateral 5250. Market 2 falls to 47 500 (+2500),
/// market 1 to 40 725.3125 in four 5 % steps with nobody quoting market 1:
/// equity 5250 - 9274.6875 + 2500 = -1524.6875, deficit of the market-1
/// position alone: 4024.6875.
fn adl_two_markets(fund: i128) -> (Ex, Vec<Output>) {
    let mut ex = two_markets();
    if fund > 0 {
        ex.apply(EventBody::FundInsurance { amount: usd(fund) })
            .unwrap();
    }
    ex.deposit(VICTIM, 5_300);
    trade_in(&mut ex, 1, VICTIM, SELLER, Side::Buy, 50_000, 1_000);
    trade_in(&mut ex, 2, VICTIM, SELLER, Side::Sell, 50_000, 1_000);
    assert_eq!(ex.collateral(VICTIM), usd(5_250).0);
    ex.oracle_on(2, 47_500);
    ex.oracle_on(1, 47_500);
    ex.oracle_on(1, 45_125);
    let outs = oracle_raw_on(&mut ex, 1, 42_868_750_000);
    assert_eq!(
        started_equity(&outs, VICTIM)[0],
        618_750_000,
        "solvent but below maintenance, stuck: nobody quotes market 1"
    );
    assert!(adl(&outs).is_empty());
    let outs = oracle_raw_on(&mut ex, 1, 40_725_312_500);
    (ex, outs)
}

#[test]
fn adl_pays_the_fund_balance_when_it_is_below_the_deficit_of_the_position() {
    let (ex, outs) = adl_two_markets(100);
    assert_eq!(started_equity(&outs, VICTIM)[0], -1_524_687_500);
    assert!(
        liq_fills(&outs).is_empty(),
        "fund < allowance: no book sale"
    );
    assert_eq!(paid(&outs), vec![(VICTIM, usd(100).0)]);
    // remaining deficit 3924.6875 -> bankruptcy price 40 725.3125 + 3924.6875
    assert_eq!(
        adl(&outs),
        vec![(1, VICTIM, SELLER, px(44_650).0, 10_000)],
        "market 1 only, against the short counterparty"
    );
    assert_eq!(ex.engine.insurance_fund(), Money(0));
    assert_eq!(ex.collateral(VICTIM), 0);
    assert_eq!(ex.engine.position(VICTIM, 1), None);
    // the short in market 2 is not touched: its gain stays with the account
    assert_eq!(ex.size_in(VICTIM, 2), -10_000);
    assert_eq!(ex.engine.summary().adl_fills, 1);
    assert_eq!(ex.engine.bad_debt(), Money(0));
    identity(&ex);
}

#[test]
fn adl_pays_exactly_the_deficit_of_the_position_when_the_fund_is_larger() {
    // fund 4100 > deficit 4024.6875 but < deficit + slippage allowance: ADL at
    // the mark (nothing left to cover), the fund keeps 75.3125
    let (ex, outs) = adl_two_markets(4_100);
    assert_eq!(paid(&outs), vec![(VICTIM, 4_024_687_500)]);
    assert_eq!(
        adl(&outs),
        vec![(1, VICTIM, SELLER, 40_725_312_500, 10_000)]
    );
    assert_eq!(ex.engine.insurance_fund(), Money(75_312_500));
    assert_eq!(ex.collateral(VICTIM), 0);
    assert_eq!(ex.engine.position(VICTIM, 1), None);
    assert_eq!(ex.size_in(VICTIM, 2), -10_000);
    identity(&ex);
}

#[test]
fn adl_closes_the_position_of_one_market_even_when_the_account_is_not_flat_after() {
    let (ex, _) = adl_two_markets(0);
    // fund 0: no payment, bankruptcy price = entry - collateral / size
    // = 50 000 - 5250 = 44 750
    assert_eq!(ex.engine.position(VICTIM, 1), None);
    assert_eq!(ex.collateral(VICTIM), 0);
    assert_eq!(ex.size_in(VICTIM, 2), -10_000);
    // after the ADL the remaining short is healthy (equity 2500 vs 1187.5):
    // nothing else was liquidated in that event
    assert!(!ex.engine.underwater_accounts().contains(&VICTIM));
    assert_eq!(ex.engine.summary().adl_fills, 1);
    identity(&ex);
}

trait SizeIn {
    fn size_in(&self, account: u64, market: u32) -> i64;
}

impl SizeIn for Ex {
    fn size_in(&self, account: u64, market: u32) -> i64 {
        self.engine.position(account, market).map_or(0, |p| p.size)
    }
}

// ---------------------------------------------------------------------------
// seeded wild-oracle run
// ---------------------------------------------------------------------------

#[test]
fn fund_only_pays_realised_shortfalls_over_a_wild_oracle_run() {
    let mut gen = TrafficGenerator::new(TrafficConfig {
        seed: 77,
        accounts: 80,
        markets: 2,
        oracle_every: 40,
        tick_every: 150,
    });
    let mut engine = Engine::new();
    for ev in gen.bootstrap() {
        engine.apply(&ev).unwrap();
    }
    let mut rng = Rng::new(0xFA1B);
    let mut pending: Vec<Event> = Vec::new();
    let mut trend: [i64; 3] = [0, 0, 0];
    let mut payments = 0u64;
    let mut steps_seen = 0u64;
    for i in 0..60_000u64 {
        // a wild oracle: every ~15 events one market moves by up to 5 %, with
        // momentum so that trends of many consecutive moves (crashes and
        // squeezes) occur
        if pending.is_empty() && rng.chance(1, 15) {
            let market = u32::try_from(rng.range(1, 2)).unwrap();
            if rng.chance(1, 8) {
                trend[market as usize] = if rng.chance(1, 2) { -1 } else { 1 };
            } else if rng.chance(1, 10) {
                trend[market as usize] = 0;
            }
            let dir = if trend[market as usize] != 0 && rng.chance(4, 5) {
                trend[market as usize]
            } else if rng.chance(1, 2) {
                -1
            } else {
                1
            };
            let bps = rng.range(100, 500);
            let cur = engine.index_price(market).unwrap().0;
            let next = (u128::from(cur)
                * u128::from(if dir < 0 { 10_000 - bps } else { 10_000 + bps })
                / 10_000) as u64;
            let ts = engine.clock() + 1;
            pending.push(Event {
                ts,
                body: EventBody::Oracle {
                    market,
                    index_price: Price(next.clamp(1_000_000, 1_000_000_000_000)),
                    source_ts: ts,
                },
            });
        }
        let ev = pending.pop().unwrap_or_else(|| gen.next_event(&engine));
        let before = engine.summary();
        let Ok(outs) = engine.apply(&ev) else {
            continue;
        };
        assert!(
            !engine.is_halted(),
            "halted at {i}: {:?}",
            engine.halt_reason()
        );
        steps_seen += engine.summary().liquidation_steps - before.liquidation_steps;
        for (idx, o) in outs.iter().enumerate() {
            let Output::InsurancePaid { account, amount } = o else {
                continue;
            };
            payments += 1;
            assert!(amount.0 > 0);
            // every payment lifts the account to zero collateral (book path:
            // exactly the realised shortfall; ADL path: min(deficit, fund)
            // followed by the closing Deleveraged event) ...
            let later_activity = outs[idx + 1..].iter().any(|n| match n {
                Output::Fill { taker, maker, .. } => taker == account || maker == account,
                Output::Deleveraged {
                    bankrupt,
                    counterparty,
                    ..
                } => bankrupt == account || counterparty == account,
                _ => false,
            });
            if !later_activity {
                assert_eq!(
                    engine.collateral(*account),
                    Some(Money(0)),
                    "event {i}: {o:?} left collateral behind"
                );
            }
        }
        // a flat account never keeps a loss nobody covered
        if i % 7 == 0 {
            for id in engine.account_ids() {
                let flat = engine.position(id, 1).is_none() && engine.position(id, 2).is_none();
                if flat {
                    assert!(
                        engine.collateral(id).unwrap().0 >= 0,
                        "event {i}: flat account {id} owes money"
                    );
                }
            }
        }
        if i % 5_000 == 0 {
            engine
                .check_invariants()
                .unwrap_or_else(|why| panic!("event {i}: {why}"));
        }
    }
    engine.check_invariants().unwrap();
    let s = engine.summary();
    eprintln!(
        "wild run: liq_steps={steps_seen} payments={payments} adl={} bad_debt={} fund={}",
        s.adl_fills, s.bad_debt.0, s.insurance_fund.0
    );
    assert!(steps_seen > 100, "the run must liquidate: {steps_seen}");
    assert_eq!(s.bad_debt, Money(0), "covered by the fund or by ADL");
    assert!(payments > 0, "the run must exercise the fund");
}
