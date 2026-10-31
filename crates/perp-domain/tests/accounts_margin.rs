//! Domain tests: configuration, accounts, collateral, cross-margin accounting,
//! position lifecycle, margin schedule, fees, time and halt behaviour of the
//! exchange aggregate.
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
use perp_domain::fixed::{Money, Price, MAX_AMOUNT};
use perp_domain::types::{
    EngineConfig, Event, EventBody, FeeTier, MarketConfig, MarketId, OrderKind, Output, PlaceOrder,
    Side, Tif,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// `sum(collateral) + insurance + fee_pool - sum(basis) == net_deposits + bad_debt`
fn assert_conserved(ex: &Ex) {
    let e = &ex.engine;
    assert_eq!(
        e.total_collateral().0 + e.insurance_fund().0 + e.fee_pool().0 - e.total_basis().0,
        e.net_deposits().0 + e.bad_debt().0,
        "money identity"
    );
}

fn raw_apply(e: &mut Engine, ts: u64, body: EventBody) -> Result<Vec<Output>, Error> {
    e.apply(&Event { ts, body })
}

/// Rest a limit order in an arbitrary market.
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

/// Trade in an arbitrary market: `maker` rests, `taker` crosses with IOC.
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
    assert!(!fills(&outs).is_empty(), "no fill: {outs:?}");
}

/// Limit order at a raw (sub-unit) price.
fn limit_raw(
    ex: &mut Ex,
    account: u64,
    side: Side,
    raw_price: u64,
    milli: u64,
    tif: Tif,
) -> Result<Vec<Output>, Error> {
    let p: PlaceOrder = ex.order(
        account,
        side,
        OrderKind::Limit {
            price: Price(raw_price),
            tif,
        },
        milli,
    );
    ex.place(p)
}

fn fee_of(outs: &[Output]) -> (Money, Money) {
    let mut it = outs.iter().filter_map(|o| match o {
        Output::Fill {
            taker_fee,
            maker_fee,
            ..
        } => Some((*taker_fee, *maker_fee)),
        _ => None,
    });
    let f = it.next().expect("a fill");
    assert!(it.next().is_none(), "exactly one fill expected");
    f
}

fn init_margin(ex: &Ex, account: u64) -> Money {
    ex.engine.account_view(account).unwrap().initial_margin
}

fn maint_margin(ex: &Ex, account: u64) -> Money {
    ex.engine.account_view(account).unwrap().maintenance_margin
}

// ---------------------------------------------------------------------------
// Configure
// ---------------------------------------------------------------------------

#[test]
fn events_before_configure_are_rejected_with_not_configured() {
    let mut e = Engine::new();
    for body in [
        EventBody::Deposit {
            account: 1,
            amount: usd(1),
        },
        EventBody::AddMarket(MarketConfig::example(1)),
        EventBody::FundInsurance { amount: usd(1) },
        EventBody::Tick,
    ] {
        assert_eq!(raw_apply(&mut e, 10, body), Err(Error::NotConfigured));
    }
    assert!(e.config().is_none());
    assert!(e.market_ids().is_empty());
    assert!(e.account_ids().is_empty());
    assert_eq!(e.events_applied(), 4);
    assert_eq!(e.clock(), 0, "rejected events do not move the clock");
}

#[test]
fn configure_succeeds_once_then_is_already_configured() {
    let mut e = Engine::new();
    let outs = raw_apply(&mut e, 5, EventBody::Configure(EngineConfig::default())).unwrap();
    assert_eq!(outs, vec![Output::Configured]);
    assert_eq!(e.config(), Some(&EngineConfig::default()));
    assert_eq!(e.clock(), 5);

    let other = EngineConfig {
        max_accounts: 7,
        ..EngineConfig::default()
    };
    let before = e.content_hash();
    assert_eq!(
        raw_apply(&mut e, 6, EventBody::Configure(other)),
        Err(Error::AlreadyConfigured)
    );
    assert_eq!(e.config(), Some(&EngineConfig::default()), "config kept");
    assert_eq!(e.content_hash(), before);
}

#[test]
fn invalid_engine_configs_are_rejected_and_engine_stays_unconfigured() {
    let tier = |m: i32, t: u32| FeeTier {
        maker_ppm: m,
        taker_ppm: t,
    };
    let base = EngineConfig::default();
    let cases: Vec<(EngineConfig, &'static str)> = vec![
        (
            EngineConfig {
                max_accounts: 0,
                ..base.clone()
            },
            "max_accounts",
        ),
        (
            EngineConfig {
                max_accounts: 1_000_001,
                ..base.clone()
            },
            "max_accounts",
        ),
        (
            EngineConfig {
                max_liquidations_per_event: 0,
                ..base.clone()
            },
            "max_liquidations_per_event",
        ),
        (
            EngineConfig {
                max_liquidations_per_event: 10_001,
                ..base.clone()
            },
            "max_liquidations_per_event",
        ),
        (
            EngineConfig {
                fee_tiers: vec![],
                ..base.clone()
            },
            "fee_tiers count",
        ),
        (
            EngineConfig {
                fee_tiers: vec![tier(0, 100); 9],
                ..base.clone()
            },
            "fee_tiers count",
        ),
        (
            EngineConfig {
                fee_tiers: vec![tier(0, 100_001)],
                ..base.clone()
            },
            "fee tier range",
        ),
        (
            EngineConfig {
                fee_tiers: vec![tier(-100_001, 200_000)],
                ..base.clone()
            },
            "fee tier range",
        ),
        (
            EngineConfig {
                fee_tiers: vec![tier(-400, 300)],
                ..base.clone()
            },
            "rebate exceeds taker fee",
        ),
        (
            EngineConfig {
                fee_tiers: vec![tier(-400, 500), tier(0, 300)],
                ..base.clone()
            },
            "rebate exceeds lowest taker fee",
        ),
    ];
    for (cfg, why) in cases {
        let mut e = Engine::new();
        assert_eq!(
            raw_apply(&mut e, 1, EventBody::Configure(cfg)),
            Err(Error::InvalidConfig(why))
        );
        assert!(e.config().is_none());
        // the engine still expects a (valid) Configure
        assert_eq!(
            raw_apply(&mut e, 2, EventBody::Tick),
            Err(Error::NotConfigured)
        );
        raw_apply(&mut e, 3, EventBody::Configure(EngineConfig::default())).unwrap();
    }
}

#[test]
fn engine_config_boundaries_are_accepted() {
    let mut e = Engine::new();
    let cfg = EngineConfig {
        max_accounts: 1_000_000,
        max_liquidations_per_event: 10_000,
        fee_tiers: vec![
            FeeTier {
                maker_ppm: -300,
                taker_ppm: 300,
            };
            8
        ],
    };
    assert_eq!(
        raw_apply(&mut e, 1, EventBody::Configure(cfg)),
        Ok(vec![Output::Configured])
    );
}

// ---------------------------------------------------------------------------
// AddMarket
// ---------------------------------------------------------------------------

#[test]
fn add_market_emits_event_and_registers_market() {
    let mut ex = Ex::bare();
    let outs = ex
        .apply(EventBody::AddMarket(MarketConfig::example(7)))
        .unwrap();
    assert_eq!(outs, vec![Output::MarketAdded { market: 7 }]);
    ex.apply(EventBody::AddMarket(MarketConfig::example(3)))
        .unwrap();
    assert_eq!(ex.engine.market_ids(), vec![3, 7], "ascending ids");
    assert_eq!(ex.engine.market_config(7), Some(&MarketConfig::example(7)));
    assert!(ex.engine.mark_price(7).is_none(), "no price before oracle");
    ex.check();
}

#[test]
fn add_market_twice_is_market_exists() {
    let mut ex = Ex::new();
    let before = ex.engine.content_hash();
    assert_eq!(
        ex.apply(EventBody::AddMarket(MarketConfig::example(M))),
        Err(Error::MarketExists)
    );
    assert_eq!(ex.engine.content_hash(), before);
    assert_eq!(ex.engine.market_ids(), vec![M]);
}

#[test]
fn market_count_is_capped_at_64() {
    let mut ex = Ex::bare();
    for id in 1..=64u32 {
        let outs = ex
            .apply(EventBody::AddMarket(MarketConfig::example(id)))
            .unwrap();
        assert_eq!(outs, vec![Output::MarketAdded { market: id }]);
    }
    assert_eq!(
        ex.apply(EventBody::AddMarket(MarketConfig::example(65))),
        Err(Error::TooManyMarkets)
    );
    // an already-registered id reports MarketExists even when the table is full
    assert_eq!(
        ex.apply(EventBody::AddMarket(MarketConfig::example(3))),
        Err(Error::MarketExists)
    );
    assert_eq!(ex.engine.market_ids().len(), 64);
    ex.check();
}

#[test]
fn invalid_market_configs_name_the_offending_field() {
    type Mutator = Box<dyn Fn(&mut MarketConfig)>;
    let cases: Vec<(&'static str, Mutator)> = vec![
        ("tick", Box::new(|c| c.tick = 0)),
        ("lot", Box::new(|c| c.lot = 0)),
        ("min_notional", Box::new(|c| c.min_notional = Money(-1))),
        ("max_order_qty", Box::new(|c| c.max_order_qty = 5)),
        ("max_order_qty", Box::new(|c| c.max_order_qty = 1_000_005)),
        ("max_position", Box::new(|c| c.max_position = 999_990)),
        ("max_leverage", Box::new(|c| c.max_leverage = 0)),
        ("max_leverage", Box::new(|c| c.max_leverage = 1001)),
        ("tiers count", Box::new(|c| c.tiers.clear())),
        (
            "tiers not ascending",
            Box::new(|c| c.tiers[1].up_to = c.tiers[0].up_to),
        ),
        (
            "last tier must be unbounded",
            Box::new(|c| c.tiers[2].up_to = Money(9_000_000_000_000)),
        ),
        ("tier rates", Box::new(|c| c.tiers[0].mmr_bps = 0)),
        (
            "tier rates",
            Box::new(|c| c.tiers[0].mmr_bps = c.tiers[0].imr_bps),
        ),
        (
            "tier imr decreasing",
            Box::new(|c| {
                c.tiers[1].imr_bps = 400;
                c.tiers[1].mmr_bps = 200;
            }),
        ),
        (
            "max_orders_per_account",
            Box::new(|c| c.max_orders_per_account = 0),
        ),
        ("liq_chunk_bps", Box::new(|c| c.liq_chunk_bps = 0)),
        ("liq_band_bps", Box::new(|c| c.liq_band_bps = 0)),
        ("liq_fee_ppm", Box::new(|c| c.liq_fee_ppm = 100_001)),
        (
            "oracle_max_change_bps",
            Box::new(|c| c.oracle_max_change_bps = 0),
        ),
        (
            "oracle_max_staleness_ms",
            Box::new(|c| c.oracle_max_staleness_ms = 0),
        ),
        (
            "max_mark_deviation_bps",
            Box::new(|c| c.max_mark_deviation_bps = 0),
        ),
        (
            "funding_interval_ms",
            Box::new(|c| c.funding_interval_ms = 999),
        ),
        ("funding_cap_ppb", Box::new(|c| c.funding_cap_ppb = -1)),
        (
            "funding_interest_ppb",
            Box::new(|c| c.funding_interest_ppb = 5_000_001),
        ),
    ];
    for (why, mutate) in cases {
        let mut ex = Ex::bare();
        let mut cfg = MarketConfig::example(1);
        mutate(&mut cfg);
        assert_eq!(
            ex.apply(EventBody::AddMarket(cfg)),
            Err(Error::InvalidConfig(why)),
            "case {why}"
        );
        assert!(ex.engine.market_ids().is_empty(), "case {why}");
    }
}

#[test]
fn market_config_boundaries_are_accepted() {
    let mut ex = Ex::bare();
    let mut cfg = MarketConfig::example(1);
    cfg.max_leverage = 1000;
    cfg.funding_interest_ppb = cfg.funding_cap_ppb; // interest == cap is allowed
    cfg.funding_interval_ms = 1_000;
    assert_eq!(
        ex.apply(EventBody::AddMarket(cfg)),
        Ok(vec![Output::MarketAdded { market: 1 }])
    );
}

// ---------------------------------------------------------------------------
// SetFeeTier
// ---------------------------------------------------------------------------

#[test]
fn set_fee_tier_updates_the_account_and_emits_event() {
    let mut ex = Ex::new();
    ex.deposit(1, 100);
    assert_eq!(ex.engine.account_view(1).unwrap().fee_tier, 0);
    let outs = ex
        .apply(EventBody::SetFeeTier {
            account: 1,
            tier: 1,
        })
        .unwrap();
    assert_eq!(
        outs,
        vec![Output::FeeTierSet {
            account: 1,
            tier: 1
        }]
    );
    assert_eq!(ex.engine.account_view(1).unwrap().fee_tier, 1);
    ex.check();
}

#[test]
fn set_fee_tier_rejects_unknown_tier_and_unknown_account() {
    let mut ex = Ex::new();
    ex.deposit(1, 100);
    let before = ex.engine.content_hash();
    // the default config has tiers 0 and 1 only
    assert_eq!(
        ex.apply(EventBody::SetFeeTier {
            account: 1,
            tier: 2
        }),
        Err(Error::UnknownFeeTier)
    );
    assert_eq!(
        ex.apply(EventBody::SetFeeTier {
            account: 99,
            tier: 1
        }),
        Err(Error::UnknownAccount)
    );
    // the tier is validated first
    assert_eq!(
        ex.apply(EventBody::SetFeeTier {
            account: 99,
            tier: 9
        }),
        Err(Error::UnknownFeeTier)
    );
    assert_eq!(ex.engine.content_hash(), before);
    assert_eq!(ex.engine.account_view(1).unwrap().fee_tier, 0);
}

// ---------------------------------------------------------------------------
// Deposit / Withdraw / FundInsurance
// ---------------------------------------------------------------------------

#[test]
fn deposit_creates_account_and_reports_running_balance() {
    let mut ex = Ex::new();
    let o1 = ex
        .apply(EventBody::Deposit {
            account: 5,
            amount: usd(100),
        })
        .unwrap();
    assert_eq!(
        o1,
        vec![Output::Deposited {
            account: 5,
            amount: usd(100),
            balance: usd(100)
        }]
    );
    let o2 = ex
        .apply(EventBody::Deposit {
            account: 5,
            amount: Money(1),
        })
        .unwrap();
    assert_eq!(
        o2,
        vec![Output::Deposited {
            account: 5,
            amount: Money(1),
            balance: Money(100_000_001)
        }]
    );
    assert_eq!(ex.engine.account_ids(), vec![5]);
    assert_eq!(ex.engine.net_deposits(), Money(100_000_001));
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn deposit_rejects_non_positive_and_oversized_amounts() {
    let mut ex = Ex::new();
    ex.deposit(1, 10);
    let before = ex.engine.content_hash();
    for amount in [Money(0), Money(-5)] {
        assert_eq!(
            ex.apply(EventBody::Deposit { account: 1, amount }),
            Err(Error::ZeroAmount)
        );
    }
    assert_eq!(
        ex.apply(EventBody::Deposit {
            account: 1,
            amount: Money(MAX_AMOUNT + 1)
        }),
        Err(Error::AmountTooLarge)
    );
    assert_eq!(ex.engine.content_hash(), before);
    // exactly MAX_AMOUNT is fine
    ex.apply(EventBody::Deposit {
        account: 2,
        amount: Money(MAX_AMOUNT),
    })
    .unwrap();
    assert_eq!(ex.engine.collateral(2), Some(Money(MAX_AMOUNT)));
}

#[test]
fn deposit_cannot_push_a_balance_beyond_the_balance_ceiling() {
    let mut ex = Ex::new();
    // 1000 * MAX_AMOUNT is exactly the largest accepted balance
    for _ in 0..1000 {
        ex.apply(EventBody::Deposit {
            account: 1,
            amount: Money(MAX_AMOUNT),
        })
        .unwrap();
    }
    let before = ex.engine.content_hash();
    assert_eq!(
        ex.apply(EventBody::Deposit {
            account: 1,
            amount: Money(1)
        }),
        Err(Error::AmountTooLarge)
    );
    assert_eq!(ex.engine.content_hash(), before);
    assert_conserved(&ex);
}

#[test]
fn account_table_is_capped_by_max_accounts() {
    let mut e = Engine::new();
    let cfg = EngineConfig {
        max_accounts: 2,
        ..EngineConfig::default()
    };
    raw_apply(&mut e, 1, EventBody::Configure(cfg)).unwrap();
    for (ts, account) in [(2, 10u64), (3, 20)] {
        raw_apply(
            &mut e,
            ts,
            EventBody::Deposit {
                account,
                amount: usd(1),
            },
        )
        .unwrap();
    }
    assert_eq!(
        raw_apply(
            &mut e,
            4,
            EventBody::Deposit {
                account: 30,
                amount: usd(1)
            }
        ),
        Err(Error::TooManyAccounts)
    );
    assert_eq!(e.account_ids(), vec![10, 20]);
    // existing accounts can still deposit
    assert!(raw_apply(
        &mut e,
        5,
        EventBody::Deposit {
            account: 10,
            amount: usd(1)
        }
    )
    .is_ok());
    e.check_invariants().unwrap();
}

#[test]
fn withdraw_validates_amount_account_and_balance_in_that_order() {
    let mut ex = Ex::new();
    ex.deposit(1, 100);
    let before = ex.engine.content_hash();
    assert_eq!(
        ex.apply(EventBody::Withdraw {
            account: 99,
            amount: Money(0)
        }),
        Err(Error::ZeroAmount)
    );
    assert_eq!(
        ex.apply(EventBody::Withdraw {
            account: 99,
            amount: usd(1)
        }),
        Err(Error::UnknownAccount)
    );
    assert_eq!(
        ex.apply(EventBody::Withdraw {
            account: 1,
            amount: Money(usd(100).0 + 1)
        }),
        Err(Error::InsufficientCollateral)
    );
    assert_eq!(ex.engine.content_hash(), before);
}

#[test]
fn withdraw_without_positions_pays_out_everything() {
    let mut ex = Ex::new();
    ex.deposit(1, 100);
    let o = ex
        .apply(EventBody::Withdraw {
            account: 1,
            amount: usd(40),
        })
        .unwrap();
    assert_eq!(
        o,
        vec![Output::Withdrawn {
            account: 1,
            amount: usd(40),
            balance: usd(60)
        }]
    );
    assert_eq!(ex.engine.net_deposits(), usd(60));
    let o = ex
        .apply(EventBody::Withdraw {
            account: 1,
            amount: usd(60),
        })
        .unwrap();
    assert_eq!(
        o,
        vec![Output::Withdrawn {
            account: 1,
            amount: usd(60),
            balance: usd(0)
        }]
    );
    assert_eq!(ex.engine.collateral(1), Some(usd(0)));
    assert_eq!(ex.engine.account_ids(), vec![1], "account stays registered");
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn withdraw_with_open_position_stops_exactly_at_initial_margin() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 10_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000); // 1 base long at 50 000
                                              // taker fee 500 ppm of 50 000 = 25 -> collateral 9 975; IM = 5% = 2 500
    assert_eq!(ex.engine.collateral(1), Some(usd(9_975)));
    assert_eq!(init_margin(&ex, 1), usd(2_500));
    let max_out = usd(9_975 - 2_500);

    let before = ex.engine.content_hash();
    assert_eq!(
        ex.apply(EventBody::Withdraw {
            account: 1,
            amount: Money(max_out.0 + 1)
        }),
        Err(Error::MarginBreach),
        "one micro-unit too much"
    );
    assert_eq!(ex.engine.content_hash(), before);

    let o = ex
        .apply(EventBody::Withdraw {
            account: 1,
            amount: max_out,
        })
        .unwrap();
    assert_eq!(
        o,
        vec![Output::Withdrawn {
            account: 1,
            amount: max_out,
            balance: usd(2_500)
        }]
    );
    // now nothing more can leave
    assert_eq!(
        ex.apply(EventBody::Withdraw {
            account: 1,
            amount: Money(1)
        }),
        Err(Error::MarginBreach)
    );
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn withdraw_must_leave_margin_for_resting_orders() {
    let mut ex = Ex::new();
    ex.deposit(1, 3_000);
    let id = ex.gtc(1, Side::Buy, 49_000, 1_000); // worst case 1 base at mark 50 000
    assert_eq!(init_margin(&ex, 1), usd(2_500));
    assert_eq!(
        ex.apply(EventBody::Withdraw {
            account: 1,
            amount: Money(usd(500).0 + 1)
        }),
        Err(Error::MarginBreach)
    );
    ex.apply(EventBody::Withdraw {
        account: 1,
        amount: usd(500),
    })
    .unwrap();
    // cancelling the order frees the rest
    ex.cancel(1, id).unwrap();
    ex.apply(EventBody::Withdraw {
        account: 1,
        amount: usd(2_500),
    })
    .unwrap();
    assert_eq!(ex.engine.collateral(1), Some(usd(0)));
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn withdraw_with_position_needs_a_fresh_oracle() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 10_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    ex.oracle(50_000).unwrap();
    let t = ex.ts;
    let before = ex.engine.content_hash();
    // exactly at the staleness limit is still fresh
    ex.apply_at(
        t + 60_000,
        EventBody::Withdraw {
            account: 1,
            amount: usd(1),
        },
    )
    .unwrap();
    let mid = ex.engine.content_hash();
    assert_ne!(mid, before);
    assert_eq!(
        ex.apply_at(
            t + 60_001,
            EventBody::Withdraw {
                account: 1,
                amount: usd(1)
            }
        ),
        Err(Error::StaleOracle)
    );
    assert_eq!(ex.engine.content_hash(), mid);
    // an account without positions or orders is not affected by staleness
    ex.apply_at(
        t + 60_002,
        EventBody::Deposit {
            account: 3,
            amount: usd(5),
        },
    )
    .unwrap();
    ex.apply_at(
        t + 60_003,
        EventBody::Withdraw {
            account: 3,
            amount: usd(5),
        },
    )
    .unwrap();
}

#[test]
fn fund_insurance_credits_the_fund_without_any_account() {
    let mut ex = Ex::new();
    let o = ex
        .apply(EventBody::FundInsurance { amount: usd(500) })
        .unwrap();
    assert_eq!(o, vec![Output::InsuranceFunded { amount: usd(500) }]);
    assert_eq!(ex.engine.insurance_fund(), usd(500));
    assert_eq!(ex.engine.net_deposits(), usd(500));
    assert!(ex.engine.account_ids().is_empty());
    ex.apply(EventBody::FundInsurance { amount: usd(1) })
        .unwrap();
    assert_eq!(ex.engine.insurance_fund(), usd(501));
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn fund_insurance_rejects_zero_negative_and_oversized() {
    let mut ex = Ex::new();
    let before = ex.engine.content_hash();
    for amount in [Money(0), Money(-1)] {
        assert_eq!(
            ex.apply(EventBody::FundInsurance { amount }),
            Err(Error::ZeroAmount)
        );
    }
    assert_eq!(
        ex.apply(EventBody::FundInsurance {
            amount: Money(MAX_AMOUNT + 1)
        }),
        Err(Error::AmountTooLarge)
    );
    assert_eq!(ex.engine.content_hash(), before);
}

// ---------------------------------------------------------------------------
// Cross-margin over two markets
// ---------------------------------------------------------------------------

/// Account 1: long 1.0 in market 1 @ 50 000 and short 10.0 in market 2 @ 2 000.
fn two_market_account() -> Ex {
    let mut ex = Ex::bare();
    ex.apply(EventBody::AddMarket(MarketConfig::example(1)))
        .unwrap();
    ex.apply(EventBody::AddMarket(MarketConfig::example(2)))
        .unwrap();
    ex.oracle_on(1, 50_000);
    ex.oracle_on(2, 2_000);
    ex.fund(&[1, 2, 3], 10_000);
    trade_on(&mut ex, 1, 1, 2, Side::Buy, 50_000, 1_000);
    trade_on(&mut ex, 2, 1, 3, Side::Sell, 2_000, 10_000);
    ex
}

#[test]
fn cross_margin_equity_uses_every_market_mark() {
    let mut ex = two_market_account();
    // fees: 500 ppm of 50 000 = 25 and 500 ppm of 20 000 = 10
    assert_eq!(ex.engine.collateral(1), Some(usd(10_000 - 25 - 10)));
    ex.oracle_on(1, 52_000); // long gains 2 000
    ex.oracle_on(2, 2_050); // short loses 10 * 50 = 500
    assert_eq!(ex.engine.mark_price(1), Some(px(52_000)));
    assert_eq!(ex.engine.mark_price(2), Some(px(2_050)));

    let v = ex.engine.account_view(1).unwrap();
    assert_eq!(v.positions.len(), 2);
    assert_eq!(v.positions[0].market, 1);
    assert_eq!(v.positions[0].unrealized, usd(2_000));
    assert_eq!(v.positions[1].market, 2);
    assert_eq!(v.positions[1].size, -100_000);
    assert_eq!(v.positions[1].unrealized, usd(-500));
    assert_eq!(v.equity, usd(9_965 + 2_000 - 500));
    assert_eq!(ex.engine.equity(1), Some(usd(11_465)));
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn cross_margin_requirements_add_up_across_markets() {
    let mut ex = two_market_account();
    ex.oracle_on(1, 52_000);
    ex.oracle_on(2, 2_050);
    // IM: 5% of 52 000 = 2 600 and 5% of 20 500 = 1 025
    assert_eq!(init_margin(&ex, 1), usd(3_625));
    // MM: 2.5% of 52 000 = 1 300 and 2.5% of 20 500 = 512.5
    assert_eq!(maint_margin(&ex, 1), Money(1_812_500_000));
}

#[test]
fn cross_margin_loss_in_one_market_limits_withdrawal_against_the_other() {
    let mut ex = two_market_account();
    ex.oracle_on(1, 52_000);
    ex.oracle_on(2, 2_050);
    // equity 11 465, requirement 3 625 -> at most 7 840 may leave
    assert_eq!(
        ex.apply(EventBody::Withdraw {
            account: 1,
            amount: Money(usd(7_840).0 + 1)
        }),
        Err(Error::MarginBreach)
    );
    ex.apply(EventBody::Withdraw {
        account: 1,
        amount: usd(7_840),
    })
    .unwrap();
    assert_eq!(ex.engine.equity(1), Some(usd(3_625)));
    assert_conserved(&ex);
    ex.check();
}

// ---------------------------------------------------------------------------
// Position accounting
// ---------------------------------------------------------------------------

#[test]
fn long_position_lifecycle_open_extend_partial_close_full_close() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    // open 2.0 @ 50 000: taker fee 500 ppm of 100 000 = 50
    ex.trade(1, 2, Side::Buy, 50_000, 2_000);
    let p = ex.engine.position(1, M).unwrap();
    assert_eq!((p.size, p.basis), (20_000, usd(100_000).0));
    assert_eq!(ex.collateral(1), usd(100_000 - 50).0);
    assert_eq!(
        ex.engine.account_view(1).unwrap().positions[0].entry_price,
        Some(px(50_000))
    );

    // extend by 1.0 @ 51 000: basis 151 000 over 3.0, fee 25.5
    ex.trade(1, 2, Side::Buy, 51_000, 1_000);
    let p = ex.engine.position(1, M).unwrap();
    assert_eq!((p.size, p.basis), (30_000, usd(151_000).0));
    assert_eq!(ex.collateral(1), 99_924_500_000);
    // entry = 151 000 / 3 = 50 333.333333 (floored)
    assert_eq!(
        ex.engine.account_view(1).unwrap().positions[0].entry_price,
        Some(Price(50_333_333_333))
    );
    // mark is still 50 000: unrealised = 150 000 - 151 000
    assert_eq!(
        ex.engine.account_view(1).unwrap().positions[0].unrealized,
        usd(-1_000)
    );

    // close 1.0 @ 52 000: released basis = ceil(151 000 / 3), fee 26
    ex.oracle(52_000).unwrap(); // keep the quotes inside the 2.5% price band
    ex.trade(1, 2, Side::Sell, 52_000, 1_000);
    let p = ex.engine.position(1, M).unwrap();
    assert_eq!(p.size, 20_000);
    assert_eq!(p.basis, 151_000_000_000 - 50_333_333_334);
    // realised = 52 000 - 50 333.333334 = 1 666.666666
    assert_eq!(
        ex.collateral(1),
        99_924_500_000 + 1_666_666_666 - 26_000_000
    );
    assert_eq!(ex.collateral(1), 101_565_166_666);

    // close the rest 2.0 @ 53 000: realised 106 000 - 100 666.666666, fee 53
    ex.oracle(53_000).unwrap();
    ex.trade(1, 2, Side::Sell, 53_000, 2_000);
    assert!(ex.engine.position(1, M).is_none(), "flat after full close");
    assert_eq!(ex.collateral(1), 106_845_500_000);
    // overall: bought 3 for 151 000, sold 3 for 158 000 = +7 000, fees 154.5
    assert_eq!(ex.collateral(1), usd(100_000 + 7_000).0 - 154_500_000);
    // the maker side mirrors it: -7 000 PnL, 61.8 of maker fees
    assert_eq!(ex.collateral(2), 92_938_200_000);
    assert_eq!(ex.engine.fee_pool(), Money(216_300_000));
    assert_eq!(ex.engine.total_basis(), Money(0));
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn short_partial_close_releases_basis_rounded_towards_the_exchange() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.trade(1, 2, Side::Sell, 50_000, 2_000);
    ex.trade(1, 2, Side::Sell, 51_000, 1_000);
    let p = ex.engine.position(1, M).unwrap();
    assert_eq!((p.size, p.basis), (-30_000, -usd(151_000).0));
    let before = ex.collateral(1);
    // buy back 1.0 @ 50 000: released = ceil(-151 000/3) = -50 333.333333
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    let p = ex.engine.position(1, M).unwrap();
    assert_eq!(p.size, -20_000);
    assert_eq!(p.basis, -151_000_000_000 + 50_333_333_333);
    // realised = 50 333.333333 - 50 000 = 333.333333 (fee 25 on top)
    assert_eq!(ex.collateral(1), before + 333_333_333 - 25_000_000);
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn position_flips_through_zero_realising_only_the_closed_part() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000); // long 1.0
    ex.trade(1, 2, Side::Sell, 51_000, 3_000); // sell 3.0: close 1.0, open short 2.0
    let p = ex.engine.position(1, M).unwrap();
    assert_eq!(p.size, -20_000);
    assert_eq!(
        p.basis,
        -usd(102_000).0,
        "only the opened part is cost basis"
    );
    // realised 1 000 on the closed unit; fees 25 + 76.5
    assert_eq!(
        ex.collateral(1),
        usd(100_000 + 1_000).0 - 25_000_000 - 76_500_000
    );
    let v = ex.engine.account_view(1).unwrap();
    assert_eq!(v.positions[0].entry_price, Some(px(51_000)));
    // mark 50 000 vs short entry 51 000: +2 000 unrealised
    assert_eq!(v.positions[0].unrealized, usd(2_000));
    assert_eq!(ex.size(2), 20_000, "counterparty flipped the other way");
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn unrealised_pnl_and_equity_follow_the_mark_for_both_sides() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    let outs = ex.oracle(52_000).unwrap();
    assert!(has(&outs, |o| matches!(
        o,
        Output::MarkUpdated { mark, .. } if *mark == px(52_000)
    )));
    let long = ex.engine.account_view(1).unwrap();
    let short = ex.engine.account_view(2).unwrap();
    assert_eq!(long.positions[0].unrealized, usd(2_000));
    assert_eq!(short.positions[0].unrealized, usd(-2_000));
    assert_eq!(long.equity, usd(100_000 - 25 + 2_000));
    assert_eq!(short.equity, usd(100_000 - 10 - 2_000));
    // the unrealised PnL is not collateral until realised
    assert_eq!(long.collateral, usd(100_000 - 25));
    assert_conserved(&ex);
}

// ---------------------------------------------------------------------------
// Initial margin / leverage / position limit
// ---------------------------------------------------------------------------

#[test]
fn order_is_accepted_at_exactly_the_required_equity_and_rejected_one_unit_below() {
    let mut ex = Ex::new();
    // need IM 5% of 50 000 = 2 500 plus a taker-fee reserve of 25
    ex.apply(EventBody::Deposit {
        account: 1,
        amount: Money(usd(2_525).0 - 1),
    })
    .unwrap();
    ex.deposit(2, 2_525);
    let before = ex.engine.content_hash();
    assert_eq!(
        ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Gtc),
        Err(Error::MarginBreach)
    );
    assert_eq!(ex.engine.content_hash(), before);
    assert!(ex.engine.open_orders_of(1).is_empty());

    let outs = ex.limit(2, Side::Buy, 50_000, 1_000, Tif::Gtc).unwrap();
    assert!(has(&outs, |o| matches!(o, Output::Rested { .. })));
    ex.check();
}

#[test]
fn max_leverage_raises_the_requirement_above_the_tier_schedule() {
    let mut cfg = MarketConfig::example(M);
    cfg.max_leverage = 10; // 10% margin even though tier 0 says 5%
    let mut ex = Ex::with_market(cfg);
    ex.apply(EventBody::Deposit {
        account: 1,
        amount: Money(usd(5_025).0 - 1),
    })
    .unwrap();
    ex.deposit(2, 5_025);
    ex.deposit(3, 100_000);
    ex.gtc(3, Side::Sell, 50_000, 1_000);
    assert_eq!(
        ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Ioc),
        Err(Error::MarginBreach),
        "needs 5 000 + 25 fee reserve, has one micro-unit less"
    );
    let outs = ex.limit(2, Side::Buy, 50_000, 1_000, Tif::Ioc).unwrap();
    assert_eq!(filled_qty(&outs), 10_000);
    assert_eq!(init_margin(&ex, 2), usd(5_000));
    assert_eq!(
        maint_margin(&ex, 2),
        usd(1_250),
        "maintenance ignores leverage"
    );
    ex.check();
}

#[test]
fn position_limit_counts_resting_exposure_but_not_reducing_orders() {
    let mut ex = Ex::new();
    ex.deposit(1, 100_000_000);
    // max_position is 5 000 000 raw = 500 base; fill it with five 100-base bids
    for _ in 0..5 {
        ex.gtc(1, Side::Buy, 49_000, 100_000);
    }
    assert_eq!(ex.engine.open_orders_of(1).len(), 5);
    let before = ex.engine.content_hash();
    assert_eq!(
        ex.limit(1, Side::Buy, 49_000, 1, Tif::Gtc),
        Err(Error::PositionLimit)
    );
    assert_eq!(ex.engine.content_hash(), before);
    // an order on the other side does not raise the worst case
    ex.limit(1, Side::Sell, 60_000, 1, Tif::Gtc).unwrap();
    ex.check();
}

#[test]
fn position_limit_counts_the_filled_position() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000_000);
    for _ in 0..5 {
        ex.trade(1, 2, Side::Buy, 50_000, 100_000);
    }
    assert_eq!(ex.size(1), 5_000_000);
    assert_eq!(
        ex.limit(1, Side::Buy, 50_000, 1, Tif::Gtc),
        Err(Error::PositionLimit)
    );
    // selling is fine
    ex.limit(1, Side::Sell, 50_000, 1, Tif::Gtc).unwrap();
    assert_conserved(&ex);
    ex.check();
}

#[test]
fn under_margined_account_may_still_place_orders_that_do_not_grow_worst_case_exposure() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 3_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000); // long 1.0, collateral 2 975
    ex.oracle(49_000).unwrap(); // equity 1 975 < IM 2 450, above MM 1 225
    let v = ex.engine.account_view(1).unwrap();
    assert_eq!(v.equity, usd(1_975));
    assert_eq!(v.initial_margin, usd(2_450));
    assert_eq!(v.maintenance_margin, Money(1_225_000_000));
    assert!(ex.engine.open_orders_of(1).is_empty());

    // adding risk is refused
    assert_eq!(
        ex.limit(1, Side::Buy, 49_000, 1, Tif::Gtc),
        Err(Error::MarginBreach)
    );
    // 2.5 sold would flip to a larger short: refused
    assert_eq!(
        ex.limit(1, Side::Sell, 60_000, 2_500, Tif::Gtc),
        Err(Error::MarginBreach)
    );
    // reducing orders (up to a flip of equal size) are allowed
    ex.limit(1, Side::Sell, 60_000, 1_000, Tif::Gtc).unwrap();
    ex.limit(1, Side::Sell, 60_010, 1_000, Tif::Gtc).unwrap();
    // but no withdrawal
    assert_eq!(
        ex.apply(EventBody::Withdraw {
            account: 1,
            amount: Money(1)
        }),
        Err(Error::MarginBreach)
    );
    ex.check();
}

// ---------------------------------------------------------------------------
// Tiered margin schedule
// ---------------------------------------------------------------------------

#[test]
fn margin_schedule_is_marginal_across_brackets() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 10_000_000);
    // (additional milli-base, expected IM in quote, expected MM in micro-units)
    let steps: [(u64, i128, i128); 5] = [
        // 10 base = 500 000 notional: top of bracket 0
        (10_000, 25_000, 12_500_000_000),
        // 10.001 base: 50 over the edge at 10% / 5%
        (1, 25_005, 12_502_500_000),
        // 11 base = 550 000: 25 000 + 10% of 50 000 / 12 500 + 5% of 50 000
        (999, 30_000, 15_000_000_000),
        // 100 base = 5 000 000: 25 000 + 10% of 4 500 000 / 12 500 + 5% of 4 500 000
        (89_000, 475_000, 237_500_000_000),
        // 110 base = 5 500 000: + 20% / 10% of the 500 000 beyond
        (10_000, 575_000, 287_500_000_000),
    ];
    for (add, im, mm) in steps {
        ex.trade(1, 2, Side::Buy, 50_000, add);
        assert_eq!(init_margin(&ex, 1), usd(im), "IM after +{add}");
        assert_eq!(maint_margin(&ex, 1), Money(mm), "MM after +{add}");
        // the short side carries the same requirement
        assert_eq!(init_margin(&ex, 2), usd(im));
    }
    assert_eq!(ex.size(1), 1_100_000);
    assert_conserved(&ex);
    ex.check();
}

// ---------------------------------------------------------------------------
// Fees
// ---------------------------------------------------------------------------

#[test]
fn fills_report_default_tier_fees_taker_500_ppm_maker_200_ppm() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 10_000);
    ex.gtc(2, Side::Sell, 50_000, 1_000);
    let outs = ex.limit(1, Side::Buy, 50_000, 1_000, Tif::Ioc).unwrap();
    assert_eq!(fee_of(&outs), (usd(25), usd(10)));
    assert_eq!(ex.collateral(1), usd(10_000 - 25).0);
    assert_eq!(ex.collateral(2), usd(10_000 - 10).0);
    assert_eq!(ex.engine.fee_pool(), usd(35));
    assert_conserved(&ex);
}

#[test]
fn fees_round_up_and_rebates_round_down() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3, 4], 100_000);
    for acct in [3, 4] {
        ex.apply(EventBody::SetFeeTier {
            account: acct,
            tier: 1,
        })
        .unwrap();
    }
    // 0.001 base at 50 000.01: notional 50.00001 = 50_000_010 micro-units
    let price = 50_000_010_000;
    // A: tier 0 taker (500 ppm) vs tier 0 maker (200 ppm)
    limit_raw(&mut ex, 2, Side::Buy, price, 1, Tif::Gtc).unwrap();
    let a = limit_raw(&mut ex, 1, Side::Sell, price, 1, Tif::Ioc).unwrap();
    // ceil(25_000.005) = 25_001, ceil(10_000.002) = 10_001
    assert_eq!(fee_of(&a), (Money(25_001), Money(10_001)));
    // B: tier 1 taker (300 ppm) vs tier 1 maker (-50 ppm rebate)
    limit_raw(&mut ex, 4, Side::Buy, price, 1, Tif::Gtc).unwrap();
    let b = limit_raw(&mut ex, 3, Side::Sell, price, 1, Tif::Ioc).unwrap();
    // ceil(15_000.003) = 15_001, rebate floor(2_500.0005) = 2_500
    assert_eq!(fee_of(&b), (Money(15_001), Money(-2_500)));
    // C: tier 0 taker against the rebate maker
    limit_raw(&mut ex, 4, Side::Buy, price, 1, Tif::Gtc).unwrap();
    let c = limit_raw(&mut ex, 1, Side::Sell, price, 1, Tif::Ioc).unwrap();
    assert_eq!(fee_of(&c), (Money(25_001), Money(-2_500)));

    assert_eq!(ex.collateral(1), usd(100_000).0 - 2 * 25_001);
    assert_eq!(ex.collateral(2), usd(100_000).0 - 10_001);
    assert_eq!(ex.collateral(3), usd(100_000).0 - 15_001);
    assert_eq!(
        ex.collateral(4),
        usd(100_000).0 + 2 * 2_500,
        "rebates credited"
    );
    // fee pool = fees collected minus rebates paid
    assert_eq!(ex.engine.fee_pool(), Money(35_002 + 12_501 + 22_501));
    assert_conserved(&ex);
    ex.check();
}

// ---------------------------------------------------------------------------
// Time, counters, halt
// ---------------------------------------------------------------------------

#[test]
fn event_timestamps_may_repeat_but_never_go_backwards() {
    let mut ex = Ex::new();
    let clock = ex.engine.clock();
    // equal timestamp is accepted
    ex.apply_at(
        clock,
        EventBody::Deposit {
            account: 1,
            amount: usd(1),
        },
    )
    .unwrap();
    ex.apply_at(
        clock,
        EventBody::Deposit {
            account: 1,
            amount: usd(1),
        },
    )
    .unwrap();
    let before = ex.engine.content_hash();
    assert_eq!(
        ex.apply_at(clock - 1, EventBody::Tick),
        Err(Error::TimeRegression)
    );
    assert_eq!(
        ex.apply_at(
            clock - 1,
            EventBody::Deposit {
                account: 1,
                amount: usd(1)
            }
        ),
        Err(Error::TimeRegression)
    );
    assert_eq!(ex.engine.content_hash(), before);
    assert_eq!(ex.engine.clock(), clock);
    ex.apply_at(clock + 1, EventBody::Tick).unwrap();
    assert_eq!(ex.engine.clock(), clock + 1);
}

#[test]
fn events_applied_counts_accepted_and_rejected_events() {
    let mut ex = Ex::bare();
    assert_eq!(ex.engine.events_applied(), 1); // Configure
    ex.deposit(1, 10);
    assert_eq!(ex.engine.events_applied(), 2);
    assert!(ex
        .apply(EventBody::Deposit {
            account: 1,
            amount: Money(0)
        })
        .is_err());
    assert_eq!(ex.engine.events_applied(), 3);
    let clock = ex.engine.clock();
    assert_eq!(
        ex.apply_at(clock - 1, EventBody::Tick),
        Err(Error::TimeRegression)
    );
    assert_eq!(ex.engine.events_applied(), 4);
    assert_eq!(ex.engine.summary().events_applied, 4);
}

#[test]
fn rejected_events_change_the_state_hash_only_through_the_counter() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 3_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    ex.gtc(1, Side::Sell, 60_000, 500);
    let content = ex.engine.content_hash();
    let full = ex.engine.state_hash();
    let rejected = [
        EventBody::Deposit {
            account: 1,
            amount: Money(0),
        },
        EventBody::Withdraw {
            account: 1,
            amount: usd(2_900),
        },
        EventBody::Withdraw {
            account: 77,
            amount: usd(1),
        },
        EventBody::SetFeeTier {
            account: 1,
            tier: 5,
        },
        EventBody::AddMarket(MarketConfig::example(M)),
        EventBody::Configure(EngineConfig::default()),
        EventBody::Cancel {
            account: 1,
            order_id: 999,
        },
        EventBody::FundInsurance { amount: Money(0) },
    ];
    let n = rejected.len() as u64;
    let applied = ex.engine.events_applied();
    for body in rejected {
        assert!(ex.apply(body).is_err());
    }
    assert_eq!(ex.engine.content_hash(), content);
    assert_ne!(
        ex.engine.state_hash(),
        full,
        "the counter is part of the full hash"
    );
    assert_eq!(ex.engine.events_applied(), applied + n);
    ex.check();
}

#[test]
fn place_for_unknown_account_or_market_is_rejected() {
    let mut ex = Ex::new();
    ex.deposit(1, 1_000);
    assert_eq!(
        ex.limit(42, Side::Buy, 50_000, 1, Tif::Gtc),
        Err(Error::UnknownAccount)
    );
    let mut p = ex.order(
        1,
        Side::Buy,
        OrderKind::Limit {
            price: px(50_000),
            tif: Tif::Gtc,
        },
        1,
    );
    p.market = 9;
    assert_eq!(ex.place(p), Err(Error::UnknownMarket));
}

#[test]
fn emergency_halt_stops_the_engine_and_freezes_state() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 10_000);
    ex.trade(1, 2, Side::Buy, 50_000, 1_000);
    assert!(!ex.engine.is_halted());
    assert_eq!(ex.engine.halt_reason(), None);
    assert!(!ex.engine.summary().halted);

    ex.engine.emergency_halt();
    assert!(ex.engine.is_halted());
    assert_eq!(ex.engine.halt_reason(), Some("internal invariant violated"));
    assert!(ex.engine.summary().halted);

    let hash = ex.engine.state_hash();
    let applied = ex.engine.events_applied();
    for body in [
        EventBody::Deposit {
            account: 1,
            amount: usd(1),
        },
        EventBody::Tick,
        EventBody::Withdraw {
            account: 1,
            amount: usd(1),
        },
        EventBody::Cancel {
            account: 1,
            order_id: 1,
        },
    ] {
        assert_eq!(ex.apply(body), Err(Error::Halted));
    }
    // even the event counter stays put: nothing at all changed
    assert_eq!(ex.engine.state_hash(), hash);
    assert_eq!(ex.engine.events_applied(), applied);
    assert_eq!(ex.size(1), 10_000, "positions survive the halt");
    // halting twice is harmless
    ex.engine.emergency_halt();
    assert_eq!(ex.engine.state_hash(), hash);
    assert_conserved(&ex);
}
