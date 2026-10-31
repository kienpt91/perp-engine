//! Domain tests: order entry, order book, matching, cancel/replace, self-trade
//! prevention and the fee outputs of fills.
//!
//! Units: `px(n)` is `n` quote, `qty(m)` is `m` milli-base (1 lot), one base is
//! `qty(1_000)`. At a price of 50 000, one base has a notional of 50 000 quote,
//! i.e. `5e10` raw money units.
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
use perp_domain::fixed::{Money, Price, Qty};
use perp_domain::rng::Rng;
use perp_domain::types::{
    AccountId, CancelReason, EventBody, MarketConfig, OrderId, OrderKind, Output, PlaceOrder, Side,
    StpMode, Tif,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Engine with accounts 1..=4 funded with 1 000 000 quote each.
fn ex4() -> Ex {
    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3, 4], 1_000_000);
    ex
}

fn ex4_with(cfg: MarketConfig) -> Ex {
    let mut ex = Ex::with_market(cfg);
    ex.fund(&[1, 2, 3, 4], 1_000_000);
    ex
}

fn limit_at(
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

fn ask(ex: &mut Ex, account: AccountId, price: u64, milli: u64) -> OrderId {
    ex.gtc(account, Side::Sell, price, milli)
}

fn bid(ex: &mut Ex, account: AccountId, price: u64, milli: u64) -> OrderId {
    ex.gtc(account, Side::Buy, price, milli)
}

fn ro_order(
    ex: &mut Ex,
    account: AccountId,
    side: Side,
    price: u64,
    milli: u64,
    tif: Tif,
) -> PlaceOrder {
    let mut p = ex.order(
        account,
        side,
        OrderKind::Limit {
            price: px(price),
            tif,
        },
        milli,
    );
    p.reduce_only = true;
    p
}

fn stp_order(
    ex: &mut Ex,
    account: AccountId,
    side: Side,
    price: u64,
    milli: u64,
    tif: Tif,
    stp: StpMode,
) -> PlaceOrder {
    let mut p = ex.order(
        account,
        side,
        OrderKind::Limit {
            price: px(price),
            tif,
        },
        milli,
    );
    p.stp = stp;
    p
}

/// Outputs without the leading `Accepted`.
fn body(outs: &[Output]) -> Vec<Output> {
    outs.iter()
        .filter(|o| !matches!(o, Output::Accepted { .. }))
        .cloned()
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn fill(
    taker_order: OrderId,
    maker_order: OrderId,
    taker: AccountId,
    maker: AccountId,
    price: Price,
    milli: u64,
    taker_side: Side,
    taker_fee: i128,
    maker_fee: i128,
) -> Output {
    Output::Fill {
        market: M,
        taker_order,
        maker_order,
        taker,
        maker,
        price,
        qty: qty(milli),
        taker_side,
        taker_fee: Money(taker_fee),
        maker_fee: Money(maker_fee),
        liquidation: false,
    }
}

fn canceled(order_id: OrderId, account: AccountId, milli: u64, reason: CancelReason) -> Output {
    Output::Canceled {
        order_id,
        account,
        qty: qty(milli),
        reason,
    }
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

fn assert_conserved(ex: &Ex) {
    let e = &ex.engine;
    assert_eq!(
        e.total_collateral().0 + e.insurance_fund().0 + e.fee_pool().0 - e.total_basis().0,
        e.net_deposits().0 + e.bad_debt().0
    );
}

fn best(ex: &Ex, side: Side) -> Option<Price> {
    ex.engine.best_price(M, side)
}

fn resting_qty(ex: &Ex, id: OrderId) -> Option<u64> {
    ex.engine.order(M, id).map(|o| o.qty)
}

fn is_rested(outs: &[Output]) -> bool {
    has(outs, |o| matches!(o, Output::Rested { .. }))
}

fn is_canceled(outs: &[Output]) -> bool {
    has(outs, |o| matches!(o, Output::Canceled { .. }))
}

// ---------------------------------------------------------------------------
// price-time priority and execution
// ---------------------------------------------------------------------------

#[test]
fn best_price_first_and_execution_at_maker_price() {
    let mut ex = ex4();
    let far = ask(&mut ex, 2, 50_000, 1_000);
    let near = ask(&mut ex, 3, 49_900, 1_000);
    // a buy limit of 50_000 crosses both levels but takes the cheaper first
    let outs = ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Gtc).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![fill(
            taker,
            near,
            1,
            3,
            px(49_900),
            1_000,
            Side::Buy,
            24_950_000, // ceil(49_900e6 * 500ppm)
            9_980_000   // ceil(49_900e6 * 200ppm)
        )]
    );
    // the 50_000 ask was not touched, the buyer holds exactly one base at 49_900
    assert_eq!(resting_qty(&ex, far), Some(qty(1_000).0));
    assert_eq!(ex.size(1), qty(1_000).0 as i64);
    assert_eq!(ex.size(3), -10_000);
    assert!(ex.engine.order(M, near).is_none());
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn fifo_within_a_price_level() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 1_000);
    let b = ask(&mut ex, 3, 50_000, 1_000);
    let c = ask(&mut ex, 4, 50_000, 1_000);
    let outs = ex.limit(1, Side::Buy, 50_000, 2_000, Tif::Ioc).unwrap();
    assert_eq!(
        fill_list(&outs),
        vec![(a, 50_000, 1_000), (b, 50_000, 1_000)]
    );
    assert_eq!(resting_qty(&ex, c), Some(qty(1_000).0));
    assert!(!is_canceled(&outs));
    ex.check();
}

#[test]
fn partial_fill_leaves_maker_at_front_with_reduced_quantity() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 1_000);
    let b = ask(&mut ex, 3, 50_000, 1_000);
    let outs = ex.limit(1, Side::Buy, 50_000, 400, Tif::Ioc).unwrap();
    assert_eq!(fill_list(&outs), vec![(a, 50_000, 400)]);
    assert_eq!(resting_qty(&ex, a), Some(qty(600).0));
    assert_eq!(ex.engine.book_view(M, 1).unwrap().asks[0].qty, qty(1_600));
    // the partially filled maker keeps the front of the queue
    let outs = ex.limit(1, Side::Buy, 50_000, 700, Tif::Ioc).unwrap();
    assert_eq!(fill_list(&outs), vec![(a, 50_000, 600), (b, 50_000, 100)]);
    ex.check();
}

#[test]
fn market_order_sweeps_several_levels_at_each_makers_price() {
    let mut ex = ex4();
    let l1 = ask(&mut ex, 2, 50_000, 300);
    let l2 = ask(&mut ex, 3, 50_100, 300);
    let l3 = ask(&mut ex, 4, 50_200, 400);
    let outs = ex.market(1, Side::Buy, 1_000, 100).unwrap();
    assert_eq!(
        fill_list(&outs),
        vec![(l1, 50_000, 300), (l2, 50_100, 300), (l3, 50_200, 400)]
    );
    assert!(best(&ex, Side::Sell).is_none());
    assert_eq!(ex.size(1), qty(1_000).0 as i64);
    assert!(!is_canceled(&outs) && !is_rested(&outs));
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn sell_takes_highest_bid_first_at_maker_price() {
    let mut ex = ex4();
    let low = bid(&mut ex, 2, 49_900, 1_000);
    let high = bid(&mut ex, 3, 50_000, 1_000);
    let outs = ex.limit(1, Side::Sell, 49_900, 1_500, Tif::Ioc).unwrap();
    assert_eq!(
        fill_list(&outs),
        vec![(high, 50_000, 1_000), (low, 49_900, 500)]
    );
    assert_eq!(resting_qty(&ex, low), Some(qty(500).0));
    assert_eq!(ex.size(1), -(qty(1_500).0 as i64));
    ex.check();
}

#[test]
fn limit_that_does_not_cross_never_trades() {
    let mut ex = ex4();
    ask(&mut ex, 2, 50_100, 1_000);
    let outs = ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Gtc).unwrap();
    assert!(fills(&outs).is_empty());
    assert!(is_rested(&outs));
    assert_eq!(best(&ex, Side::Buy), Some(px(50_000)));
    assert_eq!(best(&ex, Side::Sell), Some(px(50_100)));
    ex.check();
}

// ---------------------------------------------------------------------------
// time in force
// ---------------------------------------------------------------------------

#[test]
fn gtc_remainder_rests_after_partial_fill() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 400);
    let outs = ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Gtc).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![
            fill(
                taker,
                a,
                1,
                2,
                px(50_000),
                400,
                Side::Buy,
                10_000_000,
                4_000_000
            ),
            Output::Rested {
                order_id: taker,
                qty: qty(600),
                price: px(50_000)
            },
        ]
    );
    assert_eq!(best(&ex, Side::Buy), Some(px(50_000)));
    assert_eq!(resting_qty(&ex, taker), Some(qty(600).0));
    assert_eq!(ex.engine.open_orders_of(1), vec![taker]);
    ex.check();
}

#[test]
fn fully_filled_gtc_neither_rests_nor_cancels() {
    let mut ex = ex4();
    ask(&mut ex, 2, 50_000, 1_000);
    let outs = ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Gtc).unwrap();
    assert_eq!(fills(&outs).len(), 1);
    assert!(!is_rested(&outs));
    assert!(!is_canceled(&outs));
    assert!(ex.engine.open_orders_of(1).is_empty());
}

#[test]
fn ioc_remainder_is_cancelled_with_remainder_reason() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 400);
    let outs = ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Ioc).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![
            fill(
                taker,
                a,
                1,
                2,
                px(50_000),
                400,
                Side::Buy,
                10_000_000,
                4_000_000
            ),
            canceled(taker, 1, 600, CancelReason::Remainder),
        ]
    );
    assert!(best(&ex, Side::Buy).is_none());
    assert!(ex.engine.open_orders_of(1).is_empty());
    ex.check();
}

#[test]
fn ioc_with_nothing_to_match_is_cancelled_entirely() {
    let mut ex = ex4();
    let outs = ex.limit(1, Side::Buy, 49_000, 1_000, Tif::Ioc).unwrap();
    let id = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![canceled(id, 1, 1_000, CancelReason::Remainder)]
    );
    assert!(ex.engine.book_view(M, 5).unwrap().bids.is_empty());
}

#[test]
fn post_only_rests_when_it_does_not_cross() {
    let mut ex = ex4();
    ask(&mut ex, 2, 50_000, 1_000);
    // one tick (0.01) below the best ask
    let one_tick_below = Price(px(50_000).0 - 10_000);
    let outs = limit_at(&mut ex, 1, Side::Buy, one_tick_below, 500, Tif::PostOnly).unwrap();
    let id = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![Output::Rested {
            order_id: id,
            qty: qty(500),
            price: one_tick_below
        }]
    );
    assert_eq!(best(&ex, Side::Buy), Some(one_tick_below));
    ex.check();
}

#[test]
fn post_only_that_would_cross_is_rejected_and_changes_nothing() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 1_000);
    let b = bid(&mut ex, 3, 49_900, 1_000);
    let hash = ex.engine.state_hash();
    // equal price counts as crossing
    assert_eq!(
        ex.limit(1, Side::Buy, 50_000, 500, Tif::PostOnly),
        Err(Error::WouldCross)
    );
    assert_eq!(
        ex.limit(1, Side::Buy, 50_100, 500, Tif::PostOnly),
        Err(Error::WouldCross)
    );
    assert_eq!(
        ex.limit(1, Side::Sell, 49_900, 500, Tif::PostOnly),
        Err(Error::WouldCross)
    );
    assert_eq!(resting_qty(&ex, a), Some(qty(1_000).0));
    assert_eq!(resting_qty(&ex, b), Some(qty(1_000).0));
    assert!(ex.engine.open_orders_of(1).is_empty());
    assert_ne!(hash, ex.engine.state_hash()); // event counter moved, book did not
    ex.check();
}

#[test]
fn fok_fills_completely_across_several_makers_and_leaves_no_residue() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 600);
    let b = ask(&mut ex, 3, 50_000, 300);
    let c = ask(&mut ex, 4, 50_100, 500);
    let outs = ex.limit(1, Side::Buy, 50_100, 1_200, Tif::Fok).unwrap();
    assert_eq!(
        fill_list(&outs),
        vec![(a, 50_000, 600), (b, 50_000, 300), (c, 50_100, 300)]
    );
    assert!(!is_rested(&outs) && !is_canceled(&outs));
    assert!(ex.engine.open_orders_of(1).is_empty());
    assert_eq!(resting_qty(&ex, c), Some(qty(200).0));
    ex.check();
}

#[test]
fn fok_that_cannot_fill_is_rejected_without_side_effects() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 600);
    let b = ask(&mut ex, 3, 50_100, 300);
    // only 900 available
    assert_eq!(
        ex.limit(1, Side::Buy, 50_100, 1_000, Tif::Fok),
        Err(Error::FokNotFillable)
    );
    assert_eq!(resting_qty(&ex, a), Some(qty(600).0));
    assert_eq!(resting_qty(&ex, b), Some(qty(300).0));
    assert_eq!(ex.size(1), 0);
    assert!(ex.engine.open_orders_of(1).is_empty());
    // the same quantity that is available works
    let outs = ex.limit(1, Side::Buy, 50_100, 900, Tif::Fok).unwrap();
    assert_eq!(filled_qty(&outs), qty(900).0);
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn fok_does_not_count_liquidity_beyond_its_limit_price() {
    let mut ex = ex4();
    ask(&mut ex, 2, 50_000, 600);
    ask(&mut ex, 3, 50_100, 600);
    assert_eq!(
        ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Fok),
        Err(Error::FokNotFillable)
    );
    assert_eq!(
        ex.limit(1, Side::Buy, 50_100, 1_000, Tif::Fok)
            .map(|o| filled_qty(&o)),
        Ok(qty(1_000).0)
    );
}

#[test]
fn fok_on_empty_book_is_not_fillable() {
    let mut ex = ex4();
    assert_eq!(
        ex.limit(1, Side::Buy, 50_000, 100, Tif::Fok),
        Err(Error::FokNotFillable)
    );
}

#[test]
fn fok_is_not_fillable_when_own_resting_order_is_in_the_way() {
    let mut ex = ex4();
    let own = ask(&mut ex, 1, 50_000, 1_000);
    ask(&mut ex, 2, 50_000, 1_000); // behind the own order
    assert_eq!(
        ex.limit(1, Side::Buy, 50_000, 500, Tif::Fok),
        Err(Error::FokNotFillable)
    );
    assert_eq!(resting_qty(&ex, own), Some(qty(1_000).0));
}

#[test]
fn fok_with_cancel_oldest_skips_own_order_and_fills_the_rest() {
    let mut ex = ex4();
    let own = ask(&mut ex, 1, 50_000, 1_000);
    let other = ask(&mut ex, 2, 50_000, 1_000);
    let p = stp_order(
        &mut ex,
        1,
        Side::Buy,
        50_000,
        500,
        Tif::Fok,
        StpMode::CancelOldest,
    );
    let outs = ex.place(p).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![
            canceled(own, 1, 1_000, CancelReason::SelfTrade),
            fill(
                taker,
                other,
                1,
                2,
                px(50_000),
                500,
                Side::Buy,
                12_500_000,
                5_000_000
            ),
        ]
    );
    ex.check();
}

// ---------------------------------------------------------------------------
// market orders
// ---------------------------------------------------------------------------

#[test]
fn market_order_on_empty_opposite_side_has_no_liquidity() {
    let mut ex = ex4();
    assert_eq!(ex.market(1, Side::Buy, 100, 100), Err(Error::NoLiquidity));
    assert_eq!(ex.market(1, Side::Sell, 100, 100), Err(Error::NoLiquidity));
    // liquidity on the same side does not help
    bid(&mut ex, 2, 49_900, 1_000);
    assert_eq!(ex.market(1, Side::Buy, 100, 100), Err(Error::NoLiquidity));
    ex.check();
}

#[test]
fn market_slippage_above_five_thousand_bps_is_invalid() {
    let mut ex = ex4();
    ask(&mut ex, 2, 50_000, 1_000);
    // checked before liquidity: invalid even on an empty side
    assert_eq!(
        ex.market(1, Side::Sell, 100, 5_001),
        Err(Error::InvalidSlippage)
    );
    assert_eq!(
        ex.market(1, Side::Buy, 100, 5_001),
        Err(Error::InvalidSlippage)
    );
    // the bound itself is accepted
    let outs = ex.market(1, Side::Buy, 100, 5_000).unwrap();
    assert_eq!(filled_qty(&outs), qty(100).0);
}

#[test]
fn market_buy_stops_at_protection_price_and_cancels_remainder() {
    let mut ex = ex4();
    let l1 = ask(&mut ex, 2, 50_000, 1_000);
    let l2 = ask(&mut ex, 3, 50_500, 1_000);
    let l3 = ask(&mut ex, 4, 50_600, 1_000);
    // 100 bps above the best ask 50_000 is 50_500: inclusive
    let outs = ex.market(1, Side::Buy, 3_000, 100).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        fill_list(&outs),
        vec![(l1, 50_000, 1_000), (l2, 50_500, 1_000)]
    );
    assert!(has(&outs, |o| *o
        == canceled(taker, 1, 1_000, CancelReason::Remainder)));
    assert!(!is_rested(&outs));
    assert_eq!(resting_qty(&ex, l3), Some(qty(1_000).0));
    assert!(ex.engine.open_orders_of(1).is_empty());
    ex.check();
}

#[test]
fn market_sell_stops_at_protection_price() {
    let mut ex = ex4();
    let l1 = bid(&mut ex, 2, 50_000, 1_000);
    let l2 = bid(&mut ex, 3, 49_500, 1_000);
    let l3 = bid(&mut ex, 4, 49_400, 1_000);
    let outs = ex.market(1, Side::Sell, 3_000, 100).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        fill_list(&outs),
        vec![(l1, 50_000, 1_000), (l2, 49_500, 1_000)]
    );
    assert!(has(&outs, |o| *o
        == canceled(taker, 1, 1_000, CancelReason::Remainder)));
    assert_eq!(resting_qty(&ex, l3), Some(qty(1_000).0));
    ex.check();
}

#[test]
fn market_order_with_zero_slippage_only_takes_the_best_price() {
    let mut ex = ex4();
    let l1 = ask(&mut ex, 2, 50_000, 500);
    let tick_up = Price(px(50_000).0 + 10_000);
    let p = ex.order(
        3,
        Side::Sell,
        OrderKind::Limit {
            price: tick_up,
            tif: Tif::Gtc,
        },
        500,
    );
    ex.place(p).unwrap();
    let outs = ex.market(1, Side::Buy, 1_000, 0).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(fill_list(&outs), vec![(l1, 50_000, 500)]);
    assert!(has(&outs, |o| *o
        == canceled(taker, 1, 500, CancelReason::Remainder)));
    assert_eq!(best(&ex, Side::Sell), Some(tick_up));
}

#[test]
fn market_order_filled_completely_emits_no_cancel() {
    let mut ex = ex4();
    ask(&mut ex, 2, 50_000, 1_000);
    let outs = ex.market(1, Side::Buy, 1_000, 50).unwrap();
    assert_eq!(fills(&outs).len(), 1);
    assert!(!is_canceled(&outs));
    assert!(!is_rested(&outs));
}

#[test]
fn market_order_is_checked_for_minimum_notional_at_the_best_price() {
    let mut ex = ex4();
    // 3 milli at 5_000 = 15 quote: acceptable as a resting ask
    ask(&mut ex, 2, 5_000, 3);
    // 1 milli at the best price 5_000 = 5 quote < 10
    assert_eq!(
        ex.market(1, Side::Buy, 1, 100),
        Err(Error::BelowMinNotional)
    );
    assert_eq!(
        filled_qty(&ex.market(1, Side::Buy, 2, 100).unwrap()),
        qty(2).0
    );
}

// ---------------------------------------------------------------------------
// reduce-only
// ---------------------------------------------------------------------------

#[test]
fn reduce_only_is_rejected_when_flat() {
    let mut ex = ex4();
    bid(&mut ex, 2, 50_000, 1_000);
    let p = ro_order(&mut ex, 1, Side::Sell, 50_000, 500, Tif::Ioc);
    assert_eq!(ex.place(p), Err(Error::ReduceOnlyViolation));
    let p = ro_order(&mut ex, 1, Side::Buy, 49_000, 500, Tif::Gtc);
    assert_eq!(ex.place(p), Err(Error::ReduceOnlyViolation));
    assert_eq!(ex.size(1), 0);
}

#[test]
fn reduce_only_in_the_direction_of_the_position_is_rejected() {
    let mut ex = ex4();
    ex.trade(1, 2, Side::Buy, 50_000, 1_000); // account 1 is long 1.0
    ask(&mut ex, 3, 50_000, 1_000);
    let p = ro_order(&mut ex, 1, Side::Buy, 50_000, 500, Tif::Ioc);
    assert_eq!(ex.place(p), Err(Error::ReduceOnlyViolation));
    // account 2 is short: a reduce-only sell is wrong for it
    let p = ro_order(&mut ex, 2, Side::Sell, 49_000, 500, Tif::Gtc);
    assert_eq!(ex.place(p), Err(Error::ReduceOnlyViolation));
    // ... while the opposite directions are fine
    let p = ro_order(&mut ex, 1, Side::Sell, 51_000, 500, Tif::Gtc);
    assert!(ex.place(p).is_ok());
    let p = ro_order(&mut ex, 2, Side::Buy, 49_000, 500, Tif::Gtc);
    assert!(ex.place(p).is_ok());
    ex.check();
}

#[test]
fn reduce_only_quantity_is_clamped_to_the_position() {
    let mut ex = ex4();
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    let bid3 = bid(&mut ex, 3, 50_000, 3_000);
    let p = ro_order(&mut ex, 1, Side::Sell, 50_000, 5_000, Tif::Ioc);
    let outs = ex.place(p).unwrap();
    let taker = accepted_id(&outs);
    // clamped to 1.0: one fill, no remainder, position closed (never flipped)
    assert_eq!(
        body(&outs),
        vec![fill(
            taker,
            bid3,
            1,
            3,
            px(50_000),
            1_000,
            Side::Sell,
            25_000_000,
            10_000_000
        )]
    );
    assert_eq!(ex.size(1), 0);
    assert_eq!(resting_qty(&ex, bid3), Some(qty(2_000).0));
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn resting_reduce_only_order_is_clamped_on_entry() {
    let mut ex = ex4();
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    let p = ro_order(&mut ex, 1, Side::Sell, 50_100, 2_000, Tif::Gtc);
    let outs = ex.place(p).unwrap();
    let id = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![Output::Rested {
            order_id: id,
            qty: qty(1_000),
            price: px(50_100)
        }]
    );
    ex.check();
}

#[test]
fn resting_reduce_only_order_is_cancelled_when_the_position_vanished() {
    let mut ex = ex4();
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    let p = ro_order(&mut ex, 1, Side::Sell, 50_100, 1_000, Tif::Gtc);
    let ro_id = accepted_id(&ex.place(p).unwrap());
    // account 1 closes its position by other means
    bid(&mut ex, 3, 50_000, 1_000);
    ex.limit(1, Side::Sell, 50_000, 1_000, Tif::Ioc).unwrap();
    assert_eq!(ex.size(1), 0);
    assert_eq!(resting_qty(&ex, ro_id), Some(qty(1_000).0)); // lazily cleaned up

    // a buyer arrives: the stale reduce-only maker is removed, no fill happens
    let outs = ex.limit(4, Side::Buy, 50_100, 500, Tif::Gtc).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![
            canceled(ro_id, 1, 1_000, CancelReason::ReduceOnly),
            Output::Rested {
                order_id: taker,
                qty: qty(500),
                price: px(50_100)
            },
        ]
    );
    assert!(ex.engine.open_orders_of(1).is_empty());
    assert!(best(&ex, Side::Sell).is_none());
    ex.check();
}

#[test]
fn reduce_only_is_exempt_from_minimum_notional_but_regular_orders_are_not() {
    let mut ex = ex4();
    ex.trade(1, 2, Side::Buy, 50_000, 1); // 1 milli long
    bid(&mut ex, 3, 50_000, 5);
    // limit price 100: notional 0.1 quote < 10
    assert_eq!(
        ex.limit(1, Side::Sell, 100, 1, Tif::Ioc),
        Err(Error::BelowMinNotional)
    );
    let p = ro_order(&mut ex, 1, Side::Sell, 100, 1, Tif::Ioc);
    let outs = ex.place(p).unwrap();
    assert_eq!(fill_list(&outs).len(), 1);
    assert_eq!(filled_qty(&outs), qty(1).0);
    assert_eq!(ex.size(1), 0);
    ex.check();
}

// ---------------------------------------------------------------------------
// cancel / cancel-all
// ---------------------------------------------------------------------------

#[test]
fn cancel_removes_the_order_with_user_reason() {
    let mut ex = ex4();
    let id = bid(&mut ex, 1, 49_000, 1_000);
    let outs = ex.cancel(1, id).unwrap();
    assert_eq!(outs, vec![canceled(id, 1, 1_000, CancelReason::User)]);
    assert!(ex.engine.open_orders_of(1).is_empty());
    assert!(best(&ex, Side::Buy).is_none());
    assert!(ex.engine.order(M, id).is_none());
    ex.check();
}

#[test]
fn cancel_reports_only_the_open_quantity_of_a_partly_filled_order() {
    let mut ex = ex4();
    let id = ask(&mut ex, 2, 50_000, 1_000);
    ex.limit(1, Side::Buy, 50_000, 300, Tif::Ioc).unwrap();
    let outs = ex.cancel(2, id).unwrap();
    assert_eq!(outs, vec![canceled(id, 2, 700, CancelReason::User)]);
    ex.check();
}

#[test]
fn cancel_of_unknown_foreign_filled_or_already_cancelled_order_is_unknown_order() {
    let mut ex = ex4();
    let mine = bid(&mut ex, 1, 49_000, 1_000);
    let maker = ask(&mut ex, 2, 50_000, 500);
    ex.limit(1, Side::Buy, 50_000, 500, Tif::Ioc).unwrap(); // fills `maker`
    assert_eq!(ex.cancel(1, 9_999), Err(Error::UnknownOrder));
    assert_eq!(ex.cancel(2, mine), Err(Error::UnknownOrder)); // foreign
    assert_eq!(ex.cancel(2, maker), Err(Error::UnknownOrder)); // filled
    ex.cancel(1, mine).unwrap();
    assert_eq!(ex.cancel(1, mine), Err(Error::UnknownOrder)); // twice
    ex.check();
}

#[test]
fn cancel_for_unknown_account_is_rejected() {
    let mut ex = ex4();
    let id = bid(&mut ex, 1, 49_000, 1_000);
    assert_eq!(ex.cancel(99, id), Err(Error::UnknownAccount));
    assert_eq!(resting_qty(&ex, id), Some(qty(1_000).0));
}

fn place_on(ex: &mut Ex, market: u32, account: AccountId, price: u64, milli: u64) -> OrderId {
    let mut p = ex.order(
        account,
        Side::Buy,
        OrderKind::Limit {
            price: px(price),
            tif: Tif::Gtc,
        },
        milli,
    );
    p.market = market;
    accepted_id(&ex.place(p).unwrap())
}

#[test]
fn cancel_all_without_market_cancels_everything_in_id_order() {
    let mut ex = ex4();
    ex.apply(EventBody::AddMarket(MarketConfig::example(2)))
        .unwrap();
    ex.oracle_on(2, 50_000);
    let o1 = place_on(&mut ex, 1, 1, 49_000, 1_000);
    let o2 = place_on(&mut ex, 2, 1, 49_000, 1_000);
    let o3 = place_on(&mut ex, 1, 1, 48_900, 500);
    let other = place_on(&mut ex, 1, 2, 49_000, 1_000);
    let outs = ex
        .apply(EventBody::CancelAll {
            account: 1,
            market: None,
        })
        .unwrap();
    assert_eq!(
        outs,
        vec![
            canceled(o1, 1, 1_000, CancelReason::User),
            canceled(o2, 1, 1_000, CancelReason::User),
            canceled(o3, 1, 500, CancelReason::User),
        ]
    );
    assert!(ex.engine.open_orders_of(1).is_empty());
    assert_eq!(ex.engine.open_orders_of(2), vec![other]);
    ex.check();
}

#[test]
fn cancel_all_for_one_market_leaves_other_markets_untouched() {
    let mut ex = ex4();
    ex.apply(EventBody::AddMarket(MarketConfig::example(2)))
        .unwrap();
    ex.oracle_on(2, 50_000);
    let o1 = place_on(&mut ex, 1, 1, 49_000, 1_000);
    let o2 = place_on(&mut ex, 2, 1, 49_000, 1_000);
    let o3 = place_on(&mut ex, 1, 1, 48_900, 500);
    let outs = ex
        .apply(EventBody::CancelAll {
            account: 1,
            market: Some(M),
        })
        .unwrap();
    assert_eq!(
        outs,
        vec![
            canceled(o1, 1, 1_000, CancelReason::User),
            canceled(o3, 1, 500, CancelReason::User),
        ]
    );
    assert_eq!(ex.engine.open_orders_of(1), vec![o2]);
    assert!(ex.engine.best_price(2, Side::Buy).is_some());
    assert!(best(&ex, Side::Buy).is_none());
    ex.check();
}

#[test]
fn cancel_all_with_nothing_to_cancel_succeeds_silently() {
    let mut ex = ex4();
    assert_eq!(
        ex.apply(EventBody::CancelAll {
            account: 1,
            market: None
        }),
        Ok(vec![])
    );
    assert_eq!(
        ex.apply(EventBody::CancelAll {
            account: 1,
            market: Some(M)
        }),
        Ok(vec![])
    );
    assert_eq!(
        ex.apply(EventBody::CancelAll {
            account: 99,
            market: None
        }),
        Err(Error::UnknownAccount)
    );
}

// ---------------------------------------------------------------------------
// replace
// ---------------------------------------------------------------------------

/// Three asks of 1.0 at 50_000 (accounts 2, 3, 4 in time order).
fn queue3() -> (Ex, OrderId, OrderId, OrderId) {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 1_000);
    let b = ask(&mut ex, 3, 50_000, 1_000);
    let c = ask(&mut ex, 4, 50_000, 1_000);
    (ex, a, b, c)
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

#[test]
fn replace_with_smaller_quantity_keeps_time_priority() {
    let (mut ex, a, b, _c) = queue3();
    let outs = replace(&mut ex, 2, a, None, Some(qty(500))).unwrap();
    assert_eq!(
        outs,
        vec![Output::Replaced {
            order_id: a,
            kept_priority: true
        }]
    );
    assert_eq!(resting_qty(&ex, a), Some(qty(500).0));
    // A is still first in line
    let outs = ex.limit(1, Side::Buy, 50_000, 700, Tif::Ioc).unwrap();
    assert_eq!(fill_list(&outs), vec![(a, 50_000, 500), (b, 50_000, 200)]);
    ex.check();
}

#[test]
fn replace_with_larger_quantity_loses_time_priority() {
    let (mut ex, a, b, c) = queue3();
    let outs = replace(&mut ex, 2, a, None, Some(qty(1_500))).unwrap();
    assert_eq!(
        outs,
        vec![Output::Replaced {
            order_id: a,
            kept_priority: false
        }]
    );
    assert_eq!(resting_qty(&ex, a), Some(qty(1_500).0));
    let outs = ex.limit(1, Side::Buy, 50_000, 2_000, Tif::Ioc).unwrap();
    assert_eq!(
        fill_list(&outs),
        vec![(b, 50_000, 1_000), (c, 50_000, 1_000)]
    );
    assert_eq!(resting_qty(&ex, a), Some(qty(1_500).0));
    ex.check();
}

#[test]
fn replace_to_another_price_loses_priority_and_joins_the_back_of_that_level() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 1_000);
    let b = ask(&mut ex, 3, 50_100, 1_000);
    let c = ask(&mut ex, 4, 50_000, 1_000);
    // B moves into A and C's level: it queues behind both
    let outs = replace(&mut ex, 3, b, Some(px(50_000)), None).unwrap();
    assert_eq!(
        outs,
        vec![Output::Replaced {
            order_id: b,
            kept_priority: false
        }]
    );
    let view = ex.engine.book_view(M, 5).unwrap();
    assert_eq!(view.asks.len(), 1);
    assert_eq!(view.asks[0].qty, qty(3_000));
    assert_eq!(view.asks[0].orders, 3);
    let outs = ex.limit(1, Side::Buy, 50_000, 3_000, Tif::Ioc).unwrap();
    assert_eq!(
        fill_list(&outs),
        vec![(a, 50_000, 1_000), (c, 50_000, 1_000), (b, 50_000, 1_000)]
    );
    ex.check();
}

#[test]
fn replace_to_a_better_price_changes_the_best_price_and_loses_priority() {
    let (mut ex, a, _b, _c) = queue3();
    let better = Price(px(50_000).0 - 10_000);
    let outs = replace(&mut ex, 2, a, Some(better), None).unwrap();
    assert_eq!(
        outs,
        vec![Output::Replaced {
            order_id: a,
            kept_priority: false
        }]
    );
    assert_eq!(best(&ex, Side::Sell), Some(better));
    assert_eq!(ex.engine.order(M, a).unwrap().price, better);
    ex.check();
}

#[test]
fn replace_with_identical_price_and_quantity_is_a_noop_that_keeps_priority() {
    let (mut ex, a, b, _c) = queue3();
    let outs = replace(&mut ex, 2, a, Some(px(50_000)), Some(qty(1_000))).unwrap();
    assert_eq!(
        outs,
        vec![Output::Replaced {
            order_id: a,
            kept_priority: true
        }]
    );
    let outs = replace(&mut ex, 2, a, None, None).unwrap();
    assert_eq!(
        outs,
        vec![Output::Replaced {
            order_id: a,
            kept_priority: true
        }]
    );
    assert_eq!(resting_qty(&ex, a), Some(qty(1_000).0));
    let outs = ex.limit(1, Side::Buy, 50_000, 1_500, Tif::Ioc).unwrap();
    assert_eq!(fill_list(&outs), vec![(a, 50_000, 1_000), (b, 50_000, 500)]);
    ex.check();
}

#[test]
fn replace_that_would_cross_the_book_is_rejected() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 1_000);
    let b = bid(&mut ex, 1, 49_900, 1_000);
    assert_eq!(
        replace(&mut ex, 1, b, Some(px(50_000)), None),
        Err(Error::WouldCross)
    );
    assert_eq!(
        replace(&mut ex, 1, b, Some(px(50_100)), None),
        Err(Error::WouldCross)
    );
    assert_eq!(ex.engine.order(M, b).unwrap().price, px(49_900));
    // seller side: lowering through the bid
    assert_eq!(
        replace(&mut ex, 2, a, Some(px(49_900)), None),
        Err(Error::WouldCross)
    );
    // one tick short of crossing is fine
    let ok = Price(px(50_000).0 - 10_000);
    assert_eq!(
        replace(&mut ex, 1, b, Some(ok), None),
        Ok(vec![Output::Replaced {
            order_id: b,
            kept_priority: false
        }])
    );
    assert_eq!(ex.size(1), 0);
    ex.check();
}

#[test]
fn replace_validates_price_and_quantity() {
    let mut ex = ex4();
    let id = bid(&mut ex, 1, 49_900, 1_000);
    assert_eq!(
        replace(&mut ex, 1, id, Some(Price(0)), None),
        Err(Error::InvalidPrice)
    );
    assert_eq!(
        replace(&mut ex, 1, id, Some(Price(49_900_005_000)), None),
        Err(Error::InvalidPrice)
    );
    assert_eq!(
        replace(&mut ex, 1, id, None, Some(Qty(0))),
        Err(Error::ZeroQty)
    );
    assert_eq!(
        replace(&mut ex, 1, id, None, Some(Qty(5))),
        Err(Error::InvalidQty)
    );
    assert_eq!(
        replace(&mut ex, 1, id, None, Some(qty(100_001))),
        Err(Error::InvalidQty)
    );
    // 1.0 at price 1 = 1 quote < 10
    assert_eq!(
        replace(&mut ex, 1, id, Some(px(1)), None),
        Err(Error::BelowMinNotional)
    );
    // nothing changed
    let o = ex.engine.order(M, id).unwrap();
    assert_eq!((o.price, o.qty), (px(49_900), qty(1_000).0));
    ex.check();
}

#[test]
fn replace_of_unknown_foreign_or_filled_order_is_unknown_order() {
    let mut ex = ex4();
    let mine = bid(&mut ex, 1, 49_000, 1_000);
    let maker = ask(&mut ex, 2, 50_000, 500);
    ex.limit(1, Side::Buy, 50_000, 500, Tif::Ioc).unwrap();
    assert_eq!(
        replace(&mut ex, 1, 9_999, None, None),
        Err(Error::UnknownOrder)
    );
    assert_eq!(
        replace(&mut ex, 2, mine, None, None),
        Err(Error::UnknownOrder)
    );
    assert_eq!(
        replace(&mut ex, 2, maker, None, None),
        Err(Error::UnknownOrder)
    );
    assert_eq!(
        replace(&mut ex, 99, mine, None, None),
        Err(Error::UnknownAccount)
    );
}

#[test]
fn replace_on_a_stale_oracle_only_allows_changes_that_do_not_add_risk() {
    let mut ex = ex4();
    let id = bid(&mut ex, 1, 49_900, 1_000);
    ex.advance(70_000);
    assert_eq!(
        replace(&mut ex, 1, id, Some(px(49_800)), None),
        Err(Error::StaleOracle)
    );
    assert_eq!(
        replace(&mut ex, 1, id, None, Some(qty(2_000))),
        Err(Error::StaleOracle)
    );
    assert_eq!(
        replace(&mut ex, 1, id, None, Some(qty(500))),
        Ok(vec![Output::Replaced {
            order_id: id,
            kept_priority: true
        }])
    );
}

#[test]
fn replace_keeps_the_order_id() {
    let (mut ex, a, _b, _c) = queue3();
    replace(&mut ex, 2, a, Some(px(50_200)), Some(qty(2_000))).unwrap();
    let o = ex.engine.order(M, a).unwrap();
    assert_eq!(
        (o.id, o.price, o.qty, o.account),
        (a, px(50_200), qty(2_000).0, 2)
    );
    assert_eq!(ex.engine.open_orders_of(2), vec![a]);
}

// ---------------------------------------------------------------------------
// self-trade prevention
// ---------------------------------------------------------------------------

#[test]
fn stp_cancel_newest_cancels_the_incoming_order_and_keeps_the_resting_one() {
    let mut ex = ex4();
    let own = ask(&mut ex, 1, 50_000, 1_000);
    let other = ask(&mut ex, 2, 50_000, 1_000); // behind, must not be reached
    let p = stp_order(
        &mut ex,
        1,
        Side::Buy,
        50_000,
        500,
        Tif::Gtc,
        StpMode::CancelNewest,
    );
    let outs = ex.place(p).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![canceled(taker, 1, 500, CancelReason::SelfTrade)]
    );
    assert_eq!(resting_qty(&ex, own), Some(qty(1_000).0));
    assert_eq!(resting_qty(&ex, other), Some(qty(1_000).0));
    assert_eq!(ex.engine.open_orders_of(1), vec![own]);
    assert_eq!(ex.size(1), 0);
    assert_eq!(ex.size(2), 0);
    ex.check();
}

#[test]
fn stp_cancel_newest_after_a_partial_fill_cancels_only_the_remainder() {
    let mut ex = ex4();
    let first = ask(&mut ex, 2, 50_000, 300);
    let own = ask(&mut ex, 1, 50_000, 1_000);
    let outs = ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Gtc).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![
            fill(
                taker,
                first,
                1,
                2,
                px(50_000),
                300,
                Side::Buy,
                7_500_000,
                3_000_000
            ),
            canceled(taker, 1, 700, CancelReason::SelfTrade),
        ]
    );
    assert!(!is_rested(&outs));
    assert_eq!(resting_qty(&ex, own), Some(qty(1_000).0));
    assert_eq!(ex.size(1), qty(300).0 as i64);
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn stp_cancel_oldest_cancels_the_resting_order_and_keeps_matching() {
    let mut ex = ex4();
    let own = ask(&mut ex, 1, 50_000, 1_000);
    let other = ask(&mut ex, 2, 50_000, 1_000);
    let p = stp_order(
        &mut ex,
        1,
        Side::Buy,
        50_000,
        500,
        Tif::Gtc,
        StpMode::CancelOldest,
    );
    let outs = ex.place(p).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![
            canceled(own, 1, 1_000, CancelReason::SelfTrade),
            fill(
                taker,
                other,
                1,
                2,
                px(50_000),
                500,
                Side::Buy,
                12_500_000,
                5_000_000
            ),
        ]
    );
    assert!(ex.engine.order(M, own).is_none());
    assert_eq!(resting_qty(&ex, other), Some(qty(500).0));
    assert!(ex.engine.open_orders_of(1).is_empty());
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn stp_cancel_oldest_with_only_own_liquidity_lets_the_gtc_remainder_rest() {
    let mut ex = ex4();
    let own = ask(&mut ex, 1, 50_000, 1_000);
    let p = stp_order(
        &mut ex,
        1,
        Side::Buy,
        50_000,
        500,
        Tif::Gtc,
        StpMode::CancelOldest,
    );
    let outs = ex.place(p).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![
            canceled(own, 1, 1_000, CancelReason::SelfTrade),
            Output::Rested {
                order_id: taker,
                qty: qty(500),
                price: px(50_000)
            },
        ]
    );
    assert_eq!(best(&ex, Side::Buy), Some(px(50_000)));
    assert!(best(&ex, Side::Sell).is_none());
    ex.check();
}

#[test]
fn stp_cancel_both_cancels_resting_and_incoming_without_a_trade() {
    let mut ex = ex4();
    let own = ask(&mut ex, 1, 50_000, 1_000);
    let other = ask(&mut ex, 2, 50_000, 1_000);
    let p = stp_order(
        &mut ex,
        1,
        Side::Buy,
        50_000,
        500,
        Tif::Gtc,
        StpMode::CancelBoth,
    );
    let outs = ex.place(p).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![
            canceled(own, 1, 1_000, CancelReason::SelfTrade),
            canceled(taker, 1, 500, CancelReason::SelfTrade),
        ]
    );
    assert!(fills(&outs).is_empty());
    assert!(ex.engine.open_orders_of(1).is_empty());
    assert_eq!(resting_qty(&ex, other), Some(qty(1_000).0));
    assert_eq!(ex.size(1), 0);
    ex.check();
}

#[test]
fn stp_applies_to_the_taker_mode_not_the_makers_mode() {
    let mut ex = ex4();
    // the resting order asked for CancelBoth, the incoming one for CancelNewest
    let p = stp_order(
        &mut ex,
        1,
        Side::Sell,
        50_000,
        1_000,
        Tif::Gtc,
        StpMode::CancelBoth,
    );
    let own = accepted_id(&ex.place(p).unwrap());
    let p = stp_order(
        &mut ex,
        1,
        Side::Buy,
        50_000,
        500,
        Tif::Ioc,
        StpMode::CancelNewest,
    );
    let outs = ex.place(p).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![canceled(taker, 1, 500, CancelReason::SelfTrade)]
    );
    assert_eq!(resting_qty(&ex, own), Some(qty(1_000).0));
}

#[test]
fn orders_of_different_accounts_never_trigger_stp() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 1_000);
    let p = stp_order(
        &mut ex,
        1,
        Side::Buy,
        50_000,
        500,
        Tif::Ioc,
        StpMode::CancelBoth,
    );
    let outs = ex.place(p).unwrap();
    assert_eq!(fill_list(&outs), vec![(a, 50_000, 500)]);
    assert!(!has(&outs, |o| matches!(
        o,
        Output::Canceled {
            reason: CancelReason::SelfTrade,
            ..
        }
    )));
}

// ---------------------------------------------------------------------------
// validation: tick / lot / notional / size
// ---------------------------------------------------------------------------

#[test]
fn price_must_be_a_positive_multiple_of_the_tick() {
    let mut ex = ex4();
    for bad in [0u64, 1, 5_000, 50_000_005_000, 10_000_000_000_001] {
        assert_eq!(
            limit_at(&mut ex, 1, Side::Buy, Price(bad), 1_000, Tif::Gtc),
            Err(Error::InvalidPrice),
            "price {bad}"
        );
    }
    // one tick is a valid price shape (the notional check then rejects it)
    assert_eq!(
        limit_at(&mut ex, 1, Side::Buy, Price(10_000), 1_000, Tif::Gtc),
        Err(Error::BelowMinNotional)
    );
    let outs = limit_at(
        &mut ex,
        1,
        Side::Buy,
        Price(50_000_010_000),
        1_000,
        Tif::Gtc,
    )
    .unwrap();
    assert!(is_rested(&outs));
    ex.check();
}

#[test]
fn quantity_must_be_a_positive_multiple_of_the_lot_within_the_maximum() {
    let mut ex = ex4();
    let place = |ex: &mut Ex, raw: u64| {
        let mut p = ex.order(
            1,
            Side::Buy,
            OrderKind::Limit {
                price: px(40_000),
                tif: Tif::Gtc,
            },
            1,
        );
        p.qty = Qty(raw);
        ex.place(p)
    };
    assert_eq!(place(&mut ex, 0), Err(Error::ZeroQty));
    assert_eq!(place(&mut ex, 5), Err(Error::InvalidQty));
    assert_eq!(place(&mut ex, 15), Err(Error::InvalidQty));
    assert_eq!(place(&mut ex, 1_000_010), Err(Error::InvalidQty)); // > max_order_qty
    let outs = place(&mut ex, 1_000_000).unwrap(); // == max_order_qty
    assert!(is_rested(&outs));
    ex.check();
}

#[test]
fn minimum_notional_is_inclusive() {
    let mut ex = ex4();
    // exactly 10 quote: 1 milli at 10_000
    assert_eq!(
        limit_at(&mut ex, 1, Side::Buy, px(10_000), 1, Tif::Gtc).map(|o| is_rested(&o)),
        Ok(true)
    );
    // 9.99999 quote (floored notional is below the minimum)
    assert_eq!(
        limit_at(&mut ex, 1, Side::Buy, Price(9_999_990_000), 1, Tif::Gtc),
        Err(Error::BelowMinNotional)
    );
    ex.check();
}

// ---------------------------------------------------------------------------
// client ids and order ids
// ---------------------------------------------------------------------------

fn gtc_client(
    ex: &mut Ex,
    account: AccountId,
    side: Side,
    price: u64,
    milli: u64,
    client: u64,
) -> Result<Vec<Output>, Error> {
    let mut p = ex.order(
        account,
        side,
        OrderKind::Limit {
            price: px(price),
            tif: Tif::Gtc,
        },
        milli,
    );
    p.client_id = client;
    ex.place(p)
}

#[test]
fn live_client_order_id_cannot_be_reused_by_the_same_account() {
    let mut ex = ex4();
    let first = accepted_id(&gtc_client(&mut ex, 1, Side::Buy, 49_000, 1_000, 7).unwrap());
    assert_eq!(
        gtc_client(&mut ex, 1, Side::Buy, 48_000, 500, 7),
        Err(Error::DuplicateClientOrderId)
    );
    // also for orders that would never rest
    let mut p = ex.order(
        1,
        Side::Buy,
        OrderKind::Limit {
            price: px(49_000),
            tif: Tif::Ioc,
        },
        100,
    );
    p.client_id = 7;
    assert_eq!(ex.place(p), Err(Error::DuplicateClientOrderId));
    assert_eq!(ex.engine.open_orders_of(1), vec![first]);
    // a rejection consumes no order id
    let next = accepted_id(&gtc_client(&mut ex, 1, Side::Buy, 48_000, 500, 8).unwrap());
    assert_eq!(next, first + 1);
    // client ids are scoped per account
    assert!(gtc_client(&mut ex, 2, Side::Buy, 47_000, 500, 7).is_ok());
    ex.check();
}

#[test]
fn client_order_id_is_reusable_after_cancel_and_after_a_full_fill() {
    let mut ex = ex4();
    let id = accepted_id(&gtc_client(&mut ex, 1, Side::Buy, 49_000, 1_000, 7).unwrap());
    ex.cancel(1, id).unwrap();
    assert!(gtc_client(&mut ex, 1, Side::Buy, 49_000, 1_000, 7).is_ok());

    let maker = accepted_id(&gtc_client(&mut ex, 2, Side::Sell, 50_000, 1_000, 9).unwrap());
    // partially filled: still live, still blocked
    ex.limit(3, Side::Buy, 50_000, 400, Tif::Ioc).unwrap();
    assert_eq!(resting_qty(&ex, maker), Some(qty(600).0));
    assert_eq!(
        gtc_client(&mut ex, 2, Side::Sell, 50_100, 100, 9),
        Err(Error::DuplicateClientOrderId)
    );
    ex.limit(3, Side::Buy, 50_000, 600, Tif::Ioc).unwrap();
    assert!(ex.engine.order(M, maker).is_none());
    assert!(gtc_client(&mut ex, 2, Side::Sell, 50_100, 100, 9).is_ok());
    ex.check();
}

#[test]
fn client_order_id_of_a_non_resting_order_is_free_immediately() {
    let mut ex = ex4();
    for _ in 0..2 {
        let mut p = ex.order(
            1,
            Side::Buy,
            OrderKind::Limit {
                price: px(49_000),
                tif: Tif::Ioc,
            },
            100,
        );
        p.client_id = 5;
        assert!(ex.place(p).is_ok());
    }
}

#[test]
fn order_ids_are_strictly_increasing_and_never_reused() {
    let mut ex = ex4();
    let a = bid(&mut ex, 1, 49_000, 1_000);
    let b = accepted_id(&ex.limit(1, Side::Buy, 48_000, 100, Tif::Ioc).unwrap());
    assert_eq!(b, a + 1); // IOC takes an id too
    let bad = limit_at(&mut ex, 1, Side::Buy, Price(48_000_005_000), 100, Tif::Gtc);
    assert_eq!(bad, Err(Error::InvalidPrice)); // rejected: no id
    let c = bid(&mut ex, 1, 48_000, 100);
    assert_eq!(c, b + 1);
    ex.cancel(1, a).unwrap();
    let d = bid(&mut ex, 1, 49_000, 1_000);
    assert_eq!(d, c + 1); // cancelled id is not recycled
    assert_ne!(d, a);
    // taker ids are larger than the maker's it traded against
    let m = ask(&mut ex, 2, 50_000, 100);
    let outs = ex.limit(3, Side::Buy, 50_000, 100, Tif::Ioc).unwrap();
    assert!(accepted_id(&outs) > m);
}

// ---------------------------------------------------------------------------
// bounded book / per-account limits
// ---------------------------------------------------------------------------

#[test]
fn per_account_order_limit_rejects_resting_orders_only() {
    let mut cfg = MarketConfig::example(M);
    cfg.max_orders_per_account = 2;
    let mut ex = ex4_with(cfg);
    let o1 = bid(&mut ex, 1, 49_000, 1_000);
    bid(&mut ex, 1, 48_900, 1_000);
    assert_eq!(
        ex.limit(1, Side::Buy, 48_800, 1_000, Tif::Gtc),
        Err(Error::TooManyOrders)
    );
    assert_eq!(
        ex.limit(1, Side::Buy, 48_800, 1_000, Tif::PostOnly),
        Err(Error::TooManyOrders)
    );
    // non-resting orders are not limited
    assert!(ex.limit(1, Side::Buy, 48_800, 1_000, Tif::Ioc).is_ok());
    // other accounts are not affected
    assert!(ex.limit(2, Side::Buy, 48_800, 1_000, Tif::Gtc).is_ok());
    // freeing a slot helps
    ex.cancel(1, o1).unwrap();
    assert!(ex.limit(1, Side::Buy, 48_800, 1_000, Tif::Gtc).is_ok());
    ex.check();
}

#[test]
fn per_account_limit_is_checked_up_front_even_for_an_order_that_would_fill() {
    // Behaviour: the capacity check precedes matching (documented by the
    // order of checks in `place`), so a crossing GTC is refused too.
    let mut cfg = MarketConfig::example(M);
    cfg.max_orders_per_account = 1;
    let mut ex = ex4_with(cfg);
    bid(&mut ex, 1, 49_000, 1_000);
    ask(&mut ex, 2, 50_000, 1_000);
    assert_eq!(
        ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Gtc),
        Err(Error::TooManyOrders)
    );
    assert!(ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Ioc).is_ok());
}

#[test]
fn full_book_rejects_resting_orders_but_not_those_that_free_space() {
    let mut cfg = MarketConfig::example(M);
    cfg.max_book_orders = 2;
    let mut ex = ex4_with(cfg);
    let a = ask(&mut ex, 2, 50_000, 1_000);
    bid(&mut ex, 3, 49_000, 1_000);
    assert_eq!(
        ex.limit(4, Side::Buy, 48_000, 1_000, Tif::Gtc),
        Err(Error::BookFull)
    );
    assert_eq!(
        ex.limit(4, Side::Sell, 51_000, 1_000, Tif::PostOnly),
        Err(Error::BookFull)
    );
    // an IOC consumes liquidity and is still allowed
    let outs = ex.limit(4, Side::Buy, 50_000, 1_000, Tif::Ioc).unwrap();
    assert_eq!(fill_list(&outs), vec![(a, 50_000, 1_000)]);
    // now there is room again
    assert!(ex.limit(4, Side::Buy, 48_000, 1_000, Tif::Gtc).is_ok());
    ex.check();
}

#[test]
fn level_limit_cancels_the_remainder_with_book_full_reason() {
    let mut cfg = MarketConfig::example(M);
    cfg.max_levels_per_side = 1;
    let mut ex = ex4_with(cfg);
    bid(&mut ex, 2, 49_000, 1_000); // the only allowed bid level
    let l = ask(&mut ex, 3, 49_900, 1_000); // the only allowed ask level

    // new bid level 49_500 is not allowed, the order is accepted but cannot rest
    let outs = ex.limit(1, Side::Buy, 49_500, 500, Tif::Gtc).unwrap();
    let id = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![canceled(id, 1, 500, CancelReason::BookFull)]
    );
    assert!(ex.engine.open_orders_of(1).is_empty());

    // fill first, then the unrestable remainder is cancelled with BookFull
    let outs = ex.limit(1, Side::Buy, 50_000, 1_500, Tif::Gtc).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![
            fill(
                taker,
                l,
                1,
                3,
                px(49_900),
                1_000,
                Side::Buy,
                24_950_000,
                9_980_000
            ),
            canceled(taker, 1, 500, CancelReason::BookFull),
        ]
    );
    // joining an existing level is always possible
    let outs = ex.limit(1, Side::Buy, 49_000, 500, Tif::Gtc).unwrap();
    assert!(is_rested(&outs));
    assert_eq!(ex.engine.book_view(M, 5).unwrap().bids.len(), 1);
    ex.check();
    assert_conserved(&ex);
}

// ---------------------------------------------------------------------------
// preconditions: market, account, oracle
// ---------------------------------------------------------------------------

#[test]
fn unknown_market_and_unknown_account_are_rejected() {
    let mut ex = ex4();
    let mut p = ex.order(
        1,
        Side::Buy,
        OrderKind::Limit {
            price: px(49_000),
            tif: Tif::Gtc,
        },
        1_000,
    );
    p.market = 99;
    assert_eq!(ex.place(p), Err(Error::UnknownMarket));
    let mut p = ex.order(
        99,
        Side::Buy,
        OrderKind::Limit {
            price: px(49_000),
            tif: Tif::Gtc,
        },
        1_000,
    );
    assert_eq!(ex.place(p), Err(Error::UnknownAccount));
    // the market is checked first
    p.market = 99;
    assert_eq!(ex.place(p), Err(Error::UnknownMarket));
    assert_eq!(ex.engine.account_ids(), vec![1, 2, 3, 4]);
}

#[test]
fn market_without_a_mark_price_rejects_orders() {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    ex.deposit(1, 1_000_000);
    ex.deposit(2, 1_000_000);
    assert_eq!(
        ex.limit(1, Side::Buy, 49_000, 1_000, Tif::Gtc),
        Err(Error::NoMarkPrice)
    );
    assert_eq!(
        ex.limit(1, Side::Buy, 49_000, 1_000, Tif::Ioc),
        Err(Error::NoMarkPrice)
    );
    // a market order without liquidity fails earlier
    assert_eq!(ex.market(1, Side::Buy, 1_000, 100), Err(Error::NoLiquidity));
    // after the first oracle input orders are accepted
    ex.oracle_on(M, 50_000);
    assert!(ex.limit(1, Side::Buy, 49_000, 1_000, Tif::Gtc).is_ok());
    ex.check();
}

#[test]
fn stale_oracle_rejects_new_risk_but_still_allows_reduce_only() {
    let mut ex = ex4();
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    let b = bid(&mut ex, 3, 50_000, 1_000);
    ex.advance(70_000);
    // non-reduce-only: every flavour is rejected
    assert_eq!(
        ex.limit(4, Side::Buy, 49_000, 1_000, Tif::Gtc),
        Err(Error::StaleOracle)
    );
    assert_eq!(
        ex.limit(4, Side::Sell, 50_000, 1_000, Tif::Ioc),
        Err(Error::StaleOracle)
    );
    assert_eq!(
        ex.market(4, Side::Sell, 1_000, 100),
        Err(Error::StaleOracle)
    );
    assert_eq!(resting_qty(&ex, b), Some(qty(1_000).0));
    // reduce-only closing order still works
    let p = ro_order(&mut ex, 1, Side::Sell, 50_000, 1_000, Tif::Ioc);
    let outs = ex.place(p).unwrap();
    assert_eq!(fill_list(&outs), vec![(b, 50_000, 1_000)]);
    assert_eq!(ex.size(1), 0);
    // a fresh oracle input re-enables regular orders
    ex.oracle(50_000).unwrap();
    assert!(ex.limit(4, Side::Buy, 49_000, 1_000, Tif::Gtc).is_ok());
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn staleness_limit_is_inclusive_at_sixty_seconds() {
    let mut ex = ex4();
    // the oracle was stamped `ts_o`; the next event is at ts_o + delta
    ex.oracle(50_000).unwrap();
    let ts_o = ex.ts;
    ex.apply_at(
        ts_o + 60_000,
        EventBody::Deposit {
            account: 1,
            amount: usd(1),
        },
    )
    .unwrap();
    let outs = ex
        .apply_at(
            ts_o + 60_000,
            EventBody::Place(PlaceOrder {
                market: M,
                account: 1,
                side: Side::Buy,
                kind: OrderKind::Limit {
                    price: px(49_000),
                    tif: Tif::Gtc,
                },
                qty: qty(1_000),
                client_id: 1,
                reduce_only: false,
                stp: StpMode::CancelNewest,
            }),
        )
        .unwrap();
    assert!(is_rested(&outs));
    let err = ex.apply_at(
        ts_o + 60_001,
        EventBody::Place(PlaceOrder {
            market: M,
            account: 1,
            side: Side::Buy,
            kind: OrderKind::Limit {
                price: px(49_000),
                tif: Tif::Gtc,
            },
            qty: qty(1_000),
            client_id: 2,
            reduce_only: false,
            stp: StpMode::CancelNewest,
        }),
    );
    assert_eq!(err, Err(Error::StaleOracle));
}

// ---------------------------------------------------------------------------
// book invariants under load
// ---------------------------------------------------------------------------

#[test]
fn book_is_never_crossed_under_a_deterministic_mixed_sequence() {
    let mut ex = ex4();
    let mut rng = Rng::new(0x5EED_0001);
    let tifs = [Tif::Gtc, Tif::Ioc, Tif::Fok, Tif::PostOnly];
    let stps = [
        StpMode::CancelNewest,
        StpMode::CancelOldest,
        StpMode::CancelBoth,
    ];
    let (mut ok, mut rejected, mut traded) = (0u32, 0u32, 0u32);
    for step in 0..1_500u32 {
        let account = 1 + rng.below(4);
        let side = if rng.chance(1, 2) {
            Side::Buy
        } else {
            Side::Sell
        };
        let result = match rng.below(10) {
            0..=5 => {
                let price = px(49_900 + 10 * rng.below(21));
                let milli = 1 + rng.below(5);
                let tif = *rng.pick(&tifs).unwrap();
                let mut p = ex.order(account, side, OrderKind::Limit { price, tif }, milli);
                p.stp = *rng.pick(&stps).unwrap();
                p.reduce_only = rng.chance(1, 10);
                ex.place(p)
            }
            6 => ex.market(account, side, 1 + rng.below(8), rng.below(200) as u32),
            7 => {
                let live = ex.engine.open_orders_of(account);
                match rng.pick(&live) {
                    Some(id) => ex.cancel(account, *id),
                    None => ex.cancel(account, 123_456),
                }
            }
            8 => {
                let live = ex.engine.open_orders_of(account);
                let price = px(49_900 + 10 * rng.below(21));
                let q = qty(1 + rng.below(6));
                match rng.pick(&live) {
                    Some(id) => replace(&mut ex, account, *id, Some(price), Some(q)),
                    None => replace(&mut ex, account, 123_456, None, None),
                }
            }
            _ => ex.apply(EventBody::CancelAll {
                account,
                market: None,
            }),
        };
        match result {
            Ok(outs) => {
                ok += 1;
                traded += fills(&outs).len() as u32;
            }
            Err(e) => {
                rejected += 1;
                assert!(
                    !matches!(
                        e,
                        Error::Halted
                            | Error::Internal(_)
                            | Error::Overflow
                            | Error::DivisionByZero
                    ),
                    "step {step}: engine failure {e:?}"
                );
            }
        }
        if let (Some(b), Some(a)) = (best(&ex, Side::Buy), best(&ex, Side::Sell)) {
            assert!(b < a, "step {step}: crossed book {b:?} >= {a:?}");
        }
        if step % 100 == 0 {
            ex.check();
        }
    }
    assert!(
        ok > 500 && rejected > 50 && traded > 100,
        "{ok} {rejected} {traded}"
    );
    ex.check();
    assert_conserved(&ex);
}

// ---------------------------------------------------------------------------
// fill cap
// ---------------------------------------------------------------------------

/// Account 2 rests `n` one-lot asks at 50_000; returns their ids in time order.
fn many_asks(ex: &mut Ex, n: usize) -> Vec<OrderId> {
    (0..n).map(|_| ask(ex, 2, 50_000, 1)).collect()
}

fn big_cfg() -> MarketConfig {
    let mut cfg = MarketConfig::example(M);
    cfg.max_orders_per_account = 1_000;
    cfg
}

#[test]
fn market_order_stops_after_256_fills_and_cancels_the_remainder() {
    let mut ex = ex4_with(big_cfg());
    let ids = many_asks(&mut ex, 300);
    let outs = ex.market(1, Side::Buy, 300, 100).unwrap();
    let taker = accepted_id(&outs);
    let fl = fill_list(&outs);
    assert_eq!(fl.len(), 256);
    assert!(fl.iter().zip(&ids).all(|(f, id)| f.0 == *id && f.2 == 1));
    assert_eq!(
        outs.last().unwrap(),
        &canceled(taker, 1, 44, CancelReason::Remainder)
    );
    assert_eq!(ex.engine.open_orders_of(2).len(), 44);
    assert_eq!(ex.size(1), qty(256).0 as i64);
    // each fill: n = 5e7; taker ceil(500ppm) = 25_000, maker ceil(200ppm) = 10_000
    assert_eq!(ex.engine.fee_pool(), Money(256 * 35_000));
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn ioc_limit_order_is_subject_to_the_same_fill_cap() {
    let mut ex = ex4_with(big_cfg());
    many_asks(&mut ex, 300);
    let outs = ex.limit(1, Side::Buy, 50_000, 300, Tif::Ioc).unwrap();
    assert_eq!(fills(&outs).len(), 256);
    assert!(has(&outs, |o| matches!(
        o,
        Output::Canceled {
            qty: Qty(440),
            reason: CancelReason::Remainder,
            ..
        }
    )));
    ex.check();
}

#[test]
fn gtc_order_hitting_the_fill_cap_is_cancelled_instead_of_resting() {
    // Behaviour: when matching stops early because of the cap, the remainder
    // does not rest (it would sit at a crossing price).
    let mut ex = ex4_with(big_cfg());
    many_asks(&mut ex, 300);
    let outs = ex.limit(1, Side::Buy, 50_000, 300, Tif::Gtc).unwrap();
    assert_eq!(fills(&outs).len(), 256);
    assert!(!is_rested(&outs));
    assert!(has(&outs, |o| matches!(
        o,
        Output::Canceled {
            reason: CancelReason::Remainder,
            ..
        }
    )));
    assert!(ex.engine.open_orders_of(1).is_empty());
    ex.check();
}

#[test]
fn fok_needing_exactly_256_fills_succeeds_and_257_is_not_fillable() {
    let mut ex = ex4_with(big_cfg());
    many_asks(&mut ex, 300);
    assert_eq!(
        ex.limit(1, Side::Buy, 50_000, 257, Tif::Fok),
        Err(Error::FokNotFillable)
    );
    assert_eq!(ex.engine.open_orders_of(2).len(), 300); // untouched
    let outs = ex.limit(1, Side::Buy, 50_000, 256, Tif::Fok).unwrap();
    assert_eq!(fills(&outs).len(), 256);
    assert!(!is_canceled(&outs) && !is_rested(&outs));
    assert_eq!(ex.engine.open_orders_of(2).len(), 44);
    ex.check();
}

// ---------------------------------------------------------------------------
// book view
// ---------------------------------------------------------------------------

#[test]
fn book_view_orders_levels_best_first_and_aggregates_quantities() {
    let mut ex = ex4();
    bid(&mut ex, 1, 49_900, 300);
    bid(&mut ex, 2, 49_900, 200);
    bid(&mut ex, 1, 49_800, 100);
    bid(&mut ex, 3, 49_950, 50);
    ask(&mut ex, 2, 50_100, 200);
    ask(&mut ex, 3, 50_050, 100);
    ask(&mut ex, 4, 50_050, 100);
    ask(&mut ex, 4, 50_200, 70);

    let v = ex.engine.book_view(M, 2).unwrap();
    assert_eq!(v.market, M);
    let bids: Vec<_> = v.bids.iter().map(|l| (l.price, l.qty, l.orders)).collect();
    let asks: Vec<_> = v.asks.iter().map(|l| (l.price, l.qty, l.orders)).collect();
    assert_eq!(
        bids,
        vec![(px(49_950), qty(50), 1), (px(49_900), qty(500), 2)]
    );
    assert_eq!(
        asks,
        vec![(px(50_050), qty(200), 2), (px(50_100), qty(200), 1)]
    );

    // deeper than the book returns all levels, still ordered
    let v = ex.engine.book_view(M, 10).unwrap();
    assert_eq!(v.bids.len(), 3);
    assert_eq!(v.asks.len(), 3);
    assert_eq!(v.bids[2].price, px(49_800));
    assert_eq!(v.asks[2].price, px(50_200));

    // depth 0 is empty, unknown market is None
    let v = ex.engine.book_view(M, 0).unwrap();
    assert!(v.bids.is_empty() && v.asks.is_empty());
    assert!(ex.engine.book_view(99, 5).is_none());
    ex.check();
}

#[test]
fn book_view_reflects_partial_fills_and_cancels() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 500);
    ask(&mut ex, 3, 50_000, 500);
    ex.limit(1, Side::Buy, 50_000, 200, Tif::Ioc).unwrap();
    let l = &ex.engine.book_view(M, 1).unwrap().asks[0];
    assert_eq!((l.qty, l.orders), (qty(800), 2));
    ex.cancel(2, a).unwrap();
    let l = &ex.engine.book_view(M, 1).unwrap().asks[0];
    assert_eq!((l.qty, l.orders), (qty(500), 1));
}

// ---------------------------------------------------------------------------
// fees on fills
// ---------------------------------------------------------------------------

#[test]
fn default_tier_fees_are_ceil_of_notional_times_ppm() {
    let mut ex = ex4();
    let a = ask(&mut ex, 2, 50_000, 500);
    let outs = ex.limit(1, Side::Buy, 50_000, 500, Tif::Ioc).unwrap();
    let taker = accepted_id(&outs);
    // notional 25_000 quote = 2.5e10 raw; taker 500 ppm, maker 200 ppm
    assert_eq!(
        body(&outs),
        vec![fill(
            taker,
            a,
            1,
            2,
            px(50_000),
            500,
            Side::Buy,
            12_500_000,
            5_000_000
        )]
    );
    assert_eq!(ex.collateral(1), usd(1_000_000).0 - 12_500_000);
    assert_eq!(ex.collateral(2), usd(1_000_000).0 - 5_000_000);
    assert_eq!(ex.engine.fee_pool(), Money(17_500_000));
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn fee_rounding_is_ceil_for_charges_and_floor_for_rebates() {
    let mut ex = ex4();
    ex.apply(EventBody::SetFeeTier {
        account: 2,
        tier: 1,
    })
    .unwrap();
    // 1 milli at 50_000.01: notional 50_000_010 raw
    let odd = Price(50_000_010_000);
    let p = ex.order(
        2,
        Side::Sell,
        OrderKind::Limit {
            price: odd,
            tif: Tif::Gtc,
        },
        1,
    );
    let a = accepted_id(&ex.place(p).unwrap());
    let outs = limit_at(&mut ex, 1, Side::Buy, odd, 1, Tif::Ioc).unwrap();
    let taker = accepted_id(&outs);
    // taker tier 0: ceil(50_000_010 * 500 / 1e6) = ceil(25_000.005) = 25_001
    // maker tier 1: -floor(50_000_010 * 50 / 1e6) = -floor(2_500.0005) = -2_500
    assert_eq!(
        body(&outs),
        vec![fill(taker, a, 1, 2, odd, 1, Side::Buy, 25_001, -2_500)]
    );
    assert_eq!(ex.engine.fee_pool(), Money(25_001 - 2_500));
    assert_eq!(ex.collateral(2), usd(1_000_000).0 + 2_500);
    ex.check();
    assert_conserved(&ex);

    // the same maker price with a tier-0 maker rounds up: ceil(10_000.002)
    let mut ex = ex4();
    let p = ex.order(
        2,
        Side::Sell,
        OrderKind::Limit {
            price: odd,
            tif: Tif::Gtc,
        },
        1,
    );
    let a = accepted_id(&ex.place(p).unwrap());
    let outs = limit_at(&mut ex, 1, Side::Buy, odd, 1, Tif::Ioc).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![fill(taker, a, 1, 2, odd, 1, Side::Buy, 25_001, 10_001)]
    );
}

#[test]
fn tier_one_maker_gets_a_rebate_and_the_fee_pool_keeps_the_difference() {
    let mut ex = ex4();
    ex.apply(EventBody::SetFeeTier {
        account: 2,
        tier: 1,
    })
    .unwrap();
    let a = ask(&mut ex, 2, 50_000, 1_000);
    let outs = ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Ioc).unwrap();
    let taker = accepted_id(&outs);
    // 1.0 at 50_000: n = 5e10; taker tier 0: 500 ppm = 25_000_000; rebate -50 ppm
    assert_eq!(
        body(&outs),
        vec![fill(
            taker,
            a,
            1,
            2,
            px(50_000),
            1_000,
            Side::Buy,
            25_000_000,
            -2_500_000
        )]
    );
    assert_eq!(ex.collateral(1), usd(1_000_000).0 - 25_000_000);
    assert_eq!(ex.collateral(2), usd(1_000_000).0 + 2_500_000);
    assert_eq!(ex.engine.fee_pool(), Money(22_500_000));
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn tier_one_taker_pays_the_lower_taker_fee() {
    let mut ex = ex4();
    ex.apply(EventBody::SetFeeTier {
        account: 1,
        tier: 1,
    })
    .unwrap();
    let a = ask(&mut ex, 2, 50_000, 1_000);
    let outs = ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Ioc).unwrap();
    let taker = accepted_id(&outs);
    assert_eq!(
        body(&outs),
        vec![fill(
            taker,
            a,
            1,
            2,
            px(50_000),
            1_000,
            Side::Buy,
            15_000_000,
            10_000_000
        )]
    );
    assert_eq!(ex.engine.fee_pool(), Money(25_000_000));
    ex.check();
    assert_conserved(&ex);
}

#[test]
fn fees_accumulate_per_fill_and_orders_without_fills_pay_nothing() {
    let mut ex = ex4();
    // resting, cancelling and rejected-by-IOC orders are free
    let id = bid(&mut ex, 1, 49_000, 1_000);
    ex.cancel(1, id).unwrap();
    ex.limit(1, Side::Buy, 48_000, 1_000, Tif::Ioc).unwrap();
    assert_eq!(ex.engine.fee_pool(), Money(0));
    assert_eq!(ex.collateral(1), usd(1_000_000).0);

    ask(&mut ex, 2, 50_000, 300);
    ask(&mut ex, 3, 50_100, 200);
    let outs = ex.limit(1, Side::Buy, 50_100, 500, Tif::Ioc).unwrap();
    let total_fees: i128 = outs
        .iter()
        .map(|o| match o {
            Output::Fill {
                taker_fee,
                maker_fee,
                ..
            } => taker_fee.0 + maker_fee.0,
            _ => 0,
        })
        .sum();
    // fill 1: n = 1.5e10 -> 7_500_000 + 3_000_000
    // fill 2: n = 1.002e10 -> ceil(5_010_000) + ceil(2_004_000) = 5_010_000 + 2_004_000
    assert_eq!(total_fees, 7_500_000 + 3_000_000 + 5_010_000 + 2_004_000);
    assert_eq!(ex.engine.fee_pool(), Money(total_fees));
    ex.check();
    assert_conserved(&ex);
}
