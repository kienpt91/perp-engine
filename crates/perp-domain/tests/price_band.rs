//! Domain tests: the price band around the mark, market-order protection
//! measured from the mark, the execution-price loss reserve ("adverse") at
//! order entry, the sweep of band-violating resting orders on oracle updates,
//! and the `price_band_bps` configuration.
//!
//! With the example market the mark is 50 000 and the band is 250 bps, so a
//! buy limit may be at most 51 250 and a sell limit at least 48 750. With no
//! trade ever executed the mark equals the index exactly (median of index,
//! mid and last trade, where missing inputs count as the index).
//!
//! Money is shown in raw micro-quote units: 1 quote = 1 000 000.
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
use perp_domain::error::Error;
use perp_domain::fixed::{Money, Price, Qty};
use perp_domain::rng::Rng;
use perp_domain::types::{
    AccountId, CancelReason, Event, EventBody, MarginTier, MarketConfig, OrderId, OrderKind,
    Output, Side, Tif,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Example market, mark 50 000, accounts 1..=4 with 1 000 000 quote each.
fn ex4() -> Ex {
    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3, 4], 1_000_000);
    ex
}

fn deposit_raw(ex: &mut Ex, account: AccountId, raw: i128) {
    ex.apply(EventBody::Deposit {
        account,
        amount: Money(raw),
    })
    .unwrap();
}

/// Oracle update with a raw (micro-quote) index price.
fn oracle_raw(ex: &mut Ex, raw: u64) -> Vec<Output> {
    ex.ts += 1;
    let ts = ex.ts;
    ex.engine
        .apply(&Event {
            ts,
            body: EventBody::Oracle {
                market: M,
                index_price: Price(raw),
                source_ts: ts,
            },
        })
        .unwrap()
}

fn lim(
    ex: &mut Ex,
    account: AccountId,
    side: Side,
    price: Price,
    milli: u64,
    tif: Tif,
) -> Result<Vec<Output>, Error> {
    let p = ex.order(account, side, OrderKind::Limit { price, tif }, milli);
    ex.place(p)
}

fn rest(ex: &mut Ex, account: AccountId, side: Side, price: u64, milli: u64) -> OrderId {
    ex.gtc(account, side, price, milli)
}

fn replace(
    ex: &mut Ex,
    account: AccountId,
    id: OrderId,
    price: Option<Price>,
    q: Option<Qty>,
) -> Result<Vec<Output>, Error> {
    ex.apply(EventBody::Replace {
        account,
        order_id: id,
        price,
        qty: q,
    })
}

fn rested(outs: &[Output]) -> bool {
    has(outs, |o| matches!(o, Output::Rested { .. }))
}

fn canceled_reasons(outs: &[Output]) -> Vec<(OrderId, CancelReason)> {
    outs.iter()
        .filter_map(|o| match o {
            Output::Canceled {
                order_id, reason, ..
            } => Some((*order_id, *reason)),
            _ => None,
        })
        .collect()
}

/// `(maker order, price in whole quote, quantity in milli)` of every fill.
fn fill_list(outs: &[Output]) -> Vec<(OrderId, u64, u64)> {
    outs.iter()
        .filter_map(|o| match o {
            Output::Fill {
                maker_order,
                price,
                qty,
                ..
            } => Some((*maker_order, price.0 / 1_000_000, qty.0 / 10)),
            _ => None,
        })
        .collect()
}

fn mark_updated(index: Price) -> Output {
    Output::MarkUpdated {
        market: M,
        index,
        mark: index,
        clamped: false,
    }
}

fn conserved(ex: &Ex) {
    let e = &ex.engine;
    assert_eq!(
        e.total_collateral().0 + e.insurance_fund().0 + e.fee_pool().0 - e.total_basis().0,
        e.net_deposits().0 + e.bad_debt().0
    );
}

/// A fresh account whose collateral is exactly `raw`, so that margin
/// boundaries can be hit to the micro-unit.
fn exact_account(ex: &mut Ex, account: AccountId, raw: i128) {
    deposit_raw(ex, account, raw);
}

// ---------------------------------------------------------------------------
// band edges
// ---------------------------------------------------------------------------

#[test]
fn buy_at_the_band_edge_is_accepted_and_one_tick_above_is_rejected() {
    let mut ex = ex4();
    // floor(50_000 * 1.025) = 51_250
    let edge = px(51_250);
    let outs = lim(&mut ex, 1, Side::Buy, edge, 1_000, Tif::Gtc).unwrap();
    assert!(rested(&outs));
    assert_eq!(ex.engine.best_price(M, Side::Buy), Some(edge));
    assert_eq!(
        lim(
            &mut ex,
            1,
            Side::Buy,
            Price(edge.0 + 10_000),
            1_000,
            Tif::Gtc
        ),
        Err(Error::PriceOutsideBand)
    );
    assert_eq!(ex.engine.open_orders_of(1).len(), 1);
    ex.check();
}

#[test]
fn sell_at_the_band_edge_is_accepted_and_one_tick_below_is_rejected() {
    let mut ex = ex4();
    // ceil(50_000 * 0.975) = 48_750
    let edge = px(48_750);
    let outs = lim(&mut ex, 1, Side::Sell, edge, 1_000, Tif::Gtc).unwrap();
    assert!(rested(&outs));
    assert_eq!(ex.engine.best_price(M, Side::Sell), Some(edge));
    assert_eq!(
        lim(
            &mut ex,
            1,
            Side::Sell,
            Price(edge.0 - 10_000),
            1_000,
            Tif::Gtc
        ),
        Err(Error::PriceOutsideBand)
    );
    assert_eq!(ex.engine.open_orders_of(1).len(), 1);
    ex.check();
}

#[test]
fn band_edge_rounds_so_that_the_band_is_never_wider_than_configured() {
    let mut ex = ex4();
    // mark 50_000.07: buy edge floor(50_000_070_000 * 1.025) = 51_250_071_750,
    // so the highest valid tick is 51_250_070_000; sell edge is exactly
    // 48_750_068_250, so the lowest valid tick is 48_750_070_000.
    let outs = oracle_raw(&mut ex, 50_000_070_000);
    assert_eq!(outs, vec![mark_updated(Price(50_000_070_000))]);
    assert!(rested(
        &lim(
            &mut ex,
            1,
            Side::Buy,
            Price(51_250_070_000),
            1_000,
            Tif::Gtc
        )
        .unwrap()
    ));
    assert_eq!(
        lim(
            &mut ex,
            1,
            Side::Buy,
            Price(51_250_080_000),
            1_000,
            Tif::Gtc
        ),
        Err(Error::PriceOutsideBand)
    );
    ex.cancel(1, ex.engine.open_orders_of(1)[0]).unwrap();
    assert!(rested(
        &lim(
            &mut ex,
            1,
            Side::Sell,
            Price(48_750_070_000),
            1_000,
            Tif::Gtc
        )
        .unwrap()
    ));
    assert_eq!(
        lim(
            &mut ex,
            1,
            Side::Sell,
            Price(48_750_060_000),
            1_000,
            Tif::Gtc
        ),
        Err(Error::PriceOutsideBand)
    );
    ex.check();
}

#[test]
fn every_time_in_force_is_band_checked_even_on_an_empty_book() {
    let mut ex = ex4();
    let too_high = Price(px(51_250).0 + 10_000);
    let too_low = Price(px(48_750).0 - 10_000);
    for tif in [Tif::Gtc, Tif::Ioc, Tif::Fok, Tif::PostOnly] {
        assert_eq!(
            lim(&mut ex, 1, Side::Buy, too_high, 1_000, tif),
            Err(Error::PriceOutsideBand),
            "buy {tif:?}"
        );
        assert_eq!(
            lim(&mut ex, 1, Side::Sell, too_low, 1_000, tif),
            Err(Error::PriceOutsideBand),
            "sell {tif:?}"
        );
    }
    assert!(ex.engine.book_view(M, 5).unwrap().bids.is_empty());
    ex.check();
}

#[test]
fn passive_far_prices_are_accepted_on_both_sides() {
    let mut ex = ex4();
    // buy far below the mark and sell far above it: no band applies
    let b = lim(&mut ex, 1, Side::Buy, px(10_000), 1_000, Tif::Gtc).unwrap();
    let s = lim(&mut ex, 2, Side::Sell, px(90_000), 1_000, Tif::Gtc).unwrap();
    assert!(rested(&b) && rested(&s));
    let v = ex.engine.book_view(M, 1).unwrap();
    assert_eq!(v.bids[0].price, px(10_000));
    assert_eq!(v.asks[0].price, px(90_000));
    ex.check();
}

#[test]
fn invalid_price_is_reported_before_the_band_check() {
    let mut ex = ex4();
    // far above the band AND off the tick: the tick error wins
    assert_eq!(
        lim(
            &mut ex,
            1,
            Side::Buy,
            Price(80_000_000_005_000),
            1_000,
            Tif::Gtc
        ),
        Err(Error::InvalidPrice)
    );
    assert_eq!(
        lim(&mut ex, 1, Side::Buy, Price(0), 1_000, Tif::Gtc),
        Err(Error::InvalidPrice)
    );
    assert_eq!(
        lim(
            &mut ex,
            1,
            Side::Sell,
            Price(1_000_005_000),
            1_000,
            Tif::Gtc
        ),
        Err(Error::InvalidPrice)
    );
    // on the tick the band error is what remains
    assert_eq!(
        lim(&mut ex, 1, Side::Buy, px(80_000), 1_000, Tif::Gtc),
        Err(Error::PriceOutsideBand)
    );
    assert_eq!(
        lim(&mut ex, 1, Side::Sell, px(1), 1_000, Tif::Gtc),
        Err(Error::PriceOutsideBand)
    );
}

#[test]
fn band_check_needs_a_mark_price() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    ex.deposit(1, 1_000_000);
    assert_eq!(
        lim(&mut ex, 1, Side::Buy, px(50_000), 1_000, Tif::Gtc),
        Err(Error::NoMarkPrice)
    );
    // the tick check still comes first
    assert_eq!(
        lim(
            &mut ex,
            1,
            Side::Buy,
            Price(50_000_005_000),
            1_000,
            Tif::Gtc
        ),
        Err(Error::InvalidPrice)
    );
    ex.oracle_on(M, 50_000);
    assert!(rested(
        &lim(&mut ex, 1, Side::Buy, px(50_000), 1_000, Tif::Gtc).unwrap()
    ));
}

#[test]
fn band_rejection_consumes_no_id_and_frees_the_client_id() {
    let mut ex = ex4();
    let mut p = ex.order(
        1,
        Side::Buy,
        OrderKind::Limit {
            price: px(55_000),
            tif: Tif::Gtc,
        },
        1_000,
    );
    p.client_id = 77;
    let hash_before = ex.engine.state_hash();
    assert_eq!(ex.place(p), Err(Error::PriceOutsideBand));
    assert_ne!(hash_before, ex.engine.state_hash()); // only the event counter moved
                                                     // the rejected order's client id and order id are untouched
    let mut ok = p;
    ok.kind = OrderKind::Limit {
        price: px(51_000),
        tif: Tif::Gtc,
    };
    let first = accepted_id(&ex.place(ok).unwrap());
    assert_eq!(first, 1);
    ex.check();
}

#[test]
fn band_edges_follow_the_mark() {
    let mut ex = ex4();
    oracle_raw(&mut ex, px(52_000).0);
    // mark 52_000: buy edge 53_300, sell edge 50_700
    assert!(rested(
        &lim(&mut ex, 1, Side::Buy, px(53_300), 1_000, Tif::Gtc).unwrap()
    ));
    assert_eq!(
        lim(&mut ex, 1, Side::Buy, px(53_310), 1_000, Tif::Gtc),
        Err(Error::PriceOutsideBand)
    );
    // 51_250 is no longer the edge, 50_700 is the lowest sell
    ex.engine.best_price(M, Side::Buy).unwrap();
    assert_eq!(
        lim(&mut ex, 2, Side::Sell, px(50_690), 1_000, Tif::Gtc),
        Err(Error::PriceOutsideBand)
    );
    ex.cancel(1, ex.engine.open_orders_of(1)[0]).unwrap();
    assert!(rested(
        &lim(&mut ex, 2, Side::Sell, px(50_700), 1_000, Tif::Gtc).unwrap()
    ));
    ex.check();
}

// ---------------------------------------------------------------------------
// market orders: protection price measured from the mark
// ---------------------------------------------------------------------------

#[test]
fn market_buy_with_a_far_best_quote_does_not_fill_and_is_fully_cancelled() {
    let mut ex = ex4();
    rest(&mut ex, 2, Side::Sell, 51_000, 1_000);
    // 100 bps from the MARK: cap 50_500 < best ask 51_000
    let outs = ex.market(1, Side::Buy, 1_000, 100).unwrap();
    let id = accepted_id(&outs);
    assert_eq!(
        outs,
        vec![
            Output::Accepted {
                order_id: id,
                account: 1,
                market: M,
                client_id: 2,
            },
            Output::Canceled {
                order_id: id,
                account: 1,
                qty: qty(1_000),
                reason: CancelReason::Remainder,
            },
        ]
    );
    assert_eq!(ex.size(1), 0);
    assert_eq!(ex.engine.best_price(M, Side::Sell), Some(px(51_000)));
    ex.check();
}

#[test]
fn market_buy_fills_only_inside_the_cap_and_cancels_the_remainder() {
    let mut ex = ex4();
    let a = rest(&mut ex, 2, Side::Sell, 50_300, 1_000);
    let b = rest(&mut ex, 3, Side::Sell, 50_600, 1_000);
    let outs = ex.market(1, Side::Buy, 2_000, 100).unwrap(); // cap 50_500
    let id = accepted_id(&outs);
    assert_eq!(fill_list(&outs), vec![(a, 50_300, 1_000)]);
    assert_eq!(canceled_reasons(&outs), vec![(id, CancelReason::Remainder)]);
    assert_eq!(ex.engine.order(M, b).unwrap().qty, qty(1_000).0);
    assert_eq!(ex.size(1), qty(1_000).0 as i64);
    ex.check();
    conserved(&ex);
}

#[test]
fn market_cap_boundary_is_inclusive() {
    let mut ex = ex4();
    // 100 bps above 50_000 is exactly 50_500
    let a = rest(&mut ex, 2, Side::Sell, 50_500, 1_000);
    let outs = ex.market(1, Side::Buy, 1_000, 100).unwrap();
    assert_eq!(fill_list(&outs), vec![(a, 50_500, 1_000)]);
    // one tick above does not
    rest(&mut ex, 2, Side::Sell, 51_000, 1_000);
    let b = lim(
        &mut ex,
        3,
        Side::Sell,
        Price(px(50_500).0 + 10_000),
        1_000,
        Tif::Gtc,
    )
    .unwrap();
    let outs = ex.market(1, Side::Buy, 1_000, 100).unwrap();
    assert!(fill_list(&outs).is_empty());
    assert!(rested(&b));
    ex.check();
}

#[test]
fn market_order_with_zero_slippage_is_capped_at_the_mark_itself() {
    let mut ex = ex4();
    // previously the cap would have been the best ask itself and filled
    rest(&mut ex, 2, Side::Sell, 50_300, 1_000);
    let outs = ex.market(1, Side::Buy, 1_000, 0).unwrap();
    assert!(fill_list(&outs).is_empty());
    assert_eq!(canceled_reasons(&outs).len(), 1);
    // an ask exactly at the mark is taken
    let at_mark = rest(&mut ex, 3, Side::Sell, 50_000, 1_000);
    let outs = ex.market(1, Side::Buy, 1_000, 0).unwrap();
    assert_eq!(fill_list(&outs), vec![(at_mark, 50_000, 1_000)]);
    ex.check();
}

#[test]
fn market_slippage_is_clamped_to_the_band() {
    let mut ex = ex4();
    // slippage 5000 bps would reach 75_000, the band limits it to 51_250
    rest(&mut ex, 2, Side::Sell, 60_000, 1_000);
    let outs = ex.market(1, Side::Buy, 1_000, 5_000).unwrap();
    assert!(fill_list(&outs).is_empty());
    assert_eq!(canceled_reasons(&outs).len(), 1);
    let edge = rest(&mut ex, 3, Side::Sell, 51_250, 1_000);
    let outs = ex.market(1, Side::Buy, 2_000, 5_000).unwrap();
    assert_eq!(fill_list(&outs), vec![(edge, 51_250, 1_000)]);
    assert_eq!(ex.engine.best_price(M, Side::Sell), Some(px(60_000)));
    ex.check();
}

#[test]
fn market_sell_cap_is_measured_from_the_mark_and_clamped_to_the_band() {
    let mut ex = ex4();
    let near = rest(&mut ex, 2, Side::Buy, 49_600, 1_000);
    let mid = rest(&mut ex, 3, Side::Buy, 49_400, 1_000);
    let far = rest(&mut ex, 4, Side::Buy, 40_000, 1_000);
    // 100 bps: floor price ceil(50_000 * 0.99) = 49_500
    let outs = ex.market(1, Side::Sell, 2_000, 100).unwrap();
    assert_eq!(fill_list(&outs), vec![(near, 49_600, 1_000)]);
    assert_eq!(canceled_reasons(&outs).len(), 1);
    // slippage 5000 bps would allow 25_000, the band floor is 48_750: takes
    // the 49_400 bid but never the passive 40_000 one
    let outs = ex.market(1, Side::Sell, 2_000, 5_000).unwrap();
    assert_eq!(fill_list(&outs), vec![(mid, 49_400, 1_000)]);
    assert_eq!(canceled_reasons(&outs).len(), 1);
    assert_eq!(ex.engine.order(M, far).unwrap().qty, qty(1_000).0);
    let edge = rest(&mut ex, 2, Side::Buy, 48_750, 1_000);
    let outs = ex.market(1, Side::Sell, 3_000, 5_000).unwrap();
    assert_eq!(fill_list(&outs), vec![(edge, 48_750, 1_000)]);
    assert_eq!(ex.engine.order(M, far).unwrap().qty, qty(1_000).0);
    ex.check();
    conserved(&ex);
}

#[test]
fn market_order_checks_that_still_apply() {
    let mut ex = ex4();
    assert_eq!(ex.market(1, Side::Buy, 1_000, 100), Err(Error::NoLiquidity));
    rest(&mut ex, 2, Side::Sell, 50_000, 1_000);
    assert_eq!(
        ex.market(1, Side::Buy, 1_000, 5_001),
        Err(Error::InvalidSlippage)
    );
    // a market order that stays empty-handed still gets an id
    let id = accepted_id(&ex.market(1, Side::Buy, 1_000, 0).unwrap());
    assert_eq!(id, 2);
}

// ---------------------------------------------------------------------------
// replace and the band
// ---------------------------------------------------------------------------

#[test]
fn replace_of_a_bid_is_band_checked_at_the_edge() {
    let mut ex = ex4();
    let id = rest(&mut ex, 1, Side::Buy, 49_000, 1_000);
    assert_eq!(
        replace(&mut ex, 1, id, Some(Price(px(51_250).0 + 10_000)), None),
        Err(Error::PriceOutsideBand)
    );
    assert_eq!(ex.engine.order(M, id).unwrap().price, px(49_000));
    assert_eq!(
        replace(&mut ex, 1, id, Some(px(51_250)), None),
        Ok(vec![Output::Replaced {
            order_id: id,
            kept_priority: false
        }])
    );
    assert_eq!(ex.engine.order(M, id).unwrap().price, px(51_250));
    ex.check();
}

#[test]
fn replace_of_an_ask_is_band_checked_at_the_edge() {
    let mut ex = ex4();
    let id = rest(&mut ex, 1, Side::Sell, 52_000, 1_000);
    assert_eq!(
        replace(&mut ex, 1, id, Some(Price(px(48_750).0 - 10_000)), None),
        Err(Error::PriceOutsideBand)
    );
    assert_eq!(
        replace(&mut ex, 1, id, Some(px(48_750)), None),
        Ok(vec![Output::Replaced {
            order_id: id,
            kept_priority: false
        }])
    );
    // moving back far above the mark is passive and fine
    assert!(replace(&mut ex, 1, id, Some(px(90_000)), None).is_ok());
    ex.check();
}

#[test]
fn replace_reports_would_cross_before_the_band_error_and_tick_error_first() {
    let mut ex = ex4();
    rest(&mut ex, 2, Side::Sell, 49_000, 1_000);
    let id = rest(&mut ex, 1, Side::Buy, 48_900, 1_000);
    // 52_000 is outside the band and crosses the ask: WouldCross is reported
    assert_eq!(
        replace(&mut ex, 1, id, Some(px(52_000)), None),
        Err(Error::WouldCross)
    );
    // an off-tick price is rejected before everything else
    assert_eq!(
        replace(&mut ex, 1, id, Some(Price(52_000_005_000)), None),
        Err(Error::InvalidPrice)
    );
    ex.check();
}

#[test]
fn quantity_only_replace_is_not_band_checked() {
    let mut ex = ex4();
    let id = rest(&mut ex, 1, Side::Buy, 51_250, 1_000);
    assert_eq!(
        replace(&mut ex, 1, id, None, Some(qty(500))),
        Ok(vec![Output::Replaced {
            order_id: id,
            kept_priority: true
        }])
    );
    ex.check();
}

#[test]
fn replace_reserves_the_adverse_loss_of_the_new_price_to_the_micro_unit() {
    // bid 1.0 at 50_000, then replace to 2.0 at 51_000:
    //   required initial margin on 2.0 at mark 50_000     5% of 1e11 = 5_000_000_000
    //   fee reserve  ceil(500ppm of notional(51_000, 2.0) = 1.02e11)   51_000_000
    //   adverse      (51_000 - 50_000) * 2.0                         2_000_000_000
    let need: i128 = 5_000_000_000 + 51_000_000 + 2_000_000_000;
    assert_eq!(need, 7_051_000_000);
    let mut ex = Ex::new();
    exact_account(&mut ex, 5, need);
    exact_account(&mut ex, 6, need - 1);
    let ok = rest(&mut ex, 5, Side::Buy, 50_000, 1_000);
    let bad = rest(&mut ex, 6, Side::Buy, 50_000, 1_000);
    assert_eq!(
        replace(&mut ex, 6, bad, Some(px(51_000)), Some(qty(2_000))),
        Err(Error::MarginBreach)
    );
    let o = ex.engine.order(M, bad).unwrap();
    assert_eq!((o.price, o.qty), (px(50_000), qty(1_000).0));
    assert_eq!(
        replace(&mut ex, 5, ok, Some(px(51_000)), Some(qty(2_000))),
        Ok(vec![Output::Replaced {
            order_id: ok,
            kept_priority: false
        }])
    );
    ex.check();
}

// ---------------------------------------------------------------------------
// execution-price loss reserve (adverse)
// ---------------------------------------------------------------------------

#[test]
fn resting_buy_above_the_mark_reserves_its_price_loss_exactly() {
    // 1.0 at 51_000, mark 50_000:
    //   required 5% * 5e10 = 2_500_000_000, fee ceil(500ppm * 5.1e10) = 25_500_000,
    //   adverse (51_000 - 50_000) * 1.0 = 1_000_000_000
    let need: i128 = 2_500_000_000 + 25_500_000 + 1_000_000_000;
    let mut ex = Ex::new();
    exact_account(&mut ex, 5, need);
    exact_account(&mut ex, 6, need - 1);
    assert_eq!(
        lim(&mut ex, 6, Side::Buy, px(51_000), 1_000, Tif::Gtc),
        Err(Error::MarginBreach)
    );
    assert!(ex.engine.open_orders_of(6).is_empty());
    assert!(rested(
        &lim(&mut ex, 5, Side::Buy, px(51_000), 1_000, Tif::Gtc).unwrap()
    ));
    ex.check();
}

#[test]
fn an_order_priced_at_the_mark_reserves_no_price_loss() {
    // required 2_500_000_000 + fee 25_000_000, adverse 0
    let need: i128 = 2_525_000_000;
    let mut ex = Ex::new();
    exact_account(&mut ex, 5, need);
    exact_account(&mut ex, 6, need - 1);
    assert_eq!(
        lim(&mut ex, 6, Side::Buy, px(50_000), 1_000, Tif::Gtc),
        Err(Error::MarginBreach)
    );
    assert!(rested(
        &lim(&mut ex, 5, Side::Buy, px(50_000), 1_000, Tif::Gtc).unwrap()
    ));
}

#[test]
fn resting_sell_below_the_mark_reserves_its_price_loss_exactly() {
    // 1.0 at 49_000: required 2_500_000_000, fee on max(49_000, mark) = 5e10 -> 25_000_000,
    // adverse (50_000 - 49_000) * 1.0 = 1_000_000_000
    let need: i128 = 2_500_000_000 + 25_000_000 + 1_000_000_000;
    let mut ex = Ex::new();
    exact_account(&mut ex, 5, need);
    exact_account(&mut ex, 6, need - 1);
    assert_eq!(
        lim(&mut ex, 6, Side::Sell, px(49_000), 1_000, Tif::Gtc),
        Err(Error::MarginBreach)
    );
    assert!(rested(
        &lim(&mut ex, 5, Side::Sell, px(49_000), 1_000, Tif::Gtc).unwrap()
    ));
    ex.check();
}

#[test]
fn passive_prices_earn_no_discount_on_the_reserve() {
    // a sell far above the mark would "gain" 1e9 but the loss is clamped at 0;
    // the fee is reserved on the higher limit price: ceil(500ppm * 5.1e10)
    let need: i128 = 2_500_000_000 + 25_500_000;
    let mut ex = Ex::new();
    exact_account(&mut ex, 5, need);
    exact_account(&mut ex, 6, need - 1);
    assert_eq!(
        lim(&mut ex, 6, Side::Sell, px(51_000), 1_000, Tif::Gtc),
        Err(Error::MarginBreach)
    );
    assert!(rested(
        &lim(&mut ex, 5, Side::Sell, px(51_000), 1_000, Tif::Gtc).unwrap()
    ));
    ex.check();
}

#[test]
fn multi_level_sweep_nets_price_gains_against_losses() {
    // asks 1.0 @ 49_500 (gain 5e8 for a buyer) and 1.0 @ 50_500 (loss 5e8): net 0.
    // required 5% * 1e11 = 5_000_000_000, fee ceil(500ppm * 1.01e11) = 50_500_000
    let need: i128 = 5_000_000_000 + 50_500_000;
    let mut ex = Ex::new();
    ex.fund(&[2, 3], 1_000_000);
    exact_account(&mut ex, 5, need);
    exact_account(&mut ex, 6, need - 1);
    let a = rest(&mut ex, 2, Side::Sell, 49_500, 1_000);
    let b = rest(&mut ex, 3, Side::Sell, 50_500, 1_000);
    assert_eq!(
        lim(&mut ex, 6, Side::Buy, px(50_500), 2_000, Tif::Ioc),
        Err(Error::MarginBreach)
    );
    assert_eq!(ex.size(6), 0);
    let outs = lim(&mut ex, 5, Side::Buy, px(50_500), 2_000, Tif::Ioc).unwrap();
    assert_eq!(
        fill_list(&outs),
        vec![(a, 49_500, 1_000), (b, 50_500, 1_000)]
    );
    assert_eq!(ex.size(5), qty(2_000).0 as i64);
    ex.check();
    conserved(&ex);
}

#[test]
fn multi_level_sweep_with_a_net_loss_reserves_the_difference() {
    // asks 1.0 @ 49_800 (gain 2e8) and 1.0 @ 50_500 (loss 5e8): net loss 3e8
    let need: i128 = 5_000_000_000 + 50_500_000 + 300_000_000;
    let mut ex = Ex::new();
    ex.fund(&[2, 3], 1_000_000);
    exact_account(&mut ex, 5, need);
    exact_account(&mut ex, 6, need - 1);
    rest(&mut ex, 2, Side::Sell, 49_800, 1_000);
    rest(&mut ex, 3, Side::Sell, 50_500, 1_000);
    assert_eq!(
        lim(&mut ex, 6, Side::Buy, px(50_500), 2_000, Tif::Ioc),
        Err(Error::MarginBreach)
    );
    let outs = lim(&mut ex, 5, Side::Buy, px(50_500), 2_000, Tif::Ioc).unwrap();
    assert_eq!(filled_qty(&outs), qty(2_000).0);
    ex.check();
    conserved(&ex);
}

#[test]
fn resting_remainder_adds_its_own_loss_to_the_fill_loss() {
    // ask 1.0 @ 50_500; buy 2.0 GTC @ 51_000:
    //   fill loss 5e8 + resting 1.0 at 51_000 loss 1e9 = 1_500_000_000
    //   required 5e9 (2.0 worst case), fee ceil(500ppm * 1.02e11) = 51_000_000
    let need: i128 = 5_000_000_000 + 51_000_000 + 1_500_000_000;
    let mut ex = Ex::new();
    ex.fund(&[2], 1_000_000);
    exact_account(&mut ex, 5, need);
    exact_account(&mut ex, 6, need - 1);
    let a = rest(&mut ex, 2, Side::Sell, 50_500, 1_000);
    assert_eq!(
        lim(&mut ex, 6, Side::Buy, px(51_000), 2_000, Tif::Gtc),
        Err(Error::MarginBreach)
    );
    let outs = lim(&mut ex, 5, Side::Buy, px(51_000), 2_000, Tif::Gtc).unwrap();
    assert_eq!(fill_list(&outs), vec![(a, 50_500, 1_000)]);
    assert!(rested(&outs));
    ex.check();
    conserved(&ex);
}

// ---------------------------------------------------------------------------
// closing orders and the negative-equity floor
// ---------------------------------------------------------------------------

/// Account 1 is long 1.0 from 50_000 with 2 600 quote; the oracle then falls
/// 5% to 47 500, leaving equity 2 575 - 2 500 = 75 quote (below maintenance,
/// but nothing can be liquidated against because there are no bids).
fn underwater_long() -> Ex {
    let mut ex = Ex::new();
    ex.fund(&[2], 1_000_000);
    ex.deposit(1, 2_600);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    oracle_raw(&mut ex, px(47_500).0);
    ex
}

#[test]
fn underwater_account_baseline_is_as_documented() {
    let ex = underwater_long();
    assert_eq!(ex.size(1), qty(1_000).0 as i64);
    assert_eq!(ex.engine.equity(1), Some(Money(75_000_000)));
    assert_eq!(ex.engine.mark_price(M), Some(px(47_500)));
    ex.check();
    conserved(&ex);
}

#[test]
fn closing_order_that_would_drive_equity_negative_is_rejected() {
    let mut ex = underwater_long();
    // reduce-only sell 1.0 at 46_400: loss (47_500 - 46_400) * 1.0 = 1_100 quote > 75
    let mut p = ex.order(
        1,
        Side::Sell,
        OrderKind::Limit {
            price: px(46_400),
            tif: Tif::Gtc,
        },
        1_000,
    );
    p.reduce_only = true;
    assert_eq!(ex.place(p), Err(Error::MarginBreach));
    assert!(ex.engine.open_orders_of(1).is_empty());
    assert_eq!(ex.size(1), qty(1_000).0 as i64);
    ex.check();
}

#[test]
fn closing_order_is_accepted_up_to_exactly_the_remaining_equity() {
    let mut ex = underwater_long();
    // loss per base at 47_400 is 100 quote = 1e8; equity 75e6 allows 0.75 base exactly
    let mut at_limit = ex.order(
        1,
        Side::Sell,
        OrderKind::Limit {
            price: px(47_400),
            tif: Tif::Gtc,
        },
        751,
    );
    at_limit.reduce_only = true;
    assert_eq!(ex.place(at_limit), Err(Error::MarginBreach)); // 75.1e6 > 75e6
    let mut exact = at_limit;
    exact.qty = qty(750);
    exact.client_id = 99;
    let outs = ex.place(exact).unwrap();
    assert!(rested(&outs));
    assert_eq!(ex.size(1), qty(1_000).0 as i64);
    ex.check();
    conserved(&ex);
}

#[test]
fn closing_order_filled_at_a_gain_is_not_hindered_by_low_equity() {
    let mut ex = underwater_long();
    // a bid far above liquidation reach is not needed: reduce-only sell at the
    // mark itself has no adverse loss, whatever the equity
    let mut p = ex.order(
        1,
        Side::Sell,
        OrderKind::Limit {
            price: px(47_500),
            tif: Tif::Gtc,
        },
        1_000,
    );
    p.reduce_only = true;
    assert!(rested(&ex.place(p).unwrap()));
    ex.check();
}

// ---------------------------------------------------------------------------
// sweep on oracle update
// ---------------------------------------------------------------------------

#[test]
fn oracle_update_cancels_bids_above_the_new_band_after_mark_updated() {
    let mut ex = ex4();
    let b1 = rest(&mut ex, 1, Side::Buy, 51_000, 1_000);
    let b2 = rest(&mut ex, 2, Side::Buy, 51_250, 1_000);
    let b3 = rest(&mut ex, 3, Side::Buy, 50_000, 1_000);
    let b4 = rest(&mut ex, 1, Side::Buy, 51_000, 500);
    // mark -> 49_000: buy edge floor(49_000 * 1.025) = 50_225
    let outs = oracle_raw(&mut ex, px(49_000).0);
    assert_eq!(
        outs,
        vec![
            mark_updated(px(49_000)),
            Output::Canceled {
                order_id: b2,
                account: 2,
                qty: qty(1_000),
                reason: CancelReason::PriceBand
            },
            Output::Canceled {
                order_id: b1,
                account: 1,
                qty: qty(1_000),
                reason: CancelReason::PriceBand
            },
            Output::Canceled {
                order_id: b4,
                account: 1,
                qty: qty(500),
                reason: CancelReason::PriceBand
            },
        ]
    );
    assert_eq!(ex.engine.open_orders_of(3), vec![b3]);
    assert!(ex.engine.open_orders_of(1).is_empty());
    assert_eq!(ex.engine.best_price(M, Side::Buy), Some(px(50_000)));
    ex.check();
}

#[test]
fn oracle_update_cancels_asks_below_the_new_band() {
    let mut ex = ex4();
    let a1 = rest(&mut ex, 1, Side::Sell, 48_750, 1_000);
    let a2 = rest(&mut ex, 2, Side::Sell, 49_000, 1_000);
    let a3 = rest(&mut ex, 3, Side::Sell, 50_700, 1_000);
    let a4 = rest(&mut ex, 1, Side::Sell, 48_750, 500);
    // mark -> 52_000: sell edge ceil(52_000 * 0.975) = 50_700 (a3 sits on it)
    let outs = oracle_raw(&mut ex, px(52_000).0);
    assert_eq!(outs[0], mark_updated(px(52_000)));
    assert_eq!(
        canceled_reasons(&outs),
        vec![
            (a1, CancelReason::PriceBand),
            (a4, CancelReason::PriceBand),
            (a2, CancelReason::PriceBand),
        ]
    );
    assert_eq!(outs.len(), 4);
    assert_eq!(ex.engine.open_orders_of(3), vec![a3]);
    assert_eq!(ex.engine.best_price(M, Side::Sell), Some(px(50_700)));
    ex.check();
}

#[test]
fn sweep_edge_is_inclusive_one_tick_beyond_is_cancelled() {
    let mut ex = ex4();
    let on_edge = rest(&mut ex, 1, Side::Buy, 50_225, 1_000);
    let beyond = lim(
        &mut ex,
        2,
        Side::Buy,
        Price(50_225_010_000),
        1_000,
        Tif::Gtc,
    )
    .unwrap();
    let beyond = accepted_id(&beyond);
    let outs = oracle_raw(&mut ex, px(49_000).0);
    assert_eq!(
        canceled_reasons(&outs),
        vec![(beyond, CancelReason::PriceBand)]
    );
    assert_eq!(ex.engine.open_orders_of(1), vec![on_edge]);
    ex.check();
}

#[test]
fn nothing_is_swept_when_the_mark_does_not_move_or_orders_stay_inside() {
    let mut ex = ex4();
    let b = rest(&mut ex, 1, Side::Buy, 51_250, 1_000);
    let a = rest(&mut ex, 2, Side::Sell, 52_000, 1_000);
    // same index: only MarkUpdated
    let outs = oracle_raw(&mut ex, px(50_000).0);
    assert_eq!(outs, vec![mark_updated(px(50_000))]);
    // mark up 1%: the bid edge moves up to 51_762, ask edge 49_237.5: nothing to cancel
    let outs = oracle_raw(&mut ex, px(50_500).0);
    assert_eq!(outs, vec![mark_updated(px(50_500))]);
    assert_eq!(ex.engine.open_orders_of(1), vec![b]);
    assert_eq!(ex.engine.open_orders_of(2), vec![a]);
    ex.check();
}

#[test]
fn sweep_releases_exposure_so_the_owner_can_order_again() {
    let mut ex = ex4();
    // 3 600 quote: enough for one 1.0 bid at 51_000 (needs 3 525.5), not for two
    deposit_raw(&mut ex, 5, 3_600_000_000);
    let first = rest(&mut ex, 5, Side::Buy, 51_000, 1_000);
    assert_eq!(
        lim(&mut ex, 5, Side::Buy, px(50_000), 1_000, Tif::Gtc),
        Err(Error::MarginBreach)
    );
    let outs = oracle_raw(&mut ex, px(49_000).0);
    assert_eq!(
        canceled_reasons(&outs),
        vec![(first, CancelReason::PriceBand)]
    );
    assert!(ex.engine.open_orders_of(5).is_empty());
    // exposure is gone: required 5% * 4.9e10 + fee 25e6 + adverse 1e9 fits
    let outs = lim(&mut ex, 5, Side::Buy, px(50_000), 1_000, Tif::Gtc).unwrap();
    assert!(rested(&outs));
    ex.check();
}

#[test]
fn sweep_leaves_the_fifo_of_surviving_orders_untouched() {
    let mut ex = ex4();
    let s1 = rest(&mut ex, 1, Side::Buy, 50_000, 1_000);
    let swept = rest(&mut ex, 2, Side::Buy, 51_000, 1_000);
    let s2 = rest(&mut ex, 3, Side::Buy, 50_000, 1_000);
    let s3 = rest(&mut ex, 4, Side::Buy, 50_000, 1_000);
    let outs = oracle_raw(&mut ex, px(49_000).0);
    assert_eq!(
        canceled_reasons(&outs),
        vec![(swept, CancelReason::PriceBand)]
    );
    // account 5 sells into the survivors: time order s1, s2, s3 is preserved
    deposit_raw(&mut ex, 5, usd(1_000_000).0);
    let outs = lim(&mut ex, 5, Side::Sell, px(50_000), 2_500, Tif::Ioc).unwrap();
    assert_eq!(
        fill_list(&outs),
        vec![(s1, 50_000, 1_000), (s2, 50_000, 1_000), (s3, 50_000, 500)]
    );
    ex.check();
    conserved(&ex);
}

// ---------------------------------------------------------------------------
// seeded flow property
// ---------------------------------------------------------------------------

#[test]
fn every_non_liquidation_fill_is_inside_the_band_of_the_mark_in_force() {
    let band: u128 = 250;
    let hi = |mark: u64| ((u128::from(mark) * (10_000 + band)) / 10_000) as u64;
    let lo = |mark: u64| (u128::from(mark) * (10_000 - band)).div_ceil(10_000) as u64;

    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3, 4, 5, 6], 10_000_000);
    let mut rng = Rng::new(0xBA7D_0001);
    let tifs = [Tif::Gtc, Tif::Ioc, Tif::Fok, Tif::PostOnly];
    let (mut fills_seen, mut band_rejects, mut swept) = (0u32, 0u32, 0u32);

    for step in 0..2_500u32 {
        let account = 1 + rng.below(6);
        let side = if rng.chance(1, 2) {
            Side::Buy
        } else {
            Side::Sell
        };
        let mark = ex.engine.mark_price(M).unwrap().0;
        let mut outs: Vec<Output> = Vec::new();
        match rng.below(10) {
            0..=5 => {
                // prices up to 4% around the mark, whole quote units
                let base = mark / 1_000_000;
                let span = base / 25;
                let price = px(base - span + rng.below(2 * span + 1));
                let tif = *rng.pick(&tifs).unwrap();
                match lim(&mut ex, account, side, price, 1 + rng.below(4), tif) {
                    Ok(o) => outs = o,
                    Err(Error::PriceOutsideBand) => band_rejects += 1,
                    Err(_) => {}
                }
            }
            6 | 7 => {
                if let Ok(o) = ex.market(account, side, 1 + rng.below(6), rng.below(400) as u32) {
                    outs = o;
                }
            }
            8 => {
                // oracle move of up to +-1.5%
                let index = ex.engine.index_price(M).unwrap().0 / 1_000_000;
                let delta = index * rng.below(31) / 2_000;
                let next = if rng.chance(1, 2) {
                    index + delta
                } else {
                    index - delta
                };
                outs = oracle_raw(&mut ex, px(next.max(1_000)).0);
                swept += canceled_reasons(&outs)
                    .iter()
                    .filter(|(_, r)| *r == CancelReason::PriceBand)
                    .count() as u32;
            }
            _ => {
                let _ = ex.apply(EventBody::CancelAll {
                    account,
                    market: None,
                });
            }
        }
        // oracle events and liquidations may move the mark; trades happen at
        // the mark in force during this event
        let mark_now = ex.engine.mark_price(M).unwrap().0;
        for o in &outs {
            if let Output::Fill {
                price, liquidation, ..
            } = o
            {
                if !*liquidation {
                    fills_seen += 1;
                    assert!(
                        price.0 >= lo(mark_now) && price.0 <= hi(mark_now),
                        "step {step}: fill {} outside [{}, {}] of mark {mark_now}",
                        price.0,
                        lo(mark_now),
                        hi(mark_now)
                    );
                }
            }
        }
        if let (Some(b), Some(a)) = (
            ex.engine.best_price(M, Side::Buy),
            ex.engine.best_price(M, Side::Sell),
        ) {
            assert!(b < a, "step {step}: crossed book");
        }
        if step % 250 == 0 {
            ex.check();
        }
    }
    assert!(fills_seen > 150, "fills {fills_seen}");
    assert!(band_rejects > 50, "band rejects {band_rejects}");
    assert!(swept > 0, "no order was ever swept");
    ex.check();
    conserved(&ex);
}

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

const BAND_ERR: &str = "price_band_bps must be 1..=first tier mmr_bps";

#[test]
fn band_bps_must_be_between_one_and_the_first_tier_mmr() {
    let mut cfg = MarketConfig::example(M);
    assert_eq!(cfg.tiers[0].mmr_bps, 250);
    for ok in [1, 250] {
        cfg.price_band_bps = ok;
        assert_eq!(cfg.validate(), Ok(()), "band {ok}");
    }
    for bad in [0, 251, 10_000, u32::MAX] {
        cfg.price_band_bps = bad;
        assert_eq!(
            cfg.validate(),
            Err(Error::InvalidConfig(BAND_ERR)),
            "band {bad}"
        );
    }
}

#[test]
fn band_upper_bound_follows_the_first_tier_maintenance_rate() {
    let mut cfg = MarketConfig::example(M);
    cfg.tiers[0] = MarginTier {
        up_to: cfg.tiers[0].up_to,
        imr_bps: 600,
        mmr_bps: 300,
    };
    cfg.price_band_bps = 300;
    assert_eq!(cfg.validate(), Ok(()));
    cfg.price_band_bps = 301;
    assert_eq!(cfg.validate(), Err(Error::InvalidConfig(BAND_ERR)));
}

#[test]
fn add_market_applies_the_band_validation() {
    let mut ex = Ex::bare();
    let mut cfg = MarketConfig::example(M);
    cfg.price_band_bps = 0;
    assert_eq!(
        ex.apply(EventBody::AddMarket(cfg.clone())),
        Err(Error::InvalidConfig(BAND_ERR))
    );
    cfg.price_band_bps = 251;
    assert_eq!(
        ex.apply(EventBody::AddMarket(cfg.clone())),
        Err(Error::InvalidConfig(BAND_ERR))
    );
    assert!(ex.engine.market_ids().is_empty());
    cfg.price_band_bps = 250;
    assert_eq!(
        ex.apply(EventBody::AddMarket(cfg)),
        Ok(vec![Output::MarketAdded { market: M }])
    );
}

#[test]
fn a_tighter_configured_band_is_honoured() {
    let mut cfg = MarketConfig::example(M);
    cfg.price_band_bps = 100; // 1%
    let mut ex = Ex::with_market(cfg);
    ex.fund(&[1], 1_000_000);
    assert!(rested(
        &lim(&mut ex, 1, Side::Buy, px(50_500), 1_000, Tif::Gtc).unwrap()
    ));
    assert_eq!(
        lim(&mut ex, 1, Side::Buy, px(50_510), 1_000, Tif::Gtc),
        Err(Error::PriceOutsideBand)
    );
    assert_eq!(
        lim(&mut ex, 1, Side::Sell, px(49_490), 1_000, Tif::Gtc),
        Err(Error::PriceOutsideBand)
    );
    ex.check();
}

#[test]
fn price_band_bps_round_trips_through_the_codec() {
    let mut cfg = MarketConfig::example(M);
    cfg.price_band_bps = 123;
    let back: MarketConfig = from_bytes(&to_bytes(&cfg)).unwrap();
    assert_eq!(back, cfg);
    assert_eq!(back.price_band_bps, 123);

    // as part of an event
    let ev = Event {
        ts: 5,
        body: EventBody::AddMarket(cfg.clone()),
    };
    let back: Event = from_bytes(&to_bytes(&ev)).unwrap();
    assert_eq!(back, ev);

    // the field is encoded immediately after max_mark_deviation_bps (4-byte ints)
    let mut a = MarketConfig::example(M);
    let mut b = a.clone();
    a.max_mark_deviation_bps = 300;
    a.price_band_bps = 250;
    b.max_mark_deviation_bps = 301;
    b.price_band_bps = 251;
    let (ea, eb) = (to_bytes(&a), to_bytes(&b));
    assert_eq!(ea.len(), eb.len());
    let diffs: Vec<usize> = (0..ea.len()).filter(|i| ea[*i] != eb[*i]).collect();
    assert_eq!(diffs.len(), 2);
    assert_eq!(diffs[1] - diffs[0], 4);
}

#[test]
fn truncated_market_config_is_rejected_by_the_decoder() {
    let bytes = to_bytes(&MarketConfig::example(M));
    assert!(from_bytes::<MarketConfig>(&bytes[..bytes.len() - 1]).is_err());
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(from_bytes::<MarketConfig>(&longer).is_err());
    // a config as before the field was introduced (4 bytes fewer) cannot decode
    assert!(from_bytes::<MarketConfig>(&bytes[..bytes.len() - 4]).is_err());
}
