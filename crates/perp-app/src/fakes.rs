//! In-memory implementations of the ports, for application tests and for the
//! contract suites in [`crate::contract`].
//!
//! The log and snapshot fakes are cheap-to-clone handles onto shared state, so a
//! test can keep one handle while the service owns another: "restarting the
//! process" is building a new service from clones of the same handles, and
//! failures can be injected while the service is running.
//!
//! [`InMemoryEventLog`] builds the same hash chain as the file adapter in
//! `perp-infra` (same genesis, same record layout), so the heads of the two
//! agree for the same events.

use crate::ports::{Clock, EventLog, LoadedSnapshot, PortError, PortErrorKind, SnapshotStore};
use perp_domain::codec::{Encode, Reader, Writer};
use perp_domain::engine::Engine;
use perp_domain::sha256::{sha256, sha256_concat, Digest};
use perp_domain::types::{Event, Millis};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn genesis() -> Digest {
    sha256(b"perp-engine wal v1 genesis")
}

fn injected(what: &str) -> PortError {
    PortError::new(PortErrorKind::Io, format!("injected {what} failure"))
}

#[derive(Default)]
struct LogState {
    records: Vec<Event>,
    chains: Vec<Digest>,
    capacity: Option<u64>,
    repaired: u64,
    syncs: u64,
    fail_append: bool,
    fail_sync: bool,
    fail_replay: bool,
}

impl LogState {
    fn head(&self) -> Digest {
        self.chains.last().copied().unwrap_or_else(genesis)
    }

    fn len(&self) -> u64 {
        self.records.len() as u64
    }
}

/// Event log held in memory. Clones share the same records.
#[derive(Clone, Default)]
pub struct InMemoryEventLog {
    inner: Arc<Mutex<LogState>>,
}

impl InMemoryEventLog {
    /// Empty log.
    pub fn new() -> InMemoryEventLog {
        InMemoryEventLog::default()
    }

    /// Empty log that refuses to hold more than `max_records` records
    /// (`PortErrorKind::Full`).
    pub fn with_capacity_limit(max_records: u64) -> InMemoryEventLog {
        let log = InMemoryEventLog::new();
        lock(&log.inner).capacity = Some(max_records);
        log
    }

    /// Log pre-filled with `events`.
    pub fn from_events(events: &[Event]) -> InMemoryEventLog {
        let mut log = InMemoryEventLog::new();
        for (i, ev) in events.iter().enumerate() {
            // A fresh log without limits or injected failures cannot refuse.
            let _ = log.append(i as u64, ev);
        }
        log
    }

    /// All stored events, oldest first.
    pub fn events(&self) -> Vec<Event> {
        lock(&self.inner).records.clone()
    }

    /// Number of `sync` calls that reached the log.
    pub fn sync_count(&self) -> u64 {
        lock(&self.inner).syncs
    }

    /// Value reported by [`EventLog::repaired_bytes`].
    pub fn set_repaired_bytes(&self, bytes: u64) {
        lock(&self.inner).repaired = bytes;
    }

    /// Make every `append` fail with an I/O error (nothing is stored).
    pub fn set_fail_appends(&self, on: bool) {
        lock(&self.inner).fail_append = on;
    }

    /// Make every `sync` fail with an I/O error.
    pub fn set_fail_syncs(&self, on: bool) {
        lock(&self.inner).fail_sync = on;
    }

    /// Make every `replay_from` fail with an I/O error.
    pub fn set_fail_replays(&self, on: bool) {
        lock(&self.inner).fail_replay = on;
    }

    /// Drop all records after the first `keep`, as if the tail had been lost.
    pub fn truncate(&self, keep: u64) {
        let mut st = lock(&self.inner);
        let n = usize::try_from(keep).unwrap_or(usize::MAX);
        st.records.truncate(n);
        st.chains.truncate(n);
    }
}

impl EventLog for InMemoryEventLog {
    fn len(&self) -> u64 {
        lock(&self.inner).len()
    }

    fn append(&mut self, seq: u64, event: &Event) -> Result<(), PortError> {
        let mut st = lock(&self.inner);
        if st.fail_append {
            return Err(injected("append"));
        }
        if seq != st.len() {
            return Err(PortError::new(
                PortErrorKind::Invalid,
                format!("sequence mismatch: expected {}, got {seq}", st.len()),
            ));
        }
        if st.capacity.is_some_and(|cap| st.len() >= cap) {
            return Err(PortError::new(PortErrorKind::Full, "log is full"));
        }
        let mut w = Writer::with_capacity(64);
        w.u64(seq);
        event.encode(&mut w);
        let payload = w.into_bytes();
        let len_le = u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes();
        let chain = sha256_concat(&[&st.head(), &len_le, &payload]);
        st.records.push(event.clone());
        st.chains.push(chain);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), PortError> {
        let mut st = lock(&self.inner);
        if st.fail_sync {
            return Err(injected("sync"));
        }
        st.syncs += 1;
        Ok(())
    }

    fn replay_from(
        &mut self,
        from_seq: u64,
        visit: &mut dyn FnMut(u64, Event),
    ) -> Result<(), PortError> {
        // Copy the tail out so the callback runs without the lock held.
        let tail: Vec<Event> = {
            let st = lock(&self.inner);
            if st.fail_replay {
                return Err(injected("replay"));
            }
            let skip = usize::try_from(from_seq).unwrap_or(usize::MAX);
            st.records.iter().skip(skip).cloned().collect()
        };
        for (seq, ev) in (from_seq..).zip(tail) {
            visit(seq, ev);
        }
        Ok(())
    }

    fn repaired_bytes(&self) -> u64 {
        lock(&self.inner).repaired
    }

    fn head(&self) -> Digest {
        lock(&self.inner).head()
    }
}

struct StoredSnapshot {
    seq: u64,
    hash: Digest,
    payload: Vec<u8>,
}

#[derive(Default)]
struct SnapshotState {
    snapshots: Vec<StoredSnapshot>,
    fail_save: bool,
    fail_load: bool,
}

/// Snapshot store held in memory. Snapshots are kept as encoded bytes and
/// validated on load exactly like a file would be. Clones share the same
/// snapshots.
#[derive(Clone, Default)]
pub struct InMemorySnapshotStore {
    inner: Arc<Mutex<SnapshotState>>,
}

impl InMemorySnapshotStore {
    /// Empty store.
    pub fn new() -> InMemorySnapshotStore {
        InMemorySnapshotStore::default()
    }

    /// Sequence numbers of the stored snapshots, oldest first.
    pub fn seqs(&self) -> Vec<u64> {
        lock(&self.inner).snapshots.iter().map(|s| s.seq).collect()
    }

    /// Make every `save` fail with an I/O error.
    pub fn set_fail_saves(&self, on: bool) {
        lock(&self.inner).fail_save = on;
    }

    /// Make every `load_latest` fail with an I/O error.
    pub fn set_fail_loads(&self, on: bool) {
        lock(&self.inner).fail_load = on;
    }

    /// Flip one bit in the newest snapshot so that it no longer validates.
    pub fn damage_newest(&self) {
        let mut st = lock(&self.inner);
        if let Some(s) = st.snapshots.last_mut() {
            match s.payload.first_mut() {
                Some(b) => *b ^= 0x01,
                None => s.hash[0] ^= 0x01,
            }
        }
    }
}

impl SnapshotStore for InMemorySnapshotStore {
    fn save(&mut self, engine: &Engine) -> Result<u64, PortError> {
        let mut st = lock(&self.inner);
        if st.fail_save {
            return Err(injected("save"));
        }
        let seq = engine.events_applied();
        let mut w = Writer::new_buf();
        engine.encode_state(&mut w);
        let stored = StoredSnapshot {
            seq,
            hash: engine.state_hash(),
            payload: w.into_bytes(),
        };
        st.snapshots.retain(|s| s.seq != seq);
        st.snapshots.push(stored);
        st.snapshots.sort_by_key(|s| s.seq);
        Ok(seq)
    }

    fn load_latest(&mut self) -> Result<Option<LoadedSnapshot>, PortError> {
        let st = lock(&self.inner);
        if st.fail_load {
            return Err(injected("load"));
        }
        let mut skipped = 0usize;
        for s in st.snapshots.iter().rev() {
            let mut r = Reader::new(&s.payload);
            let valid = Engine::decode_state(&mut r).ok().filter(|e| {
                r.remaining() == 0 && e.state_hash() == s.hash && e.events_applied() == s.seq
            });
            match valid {
                Some(engine) => {
                    return Ok(Some(LoadedSnapshot {
                        seq: s.seq,
                        engine,
                        skipped,
                    }))
                }
                None => skipped += 1,
            }
        }
        Ok(None)
    }

    fn prune(&mut self, keep: usize) -> Result<(), PortError> {
        let mut st = lock(&self.inner);
        let drop_n = st.snapshots.len().saturating_sub(keep);
        st.snapshots.drain(..drop_n);
        Ok(())
    }
}

/// Clock whose time is set by the test. Clones share the same time.
#[derive(Clone, Default)]
pub struct ManualClock {
    now: Arc<AtomicU64>,
}

impl ManualClock {
    /// Clock showing `start_ms`.
    pub fn new(start_ms: Millis) -> ManualClock {
        ManualClock {
            now: Arc::new(AtomicU64::new(start_ms)),
        }
    }

    /// Set the time (it may move backwards).
    pub fn set(&self, ms: Millis) {
        self.now.store(ms, Ordering::SeqCst);
    }

    /// Move the time forward by `delta_ms`, saturating.
    pub fn advance(&self, delta_ms: Millis) {
        let cur = self.now.load(Ordering::SeqCst);
        self.now
            .store(cur.saturating_add(delta_ms), Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> Millis {
        self.now.load(Ordering::SeqCst)
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

    #[test]
    fn log_chain_follows_the_documented_construction() {
        let evs = scenario_events(6);
        let mut log = InMemoryEventLog::new();
        let mut expected = sha256(b"perp-engine wal v1 genesis");
        assert_eq!(log.head(), expected);
        for (i, ev) in evs.iter().enumerate() {
            log.append(i as u64, ev).unwrap();
            let mut payload = (i as u64).to_le_bytes().to_vec();
            payload.extend_from_slice(&perp_domain::codec::to_bytes(ev));
            let len = (payload.len() as u32).to_le_bytes();
            expected = sha256_concat(&[&expected, &len, &payload]);
            assert_eq!(log.head(), expected, "record {i}");
        }
    }

    #[test]
    fn clones_share_one_log() {
        let a = InMemoryEventLog::new();
        let mut b = a.clone();
        b.append(0, &scenario_events(1)[0]).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a.events(), scenario_events(1));
    }

    #[test]
    fn capacity_limit_reports_full_and_stores_nothing() {
        let mut log = InMemoryEventLog::with_capacity_limit(2);
        let evs = scenario_events(3);
        log.append(0, &evs[0]).unwrap();
        log.append(1, &evs[1]).unwrap();
        let head = log.head();
        let err = log.append(2, &evs[2]).unwrap_err();
        assert_eq!(err.kind, PortErrorKind::Full);
        assert_eq!(log.len(), 2);
        assert_eq!(log.head(), head);
    }

    #[test]
    fn injected_failures_are_io_errors_and_can_be_cleared() {
        let mut log = InMemoryEventLog::new();
        let ev = &scenario_events(1)[0];
        log.set_fail_appends(true);
        assert_eq!(log.append(0, ev).unwrap_err().kind, PortErrorKind::Io);
        assert_eq!(log.len(), 0);
        log.set_fail_appends(false);
        log.append(0, ev).unwrap();
        log.set_fail_syncs(true);
        assert_eq!(log.sync().unwrap_err().kind, PortErrorKind::Io);
        assert_eq!(log.sync_count(), 0);
        log.set_fail_syncs(false);
        log.sync().unwrap();
        assert_eq!(log.sync_count(), 1);
        log.set_fail_replays(true);
        assert_eq!(
            log.replay_from(0, &mut |_, _| {}).unwrap_err().kind,
            PortErrorKind::Io
        );
        log.set_fail_replays(false);
        assert!(log.replay_from(0, &mut |_, _| {}).is_ok());
    }

    #[test]
    fn truncate_drops_the_tail_and_restores_the_earlier_head() {
        let evs = scenario_events(5);
        let mut log = InMemoryEventLog::new();
        let mut heads = Vec::new();
        for (i, ev) in evs.iter().enumerate() {
            log.append(i as u64, ev).unwrap();
            heads.push(log.head());
        }
        log.truncate(3);
        assert_eq!(log.len(), 3);
        assert_eq!(log.head(), heads[2]);
        log.truncate(0);
        assert!(log.is_empty());
        assert_eq!(log.head(), genesis());
    }

    #[test]
    fn from_events_equals_appending_one_by_one() {
        let evs = scenario_events(9);
        let a = InMemoryEventLog::from_events(&evs);
        let mut b = InMemoryEventLog::new();
        for (i, ev) in evs.iter().enumerate() {
            b.append(i as u64, ev).unwrap();
        }
        assert_eq!(a.head(), b.head());
        assert_eq!(a.events(), evs);
    }

    #[test]
    fn repaired_bytes_is_settable() {
        let log = InMemoryEventLog::new();
        assert_eq!(log.repaired_bytes(), 0);
        log.set_repaired_bytes(17);
        assert_eq!(log.repaired_bytes(), 17);
    }

    #[test]
    fn snapshot_store_replaces_same_seq_and_orders_by_seq() {
        let mut store = InMemorySnapshotStore::new();
        for n in [20u64, 10, 20, 30] {
            store.save(&run_scenario(n)).unwrap();
        }
        assert_eq!(store.seqs(), vec![10, 20, 30]);
        store.prune(2).unwrap();
        assert_eq!(store.seqs(), vec![20, 30]);
        store.prune(5).unwrap();
        assert_eq!(store.seqs(), vec![20, 30]);
    }

    #[test]
    fn damaged_snapshot_is_skipped_and_counted() {
        let mut store = InMemorySnapshotStore::new();
        store.save(&run_scenario(10)).unwrap();
        store.save(&run_scenario(20)).unwrap();
        store.damage_newest();
        let l = store.load_latest().unwrap().unwrap();
        assert_eq!((l.seq, l.skipped), (10, 1));
        store.prune(1).unwrap(); // keeps only the damaged one
        assert!(store.load_latest().unwrap().is_none());
    }

    #[test]
    fn snapshot_failures_are_injectable() {
        let mut store = InMemorySnapshotStore::new();
        store.set_fail_saves(true);
        assert_eq!(
            store.save(&run_scenario(3)).unwrap_err().kind,
            PortErrorKind::Io
        );
        assert!(store.seqs().is_empty());
        store.set_fail_saves(false);
        store.save(&run_scenario(3)).unwrap();
        store.set_fail_loads(true);
        assert!(store.load_latest().is_err());
        store.set_fail_loads(false);
        assert!(store.load_latest().unwrap().is_some());
    }

    #[test]
    fn manual_clock_is_shared_and_can_go_backwards() {
        let c = ManualClock::new(100);
        let c2 = c.clone();
        assert_eq!(c.now_ms(), 100);
        c2.advance(50);
        assert_eq!(c.now_ms(), 150);
        c.set(7);
        assert_eq!(c2.now_ms(), 7);
        c.set(u64::MAX);
        c.advance(5);
        assert_eq!(c.now_ms(), u64::MAX);
    }
}
