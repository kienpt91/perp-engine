//! Human-readable formatting of fixed-point values and dumps.

use perp_domain::fixed::{MONEY_SCALE, PRICE_SCALE, QTY_SCALE};
use perp_domain::sha256::to_hex;
use perp_domain::views::{AccountView, BookView};
use std::fmt::Write;

/// Format `raw / 10^decimals` exactly, without floating point.
pub fn scaled(raw: i128, scale: u64) -> String {
    let scale = i128::from(scale);
    let sign = if raw < 0 { "-" } else { "" };
    let abs = raw.unsigned_abs();
    let scale_u = scale.unsigned_abs();
    let width = scale_u.to_string().len().saturating_sub(1);
    format!("{sign}{}.{:0width$}", abs / scale_u, abs % scale_u)
}

/// Price in quote units.
pub fn price(raw: u64) -> String {
    scaled(i128::from(raw), PRICE_SCALE)
}

/// Quantity in base units.
pub fn qty(raw: u64) -> String {
    scaled(i128::from(raw), QTY_SCALE)
}

/// Signed quantity in base units.
pub fn signed_qty(raw: i64) -> String {
    scaled(i128::from(raw), QTY_SCALE)
}

/// Money in quote units.
pub fn money(raw: i128) -> String {
    scaled(raw, MONEY_SCALE)
}

/// Short hex form of a digest.
pub fn short_hash(d: &[u8; 32]) -> String {
    to_hex(d)
}

/// Order book dump: asks (best last) above bids (best first).
pub fn book(view: &BookView) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "market {}", view.market);
    let _ = writeln!(s, "  {:>18} {:>16} {:>7}", "price", "size", "orders");
    for l in view.asks.iter().rev() {
        let _ = writeln!(
            s,
            "  {:>18} {:>16} {:>7}  ask",
            price(l.price.0),
            qty(l.qty.0),
            l.orders
        );
    }
    for l in &view.bids {
        let _ = writeln!(
            s,
            "  {:>18} {:>16} {:>7}  bid",
            price(l.price.0),
            qty(l.qty.0),
            l.orders
        );
    }
    if view.asks.is_empty() && view.bids.is_empty() {
        let _ = writeln!(s, "  (empty)");
    }
    s
}

/// Account and position dump.
pub fn account(a: &AccountView) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "account {}: collateral {} equity {} init-margin {} maint-margin {} open-orders {} fee-tier {}",
        a.id,
        money(a.collateral.0),
        money(a.equity.0),
        money(a.initial_margin.0),
        money(a.maintenance_margin.0),
        a.open_orders,
        a.fee_tier
    );
    for p in &a.positions {
        let entry = p
            .entry_price
            .map_or_else(|| "-".to_string(), |e| price(e.0));
        let mark = p.mark.map_or_else(|| "-".to_string(), |m| price(m.0));
        let _ = writeln!(
            s,
            "  market {} size {:>14} entry {:>16} mark {:>16} unrealized {}",
            p.market,
            signed_qty(p.size),
            entry,
            mark,
            money(p.unrealized.0)
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use perp_domain::fixed::{Money, Price, Qty};
    use perp_domain::views::{LevelView, PositionView};

    #[test]
    fn scaled_prints_exact_decimals() {
        assert_eq!(price(50_000_000_000), "50000.000000");
        assert_eq!(qty(15), "0.0015");
        assert_eq!(money(-1), "-0.000001");
        assert_eq!(money(0), "0.000000");
        assert_eq!(signed_qty(-10_000), "-1.0000");
        assert!(money(i128::MIN).starts_with('-'));
    }

    #[test]
    fn book_dump_orders_asks_above_bids() {
        let v = BookView {
            market: 1,
            bids: vec![LevelView {
                price: Price(99_000_000),
                qty: Qty(20),
                orders: 1,
            }],
            asks: vec![
                LevelView {
                    price: Price(101_000_000),
                    qty: Qty(10),
                    orders: 2,
                },
                LevelView {
                    price: Price(102_000_000),
                    qty: Qty(30),
                    orders: 1,
                },
            ],
        };
        let text = book(&v);
        let ask_far = text.find("102.000000").unwrap();
        let ask_near = text.find("101.000000").unwrap();
        let bid = text.find("99.000000").unwrap();
        assert!(ask_far < ask_near && ask_near < bid);
        assert!(book(&BookView {
            market: 2,
            bids: vec![],
            asks: vec![]
        })
        .contains("(empty)"));
    }

    #[test]
    fn account_dump_lists_positions() {
        let a = AccountView {
            id: 7,
            collateral: Money(1_000_000),
            equity: Money(900_000),
            initial_margin: Money(5),
            maintenance_margin: Money(2),
            open_orders: 1,
            fee_tier: 0,
            positions: vec![
                PositionView {
                    market: 1,
                    size: -10_000,
                    entry_price: Some(Price(1_000_000)),
                    mark: Some(Price(1_100_000)),
                    unrealized: Money(-100_000),
                },
                PositionView {
                    market: 2,
                    size: 5,
                    entry_price: None,
                    mark: None,
                    unrealized: Money(0),
                },
            ],
        };
        let t = account(&a);
        assert!(t.contains("account 7") && t.contains("collateral 1.000000"));
        assert!(t.contains("-1.0000") && t.contains("unrealized -0.100000"));
        assert!(t.contains("entry                -"));
        assert_eq!(short_hash(&[0xab; 32]).len(), 64);
    }
}
