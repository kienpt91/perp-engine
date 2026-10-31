//! Domain tests: liquidation, insurance fund, auto-deleveraging.
//!
//! Scenario conventions (all numbers are derived by hand from the documented
//! rules, see `engine/risk.rs`):
//!
//! * account 1 is the victim, account 2 sells the victim its position,
//!   account 3 (fee tier 1, i.e. a maker rebate) rests the bids that provide
//!   liquidation liquidity;
//! * the book never has both sides after the opening trade, so the mark price
//!   (median of index, mid, last trade, clamped to the index band) equals the
//!   index whenever the index is below the last trade price of 50 000;
//! * opening 1.0 base at 50 000 costs the victim a 25 quote taker fee
//!   (500 ppm), so a deposit of `D` leaves `D - 25` of collateral.
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
use perp_domain::fixed::{Money, Price};
use perp_domain::types::{
    CancelReason, EngineConfig, EventBody, MarketConfig, OrderKind, Output, Side, Tif,
};

const VICTIM: u64 = 1;
const SELLER: u64 = 2;
const BIDDER: u64 = 3;

/// Victim long 1.0 base at 50 000 (counterparty `SELLER` short). Account 3 is
/// funded and has the rebate fee tier. Collateral of the victim: `deposit - 25`.
fn long_victim(deposit: i128) -> Ex {
    let mut ex = Ex::new();
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    ex.deposit(VICTIM, deposit);
    ex.apply(EventBody::SetFeeTier {
        account: BIDDER,
        tier: 1,
    })
    .unwrap();
    ex.trade(VICTIM, SELLER, Side::Buy, 50_000, 1_000);
    assert_eq!(ex.size(VICTIM), 10_000);
    assert_eq!(ex.collateral(VICTIM), usd(deposit).0 - 25_000_000);
    ex
}

/// Victim short 1.0 base at 50 000 (counterparty `SELLER` long).
fn short_victim(deposit: i128) -> Ex {
    let mut ex = Ex::new();
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    ex.deposit(VICTIM, deposit);
    ex.apply(EventBody::SetFeeTier {
        account: BIDDER,
        tier: 1,
    })
    .unwrap();
    ex.trade(VICTIM, SELLER, Side::Sell, 50_000, 1_000);
    assert_eq!(ex.size(VICTIM), -10_000);
    ex
}

fn oracle_raw(ex: &mut Ex, raw: u64) -> Result<Vec<Output>, Error> {
    let source_ts = ex.ts + 1;
    ex.apply(EventBody::Oracle {
        market: M,
        index_price: Price(raw),
        source_ts,
    })
}

fn tick(ex: &mut Ex) -> Vec<Output> {
    ex.apply(EventBody::Tick).unwrap()
}

fn started(outs: &[Output], account: u64) -> Vec<Money> {
    outs.iter()
        .filter_map(|o| match o {
            Output::LiquidationStarted { account: a, equity } if *a == account => Some(*equity),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct LiqFill {
    market: u32,
    maker: u64,
    price: u64,
    qty: u64,
    taker_fee: i128,
    maker_fee: i128,
    side: Side,
}

fn liq_fills(outs: &[Output]) -> Vec<LiqFill> {
    outs.iter()
        .filter_map(|o| match o {
            Output::Fill {
                market,
                maker,
                price,
                qty,
                taker_fee,
                maker_fee,
                taker_side,
                liquidation: true,
                ..
            } => Some(LiqFill {
                market: *market,
                maker: *maker,
                price: price.0,
                qty: qty.0,
                taker_fee: taker_fee.0,
                maker_fee: maker_fee.0,
                side: *taker_side,
            }),
            _ => None,
        })
        .collect()
}

fn any_fill(outs: &[Output]) -> bool {
    outs.iter().any(|o| matches!(o, Output::Fill { .. }))
}

fn paid(outs: &[Output]) -> Vec<(u64, i128)> {
    outs.iter()
        .filter_map(|o| match o {
            Output::InsurancePaid { account, amount } => Some((*account, amount.0)),
            _ => None,
        })
        .collect()
}

fn adl(outs: &[Output]) -> Vec<(u64, u64, u64, u64)> {
    outs.iter()
        .filter_map(|o| match o {
            Output::Deleveraged {
                bankrupt,
                counterparty,
                price,
                qty,
                ..
            } => Some((*bankrupt, *counterparty, price.0, qty.0)),
            _ => None,
        })
        .collect()
}

/// `collateral + insurance + fee_pool - basis == net_deposits + bad_debt`.
fn identity(ex: &Ex) {
    let e = &ex.engine;
    assert_eq!(
        e.total_collateral().0 + e.insurance_fund().0 + e.fee_pool().0 - e.total_basis().0,
        e.net_deposits().0 + e.bad_debt().0,
        "money identity"
    );
    ex.check();
}

fn sum_taker_fees(outs: &[Output]) -> i128 {
    liq_fills(outs).iter().map(|f| f.taker_fee).sum()
}

fn ceil_ppm(n: i128, ppm: i128) -> i128 {
    (n * ppm + 999_999) / 1_000_000
}

fn notional_of(price: u64, qty: u64) -> i128 {
    i128::from(price) * i128::from(qty) / 10_000
}

// ---------------------------------------------------------------------------
// Trigger: equity < maintenance on mark updates and on fills
// ---------------------------------------------------------------------------

#[test]
fn healthy_account_is_not_liquidated_after_a_mark_update() {
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    // equity 1475 vs maintenance 1212.5 at 48 500
    let outs = ex.oracle(48_500).unwrap();
    assert!(started(&outs, VICTIM).is_empty());
    assert!(!any_fill(&outs));
    assert!(!ex.engine.is_pending_liquidation(VICTIM));
    assert_eq!(ex.size(VICTIM), 10_000);
    assert_eq!(ex.engine.summary().liquidation_steps, 0);
    identity(&ex);
}

#[test]
fn equity_exactly_at_maintenance_is_not_liquidated() {
    // collateral 3200; at mark 48 000: equity 3200 - 2000 = 1200 = 2.5 % of 48 000.
    let mut ex = long_victim(3_225);
    ex.gtc(BIDDER, Side::Buy, 47_000, 2_000);
    let outs = ex.oracle(48_000).unwrap();
    let view = ex.engine.account_view(VICTIM).unwrap();
    assert_eq!(view.equity, usd(1_200));
    assert_eq!(view.maintenance_margin, usd(1_200));
    assert!(started(&outs, VICTIM).is_empty(), "equity == maintenance");
    assert!(!any_fill(&outs));
    assert_eq!(ex.size(VICTIM), 10_000);
    identity(&ex);
}

#[test]
fn equity_one_cent_below_maintenance_is_liquidated() {
    let mut ex = long_victim(3_225);
    ex.gtc(BIDDER, Side::Buy, 47_000, 2_000);
    // mark 47 999.99: equity 3200 - 2000.01 = 1199.99 < 1199.99975 (ceil)
    let outs = oracle_raw(&mut ex, 47_999_990_000).unwrap();
    assert_eq!(started(&outs, VICTIM), vec![Money(1_199_990_000)]);
    let fills = liq_fills(&outs);
    assert!(!fills.is_empty());
    assert!(fills.iter().all(|f| f.side == Side::Sell));
    identity(&ex);
}

#[test]
fn boundary_then_one_step_down_in_the_same_run() {
    let mut ex = long_victim(3_225);
    ex.gtc(BIDDER, Side::Buy, 47_000, 2_000);
    assert!(started(&ex.oracle(48_000).unwrap(), VICTIM).is_empty());
    let outs = oracle_raw(&mut ex, 47_999_990_000).unwrap();
    assert_eq!(started(&outs, VICTIM).len(), 1);
    identity(&ex);
}

#[test]
fn liquidation_is_checked_after_a_fill_that_touches_a_stuck_account() {
    // The victim is underwater but nobody bids: its step runs without a fill
    // and it leaves the queue (stuck). Placing a bid does not wake it up, but a
    // fill of its own does: here a reduce-only IOC sell of 0.1 base into the new
    // bid (in band, 47 400) leaves it still below maintenance and the check that
    // follows the fill starts a new liquidation in the very same event.
    let mut ex = long_victim(3_000);
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(started(&outs, VICTIM), vec![usd(475)]);
    assert!(!any_fill(&outs));
    assert!(!ex.engine.is_pending_liquidation(VICTIM));
    let outs = ex.limit(BIDDER, Side::Buy, 47_400, 500, Tif::Gtc).unwrap();
    assert!(
        started(&outs, VICTIM).is_empty(),
        "a resting bid is no check"
    );

    let mut p = ex.order(
        VICTIM,
        Side::Sell,
        OrderKind::Limit {
            price: px(47_400),
            tif: Tif::Ioc,
        },
        100,
    );
    p.reduce_only = true;
    let outs = ex.place(p).unwrap();
    let fill_pos = outs
        .iter()
        .position(|o| {
            matches!(
                o,
                Output::Fill {
                    liquidation: false,
                    ..
                }
            )
        })
        .expect("closing fill");
    let start_pos = outs
        .iter()
        .position(|o| matches!(o, Output::LiquidationStarted { .. }))
        .expect("liquidation started");
    assert!(fill_pos < start_pos);
    // 0.1 base sold at 47 400: realised -260, taker fee 500 ppm of 4740 = 2.37
    // collateral 2975 - 260 - 2.37 = 2712.63; equity 2712.63 - 2250 = 462.63
    // (the next steps start at 418.8, ... as the remainder keeps being sold)
    assert_eq!(started(&outs, VICTIM)[0], Money(462_630_000));
    assert_eq!(started(&outs, VICTIM)[1], Money(418_800_000));
    let lf = liq_fills(&outs);
    // chunk: 25 % of 9000 raw = 2250 raw, hit into the same bid at 47 400
    assert_eq!((lf[0].price, lf[0].qty), (px(47_400).0, 2_250));
    // 0.2 % of 10 665 = 21.33
    assert_eq!(lf[0].taker_fee, 21_330_000);
    identity(&ex);
}

#[test]
fn fill_at_a_healthy_price_does_not_start_a_liquidation() {
    let mut ex = Ex::new();
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    ex.deposit(VICTIM, 2_500);
    ex.oracle(48_000).unwrap();
    ex.gtc(SELLER, Side::Sell, 48_000, 1_000);
    let outs = ex
        .limit(VICTIM, Side::Buy, 48_000, 1_000, Tif::Ioc)
        .unwrap();
    assert!(any_fill(&outs));
    assert!(started(&outs, VICTIM).is_empty());
    assert!(liq_fills(&outs).is_empty());
    // equity 2476 vs maintenance 1200
    let v = ex.engine.account_view(VICTIM).unwrap();
    assert_eq!(v.equity, usd(2_476));
    assert_eq!(v.maintenance_margin, usd(1_200));
    identity(&ex);
}

#[test]
fn account_without_a_position_is_never_liquidated() {
    let mut ex = long_victim(3_000);
    ex.deposit(4, 10_000); // only collateral
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let outs = ex.oracle(47_500).unwrap();
    assert!(started(&outs, 4).is_empty());
    // the bidder becomes a (profitable) holder by being hit, but is healthy
    assert!(started(&outs, BIDDER).is_empty());
    assert!(!started(&outs, VICTIM).is_empty());
    identity(&ex);
}

#[test]
fn profitable_counterparty_is_not_liquidated_when_the_price_falls() {
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let outs = ex.oracle(47_500).unwrap();
    assert!(started(&outs, SELLER).is_empty());
    assert!(liq_fills(&outs).iter().all(|f| f.maker != SELLER));
    identity(&ex);
}

#[test]
fn oracle_of_another_market_does_not_retry_a_stuck_account_but_tick_does() {
    let mut ex = long_victim(3_000);
    ex.apply(EventBody::AddMarket(MarketConfig::example(2)))
        .unwrap();
    ex.oracle_on(2, 50_000);
    // no bids: the victim is underwater and stuck
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(started(&outs, VICTIM).len(), 1);
    assert!(!ex.engine.is_pending_liquidation(VICTIM));
    let outs = ex.oracle_on(2, 50_100);
    assert!(started(&outs, VICTIM).is_empty());
    let outs = tick(&mut ex);
    assert_eq!(started(&outs, VICTIM).len(), 1);
}

// ---------------------------------------------------------------------------
// One liquidation step
// ---------------------------------------------------------------------------

#[test]
fn step_cancels_resting_orders_of_the_victim_only() {
    let mut ex = long_victim(3_000);
    let v1 = ex.gtc(VICTIM, Side::Buy, 40_000, 10);
    let v2 = ex.gtc(VICTIM, Side::Buy, 39_000, 20);
    let other = ex.gtc(SELLER, Side::Buy, 30_000, 100);
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let outs = ex.oracle(47_500).unwrap();
    let canceled: Vec<_> = outs
        .iter()
        .filter_map(|o| match o {
            Output::Canceled {
                order_id,
                account,
                qty,
                reason,
            } => Some((*order_id, *account, qty.0, *reason)),
            _ => None,
        })
        .collect();
    assert_eq!(
        canceled,
        vec![
            (v1, VICTIM, 100, CancelReason::Liquidation),
            (v2, VICTIM, 200, CancelReason::Liquidation),
        ]
    );
    assert!(ex.engine.open_orders_of(VICTIM).is_empty());
    assert_eq!(ex.engine.open_orders_of(SELLER), vec![other]);
    // the cancellations precede the liquidation fill
    let last_cancel = outs
        .iter()
        .rposition(|o| matches!(o, Output::Canceled { .. }))
        .unwrap();
    let first_fill = outs
        .iter()
        .position(|o| matches!(o, Output::Fill { .. }))
        .unwrap();
    assert!(last_cancel < first_fill);
    identity(&ex);
}

#[test]
fn resting_orders_are_not_cancelled_for_a_healthy_account() {
    let mut ex = long_victim(3_000);
    let v1 = ex.gtc(VICTIM, Side::Buy, 40_000, 10);
    let outs = ex.oracle(48_500).unwrap();
    assert!(!has(&outs, |o| matches!(o, Output::Canceled { .. })));
    assert_eq!(ex.engine.open_orders_of(VICTIM), vec![v1]);
}

#[test]
fn chunk_is_a_quarter_of_the_position() {
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let outs = ex.oracle(47_500).unwrap();
    let fills = liq_fills(&outs);
    assert_eq!(fills[0].qty, 2_500, "25 % of 10 000 raw");
    assert_eq!(fills[0].price, px(47_300).0);
    assert_eq!(fills[0].maker, BIDDER);
}

#[test]
fn chunk_is_rounded_up_to_the_lot() {
    // position 0.333 base = 3330 raw; 25 % = 832.5 -> 833 -> next lot (10) = 840.
    let mut ex = Ex::new();
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    ex.deposit(VICTIM, 1_200);
    ex.trade(VICTIM, SELLER, Side::Buy, 50_000, 333);
    assert_eq!(ex.size(VICTIM), 3_330);
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(started(&outs, VICTIM).len(), 1);
    assert_eq!(liq_fills(&outs)[0].qty, 840);
    identity(&ex);
}

#[test]
fn chunk_never_exceeds_the_position() {
    // 0.001 base = 10 raw: 25 % rounds up to one lot = the whole position.
    let mut ex = Ex::new();
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    ex.deposit(VICTIM, 3);
    ex.trade(VICTIM, SELLER, Side::Buy, 50_000, 1);
    assert_eq!(ex.size(VICTIM), 10);
    ex.gtc(BIDDER, Side::Buy, 47_300, 1_000);
    let outs = ex.oracle(47_500).unwrap();
    let fills = liq_fills(&outs);
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].qty, 10);
    assert_eq!(ex.size(VICTIM), 0);
    assert!(ex.collateral(VICTIM) >= 0);
    identity(&ex);
}

#[test]
fn liquidation_fill_is_flagged_and_the_opening_trade_is_not() {
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let outs = ex.oracle(47_500).unwrap();
    let flags: Vec<bool> = outs
        .iter()
        .filter_map(|o| match o {
            Output::Fill { liquidation, .. } => Some(*liquidation),
            _ => None,
        })
        .collect();
    assert!(!flags.is_empty() && flags.iter().all(|f| *f));
    let mut ex = long_victim(3_000);
    let outs = ex.limit(SELLER, Side::Buy, 49_000, 10, Tif::Gtc).unwrap();
    assert!(!any_fill(&outs));
}

#[test]
fn solvent_liquidation_never_fills_worse_than_the_equity_bound() {
    // equity 475, size 1.0: bankruptcy price 47 025. The solvent limit price is
    // mark - (equity - 512 raw dust - fee reserve): 47 500 - 379.999488.
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_120, 2_000); // one tick-multiple below the limit
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(started(&outs, VICTIM).len(), 1);
    assert!(liq_fills(&outs).is_empty(), "bid below the limit price");
    assert_eq!(ex.size(VICTIM), 10_000);
    assert!(!ex.engine.is_pending_liquidation(VICTIM));
    // a bid above the limit price (47 120.000512) is hit on the next tick
    ex.gtc(BIDDER, Side::Buy, 47_130, 2_000);
    let outs = tick(&mut ex);
    let fills = liq_fills(&outs);
    assert_eq!(fills[0].price, px(47_130).0);
    assert!(fills[0].price >= 47_025_000_000, "never below bankruptcy");
    identity(&ex);
}

#[test]
fn liquidation_price_is_bounded_by_the_band_for_an_insolvent_account() {
    // insolvent (equity -2300 at 45 125); the band price is
    // 45 125 * 0.95 = 42 868.75: a bid at 42 000 must not fill, one at 43 000 does.
    let mut ex = long_victim(2_600);
    ex.apply(EventBody::FundInsurance { amount: usd(5_000) })
        .unwrap();
    ex.gtc(BIDDER, Side::Buy, 42_000, 2_000);
    ex.oracle(47_500).unwrap();
    let outs = ex.oracle(45_125).unwrap();
    assert_eq!(started(&outs, VICTIM)[0], usd(-2_300));
    assert!(liq_fills(&outs).is_empty());
    // nothing was realised: the fund pays nothing for the unrealised -2300
    assert!(paid(&outs).is_empty());
    assert_eq!(ex.engine.insurance_fund(), usd(5_000));
    assert_eq!(ex.size(VICTIM), 10_000);
    ex.gtc(BIDDER, Side::Buy, 43_000, 2_000);
    let outs = tick(&mut ex);
    let fills = liq_fills(&outs);
    assert!(!fills.is_empty());
    assert!(
        fills.iter().all(|f| f.price == px(43_000).0),
        "only the bid inside the band is hit, best price first"
    );
    assert!(fills.iter().all(|f| f.price >= 42_868_750_000));
    assert_eq!(fills.iter().map(|f| f.qty).sum::<u64>(), 10_000);
    // realised: 1.0 sold at 43 000 = -7000 on collateral 2575 -> fund pays 4425
    assert_eq!(
        paid(&outs).iter().map(|(_, a)| a).sum::<i128>(),
        usd(4_425).0
    );
    assert_eq!(ex.engine.insurance_fund(), usd(5_000 - 4_425));
    assert!(adl(&outs).is_empty());
    assert_eq!(ex.collateral(VICTIM), 0);
    identity(&ex);
}

#[test]
fn liquidation_fee_goes_to_the_insurance_fund_not_the_fee_pool() {
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let pool_before = ex.engine.fee_pool();
    let fund_before = ex.engine.insurance_fund();
    let outs = ex.oracle(47_500).unwrap();
    let fills = liq_fills(&outs);
    assert!(!fills.is_empty());
    for f in &fills {
        let n = notional_of(f.price, f.qty);
        assert_eq!(
            f.taker_fee,
            ceil_ppm(n, 2_000),
            "0.2 % of notional, rounded up"
        );
        assert!(f.taker_fee > 0);
    }
    assert_eq!(
        ex.engine.insurance_fund().0 - fund_before.0,
        sum_taker_fees(&outs)
    );
    assert_eq!(
        ex.engine.fee_pool(),
        pool_before,
        "maker rebate not paid, no fee pool income"
    );
    identity(&ex);
}

#[test]
fn first_fill_fee_is_exactly_the_liquidation_ppm() {
    // 0.25 base at 47 300 = 11 825 quote; 2000 ppm = 23.65 quote.
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(liq_fills(&outs)[0].taker_fee, 23_650_000);
}

#[test]
fn maker_rebate_is_not_paid_on_liquidation_flow_but_is_on_normal_flow() {
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let bidder_collateral = ex.collateral(BIDDER);
    let outs = ex.oracle(47_500).unwrap();
    let fills = liq_fills(&outs);
    assert!(fills.iter().all(|f| f.maker_fee == 0));
    assert_eq!(
        ex.collateral(BIDDER),
        bidder_collateral,
        "no rebate credited"
    );

    // control: a normal taker against the same rebate-tier maker gets the rebate
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 49_000, 1_000);
    let outs = ex
        .limit(SELLER, Side::Sell, 49_000, 1_000, Tif::Ioc)
        .unwrap();
    let rebate = outs
        .iter()
        .find_map(|o| match o {
            Output::Fill {
                maker_fee,
                liquidation: false,
                ..
            } => Some(maker_fee.0),
            _ => None,
        })
        .unwrap();
    // 49 000 quote * 50 ppm = 2.45 quote
    assert_eq!(rebate, -2_450_000);
}

#[test]
fn partial_liquidation_stops_once_the_account_is_healthy() {
    // collateral 3190, at 48 000 equity 1190 < maintenance 1200. One chunk
    // (0.25 sold at 47 900, fee 23.95) leaves equity 1141.05 > 900.
    let mut ex = long_victim(3_215);
    ex.gtc(BIDDER, Side::Buy, 47_900, 2_000);
    let outs = ex.oracle(48_000).unwrap();
    assert_eq!(started(&outs, VICTIM), vec![usd(1_190)]);
    let fills = liq_fills(&outs);
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].qty, 2_500);
    assert_eq!(ex.size(VICTIM), 7_500);
    assert_eq!(ex.collateral(VICTIM), 2_641_050_000);
    let v = ex.engine.account_view(VICTIM).unwrap();
    assert_eq!(v.equity, Money(1_141_050_000));
    assert_eq!(v.maintenance_margin, usd(900));
    assert_eq!(ex.engine.summary().liquidation_steps, 1);
    assert!(!ex.engine.is_pending_liquidation(VICTIM));
    identity(&ex);
}

#[test]
fn liquidation_repeats_chunks_until_flat_or_healthy() {
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_300, 3_000);
    let outs = ex.oracle(47_500).unwrap();
    let fills = liq_fills(&outs);
    assert!(fills.len() >= 2);
    assert_eq!(fills[0].qty, 2_500);
    assert_eq!(fills[1].qty, 1_880, "25% of 7500 = 1875, up to the lot");
    assert_eq!(
        started(&outs, VICTIM).len() as u64,
        ex.engine.summary().liquidation_steps
    );
    let remaining = ex.size(VICTIM);
    assert_eq!(
        remaining,
        10_000 - fills.iter().map(|f| f.qty as i64).sum::<i64>()
    );
    if remaining != 0 {
        let v = ex.engine.account_view(VICTIM).unwrap();
        assert!(
            v.equity >= v.maintenance_margin,
            "stopped only when healthy"
        );
    }
    assert!(ex.collateral(VICTIM) >= 0);
    identity(&ex);
}

#[test]
fn a_deep_underwater_long_is_liquidated_down_to_flat() {
    // collateral 2975 at 47 500: equity 475; the book is deep so the account
    // is sold down until flat or healthy; no bad debt either way.
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_300, 3_000);
    ex.oracle(47_500).unwrap();
    ex.oracle(47_000).unwrap();
    for _ in 0..30 {
        tick(&mut ex);
    }
    assert_eq!(ex.engine.bad_debt(), Money(0));
    assert!(ex.collateral(VICTIM) >= 0);
    identity(&ex);
}

#[test]
fn short_position_is_liquidated_by_buying_from_the_asks() {
    let mut ex = short_victim(3_000);
    // 52 500: equity 2975 - 2500 = 475 < maintenance 1312.5
    ex.gtc(BIDDER, Side::Sell, 52_700, 2_000);
    let outs = ex.oracle(52_500).unwrap();
    assert_eq!(started(&outs, VICTIM)[0], usd(475));
    let fills = liq_fills(&outs);
    assert_eq!(fills[0].side, Side::Buy);
    assert_eq!(fills[0].qty, 2_500);
    assert_eq!(fills[0].price, px(52_700).0);
    // 52 700 is below the bound mark + (equity - dust - fee reserve)
    assert!(ex.size(VICTIM) > -10_000);
    identity(&ex);
}

#[test]
fn short_liquidation_does_not_buy_above_the_bound() {
    let mut ex = short_victim(3_000);
    ex.gtc(BIDDER, Side::Sell, 53_000, 2_000); // above 52 500 + ~370 = 52 870
    let outs = ex.oracle(52_500).unwrap();
    assert_eq!(started(&outs, VICTIM).len(), 1);
    assert!(liq_fills(&outs).is_empty());
    assert_eq!(ex.size(VICTIM), -10_000);
    identity(&ex);
}

// ---------------------------------------------------------------------------
// Stuck accounts, staleness, per-event budget
// ---------------------------------------------------------------------------

#[test]
fn account_is_stuck_when_the_book_is_empty_and_retried_each_event() {
    let mut ex = long_victim(3_000);
    ex.gtc(VICTIM, Side::Buy, 40_000, 10);
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(started(&outs, VICTIM).len(), 1);
    assert!(liq_fills(&outs).is_empty());
    assert_eq!(ex.size(VICTIM), 10_000, "no progress");
    // its resting order was still cancelled by the step
    assert!(ex.engine.open_orders_of(VICTIM).is_empty());
    assert!(!ex.engine.is_pending_liquidation(VICTIM));
    assert_eq!(ex.engine.summary().liquidation_steps, 1);
    // retried on the next oracle update and on the next tick
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(started(&outs, VICTIM).len(), 1);
    let outs = tick(&mut ex);
    assert_eq!(started(&outs, VICTIM).len(), 1);
    assert_eq!(ex.engine.summary().liquidation_steps, 3);
    // as soon as liquidity appears the next tick makes progress
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let outs = tick(&mut ex);
    assert_eq!(liq_fills(&outs)[0].qty, 2_500);
    assert!(ex.size(VICTIM) < 10_000);
    identity(&ex);
}

#[test]
fn no_liquidation_while_the_oracle_is_stale() {
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_000, 2_000); // below the limit: stuck
    ex.oracle(47_500).unwrap();
    assert_eq!(ex.size(VICTIM), 10_000);
    let oracle_ts = ex.ts;
    let outs = ex.apply_at(oracle_ts + 60_001, EventBody::Tick).unwrap();
    assert!(started(&outs, VICTIM).is_empty(), "stale: not evaluated");
    assert!(!ex.engine.is_pending_liquidation(VICTIM));
    assert_eq!(ex.engine.summary().liquidation_steps, 1);
    identity(&ex);
}

#[test]
fn liquidation_resumes_at_exactly_the_staleness_limit() {
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_000, 2_000);
    ex.oracle(47_500).unwrap();
    let oracle_ts = ex.ts;
    let outs = ex.apply_at(oracle_ts + 60_000, EventBody::Tick).unwrap();
    assert_eq!(
        started(&outs, VICTIM).len(),
        1,
        "60 000 ms is not yet stale"
    );
}

#[test]
fn account_is_examined_again_after_a_fresh_oracle_update() {
    let mut ex = long_victim(3_000);
    ex.gtc(BIDDER, Side::Buy, 47_000, 2_000); // below the limit: stuck
    ex.oracle(47_500).unwrap();
    let outs = ex.apply_at(ex.ts + 61_000, EventBody::Tick).unwrap();
    assert!(started(&outs, VICTIM).is_empty());
    // the oracle recovers (same price, newer source time)
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(started(&outs, VICTIM).len(), 1);
    identity(&ex);
}

fn budget_one_engine() -> Ex {
    let mut ex = Ex::bare();
    ex.engine = Engine::new();
    ex.ts = 1_000;
    let cfg = EngineConfig {
        max_liquidations_per_event: 1,
        ..EngineConfig::default()
    };
    ex.apply(EventBody::Configure(cfg)).unwrap();
    ex.apply(EventBody::AddMarket(MarketConfig::example(M)))
        .unwrap();
    ex.oracle_on(M, 50_000);
    ex
}

#[test]
fn per_event_budget_defers_excess_accounts() {
    let mut ex = budget_one_engine();
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    let victims = [11u64, 12, 13];
    for v in victims {
        ex.deposit(v, 3_000);
        ex.trade(v, SELLER, Side::Buy, 50_000, 1_000);
    }
    ex.gtc(BIDDER, Side::Buy, 47_300, 10_000);
    let outs = ex.oracle(47_500).unwrap();
    // one step only, for the lowest id
    let s: Vec<u64> = outs
        .iter()
        .filter_map(|o| match o {
            Output::LiquidationStarted { account, .. } => Some(*account),
            _ => None,
        })
        .collect();
    assert_eq!(s, vec![11]);
    assert_eq!(ex.engine.summary().liquidation_steps, 1);
    assert!(ex.engine.is_pending_liquidation(11));
    assert!(ex.engine.is_pending_liquidation(12));
    assert!(ex.engine.is_pending_liquidation(13));
    assert_eq!(ex.size(12), 10_000);
    assert_eq!(ex.size(13), 10_000);
    // later ticks drain the queue
    for _ in 0..200 {
        tick(&mut ex);
        if victims
            .iter()
            .all(|v| !ex.engine.is_pending_liquidation(*v))
        {
            break;
        }
    }
    for v in victims {
        assert!(!ex.engine.is_pending_liquidation(v));
        assert!(ex.size(v) < 10_000, "account {v} was handled");
    }
    identity(&ex);
}

#[test]
fn default_budget_handles_many_accounts_in_one_event() {
    let mut ex = Ex::new();
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    for v in [11u64, 12, 13] {
        ex.deposit(v, 3_000);
        ex.trade(v, SELLER, Side::Buy, 50_000, 1_000);
    }
    ex.gtc(BIDDER, Side::Buy, 47_300, 10_000);
    let outs = ex.oracle(47_500).unwrap();
    for v in [11u64, 12, 13] {
        assert!(!started(&outs, v).is_empty());
        assert!(ex.size(v) < 10_000);
        assert!(!ex.engine.is_pending_liquidation(v));
    }
    identity(&ex);
}

// ---------------------------------------------------------------------------
// Insolvency: insurance fund, auto-deleveraging, bad debt
// ---------------------------------------------------------------------------

/// Victim 2600 deposit (collateral 2575). Index falls 50 000 -> 47 500 (bid at
/// 45 000 is below the solvent limit so the account stays stuck) -> 45 125
/// where equity is -2300.
fn insolvent_with_fund(fund: i128) -> (Ex, Vec<Output>) {
    let mut ex = long_victim(2_600);
    if fund > 0 {
        ex.apply(EventBody::FundInsurance { amount: usd(fund) })
            .unwrap();
    }
    ex.gtc(BIDDER, Side::Buy, 45_000, 2_000);
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(started(&outs, VICTIM), vec![usd(75)]);
    assert!(liq_fills(&outs).is_empty());
    let outs = ex.oracle(45_125).unwrap();
    (ex, outs)
}

#[test]
fn insurance_fund_pays_exactly_the_realised_shortfall_while_the_book_liquidates() {
    // Bid depth 2.0 at 45 000; the victim (collateral 2575, equity -2300 at
    // 45 125) is sold down chunk by chunk (25 % each, rounded up to the lot):
    // 2500, 1880, 1410, 1060, ... raw. Every raw unit sold at 45 000 realises
    // 0.5 quote of loss (basis 5.0 per raw unit).
    let (ex, outs) = insolvent_with_fund(5_000);
    assert_eq!(started(&outs, VICTIM)[0], usd(-2_300));
    let fills = liq_fills(&outs);
    assert_eq!(fills[0].qty, 2_500);
    assert_eq!(fills[1].qty, 1_880);
    assert_eq!(fills[2].qty, 1_410);
    assert!(fills.iter().all(|f| f.price == px(45_000).0));
    assert!(
        fills.iter().all(|f| f.taker_fee == 0),
        "no liquidation fee on an insolvent account"
    );
    assert_eq!(fills.iter().map(|f| f.qty).sum::<u64>(), 10_000);
    assert_eq!(ex.engine.position(VICTIM, M), None);
    // collateral 2575 stays positive for the first two chunks (-1250, -940):
    // no payment. The third chunk (-705) takes it to -320: the fund pays 320.
    // From then on every chunk realises its whole loss on a zero balance.
    let payments = paid(&outs);
    assert_eq!(payments[0], (VICTIM, usd(320).0));
    assert_eq!(payments.len(), fills.len() - 2);
    // in total the fund pays what the sale lost beyond the collateral:
    // 10_000 raw * 0.5 = 5000 loss - 2575 collateral = 2425
    let total: i128 = payments.iter().map(|(_, a)| a).sum();
    assert_eq!(total, usd(2_425).0);
    assert_eq!(ex.engine.insurance_fund(), usd(5_000 - 2_425));
    // the payment follows the fill of its own step directly
    let first_pay = outs
        .iter()
        .position(|o| matches!(o, Output::InsurancePaid { .. }))
        .unwrap();
    assert!(matches!(
        outs[first_pay - 1],
        Output::Fill {
            liquidation: true,
            ..
        }
    ));
    assert_eq!(
        outs.iter()
            .take(first_pay)
            .filter(|o| matches!(o, Output::Fill { .. }))
            .count(),
        3
    );
    assert!(
        adl(&outs).is_empty(),
        "fund was sufficient: no deleveraging"
    );
    assert_eq!(ex.engine.summary().adl_fills, 0);
    assert_eq!(ex.engine.bad_debt(), Money(0));
    assert_eq!(ex.collateral(VICTIM), 0);
    identity(&ex);
}

#[test]
fn no_insurance_payment_is_emitted_for_a_solvent_liquidation() {
    let mut ex = long_victim(3_000);
    ex.apply(EventBody::FundInsurance { amount: usd(5_000) })
        .unwrap();
    ex.gtc(BIDDER, Side::Buy, 47_300, 2_000);
    let fund = ex.engine.insurance_fund();
    let outs = ex.oracle(47_500).unwrap();
    assert!(paid(&outs).is_empty());
    assert!(adl(&outs).is_empty());
    assert!(
        ex.engine.insurance_fund().0 > fund.0,
        "fund grows by the liquidation fees"
    );
}

/// ADL scenario. Victim 1 is long 3.5 base (bought 1.0 at 49 500 from 20 and
/// 2.5 at 50 000 from 9, 10, 11); shorts: 9, 10, 40 with 1.0 at 50 000 each,
/// 11 with 0.5 at 50 000, 20 with 1.0 at 49 500 (ratio lowest). Account 41 is
/// long 1.0 (opposite of 40). The oracle falls to 45 125 with an empty book.
fn adl_scenario(fund: i128) -> (Ex, Vec<Output>, i128) {
    let mut ex = Ex::new();
    ex.fund(&[9, 10, 11, 20, 40, 41], 100_000);
    ex.deposit(VICTIM, 9_000);
    if fund > 0 {
        ex.apply(EventBody::FundInsurance { amount: usd(fund) })
            .unwrap();
    }
    ex.trade(41, 40, Side::Buy, 50_000, 1_000);
    ex.gtc(20, Side::Sell, 49_500, 1_000);
    ex.gtc(9, Side::Sell, 50_000, 1_000);
    ex.gtc(10, Side::Sell, 50_000, 1_000);
    ex.gtc(11, Side::Sell, 50_000, 500);
    let outs = ex
        .limit(VICTIM, Side::Buy, 50_000, 3_500, Tif::Ioc)
        .unwrap();
    assert_eq!(filled_qty(&outs), 35_000);
    let outs = ex.oracle(47_500).unwrap();
    assert_eq!(started(&outs, VICTIM).len(), 1);
    assert!(adl(&outs).is_empty());
    assert_eq!(ex.size(VICTIM), 35_000);

    // deficit at 45 125 computed from the state
    let pos = ex.engine.position(VICTIM, M).unwrap();
    let col = ex.collateral(VICTIM);
    let value = i128::from(pos.size) * 45_125_000_000 / 10_000;
    let deficit = -(col + value - pos.basis);
    assert!(deficit > 0);
    let outs = ex.oracle(45_125).unwrap();
    (ex, outs, deficit)
}

fn adl_price(deficit_after_fund: i128) -> u64 {
    let per_unit = (deficit_after_fund * 10_000 + 34_999) / 35_000;
    45_125_000_000 + u64::try_from(per_unit).unwrap()
}

#[test]
fn small_fund_pays_what_it_has_then_deleverages_in_ranking_order() {
    let (ex, outs, deficit) = adl_scenario(100);
    assert_eq!(deficit, 7_649_750_000);
    assert_eq!(paid(&outs), vec![(VICTIM, usd(100).0)]);
    let price = adl_price(deficit - usd(100).0);
    // profit ratio first (20 has the smallest and is never reached), then size
    // (11 holds only 0.5), then account id (9 < 10 < 40)
    assert_eq!(
        adl(&outs),
        vec![
            (VICTIM, 9, price, 10_000),
            (VICTIM, 10, price, 10_000),
            (VICTIM, 40, price, 10_000),
            (VICTIM, 11, price, 5_000),
        ]
    );
    // payment is emitted before any deleveraging
    let p = outs
        .iter()
        .position(|o| matches!(o, Output::InsurancePaid { .. }))
        .unwrap();
    let d = outs
        .iter()
        .position(|o| matches!(o, Output::Deleveraged { .. }))
        .unwrap();
    assert!(p < d);
    assert_eq!(ex.engine.summary().adl_fills, 4);
    assert_eq!(ex.engine.insurance_fund(), Money(0));
    assert_eq!(ex.engine.position(VICTIM, M), None);
    assert!(ex.collateral(VICTIM) >= 0);
    assert_eq!(ex.engine.bad_debt(), Money(0));
    // reduced / untouched counterparties
    assert_eq!(ex.size(9), 0);
    assert_eq!(ex.size(10), 0);
    assert_eq!(ex.size(40), 0);
    assert_eq!(ex.size(11), 0);
    assert_eq!(ex.size(20), -10_000, "lowest profit ratio is untouched");
    assert_eq!(
        ex.size(41),
        10_000,
        "longs are never deleveraged against a long"
    );
    // no book liquidation was attempted on the ADL step
    assert!(liq_fills(&outs).is_empty());
    identity(&ex);
}

#[test]
fn empty_fund_deleverages_without_an_insurance_payment() {
    let (ex, outs, deficit) = adl_scenario(0);
    assert!(paid(&outs).is_empty(), "nothing to pay: no InsurancePaid");
    let price = adl_price(deficit);
    let a = adl(&outs);
    assert_eq!(a.len(), 4);
    assert!(a.iter().all(|(b, _, p, _)| *b == VICTIM && *p == price));
    assert_eq!(a.iter().map(|x| x.3).sum::<u64>(), 35_000);
    assert_eq!(ex.engine.position(VICTIM, M), None);
    assert!(ex.collateral(VICTIM) >= 0);
    assert_eq!(ex.engine.bad_debt(), Money(0));
    identity(&ex);
}

#[test]
fn adl_executes_at_the_bankruptcy_price_not_the_mark() {
    let (ex, outs, deficit) = adl_scenario(0);
    let price = adl_price(deficit);
    assert!(
        price > 45_125_000_000,
        "a long sells above mark when insolvent"
    );
    // the victim's closing proceeds are exactly its remaining equity: zero
    let a = adl(&outs);
    assert!(a.iter().all(|x| x.2 == price));
    assert!(ex.collateral(VICTIM) <= 4, "only rounding dust remains");
}

#[test]
fn adl_counterparties_are_reduced_at_the_adl_price() {
    // 9 (short 1.0 @50 000, collateral 100 000 - 10 maker fee) closes at the ADL price.
    let (ex, outs, deficit) = adl_scenario(0);
    let price = adl_price(deficit);
    let col9_before = 100_000_000_000i128 - 10_000_000;
    let n = (i128::from(price) * 10_000 + 9_999) / 10_000; // Round::Ceil for a long
    let realized = 50_000_000_000 - n;
    assert_eq!(ex.collateral(9), col9_before + realized);
    assert!(!adl(&outs).is_empty());
    identity(&ex);
}

#[test]
fn deleveraged_event_is_not_emitted_when_the_book_liquidates() {
    let (_, outs) = insolvent_with_fund(5_000);
    assert!(!has(&outs, |o| matches!(o, Output::Deleveraged { .. })));
}

// --- shortfall of a flat account --------------------------------------------
//
// A normal order can no longer realise a loss bigger than the account's
// collateral (the price band and the `equity - adverse >= 0` entry check), so a
// flat account with negative collateral can only come out of a liquidation. The
// scenarios use a 0.001 base position (one lot): a liquidation chunk is then
// the whole position.
//
//   deposit 3, fee 0.025 -> collateral 2.975, long 0.001 @ 50 000 (basis 50)
//   mark 47 500: equity 0.475 < maintenance 1.1875, but the only bid (45 000)
//                is below the solvent limit price 47 120.512 -> stuck
//   mark 45 125: equity -1.9 (insolvent)

/// Returns the engine and the outputs of the two oracle updates.
fn tiny_insolvent(fund: i128) -> (Ex, Vec<Output>, Vec<Output>) {
    let mut ex = Ex::new();
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    ex.deposit(VICTIM, 3);
    if fund > 0 {
        ex.apply(EventBody::FundInsurance { amount: usd(fund) })
            .unwrap();
    }
    ex.trade(VICTIM, SELLER, Side::Buy, 50_000, 1);
    assert_eq!(ex.size(VICTIM), 10);
    assert_eq!(ex.collateral(VICTIM), 2_975_000);
    ex.gtc(BIDDER, Side::Buy, 45_000, 2_000);
    let first = ex.oracle(47_500).unwrap();
    let second = ex.oracle(45_125).unwrap();
    (ex, first, second)
}

#[test]
fn flat_account_with_negative_collateral_is_covered_by_the_fund() {
    let (ex, first, second) = tiny_insolvent(10);
    // 47 500: liquidation attempted, nothing sold, nothing paid
    assert_eq!(started(&first, VICTIM), vec![Money(475_000)]);
    assert!(liq_fills(&first).is_empty());
    assert!(paid(&first).is_empty());
    // 45 125: the whole position (chunk = one lot) is sold at the bid 45 000:
    // realised -5 on collateral 2.975 -> flat with collateral -2.025
    assert_eq!(started(&second, VICTIM), vec![Money(-1_900_000)]);
    let fills = liq_fills(&second);
    assert_eq!(fills.len(), 1);
    assert_eq!((fills[0].price, fills[0].qty), (px(45_000).0, 10));
    assert_eq!(fills[0].taker_fee, 0);
    assert_eq!(paid(&second), vec![(VICTIM, 2_025_000)]);
    assert_eq!(ex.collateral(VICTIM), 0);
    assert_eq!(ex.engine.position(VICTIM, M), None);
    assert_eq!(ex.engine.insurance_fund(), Money(10_000_000 - 2_025_000));
    assert_eq!(ex.engine.bad_debt(), Money(0));
    assert!(!has(&second, |o| matches!(o, Output::Deleveraged { .. })));
    identity(&ex);
}

#[test]
fn empty_fund_closes_the_tiny_position_by_adl_at_the_bankruptcy_price() {
    let (ex, first, second) = tiny_insolvent(0);
    assert_eq!(started(&first, VICTIM), vec![Money(475_000)]);
    assert_eq!(started(&second, VICTIM), vec![Money(-1_900_000)]);
    // fund (0) < deficit 1.9 + slippage 2.25625 + dust: no book liquidation;
    // nothing to pay, bankruptcy price = entry - collateral / size = 47 025
    assert!(liq_fills(&second).is_empty());
    assert!(paid(&second).is_empty());
    assert_eq!(adl(&second), vec![(VICTIM, SELLER, px(47_025).0, 10)]);
    assert_eq!(ex.collateral(VICTIM), 0);
    assert_eq!(ex.engine.position(VICTIM, M), None);
    assert_eq!(ex.engine.bad_debt(), Money(0));
    assert_eq!(ex.engine.summary().adl_fills, 1);
    // the short counterparty is closed at 47 025 and gains 2.975
    assert_eq!(ex.size(SELLER), 0);
    identity(&ex);
}

#[test]
fn fund_below_the_allowance_pays_the_deficit_here_and_adl_closes_at_the_mark() {
    // allowance = 1.9 + 2.25625 + 0.000512 > 3: ADL; the fund pays
    // min(deficit 1.9, 3) = 1.9, so the bankruptcy price equals the mark.
    let (ex, _, second) = tiny_insolvent(3);
    assert_eq!(paid(&second), vec![(VICTIM, 1_900_000)]);
    assert_eq!(adl(&second), vec![(VICTIM, SELLER, px(45_125).0, 10)]);
    assert!(liq_fills(&second).is_empty());
    assert_eq!(ex.engine.insurance_fund(), Money(3_000_000 - 1_900_000));
    assert_eq!(ex.collateral(VICTIM), 0);
    assert_eq!(ex.engine.bad_debt(), Money(0));
    identity(&ex);
}

#[test]
fn fund_pays_only_realised_losses_never_the_unrealised_deficit() {
    // 1.0 base long, collateral 2575, bid only 0.1 base deep at 45 000.
    // At 45 125 the equity is -2300 but nothing is realised beyond what the
    // 0.1 base sale loses: 0.1 * (50 000 - 45 000) = 500 < 2575 collateral.
    let mut ex = long_victim(2_600);
    ex.apply(EventBody::FundInsurance { amount: usd(5_000) })
        .unwrap();
    ex.gtc(BIDDER, Side::Buy, 45_000, 100);
    ex.oracle(47_500).unwrap();
    let outs = ex.oracle(45_125).unwrap();
    assert_eq!(started(&outs, VICTIM)[0], usd(-2_300));
    let fills = liq_fills(&outs);
    assert_eq!(fills.len(), 1, "only 0.1 base can be sold");
    assert_eq!((fills[0].price, fills[0].qty), (px(45_000).0, 1_000));
    assert!(paid(&outs).is_empty(), "the unrealised -2300 is not paid");
    assert_eq!(ex.collateral(VICTIM), usd(2_075).0);
    assert_eq!(ex.engine.insurance_fund(), usd(5_000));
    assert_eq!(ex.size(VICTIM), 9_000);
    identity(&ex);
}

#[test]
fn bad_debt_is_zero_in_all_covered_scenarios() {
    let (ex, _) = insolvent_with_fund(5_000);
    assert_eq!(ex.engine.bad_debt(), Money(0));
    assert_eq!(ex.engine.summary().bad_debt, Money(0));
    let (ex, _, _) = adl_scenario(100);
    assert_eq!(ex.engine.summary().bad_debt, Money(0));
}

// ---------------------------------------------------------------------------
// Cross margin across markets
// ---------------------------------------------------------------------------

fn trade_in(ex: &mut Ex, market: u32, taker: u64, maker: u64, side: Side, price: u64, milli: u64) {
    let mut p = ex.order(
        maker,
        side.opposite(),
        OrderKind::Limit {
            price: px(price),
            tif: Tif::Gtc,
        },
        milli,
    );
    p.market = market;
    ex.place(p).unwrap();
    let mut p = ex.order(
        taker,
        side,
        OrderKind::Limit {
            price: px(price),
            tif: Tif::Ioc,
        },
        milli,
    );
    p.market = market;
    let outs = ex.place(p).unwrap();
    assert!(any_fill(&outs));
}

fn bid_in(ex: &mut Ex, market: u32, account: u64, price: u64, milli: u64) {
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
    ex.place(p).unwrap();
}

#[test]
fn cross_margin_account_is_liquidated_lowest_market_first() {
    let mut ex = Ex::new();
    ex.apply(EventBody::AddMarket(MarketConfig::example(2)))
        .unwrap();
    ex.oracle_on(2, 50_000);
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    ex.deposit(VICTIM, 5_300);
    trade_in(&mut ex, 2, VICTIM, SELLER, Side::Buy, 50_000, 1_000);
    trade_in(&mut ex, 1, VICTIM, SELLER, Side::Buy, 50_000, 1_000);
    bid_in(&mut ex, 1, BIDDER, 47_400, 3_000);
    bid_in(&mut ex, 2, BIDDER, 47_400, 3_000);

    // equity after only market 1 fell: 5250 - 2500 = 2750 > 2437.5: healthy
    let outs = ex.oracle_on(1, 47_500);
    assert!(started(&outs, VICTIM).is_empty());
    // after both fell: 250 < 2375
    let outs = ex.oracle_on(2, 47_500);
    assert!(!started(&outs, VICTIM).is_empty());
    let fills = liq_fills(&outs);
    assert_eq!(fills[0].market, 1, "lowest market id first");
    assert_eq!(fills[0].qty, 2_500);
    // every market-2 fill comes after every market-1 fill
    let last_m1 = fills.iter().rposition(|f| f.market == 1).unwrap();
    if let Some(first_m2) = fills.iter().position(|f| f.market == 2) {
        assert!(last_m1 < first_m2);
        assert_eq!(ex.engine.position(VICTIM, 1), None, "market 1 closed first");
    }
    assert_eq!(ex.engine.bad_debt(), Money(0));
    identity(&ex);
}

#[test]
fn gains_in_one_market_offset_losses_in_another_for_the_check() {
    // Long 1.0 in market 1 and short 1.0 in market 2; both indexes fall in
    // alternating 5 % steps. The long loses exactly what the short gains, so
    // equity (5150) stays above the summed maintenance margin; ignoring the
    // gain it would be 275 after the last step.
    let mut ex = Ex::new();
    ex.apply(EventBody::AddMarket(MarketConfig::example(2)))
        .unwrap();
    ex.oracle_on(2, 50_000);
    ex.fund(&[SELLER, BIDDER], 1_000_000);
    ex.deposit(VICTIM, 5_200);
    trade_in(&mut ex, 1, VICTIM, SELLER, Side::Buy, 50_000, 1_000);
    trade_in(&mut ex, 2, VICTIM, SELLER, Side::Sell, 50_000, 1_000);
    for (market, price) in [(1, 47_500), (2, 47_500), (1, 45_125), (2, 45_125)] {
        let outs = ex.oracle_on(market, price);
        assert!(
            started(&outs, VICTIM).is_empty(),
            "market {market} -> {price}"
        );
    }
    assert_eq!(
        ex.engine.account_view(VICTIM).unwrap().equity,
        Money(5_150_000_000)
    );
    assert_eq!(ex.engine.summary().liquidation_steps, 0);
    identity(&ex);
}
