//! Property-style tests: long seeded random flows against the exchange
//! aggregate, checking its invariants as it goes.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use perp_domain::engine::Engine;
use perp_domain::traffic::{TrafficConfig, TrafficGenerator};
use perp_domain::types::{Event, Output, Side};
use std::collections::BTreeSet;

struct Run {
    engine: Engine,
    log: Vec<Event>,
    rejected: usize,
}

fn run_checked(seed: u64, accounts: u32, markets: u32, events: usize, check_every: usize) -> Run {
    let cfg = TrafficConfig {
        seed,
        accounts,
        markets,
        ..TrafficConfig::default()
    };
    let mut gen = TrafficGenerator::new(cfg);
    let mut engine = Engine::new();
    let mut log = Vec::new();
    let mut liquidated = BTreeSet::new();
    let mut rejected = 0;
    for ev in gen.bootstrap() {
        engine.apply(&ev).unwrap();
        log.push(ev);
    }
    for i in 0..events {
        let ev = gen.next_event(&engine);
        match engine.apply(&ev) {
            Ok(outs) => {
                for o in outs {
                    if let Output::LiquidationStarted { account, .. } = o {
                        liquidated.insert(account);
                    }
                }
            }
            Err(_) => rejected += 1,
        }
        log.push(ev);
        assert!(
            !engine.is_halted(),
            "engine halted at event {i}: {:?}",
            engine.halt_reason()
        );
        for m in engine.market_ids() {
            if let (Some(b), Some(a)) = (
                engine.best_price(m, Side::Buy),
                engine.best_price(m, Side::Sell),
            ) {
                assert!(b < a, "crossed book at event {i}: {b:?} >= {a:?}");
            }
        }
        if i % check_every == 0 {
            engine
                .check_invariants()
                .unwrap_or_else(|e| panic!("event {i}: {e}"));
            // every underwater account has been (or is about to be) liquidated
            for id in engine.underwater_accounts() {
                assert!(
                    liquidated.contains(&id) || engine.is_pending_liquidation(id),
                    "account {id} is underwater at event {i} and liquidation never ran"
                );
            }
        }
    }
    engine.check_invariants().unwrap();
    Run {
        engine,
        log,
        rejected,
    }
}

#[test]
fn invariants_hold_over_300k_events_across_seeds() {
    let mut fills = 0;
    let mut liq = 0;
    let mut events = 0;
    for (seed, accounts, markets, n) in [
        (1, 120, 1, 120_000),
        (2, 60, 2, 100_000),
        (3, 250, 1, 80_000),
    ] {
        let run = run_checked(seed, accounts, markets, n, 500);
        let s = run.engine.summary();
        fills += s.fills;
        liq += s.liquidation_steps;
        events += run.log.len();
        eprintln!(
            "seed {seed}: events={} rejected={} fills={} liq_steps={} adl={} insurance={} bad_debt={}",
            run.log.len(),
            run.rejected,
            s.fills,
            s.liquidation_steps,
            s.adl_fills,
            s.insurance_fund.0,
            s.bad_debt.0
        );
    }
    assert!(events >= 300_000);
    assert!(
        fills > 10_000,
        "flow must actually trade, got {fills} fills"
    );
    assert!(liq > 0, "flow must exercise liquidation");
}

#[test]
fn money_in_equals_money_out_plus_fees_and_insurance() {
    let run = run_checked(11, 100, 1, 60_000, 1_000);
    let e = &run.engine;
    // sum(collateral) + insurance + fees - sum(cost basis) == deposits - withdrawals + bad debt
    let lhs = e.total_collateral().0 + e.insurance_fund().0 + e.fee_pool().0 - e.total_basis().0;
    assert_eq!(lhs, e.net_deposits().0 + e.bad_debt().0);
}

#[test]
fn position_sizes_sum_to_zero_in_every_market() {
    let run = run_checked(12, 80, 3, 50_000, 2_000);
    for m in run.engine.market_ids() {
        assert_eq!(run.engine.position_sum(m), 0, "market {m}");
    }
}

#[test]
fn replay_of_the_log_reproduces_the_state_hash() {
    let run = run_checked(21, 80, 2, 40_000, 5_000);
    let mut replay = Engine::new();
    for ev in &run.log {
        let _ = replay.apply(ev);
    }
    assert_eq!(replay.state_hash(), run.engine.state_hash());
    assert_eq!(replay.events_applied(), run.engine.events_applied());
}

#[test]
fn snapshot_plus_tail_equals_full_replay() {
    let run = run_checked(22, 80, 1, 30_000, 5_000);
    for cut in [100, 5_000, 17_321, run.log.len() - 1] {
        let mut head = Engine::new();
        for ev in &run.log[..cut] {
            let _ = head.apply(ev);
        }
        let mut w = perp_domain::codec::Writer::new_buf();
        head.encode_state(&mut w);
        let bytes = w.into_bytes();
        let mut r = perp_domain::codec::Reader::new(&bytes);
        let mut restored = Engine::decode_state(&mut r).unwrap();
        assert_eq!(restored.state_hash(), head.state_hash(), "restore at {cut}");
        for ev in &run.log[cut..] {
            let _ = restored.apply(ev);
        }
        assert_eq!(restored.state_hash(), run.engine.state_hash(), "cut {cut}");
    }
}

#[test]
fn state_hash_changes_with_every_accepted_event() {
    let cfg = TrafficConfig {
        seed: 31,
        accounts: 30,
        ..TrafficConfig::default()
    };
    let mut gen = TrafficGenerator::new(cfg);
    let mut e = Engine::new();
    for ev in gen.bootstrap() {
        e.apply(&ev).unwrap();
    }
    let mut prev = e.state_hash();
    for _ in 0..2_000 {
        let ev = gen.next_event(&e);
        let ok = e.apply(&ev).is_ok();
        let now = e.state_hash();
        // every call bumps the applied counter, so the hash always moves; a
        // rejected event must change nothing else
        assert_ne!(now, prev);
        prev = now;
        let _ = ok;
    }
}

#[test]
fn rejected_events_change_only_the_counter() {
    let cfg = TrafficConfig {
        seed: 32,
        accounts: 30,
        ..TrafficConfig::default()
    };
    let mut gen = TrafficGenerator::new(cfg);
    let mut e = Engine::new();
    for ev in gen.bootstrap() {
        e.apply(&ev).unwrap();
    }
    let mut rejected = 0;
    for _ in 0..20_000 {
        let ev = gen.next_event(&e);
        let (content, full) = (e.content_hash(), e.state_hash());
        let applied = e.events_applied();
        if e.apply(&ev).is_err() {
            assert_eq!(
                e.content_hash(),
                content,
                "rejected event {ev:?} changed state"
            );
            assert_ne!(e.state_hash(), full);
            assert_eq!(e.events_applied(), applied + 1);
            rejected += 1;
        }
    }
    assert!(
        rejected > 20,
        "the generator should produce rejections, got {rejected}"
    );
}
