//! Timing harness (std only, no criterion): orders per second and latency
//! percentiles of the exchange aggregate, alone and behind a file write-ahead
//! log with different fsync policies.
//!
//! Run with `cargo bench --bench throughput`. Environment overrides:
//! `PERP_BENCH_EVENTS` (default 400000), `PERP_BENCH_FSYNC_EVENTS` (default 3000).
//!
//! Method: a seeded run records the event stream once; every measurement then
//! replays the same stream, so all variants do identical work. Latency is the
//! time of one `apply` call measured with `Instant` around it (which itself costs
//! tens of nanoseconds; the throughput figures come from separate passes without
//! per-event timing).
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use perp_app::ports::EventLog;
use perp_domain::engine::Engine;
use perp_domain::traffic::{TrafficConfig, TrafficGenerator};
use perp_domain::types::{Event, EventBody};
use perp_infra::wal::{FileEventLog, FsyncPolicy, WalConfig};
use std::time::{Duration, Instant};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn record(events: u64) -> Vec<Event> {
    let mut gen = TrafficGenerator::new(TrafficConfig {
        seed: 42,
        accounts: 200,
        ..TrafficConfig::default()
    });
    let mut engine = Engine::new();
    let mut log = gen.bootstrap();
    for ev in &log {
        engine.apply(ev).unwrap();
    }
    for _ in 0..events {
        let ev = gen.next_event(&engine);
        let _ = engine.apply(&ev);
        log.push(ev);
    }
    log
}

fn is_order(ev: &Event) -> bool {
    matches!(ev.body, EventBody::Place(_))
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn rate(count: usize, d: Duration) -> f64 {
    count as f64 / d.as_secs_f64()
}

fn main() {
    let events = env_u64("PERP_BENCH_EVENTS", 400_000);
    let fsync_events = env_u64("PERP_BENCH_FSYNC_EVENTS", 3_000) as usize;
    let log = record(events);
    let orders = log.iter().filter(|e| is_order(e)).count();
    println!(
        "workload: {} events recorded ({} order placements), seed 42, 200 accounts, 1 market",
        log.len(),
        orders
    );
    println!(
        "build: {}",
        if cfg!(debug_assertions) {
            "debug (numbers are not meaningful)"
        } else {
            "release"
        }
    );

    // 1. throughput of the aggregate alone
    let mut engine = Engine::new();
    let start = Instant::now();
    for ev in &log {
        let _ = engine.apply(ev);
    }
    let total = start.elapsed();
    println!(
        "\n[engine only] {} events in {:.3}s",
        log.len(),
        total.as_secs_f64()
    );
    println!("  events/s        {:>12.0}", rate(log.len(), total));
    println!(
        "  orders/s        {:>12.0}  (order placements are {:.1}% of events)",
        rate(orders, total),
        100.0 * orders as f64 / log.len() as f64
    );
    println!(
        "  final state hash {}",
        perp_domain::sha256::to_hex(&engine.state_hash())
    );

    // 2. latency distribution per event type
    let mut engine = Engine::new();
    let mut order_ns = Vec::with_capacity(orders);
    let mut other_ns = Vec::with_capacity(log.len() - orders);
    for ev in &log {
        let t = Instant::now();
        let _ = engine.apply(ev);
        let ns = u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if is_order(ev) {
            order_ns.push(ns);
        } else {
            other_ns.push(ns);
        }
    }
    order_ns.sort_unstable();
    other_ns.sort_unstable();
    println!("\n[latency per apply call, ns]");
    println!(
        "  {:<22} {:>9} {:>9} {:>9} {:>11}",
        "event type", "p50", "p99", "p99.9", "max"
    );
    for (name, v) in [
        ("order placements", &order_ns),
        ("all other events", &other_ns),
    ] {
        println!(
            "  {:<22} {:>9} {:>9} {:>9} {:>11}",
            name,
            percentile(v, 0.50),
            percentile(v, 0.99),
            percentile(v, 0.999),
            v.last().copied().unwrap_or(0)
        );
    }

    // 3. behind the file WAL
    let dir = std::env::temp_dir().join(format!("perp-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    println!("\n[engine + file write-ahead log] (event appended, then applied)");
    for (label, policy, n) in [
        ("fsync never", FsyncPolicy::Never, log.len()),
        ("fsync every 1000", FsyncPolicy::EveryN(1000), log.len()),
        (
            "fsync always",
            FsyncPolicy::Always,
            fsync_events.min(log.len()),
        ),
    ] {
        let path = dir.join(format!("{}.wal", label.replace(' ', "-")));
        let mut wal = FileEventLog::open(
            &path,
            WalConfig {
                fsync: policy,
                ..WalConfig::default()
            },
        )
        .unwrap();
        let mut engine = Engine::new();
        let start = Instant::now();
        for (i, ev) in log.iter().take(n).enumerate() {
            wal.append(i as u64, ev).unwrap();
            let _ = engine.apply(ev);
        }
        wal.sync().unwrap();
        let d = start.elapsed();
        println!(
            "  {:<18} {:>9} events in {:>7.3}s -> {:>10.0} events/s",
            label,
            n,
            d.as_secs_f64(),
            rate(n, d)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!("\ncaveat: single thread, one machine, synthetic flow; see README for the host description.");
}
