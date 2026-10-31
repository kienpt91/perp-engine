//! Ports: the interfaces the application layer needs from the outside world.
//!
//! Adapters in `perp-infra` (files, system clock) and the in-memory fakes in
//! [`crate::fakes`] implement these traits. The shared contract suites in
//! [`crate::contract`] are run against every implementation.

use perp_domain::engine::Engine;
use perp_domain::sha256::Digest;
use perp_domain::types::{Event, Millis};
use std::fmt;

/// Coarse classification of an adapter failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortErrorKind {
    /// Operating-system or device failure.
    Io,
    /// Stored data failed validation (bad checksum, impossible length, ...).
    Corrupt,
    /// A configured size limit was reached.
    Full,
    /// The caller broke the port contract (for example a sequence gap).
    Invalid,
}

/// Error returned by every port operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortError {
    /// What kind of failure this is.
    pub kind: PortErrorKind,
    /// Human-readable detail. Never contains secrets.
    pub message: String,
}

impl PortError {
    /// Build an error.
    pub fn new(kind: PortErrorKind, message: impl Into<String>) -> PortError {
        PortError {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for PortError {}

/// Append-only, totally ordered log of events (the write-ahead log).
pub trait EventLog {
    /// Number of records stored; also the sequence number of the next append.
    fn len(&self) -> u64;

    /// True when no record is stored.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Append one event. `seq` must equal [`EventLog::len`], otherwise the
    /// call fails with [`PortErrorKind::Invalid`] and nothing is stored.
    fn append(&mut self, seq: u64, event: &Event) -> Result<(), PortError>;

    /// Make everything appended so far durable (as far as the medium allows).
    fn sync(&mut self) -> Result<(), PortError>;

    /// Stream records with sequence number `>= from_seq` in order. Memory use
    /// must not grow with the number of records.
    fn replay_from(
        &mut self,
        from_seq: u64,
        visit: &mut dyn FnMut(u64, Event),
    ) -> Result<(), PortError>;

    /// Bytes of a torn tail that were discarded when the log was opened.
    fn repaired_bytes(&self) -> u64;

    /// Integrity-chain head after the last record (all zero for logs that
    /// have no chain).
    fn head(&self) -> Digest;
}

/// A snapshot read back from a [`SnapshotStore`].
pub struct LoadedSnapshot {
    /// Number of events the snapshotted engine had applied.
    pub seq: u64,
    /// The restored engine.
    pub engine: Engine,
    /// Newer snapshots that were skipped because they failed validation.
    pub skipped: usize,
}

/// Store of periodic engine snapshots.
pub trait SnapshotStore {
    /// Persist a snapshot of `engine`; returns its sequence number
    /// (`engine.events_applied()`).
    fn save(&mut self, engine: &Engine) -> Result<u64, PortError>;

    /// Newest snapshot that validates, if any. Invalid newer ones are skipped
    /// and counted, never returned.
    fn load_latest(&mut self) -> Result<Option<LoadedSnapshot>, PortError>;

    /// Keep only the `keep` newest snapshots.
    fn prune(&mut self, keep: usize) -> Result<(), PortError>;
}

/// Source of wall-clock time in milliseconds. The domain never reads a clock;
/// the application stamps events with this port.
pub trait Clock {
    /// Milliseconds since an arbitrary fixed epoch.
    fn now_ms(&self) -> Millis;
}
