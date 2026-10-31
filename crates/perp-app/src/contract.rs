//! Contract suites for the ports, plus the sample event stream they use.
//!
//! Each suite is a plain generic function that takes a factory for fresh
//! instances of an implementation and asserts the semantics documented on the
//! port trait. The same function runs against the in-memory fakes (in this
//! crate's tests) and against the file adapters (in `perp-infra`'s tests), so
//! the fake and the real adapter are held to one definition of the port.
//!
//! This module is test tooling: it reports a violation by panicking, which is
//! why the crate-wide lints against `unwrap`, `expect` and `panic` are relaxed
//! here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use crate::ports::{EventLog, PortErrorKind, SnapshotStore};
use perp_domain::engine::Engine;
use perp_domain::fixed::Money;
use perp_domain::types::{EngineConfig, Event, EventBody};

/// A deterministic event stream of `n` events that any engine accepts or
/// rejects the same way on every run: one `Configure` that allows only three
/// accounts, then a repeating pattern of deposits and ticks. Deposits that
/// would open a fourth account are rejected by the engine for a reason the
/// engine's pre-check cannot see, so the stream contains legitimately rejected
/// events that a service still logs. Every event passes the pre-check, and
/// timestamps never decrease.
pub fn scenario_events(n: u64) -> Vec<Event> {
    (0..n)
        .map(|i| {
            let body = if i == 0 {
                EventBody::Configure(EngineConfig {
                    max_accounts: 3,
                    ..EngineConfig::default()
                })
            } else {
                match (i - 1) % 5 {
                    0 | 2 => EventBody::Deposit {
                        account: 1 + i % 5,
                        amount: Money(1_000_000 + i128::from(i)),
                    },
                    3 => EventBody::Deposit {
                        account: 1_000 + i,
                        amount: Money(1),
                    },
                    _ => EventBody::Tick,
                }
            };
            Event { ts: i * 10, body }
        })
        .collect()
}

/// A fresh engine after applying the first `n` events of [`scenario_events`].
pub fn run_scenario(n: u64) -> Engine {
    let mut engine = Engine::new();
    for ev in scenario_events(n) {
        // Rejections are part of the stream; only the resulting state matters.
        let _ = engine.apply(&ev);
    }
    engine
}

fn collect(log: &mut dyn EventLog, from: u64) -> Vec<(u64, Event)> {
    let mut out = Vec::new();
    log.replay_from(from, &mut |seq, ev| out.push((seq, ev)))
        .unwrap();
    out
}

/// Semantics every [`EventLog`] must have. `make` returns a new, empty,
/// independent log each time it is called.
///
/// Checked: a new log is empty and unrepaired; appends advance `len` and change
/// `head`; the head depends only on the appended events; a wrong sequence number
/// is rejected as `Invalid` without any change; `replay_from` streams exactly the
/// records at or after the requested sequence number, in order, repeatably;
/// `sync` succeeds.
pub fn event_log_contract<L: EventLog>(make: &mut dyn FnMut() -> L) {
    let events = scenario_events(40);
    let mut log = make();
    assert!(log.is_empty(), "new log must be empty");
    assert_eq!(log.len(), 0);
    assert_eq!(log.repaired_bytes(), 0, "new log has nothing repaired");
    assert!(collect(&mut log, 0).is_empty(), "replay of an empty log");
    log.sync().expect("sync of an empty log");

    let empty_head = log.head();
    let mut seen_heads = vec![empty_head];
    for (i, ev) in events.iter().enumerate() {
        let before = log.head();
        log.append(i as u64, ev).expect("append in sequence");
        assert_eq!(log.len(), i as u64 + 1, "len after append {i}");
        assert!(!log.is_empty());
        assert_ne!(log.head(), before, "head must change on append {i}");
        assert!(
            !seen_heads.contains(&log.head()),
            "head must not repeat (append {i})"
        );
        seen_heads.push(log.head());
    }

    // Wrong sequence numbers: nothing changes.
    let len = log.len();
    let head = log.head();
    for bad in [len + 1, len + 100, len - 1, 0, u64::MAX] {
        let err = log.append(bad, &events[0]).expect_err("bad seq must fail");
        assert_eq!(err.kind, PortErrorKind::Invalid, "seq {bad}");
        assert_eq!(log.len(), len, "len unchanged after bad seq {bad}");
        assert_eq!(log.head(), head, "head unchanged after bad seq {bad}");
    }

    // Replay.
    let all = collect(&mut log, 0);
    assert_eq!(all.len(), events.len());
    for (i, (seq, ev)) in all.iter().enumerate() {
        assert_eq!(*seq, i as u64, "replayed seq");
        assert_eq!(ev, &events[i], "replayed event {i}");
    }
    assert_eq!(collect(&mut log, 0), all, "replay must be repeatable");
    for from in [1u64, 17, 39] {
        let tail = collect(&mut log, from);
        assert_eq!(tail.len() as u64, len - from, "replay from {from}");
        assert_eq!(tail.first().map(|(s, _)| *s), Some(from));
        assert_eq!(tail.as_slice(), &all[from as usize..]);
    }
    assert!(collect(&mut log, len).is_empty(), "replay from len");
    assert!(collect(&mut log, len + 7).is_empty(), "replay past the end");
    log.sync().expect("sync");
    assert_eq!(collect(&mut log, 0), all, "sync must not change contents");
    assert_eq!(log.len(), len);
    assert_eq!(log.head(), head);

    // The head is a function of the events only.
    let mut other = make();
    for (i, ev) in events.iter().enumerate() {
        other.append(i as u64, ev).unwrap();
    }
    assert_eq!(other.head(), head, "same events, same head");
    assert_eq!(other.len(), len);
    let mut diverged = make();
    let mut changed = events.clone();
    changed[3].ts += 1;
    for (i, ev) in changed.iter().enumerate() {
        diverged.append(i as u64, ev).unwrap();
    }
    assert_ne!(diverged.head(), head, "different events, different head");

    // Appending after a rejected append works normally.
    let mut fresh = make();
    assert!(fresh.append(1, &events[0]).is_err());
    fresh.append(0, &events[0]).unwrap();
    assert_eq!(fresh.len(), 1);
}

/// What every [`EventLog`] must do when it cannot hold more records. `make_small`
/// returns a new, empty log that can hold at least one and fewer than 5 000 of
/// the [`scenario_events`].
///
/// Checked: the refusal is [`PortErrorKind::Full`]; it stores nothing (length,
/// head and replay are exactly those before the refused append) and repeats on
/// every retry; the records stored before it stay readable.
pub fn event_log_full_contract<L: EventLog>(make_small: &mut dyn FnMut() -> L) {
    let events = scenario_events(5_000);
    let mut log = make_small();
    let mut stored = 0u64;
    let mut refused = None;
    for (i, ev) in events.iter().enumerate() {
        match log.append(i as u64, ev) {
            Ok(()) => stored += 1,
            Err(e) => {
                refused = Some((i, e));
                break;
            }
        }
    }
    let (at, err) = refused.expect("a small log must fill up");
    assert!(stored >= 1, "the small log must hold at least one record");
    assert_eq!(at as u64, stored);
    assert_eq!(err.kind, PortErrorKind::Full, "{err}");
    assert_eq!(log.len(), stored, "a refused append stores nothing");
    let head = log.head();
    for _ in 0..3 {
        assert_eq!(
            log.append(stored, &events[at]).unwrap_err().kind,
            PortErrorKind::Full
        );
        assert_eq!(log.len(), stored);
        assert_eq!(log.head(), head);
    }
    let got: Vec<_> = collect(&mut log, 0).into_iter().map(|(_, e)| e).collect();
    assert_eq!(got, events[..stored as usize].to_vec());
    log.sync().unwrap();
}

/// Semantics every [`SnapshotStore`] must have. `make` returns a new, empty,
/// independent store each time it is called.
///
/// Checked: an empty store has no snapshot; `save` returns the engine's event
/// count; `load_latest` returns the newest snapshot with identical state, which
/// keeps evolving identically to the original; saving the same sequence twice is
/// harmless; `prune` keeps only the newest snapshots.
pub fn snapshot_store_contract<S: SnapshotStore>(make: &mut dyn FnMut() -> S) {
    let mut store = make();
    assert!(store.load_latest().unwrap().is_none(), "empty store");
    store.prune(3).expect("prune of an empty store");

    let e5 = run_scenario(5);
    assert_eq!(store.save(&e5).unwrap(), e5.events_applied());
    let l = store.load_latest().unwrap().expect("saved snapshot");
    assert_eq!(l.seq, 5);
    assert_eq!(l.skipped, 0);
    assert_eq!(l.engine.events_applied(), 5);
    assert_eq!(l.engine.state_hash(), e5.state_hash());
    assert_eq!(l.engine.summary(), e5.summary());

    // A restored engine continues exactly like the original.
    let mut restored = l.engine;
    let mut original = e5.clone();
    for ev in scenario_events(25).iter().skip(5) {
        assert_eq!(restored.apply(ev), original.apply(ev));
    }
    assert_eq!(restored.state_hash(), original.state_hash());

    // Same sequence saved twice, then newer ones.
    store.save(&e5).unwrap();
    assert_eq!(store.load_latest().unwrap().unwrap().seq, 5);
    for n in [10u64, 20, 30] {
        let e = run_scenario(n);
        assert_eq!(store.save(&e).unwrap(), n);
        let l = store.load_latest().unwrap().unwrap();
        assert_eq!(l.seq, n, "newest snapshot wins");
        assert_eq!(l.engine.state_hash(), e.state_hash());
    }

    // Pruning keeps the newest.
    store.prune(10).unwrap();
    assert_eq!(store.load_latest().unwrap().unwrap().seq, 30);
    store.prune(2).unwrap();
    assert_eq!(store.load_latest().unwrap().unwrap().seq, 30);
    store.prune(1).unwrap();
    let l = store.load_latest().unwrap().unwrap();
    assert_eq!(l.seq, 30);
    assert_eq!(l.engine.state_hash(), run_scenario(30).state_hash());
    store.prune(0).unwrap();
    assert!(
        store.load_latest().unwrap().is_none(),
        "prune(0) removes all"
    );
}

/// A damaged newest snapshot must be skipped and counted, never returned.
/// `damage_newest` corrupts the newest stored snapshot in an
/// implementation-specific way (it must make the snapshot invalid).
pub fn snapshot_store_skips_damaged_contract<S: SnapshotStore>(
    make: &mut dyn FnMut() -> S,
    damage_newest: &mut dyn FnMut(&mut S),
) {
    let mut store = make();
    for n in [10u64, 20, 30] {
        store.save(&run_scenario(n)).unwrap();
    }
    damage_newest(&mut store);
    let l = store
        .load_latest()
        .unwrap()
        .expect("older snapshot remains");
    assert_eq!(l.seq, 20, "damaged newest must be skipped");
    assert_eq!(l.skipped, 1);
    assert_eq!(l.engine.state_hash(), run_scenario(20).state_hash());

    // A valid snapshot newer than the damaged one wins and nothing is skipped.
    store.save(&run_scenario(40)).unwrap();
    let l = store.load_latest().unwrap().unwrap();
    assert_eq!((l.seq, l.skipped), (40, 0));

    // Damaging the only snapshot left behind gives "none".
    store.prune(1).unwrap();
    damage_newest(&mut store);
    let l = store.load_latest().unwrap();
    assert!(l.is_none(), "no valid snapshot remains");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{InMemoryEventLog, InMemorySnapshotStore};

    #[test]
    fn in_memory_event_log_satisfies_the_contract() {
        event_log_contract(&mut InMemoryEventLog::new);
    }

    #[test]
    fn in_memory_snapshot_store_satisfies_the_contract() {
        snapshot_store_contract(&mut InMemorySnapshotStore::new);
    }

    #[test]
    fn in_memory_snapshot_store_skips_damaged_snapshots() {
        snapshot_store_skips_damaged_contract(&mut InMemorySnapshotStore::new, &mut |s| {
            s.damage_newest();
        });
    }

    #[test]
    fn in_memory_log_satisfies_the_full_contract() {
        event_log_full_contract(&mut || InMemoryEventLog::with_capacity_limit(4));
    }

    #[test]
    fn scenario_is_deterministic_has_logged_rejections_and_no_precheck_rejections() {
        let a = scenario_events(30);
        assert_eq!(a, scenario_events(30));
        assert!(matches!(a[0].body, EventBody::Configure(_)));
        assert!(a.windows(2).all(|w| w[0].ts <= w[1].ts));
        let mut engine = Engine::new();
        let mut rejected = 0;
        for ev in &a {
            engine.precheck(ev).unwrap();
            if engine.apply(ev).is_err() {
                rejected += 1;
            }
        }
        assert!(rejected > 0, "the engine rejects some events");
        assert_eq!(run_scenario(30).events_applied(), 30);
        assert_eq!(run_scenario(30).state_hash(), run_scenario(30).state_hash());
        assert_ne!(run_scenario(30).state_hash(), run_scenario(29).state_hash());
    }

    /// The suites must reject implementations that break the port contract.
    mod broken {
        use super::*;
        use crate::ports::PortError;
        use perp_domain::sha256::Digest;

        /// A log that accepts any sequence number.
        struct LaxLog(InMemoryEventLog, u64);

        impl EventLog for LaxLog {
            fn len(&self) -> u64 {
                self.0.len()
            }
            fn append(&mut self, _seq: u64, event: &Event) -> Result<(), PortError> {
                self.0.append(self.0.len(), event)
            }
            fn sync(&mut self) -> Result<(), PortError> {
                self.0.sync()
            }
            fn replay_from(
                &mut self,
                from: u64,
                visit: &mut dyn FnMut(u64, Event),
            ) -> Result<(), PortError> {
                self.0.replay_from(from, visit)
            }
            fn repaired_bytes(&self) -> u64 {
                self.1
            }
            fn head(&self) -> Digest {
                self.0.head()
            }
        }

        #[test]
        #[should_panic(expected = "bad seq must fail")]
        fn lax_sequence_checking_is_detected() {
            event_log_contract(&mut || LaxLog(InMemoryEventLog::new(), 0));
        }

        #[test]
        #[should_panic(expected = "nothing repaired")]
        fn a_new_log_reporting_repairs_is_detected() {
            event_log_contract(&mut || LaxLog(InMemoryEventLog::new(), 9));
        }

        /// A store that never keeps anything.
        struct ForgetfulStore;

        impl SnapshotStore for ForgetfulStore {
            fn save(&mut self, e: &Engine) -> Result<u64, PortError> {
                Ok(e.events_applied())
            }
            fn load_latest(&mut self) -> Result<Option<crate::ports::LoadedSnapshot>, PortError> {
                Ok(None)
            }
            fn prune(&mut self, _keep: usize) -> Result<(), PortError> {
                Ok(())
            }
        }

        #[test]
        #[should_panic(expected = "saved snapshot")]
        fn a_store_that_forgets_is_detected() {
            snapshot_store_contract(&mut || ForgetfulStore);
        }
    }
}
