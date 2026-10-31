//! Use cases around the domain engine: crash recovery, durable event
//! application, snapshot policy and replay.
//!
//! [`ExchangeService`] owns one [`Engine`] and three ports: an [`EventLog`], a
//! [`SnapshotStore`] and a [`Clock`]. Its rules are:
//!
//! * **Cheap rejections are not logged.** [`Engine::precheck`] rejects events
//!   that cannot succeed whatever the state is (unknown account, market or
//!   order, zero amounts, bad ranges, time regression, not configured). Such an
//!   event is neither logged nor applied, so a flood of invalid requests cannot
//!   grow the log.
//! * **Log first.** An event that passed the precheck is appended to the log
//!   before the engine sees it. If the append fails, the engine is not touched.
//!   If the engine then rejects the event for a state-dependent reason (margin,
//!   fill-or-kill, ...), it stays in the log, because replay must reproduce the
//!   rejection and the counter exactly.
//! * **A halted engine logs nothing.** After an internal inconsistency the
//!   engine refuses every event; the service does not append them.
//! * **A full log degrades to read-only.** When the log reports
//!   [`PortErrorKind::Full`] the service enters read-only mode: that event and
//!   every later state-changing call fail with [`ServiceError::ReadOnly`]
//!   (nothing is logged or applied) while queries keep working. See
//!   [`ExchangeService::is_read_only`] for how an operator recovers. Any other
//!   storage failure stays [`ServiceError::Storage`].
//! * **Bounded clock steps.** [`ExchangeService::submit`] stamps events with the
//!   clock, but event time advances by at most
//!   [`ServiceConfig::max_time_step_ms`] per event, so one wild clock reading
//!   cannot be written to the log in full.
//! * **Recovery** loads the newest valid snapshot (or starts from an empty
//!   engine) and replays the log from the snapshot's sequence number, ignoring
//!   the engine's business errors.
//! * **Snapshots** are taken every [`ServiceConfig::snapshot_every`] events. The
//!   log is synced first so a snapshot never describes events the log could
//!   still lose. A failing snapshot does not fail the event that triggered it
//!   (the event is already logged and applied); it is kept in
//!   [`ExchangeService::last_snapshot_error`].
//!
//! The service is single-threaded by design (`&mut self` everywhere); the
//! network adapter serialises access.

use crate::api::{ApiError, ExchangeApi};
use crate::ports::{Clock, EventLog, PortError, PortErrorKind, SnapshotStore};
use perp_domain::engine::Engine;
use perp_domain::error::Error;
use perp_domain::sha256::Digest;
use perp_domain::types::{AccountId, Event, EventBody, MarketId, Millis, Output};
use perp_domain::views::{AccountView, BookView, Summary};
use std::fmt;

/// Snapshot and clock policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Take a snapshot after every this many applied events (accepted or
    /// rejected); 0 disables automatic snapshots.
    pub snapshot_every: u64,
    /// Snapshots to keep; values below 1 are treated as 1.
    pub keep_snapshots: usize,
    /// Largest amount event time may advance between two consecutive events
    /// stamped by [`ExchangeService::submit`], in milliseconds; 0 is treated as
    /// 1. The first event of an empty engine is stamped with the clock as is.
    pub max_time_step_ms: Millis,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        ServiceConfig {
            snapshot_every: 10_000,
            keep_snapshots: 3,
            max_time_step_ms: 3_600_000,
        }
    }
}

/// What recovery did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Sequence number of the snapshot recovery started from, if one was used.
    pub snapshot_seq: Option<u64>,
    /// Newer snapshots that failed validation and were skipped. The port only
    /// reports this together with a valid snapshot, so it is 0 when no
    /// snapshot could be used at all.
    pub skipped_snapshots: usize,
    /// Records in the log.
    pub log_records: u64,
    /// Records applied to the engine during recovery.
    pub replayed: u64,
    /// Bytes of torn log tail discarded when the log was opened.
    pub repaired_bytes: u64,
    /// State hash of the recovered engine.
    pub state_hash: Digest,
}

/// Result of a read-only full replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    /// Records replayed.
    pub records: u64,
    /// Bytes of torn tail the log reported as discarded or ignored.
    pub repaired_bytes: u64,
    /// True when the replayed engine ended up halted.
    pub halted: bool,
    /// State hash of the replayed engine.
    pub state_hash: Digest,
}

/// Failure of a service operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// The engine rejected the event (it is in the log unless the precheck
    /// rejected it) or is halted.
    Engine(Error),
    /// A port failed.
    Storage(PortError),
    /// The service is in degraded read-only mode (the log is full): nothing
    /// was logged or applied. Queries still work; see
    /// [`ExchangeService::is_read_only`].
    ReadOnly(String),
    /// The newest valid snapshot is ahead of the log: the log lost records the
    /// snapshot already contains.
    SnapshotAheadOfLog {
        /// Events the snapshot contains.
        snapshot_seq: u64,
        /// Records in the log.
        log_records: u64,
    },
    /// The log and the engine disagree about sequence numbers.
    Inconsistent(&'static str),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceError::Engine(e) => write!(f, "{e}"),
            ServiceError::Storage(e) => write!(f, "storage: {e}"),
            ServiceError::ReadOnly(why) => write!(f, "read-only: {why}"),
            ServiceError::SnapshotAheadOfLog {
                snapshot_seq,
                log_records,
            } => write!(
                f,
                "snapshot at {snapshot_seq} is ahead of the log ({log_records} records)"
            ),
            ServiceError::Inconsistent(why) => write!(f, "inconsistent state: {why}"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<PortError> for ServiceError {
    fn from(e: PortError) -> Self {
        ServiceError::Storage(e)
    }
}

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::Engine(e) => ApiError::Engine(e),
            ServiceError::ReadOnly(why) => ApiError::Degraded(why),
            other => ApiError::Storage(other.to_string()),
        }
    }
}

/// Replay the log from the engine's current event count, ignoring business
/// rejections. Returns the number of records applied and the largest event
/// timestamp seen (`ts_floor` when no event was applied).
fn replay_tail<L: EventLog + ?Sized>(
    engine: &mut Engine,
    log: &mut L,
    ts_floor: Millis,
) -> Result<(u64, Millis), ServiceError> {
    let start = engine.events_applied();
    let mut replayed = 0u64;
    let mut max_ts = ts_floor;
    let mut problem: Option<&'static str> = None;
    log.replay_from(start, &mut |seq, ev| {
        if problem.is_some() {
            return;
        }
        if seq != engine.events_applied() {
            problem = Some("log sequence does not follow the engine's event count");
            return;
        }
        // A rejected event is part of the history: it only advanced the counter.
        let _ = engine.apply(&ev);
        replayed += 1;
        max_ts = max_ts.max(ev.ts);
    })?;
    if let Some(why) = problem {
        return Err(ServiceError::Inconsistent(why));
    }
    if engine.events_applied() != log.len() {
        return Err(ServiceError::Inconsistent(
            "log length differs from the number of replayed events",
        ));
    }
    Ok((replayed, max_ts))
}

/// Rebuild the engine from the newest valid snapshot plus the log tail. Does
/// not modify the log.
pub fn recover<L: EventLog, S: SnapshotStore>(
    log: &mut L,
    snapshots: &mut S,
) -> Result<(Engine, RecoveryReport), ServiceError> {
    let (engine, report, _) = recover_with_clock(log, snapshots)?;
    Ok((engine, report))
}

fn recover_with_clock<L: EventLog, S: SnapshotStore>(
    log: &mut L,
    snapshots: &mut S,
) -> Result<(Engine, RecoveryReport, Millis), ServiceError> {
    let (mut engine, snapshot_seq, skipped) = match snapshots.load_latest()? {
        Some(l) => {
            if l.engine.events_applied() != l.seq {
                return Err(ServiceError::Inconsistent(
                    "snapshot sequence differs from its engine's event count",
                ));
            }
            (l.engine, Some(l.seq), l.skipped)
        }
        None => (Engine::new(), None, 0),
    };
    let log_records = log.len();
    if log_records < engine.events_applied() {
        return Err(ServiceError::SnapshotAheadOfLog {
            snapshot_seq: engine.events_applied(),
            log_records,
        });
    }
    let clock_floor = engine.summary().clock;
    let (replayed, last_ts) = replay_tail(&mut engine, log, clock_floor)?;
    let report = RecoveryReport {
        snapshot_seq,
        skipped_snapshots: skipped,
        log_records,
        replayed,
        repaired_bytes: log.repaired_bytes(),
        state_hash: engine.state_hash(),
    };
    Ok((engine, report, last_ts))
}

/// Replay a whole log into a fresh engine, ignoring snapshots. Read-only with
/// respect to the log (what happens to a torn tail is up to the log's
/// implementation; see [`EventLog::repaired_bytes`]).
pub fn replay_log<L: EventLog>(log: &mut L) -> Result<(Engine, ReplayReport), ServiceError> {
    let mut engine = Engine::new();
    replay_tail(&mut engine, log, 0)?;
    let report = ReplayReport {
        records: log.len(),
        repaired_bytes: log.repaired_bytes(),
        halted: engine.is_halted(),
        state_hash: engine.state_hash(),
    };
    Ok((engine, report))
}

/// The exchange as a durable service. See the module documentation.
pub struct ExchangeService<L: EventLog, S: SnapshotStore, C: Clock> {
    engine: Engine,
    log: L,
    snapshots: S,
    clock: C,
    cfg: ServiceConfig,
    last_ts: Millis,
    last_snapshot_error: Option<PortError>,
    read_only: Option<String>,
}

impl<L: EventLog, S: SnapshotStore, C: Clock> ExchangeService<L, S, C> {
    /// Recover the engine from `snapshots` and `log` and start serving.
    pub fn open(
        mut log: L,
        mut snapshots: S,
        clock: C,
        cfg: ServiceConfig,
    ) -> Result<(Self, RecoveryReport), ServiceError> {
        let (engine, report, last_ts) = recover_with_clock(&mut log, &mut snapshots)?;
        let service = ExchangeService {
            engine,
            log,
            snapshots,
            clock,
            cfg,
            last_ts,
            last_snapshot_error: None,
            read_only: None,
        };
        Ok((service, report))
    }

    /// Pre-check, log, then apply `ev`.
    ///
    /// * Read-only mode (the log was full): [`ServiceError::ReadOnly`], nothing
    ///   logged or applied.
    /// * A halted engine refuses the event with `Error::Halted`; nothing is
    ///   logged.
    /// * [`Engine::precheck`] rejects the event: [`ServiceError::Engine`],
    ///   nothing is logged or applied, the applied-event counter does not move.
    /// * The log is full: the service enters read-only mode and the call fails
    ///   with [`ServiceError::ReadOnly`]; the engine is not touched.
    /// * Any other log failure: [`ServiceError::Storage`]; the engine is not
    ///   touched.
    /// * The engine rejects the event for a state-dependent reason: the event
    ///   stays logged (replay reproduces the rejection) and the rejection is
    ///   returned as [`ServiceError::Engine`].
    pub fn apply_event(&mut self, ev: &Event) -> Result<Vec<Output>, ServiceError> {
        if let Some(why) = &self.read_only {
            return Err(ServiceError::ReadOnly(why.clone()));
        }
        if self.engine.is_halted() {
            return Err(ServiceError::Engine(Error::Halted));
        }
        self.engine.precheck(ev).map_err(ServiceError::Engine)?;
        let seq = self.engine.events_applied();
        if let Err(e) = self.log.append(seq, ev) {
            if e.kind == PortErrorKind::Full {
                let why = format!("the event log is full ({})", e.message);
                self.read_only = Some(why.clone());
                return Err(ServiceError::ReadOnly(why));
            }
            return Err(ServiceError::Storage(e));
        }
        let result = self.engine.apply(ev);
        self.last_ts = self.last_ts.max(ev.ts);
        let every = self.cfg.snapshot_every;
        if every > 0
            && self.engine.events_applied().is_multiple_of(every)
            && !self.engine.is_halted()
        {
            self.last_snapshot_error = self.take_snapshot().err();
        }
        result.map_err(ServiceError::Engine)
    }

    /// Stamp `body` and apply it with [`ExchangeService::apply_event`].
    ///
    /// The timestamp is the clock reading limited to the range
    /// `[last event time, last event time + max_time_step_ms]`: it never goes
    /// backwards if the clock does, and it advances by at most one step per
    /// event if the clock jumps forward (an NTP step, a VM resume), so a bad
    /// reading is never logged in full and the engine catches up with the clock
    /// step by step. While the engine is empty there is no previous event and
    /// the clock is used as is.
    pub fn submit(&mut self, body: EventBody) -> Result<Vec<Output>, ServiceError> {
        let now = self.clock.now_ms();
        let ts = if self.engine.events_applied() == 0 {
            now
        } else {
            let step = self.cfg.max_time_step_ms.max(1);
            now.max(self.last_ts).min(self.last_ts.saturating_add(step))
        };
        self.apply_event(&Event { ts, body })
    }

    fn take_snapshot(&mut self) -> Result<u64, PortError> {
        self.log.sync()?;
        let seq = self.snapshots.save(&self.engine)?;
        self.snapshots.prune(self.cfg.keep_snapshots.max(1))?;
        Ok(seq)
    }

    /// Sync the log, save a snapshot and prune old ones. Returns the
    /// snapshot's sequence number. Refused while the engine is halted.
    pub fn snapshot_now(&mut self) -> Result<u64, ServiceError> {
        if self.engine.is_halted() {
            return Err(ServiceError::Engine(Error::Halted));
        }
        Ok(self.take_snapshot()?)
    }

    /// Make everything logged so far durable.
    pub fn sync(&mut self) -> Result<(), ServiceError> {
        Ok(self.log.sync()?)
    }

    /// Read-only access to the engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Records in the log.
    pub fn log_len(&self) -> u64 {
        self.log.len()
    }

    /// Integrity-chain head of the log.
    pub fn log_head(&self) -> Digest {
        self.log.head()
    }

    /// Failure of the most recent automatic snapshot, if it failed.
    pub fn last_snapshot_error(&self) -> Option<&PortError> {
        self.last_snapshot_error.as_ref()
    }

    /// Largest event timestamp applied so far; `submit` never stamps less.
    pub fn last_ts(&self) -> Millis {
        self.last_ts
    }

    /// True once the log reported that it is full. Every state-changing call
    /// then fails with [`ServiceError::ReadOnly`] while queries keep working.
    ///
    /// Recovery is an operator action: free disk space or raise the log size
    /// limit (`serve --max-log-mib`), then restart the process; the log is
    /// replayed from disk and the service starts in normal mode again.
    pub fn is_read_only(&self) -> bool {
        self.read_only.is_some()
    }
}

impl<L, S, C> ExchangeApi for ExchangeService<L, S, C>
where
    L: EventLog + Send + 'static,
    S: SnapshotStore + Send + 'static,
    C: Clock + Send + 'static,
{
    fn submit(&mut self, body: EventBody) -> Result<Vec<Output>, ApiError> {
        ExchangeService::submit(self, body).map_err(ApiError::from)
    }

    fn book(&self, market: MarketId, depth: usize) -> Option<BookView> {
        self.engine.book_view(market, depth)
    }

    fn account(&self, id: AccountId) -> Option<AccountView> {
        self.engine.account_view(id)
    }

    fn summary(&self) -> Summary {
        self.engine.summary()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use crate::contract::{run_scenario, scenario_events};
    use crate::fakes::{InMemoryEventLog, InMemorySnapshotStore, ManualClock};
    use crate::ports::LoadedSnapshot;
    use perp_domain::fixed::Money;

    type Svc = ExchangeService<InMemoryEventLog, InMemorySnapshotStore, ManualClock>;

    struct Rig {
        log: InMemoryEventLog,
        snaps: InMemorySnapshotStore,
        clock: ManualClock,
    }

    impl Rig {
        fn new() -> Rig {
            Rig {
                log: InMemoryEventLog::new(),
                snaps: InMemorySnapshotStore::new(),
                clock: ManualClock::new(1_000),
            }
        }

        fn open(&self, every: u64, keep: usize) -> (Svc, RecoveryReport) {
            self.open_cfg(ServiceConfig {
                snapshot_every: every,
                keep_snapshots: keep,
                ..ServiceConfig::default()
            })
        }

        fn open_cfg(&self, cfg: ServiceConfig) -> (Svc, RecoveryReport) {
            ExchangeService::open(
                self.log.clone(),
                self.snaps.clone(),
                self.clock.clone(),
                cfg,
            )
            .unwrap()
        }
    }

    fn feed(svc: &mut Svc, n: u64) {
        for ev in scenario_events(n) {
            let _ = svc.apply_event(&ev);
        }
    }

    #[test]
    fn open_on_empty_stores_starts_a_fresh_engine() {
        let rig = Rig::new();
        let (svc, rep) = rig.open(0, 3);
        assert_eq!(
            rep,
            RecoveryReport {
                snapshot_seq: None,
                skipped_snapshots: 0,
                log_records: 0,
                replayed: 0,
                repaired_bytes: 0,
                state_hash: Engine::new().state_hash(),
            }
        );
        assert_eq!(svc.engine().events_applied(), 0);
        assert_eq!(svc.log_len(), 0);
        assert_eq!(svc.last_ts(), 0);
        assert!(svc.last_snapshot_error().is_none());
    }

    #[test]
    fn applying_events_matches_a_plain_engine_and_logs_every_event() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        let mut plain = Engine::new();
        let mut accepted = 0;
        for ev in scenario_events(60) {
            let expected = plain.apply(&ev);
            let got = svc.apply_event(&ev);
            match (expected, got) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a, b);
                    accepted += 1;
                }
                (Err(a), Err(ServiceError::Engine(b))) => assert_eq!(a, b),
                (a, b) => panic!("diverged: {a:?} vs {b:?}"),
            }
        }
        assert!(accepted > 0 && accepted < 60, "scenario has both kinds");
        assert_eq!(svc.engine().state_hash(), plain.state_hash());
        assert_eq!(svc.log_len(), 60);
        assert_eq!(rig.log.events(), scenario_events(60));
    }

    #[test]
    fn a_state_dependent_rejection_stays_in_the_log_and_advances_the_counter() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        // The scenario configures three accounts and has opened all of them.
        feed(&mut svc, 8);
        let before = svc.engine().events_applied();
        let bad = Event {
            ts: 10_000,
            body: EventBody::Deposit {
                account: 424_242,
                amount: Money(5),
            },
        };
        // The pre-check cannot see the account limit; the engine rejects it.
        assert_eq!(svc.engine().precheck(&bad), Ok(()));
        assert_eq!(
            svc.apply_event(&bad),
            Err(ServiceError::Engine(Error::TooManyAccounts))
        );
        assert_eq!(svc.engine().events_applied(), before + 1);
        assert_eq!(svc.log_len(), before + 1);
        assert_eq!(rig.log.events().last(), Some(&bad));
        // Replay reproduces the rejection.
        let (again, _) = replay_log(&mut rig.log.clone()).unwrap();
        assert_eq!(again.state_hash(), svc.engine().state_hash());
    }

    #[test]
    fn prechecked_rejections_are_neither_logged_nor_applied() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        feed(&mut svc, 10);
        let (applied, len, head, last_ts) = (
            svc.engine().events_applied(),
            svc.log_len(),
            svc.log_head(),
            svc.last_ts(),
        );
        let hash = svc.engine().state_hash();
        let junk = [
            EventBody::Withdraw {
                account: 424_242,
                amount: Money(5),
            },
            EventBody::Deposit {
                account: 1,
                amount: Money(0),
            },
            EventBody::Cancel {
                account: 2,
                order_id: 99,
            },
            EventBody::Configure(Default::default()),
        ];
        for i in 0..2_000u64 {
            let ev = Event {
                ts: 1_000_000 + i,
                body: junk[i as usize % junk.len()].clone(),
            };
            assert!(matches!(svc.apply_event(&ev), Err(ServiceError::Engine(_))));
        }
        // A regressed timestamp is also a cheap rejection.
        let late = Event {
            ts: 1,
            body: EventBody::Tick,
        };
        assert_eq!(
            svc.apply_event(&late),
            Err(ServiceError::Engine(Error::TimeRegression))
        );
        assert_eq!(svc.log_len(), len, "the log did not grow");
        assert_eq!(rig.log.len(), len);
        assert_eq!(svc.log_head(), head);
        assert_eq!(svc.engine().events_applied(), applied);
        assert_eq!(svc.engine().state_hash(), hash);
        assert_eq!(svc.last_ts(), last_ts, "a rejected event is not a time");
        // The service keeps working and the log still replays identically.
        svc.submit(EventBody::Tick).unwrap();
        assert_eq!(svc.log_len(), len + 1);
        let (again, _) = replay_log(&mut rig.log.clone()).unwrap();
        assert_eq!(again.state_hash(), svc.engine().state_hash());
    }

    #[test]
    fn restart_from_the_log_alone_reproduces_the_state() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        feed(&mut svc, 50);
        let hash = svc.engine().state_hash();
        drop(svc);
        let (svc2, rep) = rig.open(0, 3);
        assert_eq!(svc2.engine().state_hash(), hash);
        assert_eq!(rep.snapshot_seq, None);
        assert_eq!(rep.replayed, 50);
        assert_eq!(rep.log_records, 50);
        assert_eq!(rep.state_hash, hash);
        assert_eq!(svc2.engine().state_hash(), run_scenario(50).state_hash());
    }

    #[test]
    fn snapshots_are_taken_on_the_configured_cadence_and_pruned() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(10, 2);
        feed(&mut svc, 35);
        assert_eq!(rig.snaps.seqs(), vec![20, 30]);
        assert!(svc.last_snapshot_error().is_none());
        // Each snapshot synced the log first.
        assert_eq!(rig.log.sync_count(), 3);
    }

    #[test]
    fn restart_uses_the_newest_snapshot_and_replays_only_the_tail() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(10, 2);
        feed(&mut svc, 37);
        let hash = svc.engine().state_hash();
        drop(svc);
        let (svc2, rep) = rig.open(10, 2);
        assert_eq!(rep.snapshot_seq, Some(30));
        assert_eq!(rep.replayed, 7);
        assert_eq!(rep.log_records, 37);
        assert_eq!(rep.skipped_snapshots, 0);
        assert_eq!(svc2.engine().state_hash(), hash);
        // Same result as a full replay without any snapshot.
        let mut full_log = rig.log.clone();
        let (full, _) = replay_log(&mut full_log).unwrap();
        assert_eq!(full.state_hash(), hash);
    }

    #[test]
    fn zero_snapshot_every_never_snapshots() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        feed(&mut svc, 40);
        assert!(rig.snaps.seqs().is_empty());
        assert_eq!(rig.log.sync_count(), 0);
    }

    #[test]
    fn keep_zero_is_treated_as_keep_one() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(5, 0);
        feed(&mut svc, 12);
        assert_eq!(rig.snaps.seqs(), vec![10]);
    }

    #[test]
    fn snapshot_now_syncs_saves_and_prunes() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 1);
        feed(&mut svc, 8);
        assert_eq!(svc.snapshot_now().unwrap(), 8);
        feed_more(&mut svc, 8, 4);
        assert_eq!(svc.snapshot_now().unwrap(), 12);
        assert_eq!(rig.snaps.seqs(), vec![12]);
        assert_eq!(rig.log.sync_count(), 2);
    }

    /// Apply events `from..to` of the scenario.
    fn feed_more(svc: &mut Svc, from: u64, count: u64) {
        for ev in scenario_events(from + count)
            .into_iter()
            .skip(from as usize)
        {
            let _ = svc.apply_event(&ev);
        }
    }

    #[test]
    fn a_damaged_newest_snapshot_falls_back_to_an_older_one() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(10, 3);
        feed(&mut svc, 35);
        let hash = svc.engine().state_hash();
        drop(svc);
        rig.snaps.damage_newest();
        let (svc2, rep) = rig.open(10, 3);
        assert_eq!(rep.snapshot_seq, Some(20));
        assert_eq!(rep.skipped_snapshots, 1);
        assert_eq!(rep.replayed, 15);
        assert_eq!(svc2.engine().state_hash(), hash);
    }

    #[test]
    fn with_no_valid_snapshot_recovery_replays_the_whole_log() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(10, 1);
        feed(&mut svc, 25);
        let hash = svc.engine().state_hash();
        drop(svc);
        rig.snaps.damage_newest();
        let (svc2, rep) = rig.open(10, 1);
        assert_eq!(rep.snapshot_seq, None);
        // The port reports skipped snapshots only together with a valid one.
        assert_eq!(rep.skipped_snapshots, 0);
        assert_eq!(rep.replayed, 25);
        assert_eq!(svc2.engine().state_hash(), hash);
    }

    #[test]
    fn a_snapshot_ahead_of_the_log_is_an_error() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(10, 3);
        feed(&mut svc, 20);
        drop(svc);
        rig.log.truncate(15);
        let err = ExchangeService::open(
            rig.log.clone(),
            rig.snaps.clone(),
            rig.clock.clone(),
            ServiceConfig::default(),
        )
        .err()
        .unwrap();
        assert_eq!(
            err,
            ServiceError::SnapshotAheadOfLog {
                snapshot_seq: 20,
                log_records: 15
            }
        );
        assert!(err.to_string().contains("ahead of the log"));
    }

    #[test]
    fn a_failing_log_append_leaves_the_engine_unchanged() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        feed(&mut svc, 10);
        let hash = svc.engine().state_hash();
        let head = svc.log_head();
        rig.log.set_fail_appends(true);
        let ev = &scenario_events(11)[10];
        let err = svc.apply_event(ev).unwrap_err();
        assert!(matches!(
            &err,
            ServiceError::Storage(e) if e.kind == PortErrorKind::Io
        ));
        assert_eq!(svc.engine().state_hash(), hash);
        assert_eq!(svc.engine().events_applied(), 10);
        assert_eq!(svc.log_len(), 10);
        assert_eq!(svc.log_head(), head);
        rig.log.set_fail_appends(false);
        svc.apply_event(ev).unwrap();
        assert_eq!(svc.engine().events_applied(), 11);
    }

    #[test]
    fn a_full_log_turns_the_service_read_only_without_applying_anything() {
        let log = InMemoryEventLog::with_capacity_limit(3);
        let (mut svc, _) = ExchangeService::open(
            log.clone(),
            InMemorySnapshotStore::new(),
            ManualClock::new(0),
            ServiceConfig::default(),
        )
        .unwrap();
        feed(&mut svc, 3);
        assert!(!svc.is_read_only());
        let hash = svc.engine().state_hash();
        let head = svc.log_head();
        let err = svc.apply_event(&scenario_events(4)[3]).unwrap_err();
        assert!(matches!(&err, ServiceError::ReadOnly(m) if m.contains("full")));
        assert!(svc.is_read_only());
        // Every later state-changing call is refused, whatever it is.
        for _ in 0..3 {
            assert!(matches!(
                svc.apply_event(&scenario_events(4)[3]),
                Err(ServiceError::ReadOnly(_))
            ));
            assert!(matches!(
                svc.submit(EventBody::Tick),
                Err(ServiceError::ReadOnly(_))
            ));
        }
        assert_eq!(svc.engine().state_hash(), hash);
        assert_eq!(svc.engine().events_applied(), 3);
        assert_eq!(svc.log_head(), head);
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn a_read_only_service_still_answers_queries_and_maps_to_degraded() {
        let log = InMemoryEventLog::with_capacity_limit(3);
        let (svc, _) = ExchangeService::open(
            log.clone(),
            InMemorySnapshotStore::new(),
            ManualClock::new(0),
            ServiceConfig::default(),
        )
        .unwrap();
        let mut api: Box<dyn ExchangeApi> = Box::new(svc);
        api.submit(EventBody::Configure(Default::default()))
            .unwrap();
        api.submit(EventBody::Deposit {
            account: 7,
            amount: Money(2_500_000),
        })
        .unwrap();
        api.submit(EventBody::Tick).unwrap();
        for _ in 0..2 {
            let e = api.submit(EventBody::Tick).unwrap_err();
            assert!(
                matches!(&e, ApiError::Degraded(m) if m.contains("full")),
                "{e}"
            );
        }
        // Queries keep working.
        assert_eq!(api.account(7).unwrap().collateral, Money(2_500_000));
        assert_eq!(api.summary().events_applied, 3);
        assert!(api.book(1, 5).is_none());
    }

    #[test]
    fn restarting_with_more_room_resumes_normal_operation() {
        let rig = Rig::new();
        rig.log.set_capacity_limit(Some(5));
        let (mut svc, _) = rig.open(0, 3);
        feed(&mut svc, 5);
        assert!(matches!(
            svc.apply_event(&scenario_events(6)[5]),
            Err(ServiceError::ReadOnly(_))
        ));
        assert!(svc.is_read_only());
        let hash = svc.engine().state_hash();
        drop(svc);
        // The operator frees space / raises the limit and restarts.
        rig.log.set_capacity_limit(None);
        let (mut svc, rep) = rig.open(0, 3);
        assert!(!svc.is_read_only());
        assert_eq!(rep.log_records, 5);
        assert_eq!(svc.engine().state_hash(), hash);
        svc.apply_event(&scenario_events(6)[5]).unwrap();
        assert_eq!(svc.log_len(), 6);
    }

    #[test]
    fn other_storage_failures_are_not_read_only() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        feed(&mut svc, 4);
        rig.log.set_fail_appends(true);
        let err = svc.submit(EventBody::Tick).unwrap_err();
        assert!(matches!(&err, ServiceError::Storage(e) if e.kind == PortErrorKind::Io));
        assert!(!svc.is_read_only());
        assert!(matches!(
            ApiError::from(err),
            ApiError::Storage(m) if m.contains("storage")
        ));
        rig.log.set_fail_appends(false);
        svc.submit(EventBody::Tick).unwrap();
    }

    #[test]
    fn a_halted_engine_refuses_and_does_not_log() {
        let rig = Rig::new();
        let mut engine = run_scenario(10);
        engine.emergency_halt();
        let mut store = rig.snaps.clone();
        store.save(&engine).unwrap();
        let log = InMemoryEventLog::from_events(&scenario_events(10));
        let (mut svc, rep) = ExchangeService::open(
            log.clone(),
            store,
            rig.clock.clone(),
            ServiceConfig {
                snapshot_every: 1,
                keep_snapshots: 3,
                ..ServiceConfig::default()
            },
        )
        .unwrap();
        assert_eq!(rep.snapshot_seq, Some(10));
        assert!(svc.engine().is_halted());
        let ev = &scenario_events(11)[10];
        assert_eq!(
            svc.apply_event(ev),
            Err(ServiceError::Engine(Error::Halted))
        );
        assert_eq!(
            svc.submit(EventBody::Tick),
            Err(ServiceError::Engine(Error::Halted))
        );
        assert_eq!(log.len(), 10, "nothing was logged");
        assert_eq!(svc.engine().events_applied(), 10);
        assert_eq!(svc.snapshot_now(), Err(ServiceError::Engine(Error::Halted)));
        assert_eq!(rig.snaps.seqs(), vec![10], "no snapshot of a halted engine");
    }

    #[test]
    fn submit_stamps_with_the_clock_and_never_goes_backwards() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        rig.clock.set(1_000);
        svc.submit(EventBody::Configure(Default::default()))
            .unwrap();
        rig.clock.set(2_000);
        svc.submit(EventBody::Tick).unwrap();
        rig.clock.set(1_500); // clock moves backwards
        svc.submit(EventBody::Tick).unwrap();
        rig.clock.advance(10_000);
        svc.submit(EventBody::Tick).unwrap();
        let ts: Vec<u64> = rig.log.events().iter().map(|e| e.ts).collect();
        assert_eq!(ts, vec![1_000, 2_000, 2_000, 11_500]);
        assert_eq!(svc.last_ts(), 11_500);
    }

    #[test]
    fn submit_after_restart_continues_from_the_recovered_time() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        rig.clock.set(5_000);
        svc.submit(EventBody::Configure(Default::default()))
            .unwrap();
        rig.clock.set(9_000);
        svc.submit(EventBody::Tick).unwrap();
        drop(svc);
        // Restart with a clock that is behind the log.
        rig.clock.set(100);
        let (mut svc2, _) = rig.open(0, 3);
        assert_eq!(svc2.last_ts(), 9_000);
        svc2.submit(EventBody::Tick).unwrap();
        assert_eq!(rig.log.events().last().unwrap().ts, 9_000);
    }

    #[test]
    fn submit_after_restart_from_a_snapshot_uses_the_engine_clock() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(5, 3);
        rig.clock.set(7_000);
        svc.submit(EventBody::Configure(Default::default()))
            .unwrap();
        for _ in 0..4 {
            svc.submit(EventBody::Tick).unwrap();
        }
        assert_eq!(rig.snaps.seqs(), vec![5]);
        drop(svc);
        rig.clock.set(1);
        let (mut svc2, rep) = rig.open(5, 3);
        assert_eq!(rep.replayed, 0);
        assert_eq!(svc2.last_ts(), 7_000);
        svc2.submit(EventBody::Tick).unwrap();
        assert_eq!(rig.log.events().last().unwrap().ts, 7_000);
    }

    #[test]
    fn an_explicit_regressed_timestamp_is_rejected_and_not_logged() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        feed(&mut svc, 5);
        let late = Event {
            ts: 1,
            body: EventBody::Tick,
        };
        assert_eq!(
            svc.apply_event(&late),
            Err(ServiceError::Engine(Error::TimeRegression))
        );
        assert_eq!(svc.log_len(), 5);
        assert_eq!(svc.engine().events_applied(), 5);
        let (again, _) = replay_log(&mut rig.log.clone()).unwrap();
        assert_eq!(again.state_hash(), svc.engine().state_hash());
    }

    #[test]
    fn a_huge_forward_clock_jump_advances_time_by_one_step_per_event() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        rig.clock.set(1_000);
        svc.submit(EventBody::Configure(Default::default()))
            .unwrap();
        let step = ServiceConfig::default().max_time_step_ms;
        assert_eq!(step, 3_600_000);
        // NTP step / VM resume: the clock suddenly reads a year ahead.
        rig.clock.set(1_000 + 365 * 86_400_000);
        for k in 1..=4u64 {
            svc.submit(EventBody::Tick).unwrap();
            assert_eq!(svc.last_ts(), 1_000 + k * step, "event {k}");
        }
        let ts: Vec<u64> = rig.log.events().iter().map(|e| e.ts).collect();
        assert_eq!(
            ts,
            vec![
                1_000,
                1_000 + step,
                1_000 + 2 * step,
                1_000 + 3 * step,
                1_000 + 4 * step
            ]
        );
        // The clock comes back: time never regresses, and the engine follows
        // the clock again as soon as it passes the last event time.
        let last = svc.last_ts();
        rig.clock.set(5_000);
        svc.submit(EventBody::Tick).unwrap();
        assert_eq!(svc.last_ts(), last);
        rig.clock.set(last + 25);
        svc.submit(EventBody::Tick).unwrap();
        assert_eq!(svc.last_ts(), last + 25);
    }

    #[test]
    fn the_first_event_of_an_empty_service_uses_the_clock_as_is() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        rig.clock.set(9_000_000_000_000);
        svc.submit(EventBody::Configure(Default::default()))
            .unwrap();
        assert_eq!(svc.last_ts(), 9_000_000_000_000);
        // Later events are limited again.
        rig.clock.set(9_000_000_000_000 + 10 * 3_600_000);
        svc.submit(EventBody::Tick).unwrap();
        assert_eq!(svc.last_ts(), 9_000_000_000_000 + 3_600_000);
    }

    #[test]
    fn a_restart_after_a_long_downtime_catches_up_step_by_step() {
        let rig = Rig::new();
        let cfg = ServiceConfig {
            snapshot_every: 0,
            max_time_step_ms: 100,
            ..ServiceConfig::default()
        };
        let (mut svc, _) = rig.open_cfg(cfg);
        rig.clock.set(1_000);
        svc.submit(EventBody::Configure(Default::default()))
            .unwrap();
        drop(svc);
        // Down for far longer than one step.
        rig.clock.set(1_000 + 450);
        let (mut svc, _) = rig.open_cfg(cfg);
        let mut seen = Vec::new();
        for _ in 0..6 {
            svc.submit(EventBody::Tick).unwrap();
            seen.push(svc.last_ts());
        }
        assert_eq!(seen, vec![1_100, 1_200, 1_300, 1_400, 1_450, 1_450]);
    }

    #[test]
    fn a_zero_time_step_is_treated_as_one_millisecond() {
        let rig = Rig::new();
        let cfg = ServiceConfig {
            snapshot_every: 0,
            max_time_step_ms: 0,
            ..ServiceConfig::default()
        };
        let (mut svc, _) = rig.open_cfg(cfg);
        rig.clock.set(500);
        svc.submit(EventBody::Configure(Default::default()))
            .unwrap();
        rig.clock.set(1_000_000);
        svc.submit(EventBody::Tick).unwrap();
        svc.submit(EventBody::Tick).unwrap();
        assert_eq!(svc.last_ts(), 502);
    }

    #[test]
    fn a_failing_snapshot_does_not_fail_the_event_and_is_reported() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(5, 3);
        rig.snaps.set_fail_saves(true);
        feed(&mut svc, 5);
        assert_eq!(svc.engine().events_applied(), 5);
        let e = svc.last_snapshot_error().unwrap();
        assert_eq!(e.kind, PortErrorKind::Io);
        assert!(rig.snaps.seqs().is_empty());
        rig.snaps.set_fail_saves(false);
        feed_more(&mut svc, 5, 5);
        assert!(svc.last_snapshot_error().is_none());
        assert_eq!(rig.snaps.seqs(), vec![10]);
    }

    #[test]
    fn a_failing_log_sync_blocks_the_snapshot_and_is_reported() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(5, 3);
        rig.log.set_fail_syncs(true);
        feed(&mut svc, 5);
        assert!(svc.last_snapshot_error().is_some());
        assert!(
            rig.snaps.seqs().is_empty(),
            "no snapshot without a durable log"
        );
        assert!(matches!(svc.sync(), Err(ServiceError::Storage(_))));
        assert!(matches!(svc.snapshot_now(), Err(ServiceError::Storage(_))));
        rig.log.set_fail_syncs(false);
        svc.sync().unwrap();
        assert_eq!(svc.snapshot_now().unwrap(), 5);
    }

    #[test]
    fn storage_failures_during_open_are_reported() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(5, 3);
        feed(&mut svc, 7);
        drop(svc);
        rig.snaps.set_fail_loads(true);
        let r = ExchangeService::open(
            rig.log.clone(),
            rig.snaps.clone(),
            rig.clock.clone(),
            ServiceConfig::default(),
        );
        assert!(matches!(r, Err(ServiceError::Storage(_))));
        rig.snaps.set_fail_loads(false);
        rig.log.set_fail_replays(true);
        let r = ExchangeService::open(
            rig.log.clone(),
            rig.snaps.clone(),
            rig.clock.clone(),
            ServiceConfig::default(),
        );
        assert!(matches!(r, Err(ServiceError::Storage(_))));
    }

    #[test]
    fn recovery_reports_what_the_log_repaired() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(0, 3);
        feed(&mut svc, 4);
        drop(svc);
        rig.log.set_repaired_bytes(23);
        let (_, rep) = rig.open(0, 3);
        assert_eq!(rep.repaired_bytes, 23);
    }

    #[test]
    fn public_recover_matches_open() {
        let rig = Rig::new();
        let (mut svc, _) = rig.open(10, 3);
        feed(&mut svc, 33);
        let hash = svc.engine().state_hash();
        drop(svc);
        let (engine, rep) = recover(&mut rig.log.clone(), &mut rig.snaps.clone()).unwrap();
        assert_eq!(engine.state_hash(), hash);
        assert_eq!(rep.snapshot_seq, Some(30));
        assert_eq!(rep.replayed, 3);
        assert_eq!(rep.state_hash, hash);
    }

    #[test]
    fn replay_log_rebuilds_the_engine_without_touching_the_log() {
        let events = scenario_events(45);
        let mut log = InMemoryEventLog::from_events(&events);
        log.set_repaired_bytes(9);
        let head = log.head();
        let (engine, rep) = replay_log(&mut log).unwrap();
        assert_eq!(engine.state_hash(), run_scenario(45).state_hash());
        assert_eq!(
            rep,
            ReplayReport {
                records: 45,
                repaired_bytes: 9,
                halted: false,
                state_hash: engine.state_hash(),
            }
        );
        assert_eq!(log.head(), head);
        assert_eq!(log.events(), events);
    }

    #[test]
    fn replay_log_of_an_empty_log_is_a_fresh_engine() {
        let mut log = InMemoryEventLog::new();
        let (engine, rep) = replay_log(&mut log).unwrap();
        assert_eq!(rep.records, 0);
        assert_eq!(engine.state_hash(), Engine::new().state_hash());
    }

    /// A log that misreports what it holds, to exercise the consistency checks.
    struct LyingLog {
        inner: InMemoryEventLog,
        mode: Lie,
    }

    #[derive(Clone, Copy)]
    enum Lie {
        ShiftedSeq,
        DropLast,
        ExtraLen,
    }

    impl EventLog for LyingLog {
        fn len(&self) -> u64 {
            match self.mode {
                Lie::ExtraLen => self.inner.len() + 1,
                _ => self.inner.len(),
            }
        }
        fn append(&mut self, seq: u64, e: &Event) -> Result<(), PortError> {
            self.inner.append(seq, e)
        }
        fn sync(&mut self) -> Result<(), PortError> {
            self.inner.sync()
        }
        fn replay_from(
            &mut self,
            from: u64,
            visit: &mut dyn FnMut(u64, Event),
        ) -> Result<(), PortError> {
            let total = self.inner.len();
            let mode = self.mode;
            self.inner.replay_from(from, &mut |seq, ev| match mode {
                Lie::ShiftedSeq => visit(seq + 1, ev),
                Lie::DropLast if seq + 1 == total => {}
                _ => visit(seq, ev),
            })
        }
        fn repaired_bytes(&self) -> u64 {
            0
        }
        fn head(&self) -> Digest {
            self.inner.head()
        }
    }

    #[test]
    fn a_log_whose_sequence_numbers_do_not_follow_is_inconsistent() {
        for mode in [Lie::ShiftedSeq, Lie::DropLast, Lie::ExtraLen] {
            let mut log = LyingLog {
                inner: InMemoryEventLog::from_events(&scenario_events(6)),
                mode,
            };
            let err = replay_log(&mut log).map(|_| ()).unwrap_err();
            assert!(matches!(err, ServiceError::Inconsistent(_)), "{err}");
            let mut snaps = InMemorySnapshotStore::new();
            let r = recover(&mut log, &mut snaps);
            assert!(matches!(r, Err(ServiceError::Inconsistent(_))));
        }
    }

    /// A store that hands back a snapshot whose label disagrees with its engine.
    struct MislabelledStore;

    impl SnapshotStore for MislabelledStore {
        fn save(&mut self, e: &Engine) -> Result<u64, PortError> {
            Ok(e.events_applied())
        }
        fn load_latest(&mut self) -> Result<Option<LoadedSnapshot>, PortError> {
            Ok(Some(LoadedSnapshot {
                seq: 3,
                engine: run_scenario(5),
                skipped: 0,
            }))
        }
        fn prune(&mut self, _keep: usize) -> Result<(), PortError> {
            Ok(())
        }
    }

    #[test]
    fn a_mislabelled_snapshot_is_inconsistent() {
        let mut log = InMemoryEventLog::from_events(&scenario_events(8));
        let r = recover(&mut log, &mut MislabelledStore);
        assert!(matches!(r, Err(ServiceError::Inconsistent(_))));
    }

    #[test]
    fn exchange_api_submits_and_reads_through_the_service() {
        let rig = Rig::new();
        let (svc, _) = rig.open(0, 3);
        let mut api: Box<dyn ExchangeApi> = Box::new(svc);
        rig.clock.set(10);
        api.submit(EventBody::Configure(Default::default()))
            .unwrap();
        let out = api
            .submit(EventBody::Deposit {
                account: 7,
                amount: Money(2_500_000),
            })
            .unwrap();
        assert!(!out.is_empty());
        let acct = api.account(7).unwrap();
        assert_eq!(acct.collateral, Money(2_500_000));
        assert!(api.account(8).is_none());
        assert_eq!(api.summary().events_applied, 2);
        assert_eq!(api.summary().accounts, 1);
        assert_eq!(rig.log.len(), 2);
    }

    #[test]
    fn exchange_api_maps_errors() {
        let rig = Rig::new();
        let (svc, _) = rig.open(0, 3);
        let mut api: Box<dyn ExchangeApi> = Box::new(svc);
        // Not configured yet: rejected by the pre-check, nothing is logged.
        let e = api
            .submit(EventBody::Withdraw {
                account: 1,
                amount: Money(1),
            })
            .unwrap_err();
        assert!(matches!(e, ApiError::Engine(_)));
        assert_eq!(api.summary().events_applied, 0);
        api.submit(EventBody::Configure(Default::default()))
            .unwrap();
        // Unknown account: also a pre-check rejection.
        let e = api
            .submit(EventBody::Withdraw {
                account: 1,
                amount: Money(1),
            })
            .unwrap_err();
        assert_eq!(e, ApiError::Engine(Error::UnknownAccount));
        assert_eq!(api.summary().events_applied, 1);
        // Storage failure: not applied.
        rig.log.set_fail_appends(true);
        let e = api.submit(EventBody::Tick).unwrap_err();
        assert!(matches!(&e, ApiError::Storage(s) if s.contains("storage")));
        assert_eq!(api.summary().events_applied, 1);
    }

    #[test]
    fn service_error_display_and_conversion() {
        assert_eq!(
            ServiceError::Inconsistent("x").to_string(),
            "inconsistent state: x"
        );
        let s: ServiceError = PortError::new(PortErrorKind::Corrupt, "bad").into();
        assert_eq!(
            ServiceError::ReadOnly("disk".into()).to_string(),
            "read-only: disk"
        );
        assert_eq!(
            ApiError::from(ServiceError::ReadOnly("disk".into())),
            ApiError::Degraded("disk".into())
        );
        assert!(s.to_string().starts_with("storage: "));
        assert_eq!(
            ApiError::from(ServiceError::Engine(Error::Halted)),
            ApiError::Engine(Error::Halted)
        );
        assert!(matches!(
            ApiError::from(ServiceError::Inconsistent("y")),
            ApiError::Storage(m) if m.contains("inconsistent")
        ));
    }

    #[test]
    fn default_config_is_sensible() {
        let c = ServiceConfig::default();
        assert_eq!(c.snapshot_every, 10_000);
        assert_eq!(c.keep_snapshots, 3);
        assert_eq!(c.max_time_step_ms, 3_600_000);
    }
}
