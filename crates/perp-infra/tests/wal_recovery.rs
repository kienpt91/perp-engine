//! Crash-recovery behaviour of the file adapters, driven through the
//! application service, plus the port contract suites run against the real
//! file implementations (the same suites the in-memory fakes pass).
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use perp_app::contract::{
    event_log_contract, run_scenario, scenario_events, snapshot_store_contract,
    snapshot_store_skips_damaged_contract,
};
use perp_app::fakes::{InMemoryEventLog, ManualClock};
use perp_app::ports::{EventLog, PortErrorKind, SnapshotStore};
use perp_app::service::{
    recover, replay_log, ExchangeService, RecoveryReport, ServiceConfig, ServiceError,
};
use perp_infra::snapshot::{list_snapshots, snapshot_file_name, FileSnapshotStore};
use perp_infra::wal::{
    verify, FileEventLog, FsyncPolicy, ReadOnlyFileLog, WalConfig, WalError, HEADER_LEN,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("perp-recovery-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn wal(&self) -> PathBuf {
        self.0.join("engine.wal")
    }

    fn snaps(&self) -> PathBuf {
        self.0.join("snapshots")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Svc = ExchangeService<FileEventLog, FileSnapshotStore, ManualClock>;

fn wal_cfg(fsync: FsyncPolicy) -> WalConfig {
    WalConfig {
        fsync,
        max_file_bytes: 1 << 30,
    }
}

fn try_open(
    dir: &Path,
    every: u64,
    keep: usize,
    cfg: WalConfig,
) -> Result<(Svc, RecoveryReport), ServiceError> {
    let log = FileEventLog::open(dir.join("engine.wal"), cfg).unwrap();
    let snaps = FileSnapshotStore::new(dir.join("snapshots"));
    ExchangeService::open(
        log,
        snaps,
        ManualClock::new(0),
        ServiceConfig {
            snapshot_every: every,
            keep_snapshots: keep,
        },
    )
}

fn open(dir: &Path, every: u64, keep: usize) -> (Svc, RecoveryReport) {
    try_open(dir, every, keep, wal_cfg(FsyncPolicy::Never)).unwrap()
}

fn feed(svc: &mut Svc, from: u64, to: u64) {
    for ev in scenario_events(to).iter().skip(from as usize) {
        let _ = svc.apply_event(ev);
    }
}

/// End offsets of each record in a WAL file image (index 0 = header end).
fn boundaries(bytes: &[u8]) -> Vec<usize> {
    let mut out = vec![HEADER_LEN as usize];
    let mut pos = HEADER_LEN as usize;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4 + len + 32;
        if pos > bytes.len() {
            break;
        }
        out.push(pos);
    }
    out
}

// ---------------------------------------------------------------- contracts

#[test]
fn file_event_log_satisfies_the_event_log_contract() {
    let root = TempDir::new("contract-log");
    let mut n = 0;
    event_log_contract(&mut || {
        n += 1;
        FileEventLog::open(
            root.path().join(format!("log-{n}.wal")),
            wal_cfg(FsyncPolicy::Never),
        )
        .unwrap()
    });
}

#[test]
fn file_snapshot_store_satisfies_the_snapshot_contract() {
    let root = TempDir::new("contract-snap");
    let mut n = 0;
    snapshot_store_contract(&mut || {
        n += 1;
        FileSnapshotStore::new(root.path().join(format!("snaps-{n}")))
    });
}

#[test]
fn file_snapshot_store_skips_damaged_snapshots() {
    let root = TempDir::new("contract-snap-damage");
    let mut n = 0;
    snapshot_store_skips_damaged_contract(
        &mut || {
            n += 1;
            FileSnapshotStore::new(root.path().join(format!("snaps-{n}")))
        },
        &mut |store| {
            let newest = list_snapshots(store.dir()).unwrap().pop().unwrap();
            let mut bytes = std::fs::read(&newest.path).unwrap();
            let mid = bytes.len() / 2;
            bytes[mid] ^= 0x10;
            std::fs::write(&newest.path, bytes).unwrap();
        },
    );
}

#[test]
fn file_and_in_memory_logs_have_the_same_chain_head() {
    let dir = TempDir::new("same-head");
    let events = scenario_events(37);
    let mut file = FileEventLog::open(dir.wal(), wal_cfg(FsyncPolicy::Never)).unwrap();
    let mut mem = InMemoryEventLog::new();
    for (i, ev) in events.iter().enumerate() {
        file.append(i as u64, ev).unwrap();
        mem.append(i as u64, ev).unwrap();
        assert_eq!(file.head(), mem.head(), "record {i}");
    }
    assert_eq!(verify(dir.wal()).unwrap().chain_head, mem.head());
}

// ------------------------------------------------------------ service flows

#[test]
fn service_state_survives_restart_over_files() {
    let dir = TempDir::new("restart");
    let (mut svc, rep) = open(dir.path(), 0, 3);
    assert_eq!(rep.log_records, 0);
    feed(&mut svc, 0, 60);
    let hash = svc.engine().state_hash();
    drop(svc);
    let (svc2, rep) = open(dir.path(), 0, 3);
    assert_eq!(svc2.engine().state_hash(), hash);
    assert_eq!(svc2.engine().state_hash(), run_scenario(60).state_hash());
    assert_eq!(
        (rep.log_records, rep.replayed, rep.snapshot_seq),
        (60, 60, None)
    );
    assert_eq!(rep.repaired_bytes, 0);
}

#[test]
fn rejected_events_are_logged_and_replayed_identically() {
    let dir = TempDir::new("rejected");
    let (mut svc, _) = open(dir.path(), 0, 3);
    let mut rejected = 0;
    for ev in scenario_events(40) {
        if svc.apply_event(&ev).is_err() {
            rejected += 1;
        }
    }
    assert!(rejected > 0, "scenario must contain rejected events");
    assert_eq!(svc.log_len(), 40, "rejected events are in the log");
    let hash = svc.engine().state_hash();
    drop(svc);
    let mut log = ReadOnlyFileLog::open(dir.wal()).unwrap();
    let (engine, rep) = replay_log(&mut log).unwrap();
    assert_eq!(rep.records, 40);
    assert_eq!(engine.state_hash(), hash);
    assert_eq!(engine.events_applied(), 40);
}

#[test]
fn recovery_after_a_crash_at_every_byte_offset_of_the_log() {
    let src = TempDir::new("crash-src");
    let (mut svc, _) = open(src.path(), 0, 3);
    feed(&mut svc, 0, 30);
    drop(svc);
    let bytes = std::fs::read(src.wal()).unwrap();
    let bounds = boundaries(&bytes);
    assert_eq!(bounds.len(), 31);

    let work = TempDir::new("crash-work");
    for cut in 0..=bytes.len() {
        std::fs::write(work.wal(), &bytes[..cut]).unwrap();
        let k = bounds.iter().skip(1).filter(|b| **b <= cut).count();
        let boundary = if cut < HEADER_LEN as usize {
            0
        } else {
            bounds[k]
        };
        let (mut svc, rep) = open(work.path(), 0, 3);
        assert_eq!(rep.log_records, k as u64, "cut {cut}");
        assert_eq!(rep.replayed, k as u64, "cut {cut}");
        assert_eq!(rep.repaired_bytes, (cut - boundary) as u64, "cut {cut}");
        assert_eq!(
            svc.engine().state_hash(),
            run_scenario(k as u64).state_hash(),
            "cut {cut}"
        );
        // The service keeps going on the repaired log.
        let next = &scenario_events(31)[k.min(30)];
        if k < 30 {
            let _ = svc.apply_event(next);
            assert_eq!(
                svc.engine().state_hash(),
                run_scenario(k as u64 + 1).state_hash(),
                "cut {cut}"
            );
        }
        drop(svc);
        let v = verify(work.wal()).unwrap();
        assert_eq!(v.torn_bytes, 0, "cut {cut}: log is clean after repair");
    }
}

#[test]
fn crash_after_a_snapshot_replays_only_the_surviving_tail() {
    let src = TempDir::new("snapcrash-src");
    let (mut svc, _) = open(src.path(), 10, 3);
    feed(&mut svc, 0, 45);
    drop(svc);
    assert_eq!(
        list_snapshots(src.snaps())
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect::<Vec<_>>(),
        vec![20, 30, 40]
    );
    let bytes = std::fs::read(src.wal()).unwrap();
    let bounds = boundaries(&bytes);
    assert_eq!(bounds.len(), 46);

    let work = TempDir::new("snapcrash-work");
    std::fs::create_dir_all(work.snaps()).unwrap();
    for e in list_snapshots(src.snaps()).unwrap() {
        std::fs::copy(&e.path, work.snaps().join(e.path.file_name().unwrap())).unwrap();
    }
    for cut in bounds[40]..=bytes.len() {
        std::fs::write(work.wal(), &bytes[..cut]).unwrap();
        let k = bounds.iter().skip(1).filter(|b| **b <= cut).count();
        // Keep the work directory identical on every iteration: no new snapshots.
        let (svc, rep) = open(work.path(), 0, 3);
        assert_eq!(rep.snapshot_seq, Some(40), "cut {cut}");
        assert_eq!(rep.replayed, k as u64 - 40, "cut {cut}");
        assert_eq!(
            svc.engine().state_hash(),
            run_scenario(k as u64).state_hash(),
            "cut {cut}"
        );
    }
}

#[test]
fn a_log_that_lost_records_a_snapshot_contains_is_refused() {
    let src = TempDir::new("ahead-src");
    let (mut svc, _) = open(src.path(), 10, 3);
    feed(&mut svc, 0, 25);
    drop(svc);
    let bytes = std::fs::read(src.wal()).unwrap();
    let bounds = boundaries(&bytes);
    // Snapshots at 10 and 20; cut the log inside the last 5 records of the
    // newest snapshot's history.
    std::fs::write(src.wal(), &bytes[..bounds[17]]).unwrap();
    let err = try_open(src.path(), 10, 3, wal_cfg(FsyncPolicy::Never))
        .err()
        .unwrap();
    assert_eq!(
        err,
        ServiceError::SnapshotAheadOfLog {
            snapshot_seq: 20,
            log_records: 17
        }
    );
    // The log was not extended or rewritten by the failed open.
    assert_eq!(
        std::fs::read(src.wal()).unwrap(),
        bytes[..bounds[17]].to_vec()
    );
    // Removing the snapshot that is ahead lets the older one be used.
    std::fs::remove_file(src.snaps().join(snapshot_file_name(20))).unwrap();
    let (svc, rep) = open(src.path(), 0, 3);
    assert_eq!(rep.snapshot_seq, Some(10));
    assert_eq!(rep.replayed, 7);
    assert_eq!(svc.engine().state_hash(), run_scenario(17).state_hash());
}

#[test]
fn damaged_newest_snapshot_on_disk_falls_back_to_the_previous_one() {
    let dir = TempDir::new("snap-fallback");
    let (mut svc, _) = open(dir.path(), 10, 3);
    feed(&mut svc, 0, 33);
    let hash = svc.engine().state_hash();
    drop(svc);
    let newest = dir.snaps().join(snapshot_file_name(30));
    let mut b = std::fs::read(&newest).unwrap();
    let last = b.len() - 1;
    b[last] ^= 0xFF;
    std::fs::write(&newest, b).unwrap();
    let (svc2, rep) = open(dir.path(), 10, 3);
    assert_eq!(rep.snapshot_seq, Some(20));
    assert_eq!(rep.skipped_snapshots, 1);
    assert_eq!(rep.replayed, 13);
    assert_eq!(svc2.engine().state_hash(), hash);
}

#[test]
fn unusable_snapshots_fall_back_to_a_full_replay() {
    let dir = TempDir::new("snap-all-bad");
    let (mut svc, _) = open(dir.path(), 10, 2);
    feed(&mut svc, 0, 27);
    let hash = svc.engine().state_hash();
    drop(svc);
    for e in list_snapshots(dir.snaps()).unwrap() {
        std::fs::write(&e.path, b"not a snapshot").unwrap();
    }
    let (svc2, rep) = open(dir.path(), 0, 2);
    assert_eq!(rep.snapshot_seq, None);
    assert_eq!(rep.replayed, 27);
    assert_eq!(svc2.engine().state_hash(), hash);
}

#[test]
fn snapshot_recovery_equals_full_replay() {
    let dir = TempDir::new("snap-eq");
    let (mut svc, _) = open(dir.path(), 7, 2);
    feed(&mut svc, 0, 50);
    drop(svc);
    let mut log = ReadOnlyFileLog::open(dir.wal()).unwrap();
    let mut snaps = FileSnapshotStore::new(dir.snaps());
    let (with_snap, rep) = recover(&mut log, &mut snaps).unwrap();
    assert_eq!(rep.snapshot_seq, Some(49));
    assert_eq!(rep.replayed, 1);
    let (full, _) = replay_log(&mut log).unwrap();
    assert_eq!(with_snap.state_hash(), full.state_hash());
    assert_eq!(full.state_hash(), run_scenario(50).state_hash());
}

#[test]
fn snapshots_are_pruned_on_disk() {
    let dir = TempDir::new("snap-prune");
    let (mut svc, _) = open(dir.path(), 10, 2);
    feed(&mut svc, 0, 55);
    let seqs: Vec<u64> = list_snapshots(dir.snaps())
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(seqs, vec![40, 50]);
    assert!(svc.last_snapshot_error().is_none());
}

#[test]
fn mid_file_corruption_stops_recovery_and_leaves_the_file_alone() {
    let dir = TempDir::new("midcorrupt");
    let (mut svc, _) = open(dir.path(), 0, 3);
    feed(&mut svc, 0, 20);
    drop(svc);
    let mut bytes = std::fs::read(dir.wal()).unwrap();
    let bounds = boundaries(&bytes);
    bytes[bounds[8] + 7] ^= 0x01;
    std::fs::write(dir.wal(), &bytes).unwrap();
    let r = FileEventLog::open(dir.wal(), wal_cfg(FsyncPolicy::Never));
    assert!(matches!(
        r,
        Err(WalError::Corrupt { offset, .. }) if offset == bounds[8] as u64
    ));
    assert_eq!(std::fs::read(dir.wal()).unwrap(), bytes);
    assert!(ReadOnlyFileLog::open(dir.wal()).is_err());
}

#[test]
fn read_only_replay_never_repairs_the_file() {
    let dir = TempDir::new("readonly");
    let (mut svc, _) = open(dir.path(), 0, 3);
    feed(&mut svc, 0, 15);
    drop(svc);
    let mut bytes = std::fs::read(dir.wal()).unwrap();
    let bounds = boundaries(&bytes);
    bytes.truncate(bounds[15] - 9);
    std::fs::write(dir.wal(), &bytes).unwrap();

    let mut log = ReadOnlyFileLog::open(dir.wal()).unwrap();
    assert_eq!(log.len(), 14);
    assert_eq!(log.repaired_bytes(), (bounds[15] - bounds[14] - 9) as u64);
    let (engine, rep) = replay_log(&mut log).unwrap();
    assert_eq!(rep.records, 14);
    assert_eq!(rep.repaired_bytes, log.repaired_bytes());
    assert_eq!(engine.state_hash(), run_scenario(14).state_hash());
    assert_eq!(std::fs::read(dir.wal()).unwrap(), bytes, "file untouched");

    // It is read-only: appends are refused as a contract violation.
    let err = log.append(14, &scenario_events(15)[14]).unwrap_err();
    assert_eq!(err.kind, PortErrorKind::Invalid);
    assert!(log.sync().is_ok());
    assert_eq!(std::fs::read(dir.wal()).unwrap(), bytes);
}

#[test]
fn repeated_crash_and_restart_cycles_converge_on_the_same_state() {
    let dir = TempDir::new("cycles");
    let mut applied = 0u64;
    for cycle in 0..15u64 {
        let (mut svc, rep) = open(dir.path(), 5, 2);
        assert_eq!(rep.log_records, applied, "cycle {cycle}");
        assert_eq!(
            svc.engine().state_hash(),
            run_scenario(applied).state_hash(),
            "cycle {cycle}"
        );
        feed(&mut svc, applied, applied + 7);
        applied += 7;
        drop(svc);
        // A crash in the middle of the next append leaves a partial record.
        let mut bytes = std::fs::read(dir.wal()).unwrap();
        bytes.extend_from_slice(&[0x30, 0, 0, 0, cycle as u8, 9, 9, 9, 9]);
        std::fs::write(dir.wal(), bytes).unwrap();
    }
    let (svc, rep) = open(dir.path(), 5, 2);
    assert_eq!(rep.log_records, 105);
    assert_eq!(svc.engine().state_hash(), run_scenario(105).state_hash());
}

#[test]
fn a_full_log_file_refuses_events_and_the_engine_stays_in_step() {
    let dir = TempDir::new("full");
    // Room for the header and a handful of records only.
    let probe = TempDir::new("full-probe");
    let (mut p, _) = open(probe.path(), 0, 3);
    feed(&mut p, 0, 6);
    drop(p);
    let limit = std::fs::metadata(probe.wal()).unwrap().len();
    let cfg = WalConfig {
        fsync: FsyncPolicy::Never,
        max_file_bytes: limit,
    };
    let (mut svc, _) = try_open(dir.path(), 0, 3, cfg).unwrap();
    feed(&mut svc, 0, 6);
    let hash = svc.engine().state_hash();
    let err = svc.apply_event(&scenario_events(7)[6]).unwrap_err();
    assert!(matches!(
        &err,
        ServiceError::Storage(e) if e.kind == PortErrorKind::Full
    ));
    assert_eq!(svc.engine().state_hash(), hash);
    assert_eq!(svc.engine().events_applied(), 6);
    drop(svc);
    let (svc, rep) = open(dir.path(), 0, 3);
    assert_eq!(rep.log_records, 6);
    assert_eq!(svc.engine().state_hash(), hash);
}

#[test]
fn log_failures_surface_as_storage_errors_through_the_service() {
    // A path whose parent is a regular file cannot be opened as a log.
    let dir = TempDir::new("blocked");
    let blocker = dir.path().join("plain-file");
    std::fs::write(&blocker, b"x").unwrap();
    let r = FileEventLog::open(blocker.join("engine.wal"), WalConfig::default());
    assert!(matches!(r, Err(WalError::Io(_))));
    let port: perp_app::ports::PortError = r.err().unwrap().into();
    assert_eq!(port.kind, PortErrorKind::Io);
}

#[test]
fn snapshot_directory_is_created_on_first_save_and_save_errors_are_reported() {
    let dir = TempDir::new("snapdir");
    let (mut svc, _) = open(dir.path(), 0, 3);
    feed(&mut svc, 0, 5);
    assert!(!dir.snaps().exists());
    assert_eq!(svc.snapshot_now().unwrap(), 5);
    assert!(dir.snaps().is_dir());
    drop(svc);

    // A snapshot "directory" that is a file: the automatic snapshot fails,
    // the event does not.
    let dir2 = TempDir::new("snapdir-blocked");
    let (mut svc, _) = open(dir2.path(), 3, 3);
    std::fs::write(dir2.snaps(), b"x").unwrap();
    feed(&mut svc, 0, 3);
    assert_eq!(svc.engine().events_applied(), 3);
    let e = svc.last_snapshot_error().unwrap();
    assert_eq!(e.kind, PortErrorKind::Io);
    assert_eq!(svc.log_len(), 3);
}

#[test]
fn always_policy_syncs_each_event_and_snapshots_sync_the_log() {
    let dir = TempDir::new("fsync");
    let mut log = FileEventLog::open(dir.wal(), wal_cfg(FsyncPolicy::Always)).unwrap();
    for (i, ev) in scenario_events(4).iter().enumerate() {
        log.append(i as u64, ev).unwrap();
    }
    assert_eq!(log.wal().syncs(), 4);

    let dir2 = TempDir::new("fsync-never");
    let (mut svc, _) = try_open(dir2.path(), 5, 3, wal_cfg(FsyncPolicy::Never)).unwrap();
    feed(&mut svc, 0, 4);
    svc.sync().unwrap();
    assert_eq!(svc.log_len(), 4);
    let v = verify(dir2.wal()).unwrap();
    assert_eq!(v.records, 4);
}

#[test]
fn exchange_api_over_the_file_adapters() {
    use perp_app::api::ExchangeApi;
    use perp_domain::fixed::Money;
    use perp_domain::types::EventBody;
    let dir = TempDir::new("api");
    let (svc, _) = open(dir.path(), 0, 3);
    let mut api: Box<dyn ExchangeApi> = Box::new(svc);
    api.submit(EventBody::Configure(Default::default()))
        .unwrap();
    api.submit(EventBody::Deposit {
        account: 3,
        amount: Money(7_000_000),
    })
    .unwrap();
    assert_eq!(api.account(3).unwrap().collateral, Money(7_000_000));
    assert_eq!(api.summary().events_applied, 2);
    drop(api);
    let (svc, rep) = open(dir.path(), 0, 3);
    assert_eq!(rep.log_records, 2);
    assert_eq!(
        svc.engine().account_view(3).unwrap().collateral,
        Money(7_000_000)
    );
}

#[test]
fn replaying_the_file_log_matches_replaying_the_in_memory_copy() {
    let dir = TempDir::new("copy");
    let (mut svc, _) = open(dir.path(), 0, 3);
    feed(&mut svc, 0, 33);
    drop(svc);
    let mut file_log = ReadOnlyFileLog::open(dir.wal()).unwrap();
    let mut mem_log = InMemoryEventLog::from_events(&scenario_events(33));
    let (a, ra) = replay_log(&mut file_log).unwrap();
    let (b, rb) = replay_log(&mut mem_log).unwrap();
    assert_eq!(a.state_hash(), b.state_hash());
    assert_eq!(ra.state_hash, rb.state_hash);
    assert_eq!(file_log.head(), mem_log.head());
    assert_eq!(a.events_applied(), 33);
}

#[test]
fn file_snapshot_store_reports_io_errors_as_port_errors() {
    let dir = TempDir::new("store-io");
    let file = dir.path().join("plain");
    std::fs::write(&file, b"x").unwrap();
    let mut store = FileSnapshotStore::new(&file);
    let err = store.save(&run_scenario(3)).unwrap_err();
    assert_eq!(err.kind, PortErrorKind::Io);
    assert_eq!(store.dir(), file.as_path());
}
