//! Domain tests: oracle input rules, bounded-change filter, mark price,
//! staleness, and funding (rate, settlement, exactness, carry).
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod common;
use common::*;

use perp_domain::error::Error;
use perp_domain::fixed::{Money, Price, MAX_PRICE};
use perp_domain::types::{EventBody, MarketConfig, MarketId, OrderKind, Output, Side, Tif};

/// One funding interval of the example market.
const HOUR: u64 = 3_600_000;
/// Denominator of the funding carry: `QTY_SCALE * RATE_SCALE`.
const FUNDING_DEN: u128 = 10_000 * 1_000_000_000;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn assert_conserved(ex: &Ex) {
    let e = &ex.engine;
    assert_eq!(
        e.total_collateral().0 + e.insurance_fund().0 + e.fee_pool().0 - e.total_basis().0,
        e.net_deposits().0 + e.bad_debt().0,
        "money identity"
    );
}

fn oracle_body(market: MarketId, price: Price, source_ts: u64) -> EventBody {
    EventBody::Oracle {
        market,
        index_price: price,
        source_ts,
    }
}

/// Oracle update for market 1 at a raw price, stamped "now".
fn oracle_raw(ex: &mut Ex, price: Price) -> Result<Vec<Output>, Error> {
    let ts = ex.ts + 1;
    ex.apply_at(ts, oracle_body(M, price, ts))
}

/// Oracle update at an exact event time, source observed at the same time.
fn oracle_at(ex: &mut Ex, ts: u64, market: MarketId, price: u64) -> Result<Vec<Output>, Error> {
    ex.apply_at(ts, oracle_body(market, px(price), ts))
}

fn mark_updated(outs: &[Output]) -> (Price, Price, bool) {
    let mut it = outs.iter().filter_map(|o| match o {
        Output::MarkUpdated {
            index,
            mark,
            clamped,
            ..
        } => Some((*index, *mark, *clamped)),
        _ => None,
    });
    let v = it.next().expect("MarkUpdated");
    assert!(it.next().is_none());
    v
}

/// `(market, rate_ppb, transferred, carry)` of every FundingSettled output.
fn settled(outs: &[Output]) -> Vec<(MarketId, i64, Money, u128)> {
    outs.iter()
        .filter_map(|o| match o {
            Output::FundingSettled {
                market,
                rate_ppb,
                transferred,
                carry,
            } => Some((*market, *rate_ppb, *transferred, *carry)),
            _ => None,
        })
        .collect()
}

fn liquidation_started(outs: &[Output]) -> Vec<(u64, Money)> {
    outs.iter()
        .filter_map(|o| match o {
            Output::LiquidationStarted { account, equity } => Some((*account, *equity)),
            _ => None,
        })
        .collect()
}

fn limit_on(
    ex: &mut Ex,
    market: MarketId,
    account: u64,
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

fn trade_on(
    ex: &mut Ex,
    market: MarketId,
    taker: u64,
    maker: u64,
    side: Side,
    price: u64,
    milli: u64,
) {
    limit_on(ex, market, maker, side.opposite(), price, milli, Tif::Gtc).unwrap();
    let outs = limit_on(ex, market, taker, side, price, milli, Tif::Ioc).unwrap();
    assert!(!fills(&outs).is_empty());
}

/// Engine with one market, accounts 1 (long 1.0 @ 50 000), 2 (short 1.0)
/// funded with 100 000 each and account 3 (book maker) with 100 000.
/// The oracle holds 50 000; no resting orders yet.
fn funded_pair(cfg: MarketConfig) -> Ex {
    let mut ex = Ex::with_market(cfg);
    ex.fund(&[1, 2, 3], 100_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    ex
}

// ---------------------------------------------------------------------------
// Oracle input rules
// ---------------------------------------------------------------------------

#[test]
fn first_oracle_update_sets_index_and_mark_unclamped() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    assert_eq!(ex.engine.index_price(M), None);
    assert_eq!(ex.engine.mark_price(M), None);
    // the first print is never filtered, however far it is from "nothing"
    let outs = oracle_at(&mut ex, 5_000, M, 1_000_000).unwrap();
    assert_eq!(
        outs,
        vec![Output::MarkUpdated {
            market: M,
            index: px(1_000_000),
            mark: px(1_000_000),
            clamped: false
        }]
    );
    assert_eq!(ex.engine.index_price(M), Some(px(1_000_000)));
    assert_eq!(ex.engine.mark_price(M), Some(px(1_000_000)));
    ex.check();
}

#[test]
fn orders_need_a_mark_price() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    ex.deposit(1, 1_000);
    assert_eq!(
        ex.limit(1, Side::Buy, 50_000, 1, Tif::Gtc),
        Err(Error::NoMarkPrice)
    );
    oracle_at(&mut ex, 5_000, M, 50_000).unwrap();
    assert!(ex.limit(1, Side::Buy, 50_000, 1, Tif::Gtc).is_ok());
}

#[test]
fn oracle_rejects_zero_and_oversized_prices() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    let before = ex.engine.content_hash();
    assert_eq!(
        ex.apply(oracle_body(M, Price(0), ex.ts + 1)),
        Err(Error::OracleBadPrice)
    );
    assert_eq!(
        ex.apply(oracle_body(M, Price(MAX_PRICE + 1), ex.ts + 1)),
        Err(Error::OracleBadPrice)
    );
    // the price is validated before the timestamp
    assert_eq!(
        ex.apply(oracle_body(M, Price(0), 9_999_999_999)),
        Err(Error::OracleBadPrice)
    );
    assert_eq!(ex.engine.content_hash(), before);
    assert_eq!(ex.engine.index_price(M), None);
    // the ceiling itself is a valid first price
    let ts = ex.ts + 1;
    let outs = ex
        .apply_at(ts, oracle_body(M, Price(MAX_PRICE), ts))
        .unwrap();
    assert_eq!(mark_updated(&outs).0, Price(MAX_PRICE));
}

#[test]
fn oracle_for_unknown_market_is_rejected() {
    let mut ex = Ex::new();
    assert_eq!(ex.oracle_on_result(99, 50_000), Err(Error::UnknownMarket));
}

#[test]
fn oracle_source_time_in_the_future_is_a_regression() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    let before = ex.engine.content_hash();
    // even the very first update must not be observed after it was sent
    assert_eq!(
        ex.apply_at(10_000, oracle_body(M, px(50_000), 10_001)),
        Err(Error::OracleRegression)
    );
    assert_eq!(ex.engine.content_hash(), before);
    assert_eq!(ex.engine.index_price(M), None);
}

#[test]
fn oracle_update_older_than_the_staleness_limit_is_rejected() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    let before = ex.engine.content_hash();
    assert_eq!(
        ex.apply_at(100_000, oracle_body(M, px(50_000), 39_999)),
        Err(Error::StaleOracle)
    );
    assert_eq!(ex.engine.content_hash(), before);
    // exactly 60 000 ms old is still acceptable
    let outs = ex
        .apply_at(100_000, oracle_body(M, px(50_000), 40_000))
        .unwrap();
    assert!(has(&outs, |o| matches!(o, Output::MarkUpdated { .. })));
}

#[test]
fn oracle_source_time_must_strictly_increase() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    ex.apply_at(200_000, oracle_body(M, px(50_000), 199_990))
        .unwrap();
    let before = ex.engine.content_hash();
    for source_ts in [199_990, 199_980] {
        assert_eq!(
            ex.apply_at(200_001, oracle_body(M, px(50_100), source_ts)),
            Err(Error::OracleRegression),
            "source_ts {source_ts}"
        );
    }
    assert_eq!(ex.engine.content_hash(), before);
    assert_eq!(ex.engine.index_price(M), Some(px(50_000)));
    // newer than the previous one is fine even if older than the event time
    let outs = ex
        .apply_at(200_002, oracle_body(M, px(50_100), 199_991))
        .unwrap();
    assert_eq!(mark_updated(&outs).0, px(50_100));
}

// ---------------------------------------------------------------------------
// Bounded-change filter
// ---------------------------------------------------------------------------

#[test]
fn large_upward_jump_is_clamped_to_the_bound() {
    let mut ex = Ex::new(); // index 50 000, bound 5%
    let outs = ex.oracle(60_000).unwrap();
    assert_eq!(
        outs,
        vec![Output::MarkUpdated {
            market: M,
            index: px(52_500),
            mark: px(52_500),
            clamped: true
        }]
    );
    assert_eq!(ex.engine.index_price(M), Some(px(52_500)));
    // the next update is bounded relative to the clamped value, not the print
    let (index, _, clamped) = mark_updated(&ex.oracle(70_000).unwrap());
    assert_eq!((index, clamped), (px(55_125), true));
}

#[test]
fn large_downward_jump_is_clamped_to_the_bound() {
    let mut ex = Ex::new();
    let (index, mark, clamped) = mark_updated(&ex.oracle(10).unwrap());
    assert_eq!((index, clamped), (px(47_500), true));
    assert_eq!(mark, px(47_500), "median of three equal inputs");
    assert_eq!(ex.engine.index_price(M), Some(px(47_500)));
}

#[test]
fn updates_inside_the_bound_are_taken_verbatim() {
    let mut ex = Ex::new();
    let (index, mark, clamped) = mark_updated(&ex.oracle(51_000).unwrap());
    assert_eq!((index, mark, clamped), (px(51_000), px(51_000), false));
    let (index, _, clamped) = mark_updated(&ex.oracle(49_000).unwrap());
    assert_eq!((index, clamped), (px(49_000), false));
}

#[test]
fn clamp_boundary_is_exact_to_the_last_raw_unit() {
    let cases = [
        // (raw price, expected raw index, clamped)
        (52_500_000_000, 52_500_000_000, false), // exactly +5%
        (52_500_000_001, 52_500_000_000, true),  // one unit above
        (47_500_000_000, 47_500_000_000, false), // exactly -5%
        (47_499_999_999, 47_500_000_000, true),  // one unit below
    ];
    for (raw, want, want_clamped) in cases {
        let mut ex = Ex::new();
        let (index, _, clamped) = mark_updated(&oracle_raw(&mut ex, Price(raw)).unwrap());
        assert_eq!((index, clamped), (Price(want), want_clamped), "raw {raw}");
    }
}

#[test]
fn rejected_oracle_update_does_not_touch_prices() {
    let mut ex = Ex::new();
    ex.oracle(51_000).unwrap();
    let before = ex.engine.content_hash();
    let ts = ex.ts;
    // same source_ts as the accepted update: rejected, nothing moves
    assert_eq!(
        ex.apply_at(ts + 1, oracle_body(M, px(55_000), ts)),
        Err(Error::OracleRegression)
    );
    assert_eq!(ex.engine.index_price(M), Some(px(51_000)));
    assert_eq!(ex.engine.mark_price(M), Some(px(51_000)));
    assert_eq!(ex.engine.content_hash(), before);
}

// ---------------------------------------------------------------------------
// Mark price
// ---------------------------------------------------------------------------

/// Three funded accounts and an engine whose last trade is at `trade_px`
/// (account 1 buys 1.0 from account 2, who rested 2.0 so 1.0 stays on the book).
fn with_last_trade_and_remaining_ask(trade_px: u64) -> Ex {
    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3], 100_000);
    ex.gtc(2, Side::Sell, trade_px, 2_000);
    let outs = ex.limit(1, Side::Buy, trade_px, 1_000, Tif::Ioc).unwrap();
    assert_eq!(filled_qty(&outs), 10_000);
    ex
}

#[test]
fn mark_is_the_median_of_index_mid_and_last_trade() {
    let mut ex = with_last_trade_and_remaining_ask(50_600); // last trade 50 600, ask 50 600
    ex.gtc(3, Side::Buy, 50_400, 1_000); // mid = 50 500
    assert_eq!(
        ex.engine.mark_price(M),
        Some(px(50_000)),
        "not recomputed yet"
    );
    let (index, mark, clamped) = mark_updated(&ex.oracle(50_000).unwrap());
    assert_eq!((index, mark, clamped), (px(50_000), px(50_500), false));
    assert_eq!(ex.engine.mark_price(M), Some(px(50_500)));
    ex.check();
}

#[test]
fn mark_is_only_recomputed_on_oracle_updates() {
    let mut ex = with_last_trade_and_remaining_ask(50_600);
    let bid = ex.gtc(3, Side::Buy, 50_400, 1_000);
    ex.oracle(50_000).unwrap();
    assert_eq!(ex.engine.mark_price(M), Some(px(50_500)));
    // the book changes (mid disappears) but the mark stays put ...
    ex.cancel(3, bid).unwrap();
    assert_eq!(ex.engine.mark_price(M), Some(px(50_500)));
    // ... until the next oracle update: one-sided book counts as the index
    let (_, mark, _) = mark_updated(&ex.oracle(50_000).unwrap());
    assert_eq!(mark, px(50_000)); // median(index, index, last 50 600) = index
}

#[test]
fn mid_alone_cannot_move_the_mark() {
    let mut ex = Ex::new();
    ex.fund(&[2, 3], 100_000);
    ex.gtc(2, Side::Sell, 50_600, 1_000);
    ex.gtc(3, Side::Buy, 50_400, 1_000);
    let (_, mark, _) = mark_updated(&ex.oracle(50_000).unwrap());
    assert_eq!(mark, px(50_000));
}

#[test]
fn last_trade_alone_cannot_move_the_mark() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.trade(1, 2, Side::Buy, 50_600, 1_000); // book empty afterwards
    let (_, mark, _) = mark_updated(&ex.oracle(50_000).unwrap());
    assert_eq!(mark, px(50_000));
}

#[test]
fn mid_and_last_trade_on_opposite_sides_of_the_index_leave_the_mark_at_the_index() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3], 100_000);
    ex.gtc(2, Side::Buy, 49_800, 2_000);
    let outs = ex.limit(1, Side::Sell, 49_800, 1_000, Tif::Ioc).unwrap(); // last 49 800
    assert_eq!(filled_qty(&outs), 10_000);
    ex.gtc(3, Side::Sell, 50_600, 1_000); // bid 49 800 / ask 50 600 -> mid 50 200
    let (_, mark, _) = mark_updated(&ex.oracle(50_000).unwrap());
    assert_eq!(mark, px(50_000));
}

#[test]
fn mark_is_clamped_to_the_deviation_band_above_the_index() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3], 100_000);
    // last trade at the top of the 2.5% price band around the 50 000 mark
    let ask = ex.gtc(2, Side::Sell, 51_250, 2_000);
    let outs = ex.limit(1, Side::Buy, 51_250, 1_000, Tif::Ioc).unwrap();
    assert_eq!(filled_qty(&outs), 10_000);
    ex.cancel(2, ask).unwrap();
    // far passive ask, in-band bid: mid = (50 900 + 60 000) / 2 = 55 450
    ex.gtc(2, Side::Sell, 60_000, 1_000);
    ex.gtc(3, Side::Buy, 50_900, 1_000);
    // index drops 0.6%: the 51 250 trade is now more than 3% above it
    let (index, mark, clamped) = mark_updated(&ex.oracle(49_700).unwrap());
    // median(49 700, 55 450, 51 250) = 51 250, capped at 49 700 * 1.03 = 51 191
    assert_eq!((index, mark, clamped), (px(49_700), px(51_191), false));
    ex.check();
}

#[test]
fn mark_is_clamped_to_the_deviation_band_below_the_index() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3], 100_000);
    // last trade at the bottom of the 2.5% price band around the 50 000 mark
    let bid = ex.gtc(2, Side::Buy, 48_750, 2_000);
    let outs = ex.limit(1, Side::Sell, 48_750, 1_000, Tif::Ioc).unwrap();
    assert_eq!(filled_qty(&outs), 10_000);
    ex.cancel(2, bid).unwrap();
    // far passive bid, in-band ask: mid = (40 000 + 49 100) / 2 = 44 550
    ex.gtc(2, Side::Buy, 40_000, 1_000);
    ex.gtc(3, Side::Sell, 49_100, 1_000);
    // index rises 0.6%: the 48 750 trade is now more than 3% below it
    let (index, mark, clamped) = mark_updated(&ex.oracle(50_300).unwrap());
    // median(50 300, 44 550, 48 750) = 48 750, floored at 50 300 * 0.97 = 48 791
    assert_eq!((index, mark, clamped), (px(50_300), px(48_791), false));
    ex.check();
}

// ---------------------------------------------------------------------------
// Staleness
// ---------------------------------------------------------------------------

#[test]
fn stale_mark_rejects_new_risk_but_allows_reduce_only_orders() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    ex.oracle(50_000).unwrap();
    let t = ex.ts;

    // exactly at the limit is fine
    let p = ex.order(
        1,
        Side::Buy,
        OrderKind::Limit {
            price: px(49_000),
            tif: Tif::Gtc,
        },
        1_000,
    );
    ex.apply_at(t + 60_000, EventBody::Place(p)).unwrap();

    let before = ex.engine.content_hash();
    let p = ex.order(
        1,
        Side::Buy,
        OrderKind::Limit {
            price: px(49_000),
            tif: Tif::Gtc,
        },
        1_000,
    );
    assert_eq!(
        ex.apply_at(t + 60_001, EventBody::Place(p)),
        Err(Error::StaleOracle)
    );
    assert_eq!(ex.engine.content_hash(), before);

    // a reduce-only order is still accepted so positions can be closed
    let mut p = ex.order(
        1,
        Side::Sell,
        OrderKind::Limit {
            price: px(60_000),
            tif: Tif::Gtc,
        },
        1_000,
    );
    p.reduce_only = true;
    let outs = ex.apply_at(t + 60_002, EventBody::Place(p)).unwrap();
    assert!(has(&outs, |o| matches!(o, Output::Rested { .. })));

    // a fresh oracle update re-enables normal orders
    oracle_at(&mut ex, t + 60_003, M, 50_000).unwrap();
    let p = ex.order(
        1,
        Side::Buy,
        OrderKind::Limit {
            price: px(49_000),
            tif: Tif::Gtc,
        },
        1_000,
    );
    ex.apply_at(t + 60_004, EventBody::Place(p)).unwrap();
    ex.check();
}

// ---------------------------------------------------------------------------
// Funding: rate
// ---------------------------------------------------------------------------

/// Pair plus a two-sided book of account 3 around the index, one oracle
/// refresh to record a premium sample, then the boundary event.
fn premium_scenario(bid: u64, ask: u64) -> Ex {
    let mut ex = funded_pair(MarketConfig::example(M));
    ex.gtc(3, Side::Buy, bid, 100);
    ex.gtc(3, Side::Sell, ask, 100);
    // premium sample of this update: (mid - index) / index
    ex.oracle(50_000).unwrap();
    ex
}

#[test]
fn positive_premium_makes_longs_pay_shorts() {
    // mid 50 010: premium 200 000 ppb; samples {0 (initial update), 200 000}
    // -> mean 100 000, plus interest 100 000 = 200 000 ppb
    let mut ex = premium_scenario(50_000, 50_020);
    let (c1, c2, c3) = (ex.collateral(1), ex.collateral(2), ex.collateral(3));
    let total = ex.engine.total_collateral();
    let (ins, fee) = (ex.engine.insurance_fund(), ex.engine.fee_pool());

    let outs = oracle_at(&mut ex, HOUR, M, 50_000).unwrap();
    // 1 base * 50 000 * 200 000 / 1e9 = 10
    assert_eq!(settled(&outs), vec![(M, 200_000, usd(10), 0)]);
    assert!(
        matches!(outs[0], Output::FundingSettled { .. })
            && matches!(outs[1], Output::MarkUpdated { .. }),
        "funding is settled before the update is applied: {outs:?}"
    );
    assert_eq!(ex.collateral(1), c1 - usd(10).0, "long pays");
    assert_eq!(ex.collateral(2), c2 + usd(10).0, "short receives");
    assert_eq!(ex.collateral(3), c3, "flat account unaffected");
    assert_eq!(ex.engine.total_collateral(), total, "zero-sum");
    assert_eq!(
        (ex.engine.insurance_fund(), ex.engine.fee_pool()),
        (ins, fee)
    );
    assert_eq!(ex.engine.funding_state(M), Some((0, 200_000)));
    assert_eq!(ex.size(1), 10_000, "funding never changes positions");
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn negative_premium_makes_shorts_pay_longs() {
    // mid 49 950: premium -1 000 000 ppb; mean (0 - 1e6)/2 = -500 000; +100 000
    let mut ex = premium_scenario(49_900, 50_000);
    let (c1, c2) = (ex.collateral(1), ex.collateral(2));
    let total = ex.engine.total_collateral();

    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    // 1 * 50 000 * 400 000 / 1e9 = 20
    assert_eq!(settled(&outs), vec![(M, -400_000, usd(20), 0)]);
    assert_eq!(ex.collateral(1), c1 + usd(20).0, "long receives");
    assert_eq!(ex.collateral(2), c2 - usd(20).0, "short pays");
    assert_eq!(ex.engine.total_collateral(), total);
    assert_eq!(ex.engine.funding_state(M), Some((0, -400_000)));
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn rate_is_capped_at_the_positive_cap() {
    // mid 51 000: premium 20 000 000 ppb -> far above the 5 000 000 cap
    let mut ex = premium_scenario(50_900, 51_100);
    let (c1, c2) = (ex.collateral(1), ex.collateral(2));
    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    // 1 * 50 000 * 0.5% = 250
    assert_eq!(settled(&outs), vec![(M, 5_000_000, usd(250), 0)]);
    assert_eq!(ex.collateral(1), c1 - usd(250).0);
    assert_eq!(ex.collateral(2), c2 + usd(250).0);
    assert_conserved(&ex);
}

#[test]
fn rate_is_capped_at_the_negative_cap() {
    // mid 49 000: premium -20 000 000 ppb; mean -10 000 000 + 100 000 < -cap
    let mut ex = premium_scenario(48_900, 49_100);
    let (c1, c2) = (ex.collateral(1), ex.collateral(2));
    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    assert_eq!(settled(&outs), vec![(M, -5_000_000, usd(250), 0)]);
    assert_eq!(ex.collateral(1), c1 + usd(250).0);
    assert_eq!(ex.collateral(2), c2 - usd(250).0);
    assert_conserved(&ex);
}

#[test]
fn premium_samples_are_averaged_over_the_interval() {
    let mut ex = funded_pair(MarketConfig::example(M));
    ex.gtc(3, Side::Buy, 50_000, 100);
    ex.gtc(3, Side::Sell, 50_020, 100); // premium 200 000 per sample
                                        // samples so far: the initial update (0). Add three updates of 200 000.
    for _ in 0..3 {
        ex.oracle(50_000).unwrap();
    }
    // mean = floor(600 000 / 4) = 150 000, + interest 100 000
    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    assert_eq!(settled(&outs), vec![(M, 250_000, Money(12_500_000), 0)]);
}

#[test]
fn premium_cancelling_the_interest_gives_rate_zero_and_no_output() {
    // mid 49 990 -> premium -200 000; mean (0 - 200 000)/2 = -100 000 = -interest
    let mut ex = premium_scenario(49_980, 50_000);
    let before = (ex.collateral(1), ex.collateral(2));
    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    assert!(settled(&outs).is_empty());
    assert_eq!((ex.collateral(1), ex.collateral(2)), before);
    assert_eq!(ex.engine.funding_state(M), Some((0, 0)));
}

#[test]
fn zero_rate_market_emits_nothing_and_moves_no_money() {
    let mut cfg = MarketConfig::example(M);
    cfg.funding_interest_ppb = 0;
    let mut ex = funded_pair(cfg);
    let (c1, c2) = (ex.collateral(1), ex.collateral(2));
    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    assert!(outs.is_empty(), "{outs:?}");
    assert_eq!((ex.collateral(1), ex.collateral(2)), (c1, c2));
    assert_eq!(ex.engine.funding_state(M), Some((0, 0)));
}

#[test]
fn negative_interest_alone_makes_shorts_pay() {
    let mut cfg = MarketConfig::example(M);
    cfg.funding_interest_ppb = -100_000;
    let mut ex = funded_pair(cfg);
    let (c1, c2) = (ex.collateral(1), ex.collateral(2));
    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    assert_eq!(settled(&outs), vec![(M, -100_000, usd(5), 0)]);
    assert_eq!(ex.collateral(1), c1 + usd(5).0);
    assert_eq!(ex.collateral(2), c2 - usd(5).0);
}

#[test]
fn nonzero_rate_without_positions_settles_an_empty_transfer() {
    // Documented behaviour: the rate is non-zero (interest) so the event is
    // emitted, but nothing moves.
    let mut ex = Ex::new();
    ex.fund(&[1], 1_000);
    let total = ex.engine.total_collateral();
    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    assert_eq!(settled(&outs), vec![(M, 100_000, Money(0), 0)]);
    assert_eq!(ex.engine.total_collateral(), total);
    assert_eq!(ex.engine.funding_state(M), Some((0, 100_000)));
    ex.check();
}

#[test]
fn market_without_a_mark_price_settles_nothing() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    assert!(outs.is_empty());
}

// ---------------------------------------------------------------------------
// Funding: when it settles
// ---------------------------------------------------------------------------

#[test]
fn funding_settles_only_on_oracle_or_tick_events_once_the_boundary_passed() {
    let mut ex = funded_pair(MarketConfig::example(M));
    let c1 = ex.collateral(1);

    // before the boundary: nothing
    let outs = ex.apply_at(HOUR - 1, EventBody::Tick).unwrap();
    assert!(outs.is_empty());
    assert_eq!(ex.collateral(1), c1);

    // at the boundary a Deposit does not settle funding
    let outs = ex
        .apply_at(
            HOUR,
            EventBody::Deposit {
                account: 3,
                amount: usd(1),
            },
        )
        .unwrap();
    assert!(settled(&outs).is_empty());
    assert_eq!(ex.collateral(1), c1);

    // the next Tick does (rate = interest 100 000 ppb -> 5)
    let outs = ex.apply_at(HOUR + 1, EventBody::Tick).unwrap();
    assert_eq!(settled(&outs), vec![(M, 100_000, usd(5), 0)]);
    assert_eq!(ex.collateral(1), c1 - usd(5).0);

    // once per boundary
    let outs = ex.apply_at(HOUR + 2, EventBody::Tick).unwrap();
    assert!(settled(&outs).is_empty());
    assert_eq!(ex.collateral(1), c1 - usd(5).0);
}

#[test]
fn intervals_without_events_are_not_back_filled() {
    let mut ex = funded_pair(MarketConfig::example(M));
    let c1 = ex.collateral(1);
    // ten intervals pass with no event at all
    let outs = ex.apply_at(10 * HOUR + 5, EventBody::Tick).unwrap();
    assert_eq!(
        settled(&outs),
        vec![(M, 100_000, usd(5), 0)],
        "one interval only"
    );
    assert_eq!(ex.collateral(1), c1 - usd(5).0);
    // the next boundary is the first multiple after the event time
    let outs = ex.apply_at(11 * HOUR - 1, EventBody::Tick).unwrap();
    assert!(settled(&outs).is_empty());
    let outs = ex.apply_at(11 * HOUR, EventBody::Tick).unwrap();
    assert_eq!(settled(&outs).len(), 1);
    assert_eq!(ex.collateral(1), c1 - usd(10).0);
    assert_conserved(&ex);
}

// ---------------------------------------------------------------------------
// Funding: exactness
// ---------------------------------------------------------------------------

#[test]
fn funding_is_zero_sum_with_several_accounts_and_odd_amounts() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3, 4], 100_000);
    oracle_at(&mut ex, 5_000, M, 50_003).unwrap();
    // longs A=1 (+0.003), B=2 (+0.007); shorts C=3 (-0.004), D=4 (-0.006)
    ex.trade(1, 3, Side::Buy, 50_000, 3);
    ex.trade(2, 3, Side::Buy, 50_000, 1);
    ex.trade(2, 4, Side::Buy, 50_000, 6);
    oracle_at(&mut ex, 5_100, M, 50_003).unwrap(); // mark = index 50 003
    assert_eq!(ex.engine.mark_price(M), Some(px(50_003)));
    assert_eq!(
        (ex.size(1), ex.size(2), ex.size(3), ex.size(4)),
        (30, 70, -40, -60)
    );
    let before: Vec<i128> = (1..=4).map(|a| ex.collateral(a)).collect();
    let total = ex.engine.total_collateral();

    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    // exact: pool = 100 raw * 50_003e6 * 1e5 / 1e13 = 50 003.0 -> total 50 003, carry 0
    assert_eq!(settled(&outs), vec![(M, 100_000, Money(50_003), 0)]);
    // exact shares: A 15 000.9, B 35 002.1 | C 20 001.2, D 30 001.8;
    // largest remainder gives the spare unit to A (.9) and D (.8)
    let delta: Vec<i128> = (1..=4)
        .map(|a| ex.collateral(a) - before[(a - 1) as usize])
        .collect();
    assert_eq!(delta, vec![-15_001, -35_002, 20_001, 30_002]);
    assert_eq!(
        delta.iter().sum::<i128>(),
        0,
        "nothing created or destroyed"
    );
    assert_eq!(delta[0] + delta[1], -50_003, "sum paid == transferred");
    assert_eq!(delta[2] + delta[3], 50_003, "sum received == transferred");
    assert_eq!(ex.engine.total_collateral(), total);
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn remainder_is_carried_and_cumulative_transfer_is_the_floor_of_the_exact_amount() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    // 0.001 base long/short, mark = index = 50 006
    ex.trade(1, 2, Side::Buy, 50_006, 1);
    oracle_at(&mut ex, 5_000, M, 50_006).unwrap();
    assert_eq!(ex.engine.mark_price(M), Some(px(50_006)));
    let c1 = ex.collateral(1);
    let c2 = ex.collateral(2);

    // exact amount per interval (over FUNDING_DEN): 10 * 50_006e6 * 100_000
    let exact: u128 = 10 * 50_006_000_000 * 100_000;
    assert_eq!(
        exact,
        5_000 * FUNDING_DEN + 6_000_000_000_000,
        "5000.6 raw per interval"
    );

    let expect = [
        (5_000, 6_000_000_000_000u128),
        (5_001, 2_000_000_000_000),
        (5_000, 8_000_000_000_000),
        (5_001, 4_000_000_000_000),
        (5_001, 0),
    ];
    let mut cumulative: u128 = 0;
    for (k, (want_total, want_carry)) in expect.into_iter().enumerate() {
        let k = (k + 1) as u64;
        let outs = oracle_at(&mut ex, k * HOUR, M, 50_006).unwrap();
        assert_eq!(
            settled(&outs),
            vec![(M, 100_000, Money(want_total), want_carry)],
            "interval {k}"
        );
        cumulative += want_total as u128;
        // cumulative transfer * den + carry == cumulative exact amount
        assert_eq!(
            cumulative * FUNDING_DEN + want_carry,
            u128::from(k) * exact,
            "interval {k}"
        );
        assert_eq!(ex.engine.funding_state(M), Some((want_carry, 100_000)));
    }
    assert_eq!(cumulative, 25_003, "floor(5 * 5000.6)");
    assert_eq!(ex.collateral(1), c1 - 25_003);
    assert_eq!(ex.collateral(2), c2 + 25_003);
    assert_conserved(&ex);
    ex.check();
}

// ---------------------------------------------------------------------------
// Funding and liquidation
// ---------------------------------------------------------------------------

/// Account 1 long 10.0 with margin just above maintenance at mark 48 700;
/// funding at the 0.5% cap would push it under. Account 2 is the short.
fn near_liquidation() -> Ex {
    let mut cfg = MarketConfig::example(M);
    cfg.funding_interest_ppb = cfg.funding_cap_ppb; // constant 0.5% rate
    let mut ex = Ex::with_market(cfg);
    ex.deposit(1, 26_000);
    ex.deposit(2, 100_000);
    ex.trade(1, 2, Side::Buy, 50_000, 10_000); // fee 250 -> collateral 25 750
    assert_eq!(ex.collateral(1), usd(25_750).0);
    let outs = ex.oracle(48_700).unwrap();
    // equity 25 750 - 13 000 = 12 750 >= maintenance 2.5% of 487 000 = 12 175
    assert!(liquidation_started(&outs).is_empty());
    assert_eq!(ex.engine.equity(1), Some(usd(12_750)));
    assert_eq!(
        ex.engine.account_view(1).unwrap().maintenance_margin,
        Money(12_175_000_000)
    );
    ex
}

#[test]
fn funding_that_pushes_an_account_below_maintenance_starts_its_liquidation() {
    let mut ex = near_liquidation();
    let outs = oracle_at(&mut ex, HOUR, M, 48_700).unwrap();
    // 10 * 48 700 * 0.5% = 2 435 paid by the long
    assert_eq!(settled(&outs), vec![(M, 5_000_000, usd(2_435), 0)]);
    assert_eq!(ex.collateral(1), usd(25_750 - 2_435).0);
    // maker fee 200 ppm of 500 000 = 100, plus the 2 435 received
    assert_eq!(ex.collateral(2), usd(100_000 - 100 + 2_435).0);
    // the payer was re-checked in the same event: equity 10 315 < 12 175
    assert_eq!(liquidation_started(&outs), vec![(1, usd(10_315))]);
    let funding = outs
        .iter()
        .position(|o| matches!(o, Output::FundingSettled { .. }))
        .unwrap();
    let liq = outs
        .iter()
        .position(|o| matches!(o, Output::LiquidationStarted { .. }))
        .unwrap();
    assert!(
        funding < liq,
        "funding comes before the liquidation it causes"
    );
    // the receiver is not liquidated
    assert!(liquidation_started(&outs).iter().all(|(a, _)| *a != 2));
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn funding_on_a_stale_oracle_defers_the_liquidation_to_the_next_fresh_update() {
    let mut ex = near_liquidation();
    // the oracle is long stale at the boundary: funding still settles ...
    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    assert_eq!(settled(&outs), vec![(M, 5_000_000, usd(2_435), 0)]);
    // ... but liquidation is skipped while the price cannot be trusted
    assert!(liquidation_started(&outs).is_empty());
    assert_eq!(ex.size(1), 100_000);
    // a fresh update (same index) re-checks the holders
    let outs = oracle_at(&mut ex, HOUR + 1, M, 48_700).unwrap();
    assert_eq!(liquidation_started(&outs), vec![(1, usd(10_315))]);
    assert!(settled(&outs).is_empty(), "funding is not repeated");
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn funding_that_leaves_the_payer_above_maintenance_starts_no_liquidation() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.trade(1, 2, Side::Buy, 50_000, 10_000);
    let outs = oracle_at(&mut ex, HOUR, M, 50_000).unwrap();
    assert_eq!(settled(&outs).len(), 1);
    assert!(liquidation_started(&outs).is_empty());
}

// ---------------------------------------------------------------------------
// Funding: independence of markets
// ---------------------------------------------------------------------------

#[test]
fn markets_settle_independently_on_their_own_interval() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(1)))
        .unwrap();
    let mut slow = MarketConfig::example(2);
    slow.funding_interval_ms = 2 * HOUR;
    ex.apply(EventBody::AddMarket(slow)).unwrap();
    ex.oracle_on(1, 50_000);
    ex.oracle_on(2, 2_000);
    ex.fund(&[1, 2, 3], 100_000);
    trade_on(&mut ex, 1, 1, 2, Side::Buy, 50_000, 1_000); // 1.0 @ 50 000
    trade_on(&mut ex, 2, 1, 3, Side::Buy, 2_000, 10_000); // 10.0 @ 2 000
    let before = (ex.collateral(1), ex.collateral(2), ex.collateral(3));

    // first hour: only market 1 is due
    let outs = ex.apply_at(HOUR, EventBody::Tick).unwrap();
    assert_eq!(settled(&outs), vec![(1, 100_000, usd(5), 0)]);
    assert_eq!(
        ex.engine.funding_state(2),
        Some((0, 0)),
        "market 2 untouched"
    );
    assert_eq!(ex.collateral(1), before.0 - usd(5).0);
    assert_eq!(ex.collateral(2), before.1 + usd(5).0);
    assert_eq!(ex.collateral(3), before.2);

    // second hour, triggered by an oracle update of market 2: both are due and
    // settle in market order, market 1's boundary included
    let outs = oracle_at(&mut ex, 2 * HOUR, 2, 2_000).unwrap();
    assert_eq!(
        settled(&outs),
        vec![(1, 100_000, usd(5), 0), (2, 100_000, usd(2), 0)]
    );
    assert!(matches!(outs[2], Output::MarkUpdated { market: 2, .. }));
    // account 1 paid 5 + 5 + 2; its counterparties received exactly that
    assert_eq!(ex.collateral(1), before.0 - usd(12).0);
    assert_eq!(ex.collateral(2), before.1 + usd(10).0);
    assert_eq!(ex.collateral(3), before.2 + usd(2).0);
    assert_eq!(ex.engine.position_sum(1), 0);
    assert_eq!(ex.engine.position_sum(2), 0);
    assert_conserved(&ex);
    ex.check();
}
