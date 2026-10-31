//! Domain tests: canonical state encoding, snapshot decoding, state hashes.
//!
//! The state encoding is `version | config | clock | events_applied | ...`;
//! tests that patch bytes locate fields relative to the encoded configuration
//! (see `engine/state.rs` for the layout).
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod common;
use common::*;
use perp_domain::codec::{to_bytes, DecodeError, Reader, Writer};
use perp_domain::engine::Engine;
use perp_domain::error::Error;
use perp_domain::fixed::{Money, Price};
use perp_domain::traffic::{TrafficConfig, TrafficGenerator};
use perp_domain::types::{Event, EventBody, Side, Tif};

fn enc(e: &Engine) -> Vec<u8> {
    let mut w = Writer::new_buf();
    e.encode_state(&mut w);
    w.into_bytes()
}

fn dec(bytes: &[u8]) -> Result<Engine, DecodeError> {
    let mut r = Reader::new(bytes);
    let e = Engine::decode_state(&mut r)?;
    r.finish()?;
    Ok(e)
}

/// Offset of the clock field: version (1) + config flag (1) + config.
fn base(e: &Engine) -> usize {
    2 + to_bytes(e.config().unwrap()).len()
}

const OFF_COUNTER: usize = 8;
const OFF_NEXT_ORDER_ID: usize = 16;
const OFF_INSURANCE: usize = 32;
const OFF_NET_DEPOSITS: usize = 64;
const OFF_BAD_DEBT: usize = 80;
const OFF_HALT: usize = 96;
const OFF_PENDING_COUNT: usize = 121;

fn put(bytes: &mut [u8], at: usize, v: &[u8]) {
    bytes[at..at + v.len()].copy_from_slice(v);
}

fn find(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("pattern not found")
}

/// Configured engine without markets or accounts.
fn empty_engine() -> Engine {
    Ex::bare().engine
}

/// Small but non-trivial state: three accounts, one position pair and two
/// resting orders.
fn small_engine() -> Ex {
    let mut ex = Ex::new();
    ex.fund(&[1, 2, 3], 100_000);
    ex.trade(1, 2, Side::Buy, 50_000, 100);
    ex.gtc(3, Side::Buy, 40_000, 10);
    ex.gtc(2, Side::Sell, 60_000, 10);
    ex
}

fn traffic(seed: u64, accounts: u32, markets: u32) -> (Engine, TrafficGenerator) {
    let mut g = TrafficGenerator::new(TrafficConfig {
        seed,
        accounts,
        markets,
        oracle_every: 20,
        tick_every: 100,
    });
    let mut e = Engine::new();
    for ev in g.bootstrap() {
        e.apply(&ev).unwrap();
    }
    (e, g)
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[test]
fn unconfigured_engine_round_trips() {
    let e = Engine::new();
    let bytes = enc(&e);
    let d = dec(&bytes).unwrap();
    assert_eq!(d.state_hash(), e.state_hash());
    assert!(d.config().is_none());
    d.check_invariants().unwrap();
}

#[test]
fn small_state_round_trips_with_equal_hash_and_bytes() {
    let ex = small_engine();
    let bytes = enc(&ex.engine);
    let d = dec(&bytes).unwrap();
    assert_eq!(d.state_hash(), ex.engine.state_hash());
    assert_eq!(enc(&d), bytes, "re-encoding a decoded state is byte-equal");
    d.check_invariants().unwrap();
    assert_eq!(d.summary(), ex.engine.summary());
    assert_eq!(d.open_orders_of(3), ex.engine.open_orders_of(3));
    assert_eq!(d.position(1, M), ex.engine.position(1, M));
    assert_eq!(d.collateral(2), ex.engine.collateral(2));
}

#[test]
fn encoding_is_canonical_for_the_same_state() {
    let a = small_engine();
    let b = small_engine();
    assert_eq!(enc(&a.engine), enc(&b.engine));
    // encoding twice gives the same bytes and the hash equals the hash of those bytes
    assert_eq!(enc(&a.engine), enc(&a.engine));
    assert_eq!(a.engine.state_hash(), b.engine.state_hash());
}

#[test]
fn hash_of_the_encoding_equals_state_hash() {
    let ex = small_engine();
    let mut w = Writer::new_hash();
    ex.engine.encode_state(&mut w);
    assert_eq!(w.finish_hash(), ex.engine.state_hash());
}

#[test]
fn traffic_run_round_trips_at_many_checkpoints_and_continues_identically() {
    for seed in [1u64, 7] {
        let (mut live, mut gen) = traffic(seed, 40, 2);
        let mut saw_orders = false;
        let mut saw_positions = false;
        for step in 0..3_000u32 {
            let ev = gen.next_event(&live);
            let _ = live.apply(&ev);
            if step % 250 == 249 {
                let bytes = enc(&live);
                let mut copy = dec(&bytes).unwrap();
                assert_eq!(
                    copy.state_hash(),
                    live.state_hash(),
                    "seed {seed} step {step}"
                );
                assert_eq!(copy.content_hash(), live.content_hash());
                assert_eq!(enc(&copy), bytes);
                copy.check_invariants().unwrap();
                let s = live.summary();
                saw_orders |= s.open_orders > 0;
                saw_positions |= s.accounts_with_positions > 0;
                // the decoded engine and the live one evolve identically
                for _ in 0..120 {
                    let ev = gen.next_event(&live);
                    assert_eq!(live.apply(&ev), copy.apply(&ev), "seed {seed} step {step}");
                    assert_eq!(live.state_hash(), copy.state_hash());
                }
            }
        }
        assert!(saw_orders && saw_positions, "traffic exercised the engine");
        assert!(live.summary().fills > 0);
        live.check_invariants().unwrap();
    }
}

#[test]
fn decoded_engine_keeps_serving_events_like_the_original() {
    let ex = small_engine();
    let mut copy = dec(&enc(&ex.engine)).unwrap();
    let mut orig = ex.engine.clone();
    let ts = orig.clock() + 1;
    let ev = Event {
        ts,
        body: EventBody::Cancel {
            account: 3,
            order_id: orig.open_orders_of(3)[0],
        },
    };
    assert_eq!(orig.apply(&ev), copy.apply(&ev));
    assert_eq!(orig.state_hash(), copy.state_hash());
    assert!(copy.open_orders_of(3).is_empty());
}

// ---------------------------------------------------------------------------
// Rejections: truncation, mutation, trailing bytes
// ---------------------------------------------------------------------------

#[test]
fn every_truncation_of_a_small_state_is_rejected() {
    let ex = small_engine();
    let bytes = enc(&ex.engine);
    for len in 0..bytes.len() {
        assert!(
            dec(&bytes[..len]).is_err(),
            "prefix of {len} bytes accepted"
        );
    }
    assert!(dec(&bytes).is_ok());
}

#[test]
fn every_single_byte_mutation_is_rejected_or_yields_a_valid_different_state() {
    let ex = small_engine();
    let bytes = enc(&ex.engine);
    let original = ex.engine.state_hash();
    let mut accepted = 0usize;
    for i in 0..bytes.len() {
        for flip in [0x01u8, 0x80, 0xFF] {
            let mut m = bytes.clone();
            m[i] ^= flip;
            if let Ok(e) = dec(&m) {
                accepted += 1;
                e.check_invariants()
                    .unwrap_or_else(|why| panic!("byte {i}^{flip:#x} accepted but invalid: {why}"));
                assert_ne!(e.state_hash(), original, "byte {i}^{flip:#x}");
                assert_eq!(enc(&e), m, "accepted mutation is canonical (byte {i})");
            }
        }
    }
    // many fields (clock, counters, fees, ...) legitimately accept any value
    assert!(accepted > 0);
}

#[test]
fn trailing_bytes_are_rejected() {
    let ex = small_engine();
    let mut bytes = enc(&ex.engine);
    bytes.push(0);
    assert_eq!(dec(&bytes).err(), Some(DecodeError("trailing bytes")));
    let mut r = Reader::new(&bytes);
    assert!(Engine::decode_state(&mut r).is_err());
}

#[test]
fn unsupported_version_is_rejected() {
    let mut bytes = enc(&small_engine().engine);
    bytes[0] = 2;
    assert_eq!(
        dec(&bytes).err(),
        Some(DecodeError("unsupported state version"))
    );
    bytes[0] = 0;
    assert!(dec(&bytes).is_err());
}

#[test]
fn non_canonical_bool_is_rejected() {
    let mut bytes = enc(&small_engine().engine);
    bytes[1] = 2;
    assert_eq!(dec(&bytes).err(), Some(DecodeError("invalid bool")));
}

#[test]
fn invalid_engine_config_is_rejected() {
    let mut bytes = enc(&small_engine().engine);
    put(&mut bytes, 2, &0u32.to_le_bytes()); // max_accounts = 0
    assert_eq!(
        dec(&bytes).err(),
        Some(DecodeError("invalid engine config"))
    );
}

// ---------------------------------------------------------------------------
// Hostile counts and ordering
// ---------------------------------------------------------------------------

#[test]
fn hostile_pending_count_is_rejected_before_allocating() {
    let e = empty_engine();
    let mut bytes = enc(&e);
    let at = base(&e) + OFF_PENDING_COUNT;
    put(&mut bytes, at, &u32::MAX.to_le_bytes());
    assert_eq!(dec(&bytes).err(), Some(DecodeError("count above limit")));
    // within the limit but without the bytes to back it
    put(&mut bytes, at, &5u32.to_le_bytes());
    assert_eq!(
        dec(&bytes).err(),
        Some(DecodeError("count exceeds remaining input"))
    );
}

#[test]
fn hostile_market_count_is_rejected() {
    let e = empty_engine();
    let mut bytes = enc(&e);
    // pending count (4) + no ids + liquidation cursor (8) precede the market count
    let at = base(&e) + OFF_PENDING_COUNT + 4 + 8;
    put(&mut bytes, at, &u32::MAX.to_le_bytes());
    assert_eq!(dec(&bytes).err(), Some(DecodeError("count above limit")));
    put(&mut bytes, at, &65u32.to_le_bytes());
    assert_eq!(dec(&bytes).err(), Some(DecodeError("count above limit")));
    put(&mut bytes, at, &3u32.to_le_bytes());
    assert_eq!(
        dec(&bytes).err(),
        Some(DecodeError("count exceeds remaining input"))
    );
}

#[test]
fn hostile_account_count_is_rejected() {
    let e = empty_engine();
    let mut bytes = enc(&e);
    let at = bytes.len() - 4;
    put(&mut bytes, at, &u32::MAX.to_le_bytes());
    assert_eq!(dec(&bytes).err(), Some(DecodeError("count above limit")));
    put(&mut bytes, at, &10_000u32.to_le_bytes()); // == max_accounts, no bytes
    assert_eq!(
        dec(&bytes).err(),
        Some(DecodeError("count exceeds remaining input"))
    );
}

#[test]
fn hostile_order_count_is_rejected() {
    let ex = Ex::new();
    let mut bytes = enc(&ex.engine);
    // market section ends with the order count, then the account count
    let at = bytes.len() - 8;
    put(&mut bytes, at, &u32::MAX.to_le_bytes());
    assert_eq!(dec(&bytes).err(), Some(DecodeError("count above limit")));
}

#[test]
fn hostile_position_count_is_rejected() {
    let mut ex = Ex::new();
    ex.deposit(1, 10);
    let mut bytes = enc(&ex.engine);
    let at = bytes.len() - 4;
    put(&mut bytes, at, &u32::MAX.to_le_bytes());
    assert_eq!(dec(&bytes).err(), Some(DecodeError("count above limit")));
    put(&mut bytes, at, &1u32.to_le_bytes());
    assert_eq!(
        dec(&bytes).err(),
        Some(DecodeError("count exceeds remaining input"))
    );
}

#[test]
fn unsorted_or_duplicate_pending_liquidations_are_rejected() {
    let e = empty_engine();
    let bytes = enc(&e);
    let head = base(&e) + OFF_PENDING_COUNT;
    let build = |ids: &[u64]| {
        let mut v = bytes[..head].to_vec();
        v.extend_from_slice(&u32::try_from(ids.len()).unwrap().to_le_bytes());
        for id in ids {
            v.extend_from_slice(&id.to_le_bytes());
        }
        v.extend_from_slice(&0u64.to_le_bytes()); // liquidation cursor
        v.extend_from_slice(&0u32.to_le_bytes()); // markets
        v.extend_from_slice(&0u32.to_le_bytes()); // accounts
        v
    };
    assert_eq!(
        dec(&build(&[5, 3])).err(),
        Some(DecodeError("pending liquidations not ascending"))
    );
    assert_eq!(
        dec(&build(&[3, 3])).err(),
        Some(DecodeError("pending liquidations not ascending"))
    );
    // ascending ids pass the order check but reference unknown accounts
    assert_eq!(
        dec(&build(&[3, 5])).err(),
        Some(DecodeError("state invariant violated"))
    );
    assert!(dec(&build(&[])).is_ok());
}

#[test]
fn unsorted_or_duplicate_accounts_are_rejected() {
    let mut ex = Ex::new();
    ex.deposit(1, 10);
    ex.deposit(2, 20);
    let bytes = enc(&ex.engine);
    // two accounts without positions: 29 bytes each at the tail
    let a1 = bytes.len() - 58;
    let a2 = bytes.len() - 29;
    let mut swapped = bytes.clone();
    put(&mut swapped, a1, &2u64.to_le_bytes());
    put(&mut swapped, a2, &1u64.to_le_bytes());
    assert_eq!(
        dec(&swapped).err(),
        Some(DecodeError("accounts not ascending"))
    );
    let mut dup = bytes.clone();
    put(&mut dup, a2, &1u64.to_le_bytes());
    assert_eq!(dec(&dup).err(), Some(DecodeError("accounts not ascending")));
    assert!(dec(&bytes).is_ok());
}

#[test]
fn unsorted_or_duplicate_orders_are_rejected() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.gtc(1, Side::Buy, 40_000, 10);
    ex.gtc(2, Side::Buy, 39_000, 10);
    let bytes = enc(&ex.engine);
    // orders (2 x 51 bytes), then account count (4) and two accounts (2 x 29)
    let o1 = bytes.len() - 58 - 4 - 102;
    let o2 = o1 + 51;
    let id1 = u64::from_le_bytes(bytes[o1..o1 + 8].try_into().unwrap());
    let id2 = u64::from_le_bytes(bytes[o2..o2 + 8].try_into().unwrap());
    assert!(id1 < id2);
    let mut swapped = bytes.clone();
    put(&mut swapped, o1, &id2.to_le_bytes());
    put(&mut swapped, o2, &id1.to_le_bytes());
    assert_eq!(
        dec(&swapped).err(),
        Some(DecodeError("orders not ascending"))
    );
    let mut dup = bytes.clone();
    put(&mut dup, o2, &id1.to_le_bytes());
    assert_eq!(dec(&dup).err(), Some(DecodeError("orders not ascending")));
    assert!(dec(&bytes).is_ok());
}

// ---------------------------------------------------------------------------
// Invariants enforced on decode
// ---------------------------------------------------------------------------

#[test]
fn money_identity_is_enforced_on_decode() {
    let ex = small_engine();
    let e = &ex.engine;
    let bytes = enc(e);
    let b = base(e);
    let nd = e.net_deposits().0;
    let mut edited = bytes.clone();
    put(&mut edited, b + OFF_NET_DEPOSITS, &(nd + 1).to_le_bytes());
    assert_eq!(
        dec(&edited).err(),
        Some(DecodeError("state invariant violated"))
    );
    // bad debt on the other side of the identity
    let mut edited = bytes.clone();
    put(&mut edited, b + OFF_BAD_DEBT, &1i128.to_le_bytes());
    assert_eq!(
        dec(&edited).err(),
        Some(DecodeError("state invariant violated"))
    );
    // moving the same amount on both sides keeps the identity: accepted
    let mut both = bytes.clone();
    put(&mut both, b + OFF_BAD_DEBT, &7i128.to_le_bytes());
    put(&mut both, b + OFF_NET_DEPOSITS, &(nd - 7).to_le_bytes());
    let d = dec(&both).unwrap();
    assert_eq!(d.bad_debt(), Money(7));
    d.check_invariants().unwrap();
}

#[test]
fn negative_insurance_fund_is_rejected() {
    let ex = small_engine();
    let b = base(&ex.engine);
    let mut bytes = enc(&ex.engine);
    put(&mut bytes, b + OFF_INSURANCE, &(-1i128).to_le_bytes());
    assert_eq!(
        dec(&bytes).err(),
        Some(DecodeError("state invariant violated"))
    );
}

#[test]
fn changed_account_collateral_breaks_the_identity() {
    let mut ex = Ex::new();
    ex.deposit(1, 10);
    let mut bytes = enc(&ex.engine);
    // single account: id (8), collateral (16), fee tier (1), position count (4)
    let at = bytes.len() - 4 - 1 - 16;
    put(&mut bytes, at, &(usd(10).0 + 1).to_le_bytes());
    assert_eq!(
        dec(&bytes).err(),
        Some(DecodeError("state invariant violated"))
    );
}

#[test]
fn positions_that_do_not_sum_to_zero_are_rejected() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.trade(1, 2, Side::Buy, 50_000, 777); // +7770 / -7770 raw
    let bytes = enc(&ex.engine);
    let at = find(&bytes, &7_770i64.to_le_bytes());
    let mut edited = bytes.clone();
    put(&mut edited, at, &7_780i64.to_le_bytes());
    assert_eq!(
        dec(&edited).err(),
        Some(DecodeError("state invariant violated"))
    );
    // zero-size and absurd positions are rejected before the invariants
    put(&mut edited, at, &0i64.to_le_bytes());
    assert_eq!(
        dec(&edited).err(),
        Some(DecodeError("position size out of range"))
    );
    put(&mut edited, at, &i64::MAX.to_le_bytes());
    assert_eq!(
        dec(&edited).err(),
        Some(DecodeError("position size out of range"))
    );
}

#[test]
fn position_in_unknown_market_is_rejected() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.trade(1, 2, Side::Buy, 50_000, 777);
    let mut bytes = enc(&ex.engine);
    // the market id (u32 = 1) precedes the size inside the position record
    let at = find(&bytes, &7_770i64.to_le_bytes()) - 4;
    put(&mut bytes, at, &9u32.to_le_bytes());
    assert_eq!(
        dec(&bytes).err(),
        Some(DecodeError("position in unknown market"))
    );
}

#[test]
fn order_records_are_validated_on_decode() {
    let mut ex = Ex::new();
    ex.fund(&[1, 2], 100_000);
    ex.gtc(1, Side::Buy, 40_000, 123); // qty 1230 raw
    ex.gtc(1, Side::Buy, 39_000, 10);
    let bytes = enc(&ex.engine);
    assert!(dec(&bytes).is_ok());
    let price1 = px(40_000).0.to_le_bytes();
    let price2 = px(39_000).0.to_le_bytes();
    let p1 = find(&bytes, &price1);
    let rec1 = p1 - 17; // id 8, account 8, side 1
    let p2 = find(&bytes, &price2);
    let rec2 = p2 - 17;

    // off lot
    let mut m = bytes.clone();
    put(&mut m, rec1 + 25, &1_231u64.to_le_bytes());
    assert_eq!(dec(&m).err(), Some(DecodeError("state invariant violated")));
    // off tick
    let mut m = bytes.clone();
    put(&mut m, p1, &(px(40_000).0 + 1).to_le_bytes());
    assert_eq!(dec(&m).err(), Some(DecodeError("state invariant violated")));
    // zero quantity / zero price
    let mut m = bytes.clone();
    put(&mut m, rec1 + 25, &0u64.to_le_bytes());
    assert_eq!(dec(&m).err(), Some(DecodeError("order out of range")));
    let mut m = bytes.clone();
    put(&mut m, p1, &0u64.to_le_bytes());
    assert_eq!(dec(&m).err(), Some(DecodeError("order out of range")));
    // unknown owner
    let mut m = bytes.clone();
    put(&mut m, rec1 + 8, &99u64.to_le_bytes());
    assert_eq!(dec(&m).err(), Some(DecodeError("order of unknown account")));
    // duplicate client id of the same account
    let mut m = bytes.clone();
    let c1 = u64::from_le_bytes(bytes[rec1 + 41..rec1 + 49].try_into().unwrap());
    put(&mut m, rec2 + 41, &c1.to_le_bytes());
    assert_eq!(
        dec(&m).err(),
        Some(DecodeError("duplicate client order id"))
    );
    // an order id at or beyond the id counter
    let mut m = bytes.clone();
    put(
        &mut m,
        base(&ex.engine) + OFF_NEXT_ORDER_ID,
        &1u64.to_le_bytes(),
    );
    assert_eq!(dec(&m).err(), Some(DecodeError("state invariant violated")));
}

// ---------------------------------------------------------------------------
// Halt flag
// ---------------------------------------------------------------------------

#[test]
fn halted_flag_survives_encode_and_decode() {
    let mut ex = small_engine();
    let running_hash = ex.engine.state_hash();
    ex.engine.emergency_halt();
    assert!(ex.engine.is_halted());
    assert_ne!(
        ex.engine.state_hash(),
        running_hash,
        "halt changes the hash"
    );
    let bytes = enc(&ex.engine);
    let mut d = dec(&bytes).unwrap();
    assert!(d.is_halted());
    assert_eq!(d.halt_reason(), ex.engine.halt_reason());
    assert_eq!(d.halt_reason(), Some("internal invariant violated"));
    assert!(d.summary().halted);
    assert_eq!(d.state_hash(), ex.engine.state_hash());
    // a halted engine refuses everything, and refusing changes nothing
    let ev = Event {
        ts: d.clock() + 1,
        body: EventBody::Tick,
    };
    let before = d.state_hash();
    assert_eq!(d.apply(&ev), Err(Error::Halted));
    assert_eq!(d.state_hash(), before);
}

#[test]
fn halt_codes_round_trip_and_unknown_codes_are_rejected() {
    let e = small_engine().engine;
    let at = base(&e) + OFF_HALT;
    let bytes = enc(&e);
    assert_eq!(bytes[at], 0);
    for code in 1u8..=4 {
        let mut m = bytes.clone();
        m[at] = code;
        let d = dec(&m).unwrap();
        assert!(d.is_halted(), "code {code}");
        assert_eq!(enc(&d), m);
    }
    let mut m = bytes.clone();
    m[at] = 5;
    assert_eq!(dec(&m).err(), Some(DecodeError("invalid halt code")));
    m[at] = 255;
    assert_eq!(dec(&m).err(), Some(DecodeError("invalid halt code")));
}

#[test]
fn emergency_halt_is_idempotent() {
    let mut e = small_engine().engine;
    e.emergency_halt();
    let h = e.state_hash();
    e.emergency_halt();
    assert_eq!(e.state_hash(), h);
}

// ---------------------------------------------------------------------------
// state_hash / content_hash
// ---------------------------------------------------------------------------

#[test]
fn same_events_give_the_same_hash_on_independent_engines() {
    let a = small_engine();
    let b = small_engine();
    assert_eq!(a.engine.state_hash(), b.engine.state_hash());
    assert_eq!(a.engine.content_hash(), b.engine.content_hash());
}

#[test]
fn different_event_order_gives_a_different_hash() {
    // who is maker and who is taker changes the fees: same positions, other state
    let run = |first_sells: bool| {
        let mut ex = Ex::new();
        ex.fund(&[1, 2], 100_000);
        if first_sells {
            ex.gtc(1, Side::Sell, 50_000, 100);
            ex.limit(2, Side::Buy, 50_000, 100, Tif::Ioc).unwrap();
        } else {
            ex.gtc(2, Side::Buy, 50_000, 100);
            ex.limit(1, Side::Sell, 50_000, 100, Tif::Ioc).unwrap();
        }
        ex.engine
    };
    let (a, b) = (run(true), run(false));
    assert_eq!(a.position(1, M).unwrap().size, -1_000);
    assert_eq!(b.position(1, M).unwrap().size, -1_000);
    assert_ne!(a.state_hash(), b.state_hash());
}

#[test]
fn state_hash_moves_on_every_event_but_content_hash_ignores_rejections() {
    let mut ex = small_engine();
    let content = ex.engine.content_hash();
    let full = ex.engine.state_hash();
    let bytes = enc(&ex.engine);
    // rejected: unknown order
    assert_eq!(ex.cancel(1, 999_999), Err(Error::UnknownOrder));
    assert_eq!(ex.engine.content_hash(), content);
    assert_ne!(
        ex.engine.state_hash(),
        full,
        "the applied-event counter moved"
    );
    // only the 8 counter bytes differ
    let after = enc(&ex.engine);
    let b = base(&ex.engine);
    let diff: Vec<usize> = (0..bytes.len())
        .filter(|i| bytes[*i] != after[*i])
        .collect();
    assert!(!diff.is_empty());
    assert!(diff
        .iter()
        .all(|i| (b + OFF_COUNTER..b + OFF_COUNTER + 8).contains(i)));
}

#[test]
fn content_hash_changes_with_any_accepted_event() {
    let mut ex = small_engine();
    let content = ex.engine.content_hash();
    ex.apply(EventBody::Tick).unwrap(); // moves the clock
    assert_ne!(ex.engine.content_hash(), content);
    let content = ex.engine.content_hash();
    ex.deposit(9, 5);
    assert_ne!(ex.engine.content_hash(), content);
}

#[test]
fn content_hash_ignores_the_counter_between_equal_states() {
    let mut a = small_engine();
    let b = small_engine();
    for _ in 0..5 {
        let _ = a.cancel(1, 424_242);
    }
    assert_eq!(a.engine.content_hash(), b.engine.content_hash());
    assert_ne!(a.engine.state_hash(), b.engine.state_hash());
    // but the counter is part of the snapshot
    let d = dec(&enc(&a.engine)).unwrap();
    assert_eq!(d.events_applied(), a.engine.events_applied());
}

#[test]
fn rejected_events_do_not_move_content_hash_for_many_kinds() {
    let mut ex = small_engine();
    let content = ex.engine.content_hash();
    assert_eq!(ex.cancel(1, 12_345), Err(Error::UnknownOrder));
    assert_eq!(
        ex.apply(EventBody::Deposit {
            account: 1,
            amount: Money(0)
        }),
        Err(Error::ZeroAmount)
    );
    assert_eq!(ex.oracle_on_result(77, 50_000), Err(Error::UnknownMarket));
    assert_eq!(
        ex.apply(EventBody::SetFeeTier {
            account: 1,
            tier: 9
        }),
        Err(Error::UnknownFeeTier)
    );
    assert_eq!(
        ex.apply(EventBody::Oracle {
            market: M,
            index_price: Price(0),
            source_ts: 1
        }),
        Err(Error::OracleBadPrice)
    );
    assert_eq!(ex.engine.content_hash(), content);
    ex.check();
}
