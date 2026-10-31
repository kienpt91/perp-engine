//! Harness smoke test: a crossing trade opens opposite positions.
#![forbid(unsafe_code)]
mod common;
use common::*;
use perp_domain::types::Side;

#[test]
fn crossing_orders_open_opposite_positions() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 10_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    assert_eq!(ex.size(1), 10_000);
    assert_eq!(ex.size(2), -10_000);
    ex.check();
}
