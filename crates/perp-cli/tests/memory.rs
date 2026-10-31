//! Bounded-memory check: resident memory must plateau during a long
//! simulation. Measured with `VmRSS` from `/proc/self/status` (no allocator
//! hooks, no `unsafe`), so it only runs on Linux.
//!
//! Event count: `PERP_MEMORY_EVENTS` (default 400000). For the README figures
//! run `cargo test --release --test memory -- --nocapture` with a larger value.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use perp_app::service::{ExchangeService, ServiceConfig};
use perp_app::simulation::{run_simulation, SimulationConfig};
use perp_domain::traffic::TrafficConfig;
use perp_infra::clock::SystemClock;
use perp_infra::probe::ProcStatusProbe;
use perp_infra::snapshot::FileSnapshotStore;
use perp_infra::wal::{FileEventLog, FsyncPolicy, WalConfig};
use std::sync::Mutex;

/// Both tests measure process-wide memory, so they must not overlap.
static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn resident_memory_plateaus_during_a_long_simulation() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: needs /proc");
        return;
    }
    let events: u64 = std::env::var("PERP_MEMORY_EVENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400_000);
    let dir = std::env::temp_dir().join(format!("perp-memory-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = FileEventLog::open(
        dir.join("engine.wal"),
        WalConfig {
            fsync: FsyncPolicy::Never,
            ..WalConfig::default()
        },
    )
    .unwrap();
    let snaps = FileSnapshotStore::new(dir.join("snapshots"));
    let cfg = ServiceConfig {
        snapshot_every: 0,
        keep_snapshots: 1,
        ..ServiceConfig::default()
    };
    let (mut svc, _) = ExchangeService::open(log, snaps, SystemClock, cfg).unwrap();
    let sim = SimulationConfig {
        traffic: TrafficConfig {
            seed: 7,
            accounts: 200,
            ..TrafficConfig::default()
        },
        events,
        checkpoint_every: events / 10,
        check_invariants_every: 0,
    };
    let report = run_simulation(&mut svc, &sim, &ProcStatusProbe).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let samples: Vec<(u64, u64, u64)> = report
        .checkpoints
        .iter()
        .map(|c| (c.events, c.resident_bytes.unwrap(), c.open_orders))
        .collect();
    for (e, rss, orders) in &samples {
        eprintln!(
            "after {e:>9} events: rss {:>7} KiB, resting orders {orders}",
            rss / 1024
        );
    }
    assert_eq!(samples.len(), 10);
    // Skip the warm-up (first 30%), then require that the remaining samples stay
    // within 8 MiB or 10% of each other, whichever is larger.
    let steady: Vec<u64> = samples.iter().skip(3).map(|s| s.1).collect();
    let (lo, hi) = (steady.iter().min().unwrap(), steady.iter().max().unwrap());
    let allowed = (8 * 1024 * 1024).max(lo / 10);
    assert!(
        hi - lo <= allowed,
        "rss moved from {lo} to {hi} bytes in steady state"
    );
    // and book size itself is bounded by the configured limits
    let cap = 200 * 64;
    assert!(
        samples.iter().all(|s| s.2 <= cap),
        "resting orders exceed accounts x per-account limit"
    );
}

/// Second phase of the memory story: fill every book to its per-account limit
/// with resting orders that never trade, then cancel and re-place them for many
/// rounds. Memory may grow while the books fill and must then stay flat.
#[test]
fn resident_memory_plateaus_with_books_filled_to_their_limits() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use perp_app::ports::{Clock, EventLog, SnapshotStore};
    use perp_domain::fixed::Price;
    use perp_domain::fixed::Qty;
    use perp_domain::traffic::START_PRICE;
    use perp_domain::types::{Event, EventBody, OrderKind, PlaceOrder, Side, StpMode, Tif};

    fn round<L: EventLog, S: SnapshotStore, C: Clock>(
        svc: &mut ExchangeService<L, S, C>,
        round: u64,
        accounts: u64,
    ) {
        let mut ts = svc.engine().clock() + 1;
        let idx = svc.engine().index_price(1).map_or(START_PRICE, |p| p.0);
        let oracle_ts = ts;
        let mut apply = |svc: &mut ExchangeService<L, S, C>, body: EventBody| {
            let _ = svc.apply_event(&Event { ts, body });
            ts += 1;
        };
        apply(
            svc,
            EventBody::Oracle {
                market: 1,
                index_price: Price(idx),
                source_ts: oracle_ts,
            },
        );
        let tick = 10_000;
        for acct in 1..=accounts {
            apply(
                svc,
                EventBody::CancelAll {
                    account: acct,
                    market: None,
                },
            );
            for k in 0..64u64 {
                let price = (idx / 10 * 8 / tick - k * 3) * tick;
                apply(
                    svc,
                    EventBody::Place(PlaceOrder {
                        market: 1,
                        account: acct,
                        side: Side::Buy,
                        kind: OrderKind::Limit {
                            price: Price(price),
                            tif: Tif::Gtc,
                        },
                        qty: Qty(100),
                        client_id: round * 1_000_000 + acct * 100 + k,
                        reduce_only: false,
                        stp: StpMode::CancelNewest,
                    }),
                );
            }
        }
    }

    if !cfg!(target_os = "linux") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("perp-memory-full-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = FileEventLog::open(
        dir.join("engine.wal"),
        WalConfig {
            fsync: FsyncPolicy::Never,
            ..WalConfig::default()
        },
    )
    .unwrap();
    let snaps = FileSnapshotStore::new(dir.join("snapshots"));
    let (mut svc, _) = ExchangeService::open(
        log,
        snaps,
        SystemClock,
        ServiceConfig {
            snapshot_every: 0,
            keep_snapshots: 1,
            ..ServiceConfig::default()
        },
    )
    .unwrap();
    // bootstrap only (no random flow), so the books hold nothing but our orders
    let sim = SimulationConfig {
        traffic: TrafficConfig {
            seed: 1,
            accounts: 200,
            ..TrafficConfig::default()
        },
        events: 0,
        checkpoint_every: 0,
        check_invariants_every: 0,
    };
    run_simulation(&mut svc, &sim, &ProcStatusProbe).unwrap();
    let mut samples = Vec::new();
    for r in 0..14u64 {
        round(&mut svc, r, 200);
        let rss = perp_app::simulation::ResourceProbe::resident_bytes(&ProcStatusProbe).unwrap();
        let resting = svc.engine().summary().open_orders;
        eprintln!(
            "round {r:>2}: resting orders {resting:>6}, rss {:>7} KiB",
            rss / 1024
        );
        samples.push((rss, resting));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        samples.iter().skip(1).all(|s| s.1 > 5_000),
        "the books must actually fill up: {samples:?}"
    );
    assert!(samples.iter().all(|s| s.1 <= 200 * 64));
    let steady: Vec<u64> = samples.iter().skip(4).map(|s| s.0).collect();
    let (lo, hi) = (steady.iter().min().unwrap(), steady.iter().max().unwrap());
    assert!(
        hi - lo <= (8 * 1024 * 1024).max(lo / 10),
        "rss moved from {lo} to {hi}"
    );
}
