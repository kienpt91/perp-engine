//! Engine state snapshots: whole-state files that let recovery skip most of the
//! write-ahead log.
//!
//! # File format
//!
//! Files live in one directory and are named `snapshot-<seq as 20 digits>.bin`
//! (strict parse; other names are ignored). `seq` is the number of events the
//! engine had applied.
//!
//! ```text
//! "PERPSNP1" (8) | version u32 LE | seq u64 LE | state_hash [u8; 32]
//! | payload_len u64 LE | payload (Engine::encode_state) | trailer [u8; 32]
//! ```
//!
//! The trailer is SHA-256 over every byte before it. Loading checks, in order:
//! file size against [`MAX_SNAPSHOT_BYTES`], magic and version, that the file
//! size is exactly `60 + payload_len + 32`, the trailer, a strict decode of the
//! payload that consumes every payload byte, that the decoded engine's
//! `state_hash()` equals the stored hash and that its `events_applied()` equals
//! `seq`. Sizes are checked before any buffer is allocated, so a hostile length
//! field cannot cause a large allocation.
//!
//! # Writing
//!
//! A snapshot is written to `<name>.tmp` in the same directory, synced, renamed
//! to its final name and the directory is synced. A crash leaves either no
//! snapshot or a complete one; a leftover `.tmp` file is never listed or loaded
//! and is removed by [`prune`].
//!
//! # Port adapter
//!
//! [`FileSnapshotStore`] implements `perp_app::ports::SnapshotStore` over a
//! directory using the functions below.
//!
//! # Limits
//!
//! The snapshot is not bound to a particular log: nothing stops a snapshot from
//! one run being used with the log of another. The hash and the trailer detect
//! damage, not substitution. The payload is held in memory while reading and
//! writing (at most [`MAX_SNAPSHOT_BYTES`]).

use crate::wal::sync_dir;
use perp_app::ports::{LoadedSnapshot, PortError, PortErrorKind, SnapshotStore};
use perp_domain::codec::{DecodeError, Reader, Writer};
use perp_domain::engine::Engine;
use perp_domain::sha256::{ct_eq, Digest, Sha256};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// File magic.
pub const SNAPSHOT_MAGIC: [u8; 8] = *b"PERPSNP1";
/// File format version.
pub const SNAPSHOT_VERSION: u32 = 1;
/// Largest accepted snapshot file in bytes.
pub const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;

const HEADER_LEN: usize = 8 + 4 + 8 + 32 + 8;
const TRAILER_LEN: usize = 32;
const NAME_PREFIX: &str = "snapshot-";
const NAME_SUFFIX: &str = ".bin";
const TEMP_SUFFIX: &str = ".tmp";
const SEQ_DIGITS: usize = 20;

/// Errors of snapshot reading and writing.
#[derive(Debug)]
pub enum SnapshotError {
    /// Underlying I/O failure.
    Io(io::Error),
    /// The file is damaged or inconsistent.
    Corrupt(&'static str),
    /// The file or its claimed payload exceeds the size limit.
    TooLarge,
    /// The payload is not a valid engine state.
    Decode(DecodeError),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Io(e) => write!(f, "snapshot i/o error: {e}"),
            SnapshotError::Corrupt(why) => write!(f, "snapshot corrupt: {why}"),
            SnapshotError::TooLarge => write!(f, "snapshot exceeds the size limit"),
            SnapshotError::Decode(e) => write!(f, "snapshot payload invalid: {e}"),
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SnapshotError::Io(e) => Some(e),
            SnapshotError::Decode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for SnapshotError {
    fn from(e: io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

impl From<DecodeError> for SnapshotError {
    fn from(e: DecodeError) -> Self {
        SnapshotError::Decode(e)
    }
}

/// A snapshot file found in a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    /// Sequence number taken from the file name.
    pub seq: u64,
    /// Full path.
    pub path: PathBuf,
}

/// Result of [`load_latest`].
pub struct LatestSnapshot {
    /// Sequence number and engine of the newest valid snapshot, if any.
    pub snapshot: Option<(u64, Engine)>,
    /// Newer snapshot files that were skipped because they failed to load.
    pub skipped: usize,
}

impl fmt::Debug for LatestSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LatestSnapshot")
            .field("seq", &self.snapshot.as_ref().map(|(s, _)| *s))
            .field("skipped", &self.skipped)
            .finish()
    }
}

/// File name of the snapshot with sequence number `seq`.
pub fn snapshot_file_name(seq: u64) -> String {
    format!(
        "{NAME_PREFIX}{seq:0width$}{NAME_SUFFIX}",
        width = SEQ_DIGITS
    )
}

/// Parse `snapshot-<20 digits>.bin`; anything else is `None`.
pub fn parse_snapshot_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix(NAME_PREFIX)?.strip_suffix(NAME_SUFFIX)?;
    if digits.len() != SEQ_DIGITS || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

fn temp_name(seq: u64) -> String {
    format!("{}{TEMP_SUFFIX}", snapshot_file_name(seq))
}

fn is_temp_name(name: &str) -> bool {
    name.strip_suffix(TEMP_SUFFIX)
        .is_some_and(|base| parse_snapshot_name(base).is_some())
}

fn header_bytes(seq: u64, hash: &Digest, payload_len: u64) -> [u8; HEADER_LEN] {
    let mut w = Writer::with_capacity(HEADER_LEN);
    w.bytes(&SNAPSHOT_MAGIC);
    w.u32(SNAPSHOT_VERSION);
    w.u64(seq);
    w.bytes(hash);
    w.u64(payload_len);
    let v = w.into_bytes();
    let mut out = [0u8; HEADER_LEN];
    if v.len() == HEADER_LEN {
        out.copy_from_slice(&v);
    }
    out
}

fn trailer_digest(header: &[u8], payload: &[u8]) -> Digest {
    let mut h = Sha256::new();
    h.update(header);
    h.update(payload);
    h.finalize()
}

/// Write a snapshot of `engine` into `dir` (created if missing) and return its
/// path. The sequence number is `engine.events_applied()`. Writing is atomic:
/// temp file, sync, rename, directory sync.
pub fn write_snapshot(dir: impl AsRef<Path>, engine: &Engine) -> Result<PathBuf, SnapshotError> {
    let dir = dir.as_ref();
    let seq = engine.events_applied();
    let mut w = Writer::new_buf();
    engine.encode_state(&mut w);
    let payload = w.into_bytes();
    let total = (HEADER_LEN as u64)
        .checked_add(payload.len() as u64)
        .and_then(|n| n.checked_add(TRAILER_LEN as u64))
        .ok_or(SnapshotError::TooLarge)?;
    if total > MAX_SNAPSHOT_BYTES {
        return Err(SnapshotError::TooLarge);
    }
    let header = header_bytes(seq, &engine.state_hash(), payload.len() as u64);
    let trailer = trailer_digest(&header, &payload);

    fs::create_dir_all(dir)?;
    let final_path = dir.join(snapshot_file_name(seq));
    let tmp_path = dir.join(temp_name(seq));
    let written = (|| -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        f.write_all(&header)?;
        f.write_all(&payload)?;
        f.write_all(&trailer)?;
        f.sync_all()?;
        fs::rename(&tmp_path, &final_path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp_path);
        return Err(SnapshotError::Io(e));
    }
    sync_dir(dir)?;
    Ok(final_path)
}

/// List the snapshot files in `dir`, oldest first. A missing directory is an
/// empty list; names that do not parse strictly and non-files are ignored.
pub fn list_snapshots(dir: impl AsRef<Path>) -> Result<Vec<SnapshotEntry>, SnapshotError> {
    let rd = match fs::read_dir(dir.as_ref()) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry?;
        let name = entry.file_name();
        let Some(seq) = name.to_str().and_then(parse_snapshot_name) else {
            continue;
        };
        if entry.file_type()?.is_file() {
            out.push(SnapshotEntry {
                seq,
                path: entry.path(),
            });
        }
    }
    out.sort_by_key(|e| e.seq);
    Ok(out)
}

/// Load and fully verify one snapshot file; returns `(seq, engine)`.
pub fn load_snapshot(path: impl AsRef<Path>) -> Result<(u64, Engine), SnapshotError> {
    load_with_limit(path.as_ref(), MAX_SNAPSHOT_BYTES)
}

fn load_with_limit(path: &Path, max_bytes: u64) -> Result<(u64, Engine), SnapshotError> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len > max_bytes {
        return Err(SnapshotError::TooLarge);
    }
    if file_len < (HEADER_LEN + TRAILER_LEN) as u64 {
        return Err(SnapshotError::Corrupt("file too short"));
    }
    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header)?;
    let mut r = Reader::new(&header);
    let magic = r.bytes(8)?;
    if magic != SNAPSHOT_MAGIC {
        return Err(SnapshotError::Corrupt("bad magic"));
    }
    if r.u32()? != SNAPSHOT_VERSION {
        return Err(SnapshotError::Corrupt("unsupported version"));
    }
    let seq = r.u64()?;
    let stored_hash = r.digest()?;
    let payload_len = r.u64()?;
    if payload_len > max_bytes {
        return Err(SnapshotError::TooLarge);
    }
    let expected_len = (HEADER_LEN as u64)
        .checked_add(payload_len)
        .and_then(|n| n.checked_add(TRAILER_LEN as u64))
        .ok_or(SnapshotError::TooLarge)?;
    if expected_len != file_len {
        return Err(SnapshotError::Corrupt(
            "file size does not match payload length",
        ));
    }
    // The size has been checked against the real file, so this allocation is
    // bounded by the file that actually exists.
    let n = usize::try_from(payload_len).map_err(|_| SnapshotError::TooLarge)?;
    let mut payload = vec![0u8; n];
    file.read_exact(&mut payload)?;
    let mut trailer = [0u8; TRAILER_LEN];
    file.read_exact(&mut trailer)?;
    let mut extra = [0u8; 1];
    if file.read(&mut extra)? != 0 {
        return Err(SnapshotError::Corrupt("file grew while reading"));
    }
    if !ct_eq(&trailer_digest(&header, &payload), &trailer) {
        return Err(SnapshotError::Corrupt("trailer hash mismatch"));
    }
    let mut pr = Reader::new(&payload);
    let engine = Engine::decode_state(&mut pr)?;
    if pr.remaining() != 0 {
        return Err(SnapshotError::Corrupt("trailing payload bytes"));
    }
    if !ct_eq(&engine.state_hash(), &stored_hash) {
        return Err(SnapshotError::Corrupt("state hash mismatch"));
    }
    if engine.events_applied() != seq {
        return Err(SnapshotError::Corrupt("event counter does not match seq"));
    }
    Ok((seq, engine))
}

/// Load the newest valid snapshot in `dir`, trying newest first and skipping
/// files that fail to load (damaged, truncated, or whose header disagrees with
/// the file name). Skipping is safe because the log is complete: recovery can
/// fall back to an older snapshot or to a full replay.
pub fn load_latest(dir: impl AsRef<Path>) -> Result<LatestSnapshot, SnapshotError> {
    let entries = list_snapshots(dir)?;
    let mut skipped = 0usize;
    for entry in entries.iter().rev() {
        match load_snapshot(&entry.path) {
            Ok((seq, engine)) if seq == entry.seq => {
                return Ok(LatestSnapshot {
                    snapshot: Some((seq, engine)),
                    skipped,
                });
            }
            Ok(_) | Err(_) => skipped = skipped.saturating_add(1),
        }
    }
    Ok(LatestSnapshot {
        snapshot: None,
        skipped,
    })
}

/// Delete all but the newest `keep` snapshot files (by sequence number), and any
/// leftover temp files from interrupted writes. Returns the number of snapshot
/// files removed. Files that do not match the snapshot naming are not touched.
///
/// The newest files are kept even if they are damaged; with `keep = 1` a
/// damaged newest snapshot leaves only a full replay for recovery.
pub fn prune(dir: impl AsRef<Path>, keep: usize) -> Result<usize, SnapshotError> {
    let dir = dir.as_ref();
    let entries = list_snapshots(dir)?;
    let remove_count = entries.len().saturating_sub(keep);
    let mut removed = 0usize;
    for entry in entries.iter().take(remove_count) {
        fs::remove_file(&entry.path)?;
        removed += 1;
    }
    match fs::read_dir(dir) {
        Ok(rd) => {
            for entry in rd {
                let entry = entry?;
                let is_tmp = entry.file_name().to_str().is_some_and(is_temp_name);
                if is_tmp && entry.file_type()?.is_file() {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    }
    sync_dir(dir)?;
    Ok(removed)
}

impl From<SnapshotError> for PortError {
    fn from(e: SnapshotError) -> PortError {
        let kind = match &e {
            SnapshotError::Io(_) => PortErrorKind::Io,
            SnapshotError::Corrupt(_) | SnapshotError::TooLarge | SnapshotError::Decode(_) => {
                PortErrorKind::Corrupt
            }
        };
        PortError::new(kind, e.to_string())
    }
}

/// Directory-backed implementation of the [`SnapshotStore`] port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshotStore {
    dir: PathBuf,
}

impl FileSnapshotStore {
    /// Store snapshots in `dir`; the directory is created on the first save.
    pub fn new(dir: impl Into<PathBuf>) -> FileSnapshotStore {
        FileSnapshotStore { dir: dir.into() }
    }

    /// The snapshot directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl SnapshotStore for FileSnapshotStore {
    fn save(&mut self, engine: &Engine) -> Result<u64, PortError> {
        write_snapshot(&self.dir, engine)?;
        Ok(engine.events_applied())
    }

    fn load_latest(&mut self) -> Result<Option<LoadedSnapshot>, PortError> {
        let latest = load_latest(&self.dir)?;
        Ok(latest.snapshot.map(|(seq, engine)| LoadedSnapshot {
            seq,
            engine,
            skipped: latest.skipped,
        }))
    }

    fn prune(&mut self, keep: usize) -> Result<(), PortError> {
        prune(&self.dir, keep)?;
        Ok(())
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
    use crate::wal::test_support::TempDir;
    use perp_app::contract::run_scenario;

    fn engine_at(n: u64) -> Engine {
        run_scenario(n)
    }

    /// Assemble a snapshot file from parts (for crafting damaged files).
    fn assemble(seq: u64, hash: &Digest, payload: &[u8]) -> Vec<u8> {
        let header = header_bytes(seq, hash, payload.len() as u64);
        let mut v = header.to_vec();
        v.extend_from_slice(payload);
        v.extend_from_slice(&trailer_digest(&header, payload));
        v
    }

    fn payload_of(e: &Engine) -> Vec<u8> {
        let mut w = Writer::new_buf();
        e.encode_state(&mut w);
        w.into_bytes()
    }

    fn err_of<T>(r: Result<T, SnapshotError>) -> SnapshotError {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
    }

    #[test]
    fn file_names_round_trip_and_parse_strictly() {
        assert_eq!(snapshot_file_name(0), "snapshot-00000000000000000000.bin");
        assert_eq!(snapshot_file_name(42), "snapshot-00000000000000000042.bin");
        assert_eq!(
            snapshot_file_name(u64::MAX),
            "snapshot-18446744073709551615.bin"
        );
        for seq in [0u64, 1, 99, 123_456_789, u64::MAX] {
            assert_eq!(parse_snapshot_name(&snapshot_file_name(seq)), Some(seq));
        }
        for bad in [
            "snapshot-1.bin",
            "snapshot-0000000000000000042.bin",   // 19 digits
            "snapshot-000000000000000000042.bin", // 21 digits
            "snapshot-0000000000000000004x.bin",
            "snapshot-+0000000000000000042.bin",
            "snapshot--0000000000000000042.bin",
            "Snapshot-00000000000000000042.bin",
            "snapshot-00000000000000000042.BIN",
            "snapshot-00000000000000000042.bin.tmp",
            "snapshot-00000000000000000042.bin ",
            "snapshot-99999999999999999999.bin", // overflows u64
            "xsnapshot-00000000000000000042.bin",
            "",
        ] {
            assert_eq!(parse_snapshot_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn round_trip_restores_the_same_state() {
        let dir = TempDir::new("snap-rt");
        let e = engine_at(30);
        let path = write_snapshot(dir.path(), &e).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            snapshot_file_name(e.events_applied())
        );
        let (seq, loaded) = load_snapshot(&path).unwrap();
        assert_eq!(seq, e.events_applied());
        assert_eq!(loaded.events_applied(), e.events_applied());
        assert_eq!(loaded.state_hash(), e.state_hash());
        assert_eq!(loaded.summary(), e.summary());
    }

    #[test]
    fn file_layout_matches_the_documented_format() {
        let dir = TempDir::new("snap-layout");
        let e = engine_at(12);
        let bytes = std::fs::read(write_snapshot(dir.path(), &e).unwrap()).unwrap();
        let payload = payload_of(&e);
        assert_eq!(&bytes[..8], b"PERPSNP1");
        assert_eq!(&bytes[8..12], &1u32.to_le_bytes());
        assert_eq!(&bytes[12..20], &e.events_applied().to_le_bytes());
        assert_eq!(&bytes[20..52], &e.state_hash());
        assert_eq!(&bytes[52..60], &(payload.len() as u64).to_le_bytes());
        assert_eq!(&bytes[60..60 + payload.len()], payload.as_slice());
        let body = &bytes[..bytes.len() - 32];
        assert_eq!(
            &bytes[bytes.len() - 32..],
            &perp_domain::sha256::sha256(body)
        );
        assert_eq!(bytes.len(), 60 + payload.len() + 32);
    }

    #[test]
    fn snapshot_of_an_empty_engine_loads() {
        let dir = TempDir::new("snap-empty");
        let e = Engine::new();
        let p = write_snapshot(dir.path(), &e).unwrap();
        let (seq, loaded) = load_snapshot(&p).unwrap();
        assert_eq!(seq, 0);
        assert_eq!(loaded.state_hash(), e.state_hash());
    }

    #[test]
    fn every_truncation_is_rejected() {
        let dir = TempDir::new("snap-trunc");
        let e = engine_at(20);
        let good = std::fs::read(write_snapshot(dir.path(), &e).unwrap()).unwrap();
        let work = dir.join("t.bin");
        for cut in 0..good.len() {
            std::fs::write(&work, &good[..cut]).unwrap();
            assert!(load_snapshot(&work).is_err(), "cut {cut}");
        }
        std::fs::write(&work, &good).unwrap();
        assert!(load_snapshot(&work).is_ok());
    }

    #[test]
    fn every_byte_with_a_flipped_bit_is_rejected() {
        let dir = TempDir::new("snap-flip");
        let e = engine_at(20);
        let good = std::fs::read(write_snapshot(dir.path(), &e).unwrap()).unwrap();
        let work = dir.join("f.bin");
        for i in 0..good.len() {
            for bit in [0x01u8, 0x80] {
                let mut b = good.clone();
                b[i] ^= bit;
                std::fs::write(&work, &b).unwrap();
                assert!(load_snapshot(&work).is_err(), "byte {i} bit {bit:#x}");
            }
        }
    }

    #[test]
    fn appended_bytes_are_rejected() {
        let dir = TempDir::new("snap-extra");
        let e = engine_at(5);
        let mut b = std::fs::read(write_snapshot(dir.path(), &e).unwrap()).unwrap();
        b.push(0);
        let work = dir.join("x.bin");
        std::fs::write(&work, &b).unwrap();
        assert!(matches!(
            err_of(load_snapshot(&work)),
            SnapshotError::Corrupt("file size does not match payload length")
        ));
    }

    #[test]
    fn oversize_claimed_payload_length_is_rejected_before_allocating() {
        let dir = TempDir::new("snap-oversize");
        let e = engine_at(5);
        let payload = payload_of(&e);
        let good = assemble(e.events_applied(), &e.state_hash(), &payload);
        let work = dir.join("o.bin");
        for claim in [
            u64::MAX,
            MAX_SNAPSHOT_BYTES + 1,
            MAX_SNAPSHOT_BYTES,
            1 << 40,
        ] {
            let mut b = good.clone();
            b[52..60].copy_from_slice(&claim.to_le_bytes());
            std::fs::write(&work, &b).unwrap();
            let err = err_of(load_snapshot(&work));
            if claim > MAX_SNAPSHOT_BYTES {
                assert!(matches!(err, SnapshotError::TooLarge), "{claim}");
            } else {
                assert!(matches!(err, SnapshotError::Corrupt(_)), "{claim}");
            }
        }
        // A claim that overflows when the header and trailer are added.
        let mut b = good.clone();
        b[52..60].copy_from_slice(&(u64::MAX - 10).to_le_bytes());
        std::fs::write(&work, &b).unwrap();
        assert!(matches!(
            err_of(load_snapshot(&work)),
            SnapshotError::TooLarge
        ));
    }

    #[test]
    fn file_larger_than_the_limit_is_rejected_by_size_alone() {
        let dir = TempDir::new("snap-limit");
        let e = engine_at(5);
        let p = write_snapshot(dir.path(), &e).unwrap();
        let len = std::fs::metadata(&p).unwrap().len();
        assert!(load_with_limit(&p, len).is_ok());
        assert!(matches!(
            err_of(load_with_limit(&p, len - 1)),
            SnapshotError::TooLarge
        ));
    }

    #[test]
    fn inconsistent_contents_with_a_valid_trailer_are_rejected() {
        let dir = TempDir::new("snap-tamper");
        let e = engine_at(15);
        let payload = payload_of(&e);
        let seq = e.events_applied();
        let hash = e.state_hash();
        let work = dir.join("c.bin");
        let load = |bytes: &[u8]| {
            std::fs::write(&work, bytes).unwrap();
            err_of(load_snapshot(&work))
        };
        let mut wrong_hash = hash;
        wrong_hash[0] ^= 1;
        assert!(matches!(
            load(&assemble(seq, &wrong_hash, &payload)),
            SnapshotError::Corrupt("state hash mismatch")
        ));
        assert!(matches!(
            load(&assemble(seq + 1, &hash, &payload)),
            SnapshotError::Corrupt("event counter does not match seq")
        ));
        let mut more = payload.clone();
        more.push(0);
        assert!(matches!(
            load(&assemble(seq, &hash, &more)),
            SnapshotError::Corrupt("trailing payload bytes") | SnapshotError::Decode(_)
        ));
        assert!(matches!(
            load(&assemble(seq, &hash, &payload[..payload.len() - 1])),
            SnapshotError::Decode(_)
        ));
        assert!(matches!(
            load(&assemble(seq, &hash, &[0xFF; 64])),
            SnapshotError::Decode(_)
        ));
        let mut bad_magic = assemble(seq, &hash, &payload);
        bad_magic[0] = b'X';
        assert!(matches!(load(&bad_magic), SnapshotError::Corrupt(_)));
        let mut bad_version = assemble(seq, &hash, &payload);
        bad_version[8] = 9;
        assert!(matches!(
            load(&bad_version),
            SnapshotError::Corrupt("unsupported version")
        ));
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = TempDir::new("snap-missing");
        assert!(matches!(
            err_of(load_snapshot(dir.join("nope.bin"))),
            SnapshotError::Io(_)
        ));
    }

    #[test]
    fn list_ignores_other_names_and_sorts_numerically() {
        let dir = TempDir::new("snap-list");
        for seq in [100u64, 7, 20_000, 5] {
            write_snapshot_at(dir.path(), seq);
        }
        for junk in [
            "snapshot-5.bin",
            "snapshot-00000000000000000005.bin.tmp",
            "notes.txt",
            "snapshot-0000000000000000000x.bin",
            "engine.wal",
        ] {
            std::fs::write(dir.join(junk), b"x").unwrap();
        }
        std::fs::create_dir(dir.join(snapshot_file_name(999))).unwrap();
        let got: Vec<u64> = list_snapshots(dir.path())
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(got, vec![5, 7, 100, 20_000]);
    }

    /// A syntactically valid snapshot file with the given name, not
    /// necessarily consistent with its contents.
    fn write_snapshot_at(dir: &Path, seq: u64) {
        let e = Engine::new();
        let payload = payload_of(&e);
        let bytes = assemble(seq, &e.state_hash(), &payload);
        std::fs::write(dir.join(snapshot_file_name(seq)), bytes).unwrap();
    }

    #[test]
    fn list_of_a_missing_directory_is_empty() {
        let dir = TempDir::new("snap-nodir");
        assert!(list_snapshots(dir.join("absent")).unwrap().is_empty());
        let none = load_latest(dir.join("absent")).unwrap();
        assert!(none.snapshot.is_none());
        assert_eq!(none.skipped, 0);
    }

    #[test]
    fn load_latest_picks_the_newest_valid_snapshot() {
        let dir = TempDir::new("snap-latest");
        for n in [10u64, 20, 30] {
            write_snapshot(dir.path(), &engine_at(n)).unwrap();
        }
        let l = load_latest(dir.path()).unwrap();
        assert_eq!(l.skipped, 0);
        let (seq, e) = l.snapshot.unwrap();
        assert_eq!(seq, 30);
        assert_eq!(e.state_hash(), engine_at(30).state_hash());
    }

    #[test]
    fn load_latest_falls_back_when_the_newest_is_damaged() {
        let dir = TempDir::new("snap-fallback");
        for n in [10u64, 20, 30] {
            write_snapshot(dir.path(), &engine_at(n)).unwrap();
        }
        let newest = dir.join(snapshot_file_name(30));
        let mut b = std::fs::read(&newest).unwrap();
        let mid = b.len() / 2;
        b[mid] ^= 0xFF;
        std::fs::write(&newest, &b).unwrap();
        let l = load_latest(dir.path()).unwrap();
        assert_eq!(l.skipped, 1);
        assert_eq!(l.snapshot.unwrap().0, 20);

        // Truncated second newest too.
        let second = dir.join(snapshot_file_name(20));
        std::fs::write(&second, b"short").unwrap();
        let l = load_latest(dir.path()).unwrap();
        assert_eq!(l.skipped, 2);
        assert_eq!(l.snapshot.unwrap().0, 10);

        // All damaged.
        std::fs::write(dir.join(snapshot_file_name(10)), b"").unwrap();
        let l = load_latest(dir.path()).unwrap();
        assert_eq!(l.skipped, 3);
        assert!(l.snapshot.is_none());
    }

    #[test]
    fn load_latest_skips_a_file_whose_name_disagrees_with_its_header() {
        let dir = TempDir::new("snap-renamed");
        let e = engine_at(10);
        let p = write_snapshot(dir.path(), &e).unwrap();
        let renamed = dir.join(snapshot_file_name(50));
        std::fs::rename(&p, &renamed).unwrap();
        let l = load_latest(dir.path()).unwrap();
        assert_eq!(l.skipped, 1);
        assert!(l.snapshot.is_none());
    }

    #[test]
    fn prune_keeps_the_newest_and_removes_temp_files() {
        let dir = TempDir::new("snap-prune");
        for n in [5u64, 10, 15, 20, 25] {
            write_snapshot(dir.path(), &engine_at(n)).unwrap();
        }
        std::fs::write(dir.join("snapshot-00000000000000000007.bin.tmp"), b"x").unwrap();
        std::fs::write(dir.join("keep-me.txt"), b"x").unwrap();
        assert_eq!(prune(dir.path(), 2).unwrap(), 3);
        let left: Vec<u64> = list_snapshots(dir.path())
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(left, vec![20, 25]);
        assert!(dir.join("keep-me.txt").exists());
        assert!(!dir.join("snapshot-00000000000000000007.bin.tmp").exists());
        assert_eq!(prune(dir.path(), 10).unwrap(), 0);
        assert_eq!(list_snapshots(dir.path()).unwrap().len(), 2);
        assert_eq!(prune(dir.path(), 0).unwrap(), 2);
        assert!(list_snapshots(dir.path()).unwrap().is_empty());
        assert_eq!(prune(dir.join("absent"), 1).unwrap(), 0);
    }

    #[test]
    fn write_leaves_no_temp_file_and_can_overwrite() {
        let dir = TempDir::new("snap-atomic");
        let e = engine_at(8);
        let p1 = write_snapshot(dir.path(), &e).unwrap();
        let p2 = write_snapshot(dir.path(), &e).unwrap();
        assert_eq!(p1, p2);
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![snapshot_file_name(8)]);
        assert!(load_snapshot(&p2).is_ok());
    }

    #[test]
    fn stale_temp_file_does_not_block_or_appear() {
        let dir = TempDir::new("snap-stale-tmp");
        let e = engine_at(8);
        std::fs::write(dir.join(temp_name(8)), b"partial garbage").unwrap();
        assert!(list_snapshots(dir.path()).unwrap().is_empty());
        let p = write_snapshot(dir.path(), &e).unwrap();
        assert!(load_snapshot(&p).is_ok());
        assert!(!dir.join(temp_name(8)).exists());
    }

    #[test]
    fn write_creates_the_directory() {
        let dir = TempDir::new("snap-mkdir");
        let sub = dir.join("a").join("b");
        let p = write_snapshot(&sub, &engine_at(3)).unwrap();
        assert!(p.starts_with(&sub));
        assert!(load_snapshot(&p).is_ok());
    }

    #[test]
    fn write_into_a_non_directory_fails_cleanly() {
        let dir = TempDir::new("snap-notdir");
        let file = dir.join("plain");
        std::fs::write(&file, b"x").unwrap();
        assert!(matches!(
            err_of(write_snapshot(&file, &engine_at(3))),
            SnapshotError::Io(_)
        ));
    }

    #[test]
    fn error_display_and_conversions() {
        let e: SnapshotError = io::Error::other("boom").into();
        assert!(e.to_string().contains("boom"));
        assert!(std::error::Error::source(&e).is_some());
        let d: SnapshotError = DecodeError("bad").into();
        assert!(d.to_string().contains("bad"));
        assert!(std::error::Error::source(&d).is_some());
        assert_eq!(
            SnapshotError::Corrupt("why").to_string(),
            "snapshot corrupt: why"
        );
        assert!(!SnapshotError::TooLarge.to_string().is_empty());
        assert!(std::error::Error::source(&SnapshotError::TooLarge).is_none());
    }

    #[test]
    fn snapshot_errors_map_to_port_error_kinds() {
        let kind = |e: SnapshotError| PortError::from(e).kind;
        assert_eq!(
            kind(SnapshotError::Io(io::Error::other("x"))),
            PortErrorKind::Io
        );
        assert_eq!(kind(SnapshotError::Corrupt("x")), PortErrorKind::Corrupt);
        assert_eq!(kind(SnapshotError::TooLarge), PortErrorKind::Corrupt);
        assert_eq!(
            kind(SnapshotError::Decode(DecodeError("x"))),
            PortErrorKind::Corrupt
        );
    }

    #[test]
    fn file_snapshot_store_saves_loads_and_prunes() {
        let dir = TempDir::new("snap-store");
        let mut store = FileSnapshotStore::new(dir.join("s"));
        assert_eq!(store.dir(), dir.join("s").as_path());
        assert!(store.load_latest().unwrap().is_none());
        for n in [4u64, 8, 12] {
            assert_eq!(store.save(&engine_at(n)).unwrap(), n);
        }
        store.prune(1).unwrap();
        let l = store.load_latest().unwrap().unwrap();
        assert_eq!((l.seq, l.skipped), (12, 0));
        assert_eq!(list_snapshots(dir.join("s")).unwrap().len(), 1);
    }
}
