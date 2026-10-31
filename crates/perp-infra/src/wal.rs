//! Append-only write-ahead log (WAL): one file, length-prefixed records, SHA-256
//! hash chain.
//!
//! # File format
//!
//! ```text
//! header (16 bytes): "PERPWAL1" | version u32 LE (= 1) | reserved u32 LE (= 0)
//! record:            len u32 LE | payload (len bytes) | chain [u8; 32]
//! payload:           seq u64 LE | canonical encoding of an Event
//! chain:             SHA-256(prev_chain || len as 4 LE bytes || payload)
//! ```
//!
//! The first `prev_chain` is [`genesis_chain`]. A payload is `9..=MAX_RECORD_BYTES`
//! bytes, must decode to exactly one `(seq, Event)` with no bytes left over, and
//! `seq` must equal the record's index in the file (0, 1, 2, ...).
//!
//! # Torn tails and corruption
//!
//! A crash while appending can leave a partial or damaged last record. When the
//! file is scanned (by [`Wal::open`], [`WalReader`] or [`verify`]) the first
//! record that cannot be read is classified as follows:
//!
//! * **Torn tail**: the record is *incomplete* (partial length field, a length
//!   field that is not a valid record length, or fewer bytes than the length
//!   field claims), or everything from there to the end of the file is zero
//!   bytes, *and* no later record can be found that verifies against the hash
//!   chain. [`Wal::open`] repairs it automatically: the dropped bytes are first
//!   copied to a sidecar file next to the log (see below), flushed and synced,
//!   and only then is the file truncated to the last valid record boundary.
//! * **Damaged tail** ([`WalError::DamagedTail`]): the final record is
//!   *complete* (valid length field, all of its bytes present) but fails
//!   verification, and no later record exists. That is what a bit flip or
//!   tampering with a durable, acknowledged record looks like, so [`Wal::open`]
//!   does **not** remove it: it returns the error and leaves the file
//!   untouched. Only [`WalConfig::repair_damaged_tail`] (the explicit repair
//!   option) saves the bytes to the sidecar and truncates. The read-only
//!   functions report it ([`Tail::Damaged`], [`VerifyReport::damaged_tail`]).
//! * **Corruption** ([`WalError::Corrupt`]): the damaged record is followed by
//!   data that still contains a self-consistent record, or the damaged
//!   region is longer than one maximum-size record and is not all zeros. Nothing
//!   is modified.
//!
//! # Sidecar
//!
//! Whenever [`Wal::open`] drops bytes it first writes them, exactly, to
//! `<log file name>.torn-<offset>` in the same directory (`<offset>` is the byte
//! offset where they started; a numeric suffix `.1`, `.2`, ... is added when the
//! name is taken). The file is created with `create_new` (an existing file is
//! never overwritten), flushed and synced, and the directory is synced, before
//! the log is truncated. If the sidecar cannot be written the log is left
//! untouched and the open fails. The dropped region is at most one maximum-size
//! record (about 64 KiB), except for an all-zero tail: a zero tail longer than
//! that carries no information and is truncated without a sidecar
//! ([`OpenReport::sidecar`] is `None`). A file that is only a strict prefix of
//! the header (creation was interrupted) is rewritten without a sidecar.
//!
//! "A later record that verifies" is found by looking at every offset in the
//! damaged region (at most one maximum-size record, about 64 KiB) and checking the
//! chain of a record starting there against the 32 bytes that precede it (and,
//! at the claimed record end, against the chain computed from the damaged record
//! itself). This also covers a damaged length field. It never allocates more than
//! one maximum-size record.
//!
//! A record whose chain verifies but whose payload does not decode, or whose
//! sequence number is wrong, is always reported as corruption: its bytes are
//! exactly what was written, so it cannot be a torn write.
//!
//! # Port adapters
//!
//! [`FileEventLog`] implements `perp_app::ports::EventLog` on top of [`Wal`]
//! (repairing a torn tail on open); [`ReadOnlyFileLog`] implements the same
//! port for inspection and never modifies the file.
//!
//! # Limits
//!
//! * The chain detects accidental damage and truncation of the middle of the
//!   log. It does not authenticate: anyone who can write the file can rewrite
//!   the whole chain. Dropping whole records from the end of the file (or the
//!   whole file) is not detectable by the file alone.
//! * An incomplete final record (including one whose length field was damaged
//!   into a larger or invalid value) is indistinguishable from a torn write and
//!   is removed automatically; its bytes are kept in the sidecar. A complete but
//!   damaged final record is never removed without the explicit repair option.
//! * A single writer is assumed. There is no cross-process locking and no
//!   log rotation in this version; the file only grows, up to
//!   [`WalConfig::max_file_bytes`].
//! * Durability relies on `fsync` (`File::sync_data`) behaving as documented by
//!   the operating system, filesystem and storage hardware.

use perp_app::ports::{EventLog, PortError, PortErrorKind};
use perp_domain::codec::{Decode, Encode, Reader, Writer};
use perp_domain::sha256::{ct_eq, sha256, sha256_concat, Digest};
use perp_domain::types::Event;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// File magic.
pub const MAGIC: [u8; 8] = *b"PERPWAL1";
/// File format version.
pub const VERSION: u32 = 1;
/// Size of the file header in bytes.
pub const HEADER_LEN: u64 = 16;
/// Largest accepted record payload in bytes (sequence number plus encoded event).
pub const MAX_RECORD_BYTES: usize = 65_536;

/// Smallest valid payload: a sequence number and at least one event byte.
const MIN_PAYLOAD: usize = 9;
const LEN_FIELD: usize = 4;
const CHAIN_LEN: usize = 32;
/// Smallest valid record on disk.
const MIN_RECORD: usize = LEN_FIELD + MIN_PAYLOAD + CHAIN_LEN;
/// Size on disk of the largest valid record; also the longest damaged region
/// that is still examined as a possible torn tail.
const MAX_TORN_TAIL: u64 = (LEN_FIELD + MAX_RECORD_BYTES + CHAIN_LEN) as u64;

/// Chain value before the first record.
pub fn genesis_chain() -> Digest {
    sha256(b"perp-engine wal v1 genesis")
}

fn header_bytes() -> [u8; HEADER_LEN as usize] {
    let mut h = [0u8; HEADER_LEN as usize];
    h[..8].copy_from_slice(&MAGIC);
    h[8..12].copy_from_slice(&VERSION.to_le_bytes());
    h
}

/// Flush directory metadata (a new or renamed entry) to stable storage.
/// No-op on platforms where directories cannot be opened for syncing.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Errors of the write-ahead log.
#[derive(Debug)]
pub enum WalError {
    /// Underlying I/O failure.
    Io(io::Error),
    /// The file header is missing, wrong or unsupported.
    BadHeader,
    /// A damaged record that is not a torn tail; nothing was modified.
    Corrupt {
        /// Byte offset of the damaged record.
        offset: u64,
        /// Short reason.
        why: &'static str,
    },
    /// `append` was called with a sequence number other than the next one.
    SeqMismatch {
        /// Sequence number the log expects.
        expected: u64,
        /// Sequence number supplied.
        got: u64,
    },
    /// The encoded event does not fit in [`MAX_RECORD_BYTES`].
    RecordTooLarge,
    /// Appending would exceed [`WalConfig::max_file_bytes`].
    Full,
    /// The final record is complete (valid length, all bytes present) but
    /// fails verification and nothing valid follows. It may be a durable,
    /// acknowledged event that was damaged after the fact, so it was **not**
    /// removed and the file is untouched. Opening with
    /// [`WalConfig::repair_damaged_tail`] saves the bytes to a sidecar file and
    /// truncates.
    DamagedTail {
        /// Byte offset where the damaged record starts.
        offset: u64,
        /// Size on disk of the damaged record as claimed by its length field.
        record_len: u64,
    },
}

impl fmt::Display for WalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalError::Io(e) => write!(f, "wal i/o error: {e}"),
            WalError::BadHeader => write!(f, "wal header is invalid"),
            WalError::Corrupt { offset, why } => {
                write!(f, "wal corrupt at byte {offset}: {why}")
            }
            WalError::SeqMismatch { expected, got } => {
                write!(f, "wal sequence mismatch: expected {expected}, got {got}")
            }
            WalError::RecordTooLarge => write!(f, "wal record too large"),
            WalError::Full => write!(f, "wal file size limit reached"),
            WalError::DamagedTail { offset, record_len } => write!(
                f,
                "wal final record at byte {offset} ({record_len} bytes) is complete but fails \
                 verification; it was not removed. Inspect the file, then pass the explicit \
                 repair option (serve --repair) to save it to a .torn-{offset} sidecar and \
                 truncate"
            ),
        }
    }
}

impl std::error::Error for WalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WalError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for WalError {
    fn from(e: io::Error) -> Self {
        WalError::Io(e)
    }
}

/// When appended records are flushed to stable storage with `sync_data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncPolicy {
    /// Sync after every append. An acknowledged append survives a crash, as far
    /// as the storage stack honours `fsync`.
    Always,
    /// Sync after every `n` appends. `EveryN(0)` behaves like `Always`. A crash
    /// can lose up to `n - 1` acknowledged records.
    EveryN(u32),
    /// Never sync on append; only [`Wal::sync`] flushes. A crash can lose any
    /// number of recent records (the log stays consistent, just shorter).
    Never,
}

/// Write-ahead log settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalConfig {
    /// Flush policy for appends.
    pub fsync: FsyncPolicy,
    /// Largest allowed file size in bytes, header included; `append` returns
    /// [`WalError::Full`] beyond it.
    pub max_file_bytes: u64,
    /// Allow [`Wal::open`] to remove a *complete but damaged* final record
    /// (after saving it to a sidecar file). When false (the default) such a
    /// tail makes `open` fail with [`WalError::DamagedTail`] and the file is
    /// left untouched. Genuinely torn (incomplete) tails are always repaired.
    pub repair_damaged_tail: bool,
}

impl Default for WalConfig {
    fn default() -> Self {
        WalConfig {
            fsync: FsyncPolicy::Always,
            max_file_bytes: 1 << 30,
            repair_damaged_tail: false,
        }
    }
}

/// What [`Wal::open`] found and repaired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenReport {
    /// Valid records in the log.
    pub records: u64,
    /// File length after repair (header plus valid records).
    pub valid_len: u64,
    /// Bytes removed from the end of the file as a torn tail.
    pub truncated_bytes: u64,
    /// Chain value after the last valid record.
    pub chain_head: Digest,
    /// Sidecar file the dropped bytes were saved to, when any bytes were
    /// dropped and they are worth keeping (see the module documentation).
    pub sidecar: Option<PathBuf>,
}

/// State of the end of a log after a full read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tail {
    /// The file ends exactly after the last valid record.
    Clean,
    /// Trailing bytes that are a torn write.
    Torn {
        /// Length the file would be truncated to.
        valid_len: u64,
        /// Number of trailing bytes after `valid_len`.
        extra_bytes: u64,
    },
    /// The final record is complete but fails verification (see
    /// [`WalError::DamagedTail`]). Not repaired by [`Wal::open`] unless asked.
    Damaged {
        /// Length the file would be truncated to.
        valid_len: u64,
        /// Number of trailing bytes after `valid_len`.
        extra_bytes: u64,
        /// Size on disk of the damaged record claimed by its length field.
        record_len: u64,
    },
}

/// Result of [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Valid records.
    pub records: u64,
    /// Sequence number of the last valid record.
    pub last_seq: Option<u64>,
    /// Chain value after the last valid record.
    pub chain_head: Digest,
    /// File length in bytes.
    pub file_len: u64,
    /// Bytes of torn tail (reported, not repaired).
    pub torn_bytes: u64,
    /// True when the tail is a complete but damaged final record (as opposed
    /// to an incomplete, torn one). `torn_bytes` then counts that record too.
    pub damaged_tail: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum HeaderState {
    Valid,
    /// File shorter than the header and a prefix of it: creation was torn.
    TornCreation,
}

fn check_header(file: &mut File, file_len: u64) -> Result<HeaderState, WalError> {
    let expected = header_bytes();
    file.seek(SeekFrom::Start(0))?;
    if file_len < HEADER_LEN {
        let n = usize::try_from(file_len).map_err(|_| WalError::BadHeader)?;
        let mut got = vec![0u8; n];
        file.read_exact(&mut got)?;
        return if expected.get(..n) == Some(got.as_slice()) {
            Ok(HeaderState::TornCreation)
        } else {
            Err(WalError::BadHeader)
        };
    }
    let mut got = [0u8; HEADER_LEN as usize];
    file.read_exact(&mut got)?;
    if got == expected {
        Ok(HeaderState::Valid)
    } else {
        Err(WalError::BadHeader)
    }
}

/// Why a record could not be read, and what is known about its successor.
struct Failure {
    why: &'static str,
    /// Claimed record size and the chain computed from the damaged record, when
    /// the record is complete in the file.
    claimed: Option<(usize, Digest)>,
}

enum Attempt {
    Good(Box<Event>, u64),
    Bad(Failure),
}

enum Step {
    Record(u64, Box<Event>),
    End,
    Torn {
        extra: u64,
        /// Size on disk of the damaged final record when it is complete.
        damaged_record: Option<u64>,
    },
}

/// Sequential, bounded-memory record scanner shared by all readers.
struct Scanner {
    reader: BufReader<File>,
    file_len: u64,
    /// Offset of the next record; after a torn step it is the valid length.
    offset: u64,
    chain: Digest,
    next_seq: u64,
    payload: Vec<u8>,
}

fn bad(why: &'static str) -> Result<Attempt, WalError> {
    Ok(Attempt::Bad(Failure { why, claimed: None }))
}

impl Scanner {
    fn new(mut file: File, file_len: u64) -> io::Result<Scanner> {
        file.seek(SeekFrom::Start(HEADER_LEN))?;
        Ok(Scanner {
            reader: BufReader::with_capacity(64 * 1024, file),
            file_len,
            offset: HEADER_LEN,
            chain: genesis_chain(),
            next_seq: 0,
            payload: Vec::new(),
        })
    }

    fn next(&mut self) -> Result<Step, WalError> {
        let start = self.offset;
        let rem = self.file_len.saturating_sub(start);
        if rem == 0 {
            return Ok(Step::End);
        }
        match self.try_record(start, rem)? {
            Attempt::Good(ev, total) => {
                let seq = self.next_seq;
                self.offset = start.checked_add(total).ok_or(WalError::Corrupt {
                    offset: start,
                    why: "offset overflow",
                })?;
                self.next_seq = seq.checked_add(1).ok_or(WalError::Corrupt {
                    offset: start,
                    why: "sequence overflow",
                })?;
                Ok(Step::Record(seq, ev))
            }
            Attempt::Bad(f) => self.classify(start, rem, &f),
        }
    }

    fn try_record(&mut self, start: u64, rem: u64) -> Result<Attempt, WalError> {
        if rem < LEN_FIELD as u64 {
            return bad("partial length field");
        }
        let mut len_bytes = [0u8; LEN_FIELD];
        self.reader.read_exact(&mut len_bytes)?;
        let len = usize::try_from(u32::from_le_bytes(len_bytes)).unwrap_or(usize::MAX);
        if !(MIN_PAYLOAD..=MAX_RECORD_BYTES).contains(&len) {
            return bad("invalid record length");
        }
        let total = LEN_FIELD + len + CHAIN_LEN;
        if total as u64 > rem {
            return bad("incomplete record");
        }
        self.payload.clear();
        self.payload.resize(len, 0);
        self.reader.read_exact(&mut self.payload)?;
        let mut stored = [0u8; CHAIN_LEN];
        self.reader.read_exact(&mut stored)?;
        let expected = sha256_concat(&[&self.chain, &len_bytes, &self.payload]);
        if !ct_eq(&expected, &stored) {
            return Ok(Attempt::Bad(Failure {
                why: "chain mismatch",
                claimed: Some((total, expected)),
            }));
        }
        // The chain verified: these are the bytes that were written, so any
        // problem from here on is corruption (or a writer bug), never a torn write.
        let corrupt = |why: &'static str| WalError::Corrupt { offset: start, why };
        let mut r = Reader::new(&self.payload);
        let seq = r.u64().map_err(|_| corrupt("payload does not decode"))?;
        let ev = Event::decode(&mut r).map_err(|_| corrupt("payload does not decode"))?;
        r.finish()
            .map_err(|_| corrupt("payload has trailing bytes"))?;
        if seq != self.next_seq {
            return Err(corrupt("sequence number out of order"));
        }
        self.chain = stored;
        Ok(Attempt::Good(Box::new(ev), total as u64))
    }

    /// Decide between torn tail and corruption for the failure at `start`.
    fn classify(&mut self, start: u64, rem: u64, f: &Failure) -> Result<Step, WalError> {
        self.reader.seek(SeekFrom::Start(start))?;
        if self.tail_is_zero(rem)? {
            return Ok(Step::Torn {
                extra: rem,
                damaged_record: None,
            });
        }
        if rem > MAX_TORN_TAIL {
            return Err(WalError::Corrupt {
                offset: start,
                why: f.why,
            });
        }
        self.reader.seek(SeekFrom::Start(start))?;
        let n = usize::try_from(rem).map_err(|_| WalError::Corrupt {
            offset: start,
            why: f.why,
        })?;
        let mut window = vec![0u8; n];
        self.reader.read_exact(&mut window)?;
        if window_has_valid_record(&window, f) {
            return Err(WalError::Corrupt {
                offset: start,
                why: "damaged record is followed by valid records",
            });
        }
        Ok(Step::Torn {
            extra: rem,
            damaged_record: f.claimed.map(|(total, _)| total as u64),
        })
    }

    /// True when the next `rem` bytes from the current position are all zero.
    fn tail_is_zero(&mut self, rem: u64) -> Result<bool, WalError> {
        let mut chunk = [0u8; 4096];
        let mut left = rem;
        while left > 0 {
            let n = usize::try_from(left).unwrap_or(usize::MAX).min(chunk.len());
            let Some(buf) = chunk.get_mut(..n) else {
                return Ok(false);
            };
            self.reader.read_exact(buf)?;
            if buf.iter().any(|b| *b != 0) {
                return Ok(false);
            }
            left = left.saturating_sub(n as u64);
        }
        Ok(true)
    }
}

/// Look for a record inside `window` (the bytes from a damaged record to the
/// end of the file) whose chain verifies. Such a record is proof that valid
/// data follows the damage.
fn window_has_valid_record(window: &[u8], f: &Failure) -> bool {
    let last = window.len().saturating_sub(MIN_RECORD);
    for p in MIN_RECORD..=last {
        let Some(len_le) = window.get(p..p + LEN_FIELD) else {
            continue;
        };
        let Ok(len_arr) = <[u8; LEN_FIELD]>::try_from(len_le) else {
            continue;
        };
        let len = usize::try_from(u32::from_le_bytes(len_arr)).unwrap_or(usize::MAX);
        if !(MIN_PAYLOAD..=MAX_RECORD_BYTES).contains(&len) {
            continue;
        }
        let payload_end = p + LEN_FIELD + len;
        let end = payload_end + CHAIN_LEN;
        let (Some(payload), Some(chain)) = (
            window.get(p + LEN_FIELD..payload_end),
            window.get(payload_end..end),
        ) else {
            continue;
        };
        if let Some(prev) = window.get(p - CHAIN_LEN..p) {
            if sha256_concat(&[prev, len_le, payload]).as_slice() == chain {
                return true;
            }
        }
        if let Some((claimed_end, computed)) = &f.claimed {
            if *claimed_end == p && sha256_concat(&[computed, len_le, payload]).as_slice() == chain
            {
                return true;
            }
        }
    }
    false
}

/// Writer handle of a log file.
#[derive(Debug)]
pub struct Wal {
    file: File,
    path: PathBuf,
    cfg: WalConfig,
    len: u64,
    records: u64,
    chain: Digest,
    unsynced: u64,
    syncs: u64,
    poisoned: bool,
}

/// Size on disk of a record whose payload has `payload_len` bytes.
fn frame_len(payload_len: usize) -> Result<usize, WalError> {
    if payload_len > MAX_RECORD_BYTES {
        return Err(WalError::RecordTooLarge);
    }
    Ok(LEN_FIELD + payload_len + CHAIN_LEN)
}

/// Most numeric suffixes tried when a sidecar name is taken.
const MAX_SIDECAR_SUFFIX: u32 = 1_000;

/// Copy the bytes `offset..file_len` of `file` to a new sidecar file next to
/// `path`, flush and sync it and its directory entry. Returns `None` for a
/// region longer than one maximum-size record (only an all-zero tail can be
/// that long; it holds no information).
fn save_dropped_tail(
    file: &mut File,
    path: &Path,
    offset: u64,
    file_len: u64,
) -> Result<Option<PathBuf>, WalError> {
    let extra = file_len.saturating_sub(offset);
    if extra == 0 || extra > MAX_TORN_TAIL {
        return Ok(None);
    }
    let n = usize::try_from(extra).map_err(|_| WalError::Corrupt {
        offset,
        why: "torn tail too large",
    })?;
    let mut bytes = vec![0u8; n];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    let base = path.file_name().unwrap_or_else(|| OsStr::new("wal"));
    for attempt in 0..=MAX_SIDECAR_SUFFIX {
        let mut name = base.to_os_string();
        name.push(format!(".torn-{offset}"));
        if attempt > 0 {
            name.push(format!(".{attempt}"));
        }
        let candidate = path.with_file_name(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut side) => {
                let written = side
                    .write_all(&bytes)
                    .and_then(|()| side.flush())
                    .and_then(|()| side.sync_all())
                    .and_then(|()| sync_dir(parent_dir(path)));
                if let Err(e) = written {
                    drop(side);
                    let _ = std::fs::remove_file(&candidate);
                    return Err(WalError::Io(e));
                }
                return Ok(Some(candidate));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(WalError::Io(e)),
        }
    }
    Err(WalError::Io(io::Error::other(
        "too many torn-tail sidecar files next to the log",
    )))
}

impl Wal {
    /// Open (creating if missing) the log at `path` for appending.
    ///
    /// The whole file is scanned and verified. A torn (incomplete) tail is
    /// saved to a sidecar file (see the module documentation), then truncated
    /// away and synced. A complete but damaged final record is refused with
    /// [`WalError::DamagedTail`] and the file is left untouched unless
    /// [`WalConfig::repair_damaged_tail`] is set. Mid-file corruption returns
    /// [`WalError::Corrupt`] and leaves the file untouched. A file that is a
    /// strict prefix of a valid header (creation was interrupted) is rewritten
    /// with a fresh header; any other short or wrong header is
    /// [`WalError::BadHeader`].
    pub fn open(path: impl AsRef<Path>, cfg: WalConfig) -> Result<(Wal, OpenReport), WalError> {
        let path = path.as_ref();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        let file_len = file.metadata()?.len();
        let mut sidecar = None;
        let (valid_len, records, chain, truncated) = match check_header(&mut file, file_len)? {
            HeaderState::TornCreation => {
                file.set_len(0)?;
                file.write_all(&header_bytes())?;
                file.sync_all()?;
                sync_dir(parent_dir(path))?;
                (HEADER_LEN, 0, genesis_chain(), file_len)
            }
            HeaderState::Valid => {
                let mut sc = Scanner::new(file.try_clone()?, file_len)?;
                let mut damaged_record = None;
                loop {
                    match sc.next()? {
                        Step::Record(..) => {}
                        Step::End => break,
                        Step::Torn {
                            damaged_record: d, ..
                        } => {
                            damaged_record = d;
                            break;
                        }
                    }
                }
                if sc.offset < file_len {
                    if let Some(record_len) = damaged_record {
                        if !cfg.repair_damaged_tail {
                            return Err(WalError::DamagedTail {
                                offset: sc.offset,
                                record_len,
                            });
                        }
                    }
                    sidecar = save_dropped_tail(&mut file, path, sc.offset, file_len)?;
                    file.set_len(sc.offset)?;
                    file.sync_all()?;
                }
                (
                    sc.offset,
                    sc.next_seq,
                    sc.chain,
                    file_len.saturating_sub(sc.offset),
                )
            }
        };
        let report = OpenReport {
            records,
            valid_len,
            truncated_bytes: truncated,
            chain_head: chain,
            sidecar,
        };
        let wal = Wal {
            file,
            path: path.to_path_buf(),
            cfg,
            len: valid_len,
            records,
            chain,
            unsynced: 0,
            syncs: 0,
            poisoned: false,
        };
        Ok((wal, report))
    }

    /// Append one event as record number `seq`, which must equal
    /// [`Wal::records`]. The record is built in one buffer and written with a
    /// single `write_all`, then synced according to the [`FsyncPolicy`].
    ///
    /// If the write fails the file is rolled back to its previous length; if
    /// that or a sync fails, the handle refuses further appends (reopen to
    /// recover from the file, which is the source of truth).
    pub fn append(&mut self, seq: u64, ev: &Event) -> Result<(), WalError> {
        if self.poisoned {
            return Err(poisoned_error());
        }
        if seq != self.records {
            return Err(WalError::SeqMismatch {
                expected: self.records,
                got: seq,
            });
        }
        let mut w = Writer::with_capacity(64);
        w.u64(seq);
        ev.encode(&mut w);
        let payload = w.into_bytes();
        let total = frame_len(payload.len())?;
        let new_len = self.len.checked_add(total as u64).ok_or(WalError::Full)?;
        if new_len > self.cfg.max_file_bytes {
            return Err(WalError::Full);
        }
        let len_le = u32::try_from(payload.len())
            .map_err(|_| WalError::RecordTooLarge)?
            .to_le_bytes();
        let chain = sha256_concat(&[&self.chain, &len_le, &payload]);
        let mut buf = Vec::with_capacity(total);
        buf.extend_from_slice(&len_le);
        buf.extend_from_slice(&payload);
        buf.extend_from_slice(&chain);
        if let Err(e) = self.file.write_all(&buf) {
            if self.file.set_len(self.len).is_err() {
                self.poisoned = true;
            }
            return Err(WalError::Io(e));
        }
        self.len = new_len;
        self.records = self.records.checked_add(1).ok_or(WalError::Full)?;
        self.chain = chain;
        self.unsynced = self.unsynced.saturating_add(1);
        let due = match self.cfg.fsync {
            FsyncPolicy::Always => true,
            FsyncPolicy::EveryN(n) => self.unsynced >= u64::from(n),
            FsyncPolicy::Never => false,
        };
        if due {
            self.sync_now()?;
        }
        Ok(())
    }

    fn sync_now(&mut self) -> Result<(), WalError> {
        if let Err(e) = self.file.sync_data() {
            self.poisoned = true;
            return Err(WalError::Io(e));
        }
        self.syncs = self.syncs.saturating_add(1);
        self.unsynced = 0;
        Ok(())
    }

    /// Flush appended records to stable storage. Does nothing when no record is
    /// waiting for a sync.
    pub fn sync(&mut self) -> Result<(), WalError> {
        if self.poisoned {
            return Err(poisoned_error());
        }
        if self.unsynced == 0 {
            return Ok(());
        }
        self.sync_now()
    }

    /// Number of `sync_data` calls made by this handle.
    pub fn syncs(&self) -> u64 {
        self.syncs
    }

    /// Number of records in the log.
    pub fn records(&self) -> u64 {
        self.records
    }

    /// File length in bytes (header plus records).
    pub fn len(&self) -> u64 {
        self.len
    }

    /// True when the log holds no records.
    pub fn is_empty(&self) -> bool {
        self.records == 0
    }

    /// Chain value after the last record.
    pub fn chain_head(&self) -> Digest {
        self.chain
    }

    /// Path of the log file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn poisoned_error() -> WalError {
    WalError::Io(io::Error::other(
        "wal handle stopped after an earlier i/o failure; reopen it",
    ))
}

enum ReaderState {
    Reading,
    Done(Tail),
    Failed { offset: u64, why: &'static str },
    Errored,
}

/// Read-only sequential access to a log. Never modifies the file.
pub struct WalReader {
    scanner: Option<Scanner>,
    state: ReaderState,
    file_len: u64,
}

impl WalReader {
    /// Open the log at `path`. The header is validated; a file that is a
    /// strict prefix of a valid header is an empty log with a torn tail.
    pub fn open(path: impl AsRef<Path>) -> Result<WalReader, WalError> {
        let mut file = File::open(path.as_ref())?;
        let file_len = file.metadata()?.len();
        match check_header(&mut file, file_len)? {
            HeaderState::TornCreation => Ok(WalReader {
                scanner: None,
                state: ReaderState::Done(Tail::Torn {
                    valid_len: 0,
                    extra_bytes: file_len,
                }),
                file_len,
            }),
            HeaderState::Valid => Ok(WalReader {
                scanner: Some(Scanner::new(file, file_len)?),
                state: ReaderState::Reading,
                file_len,
            }),
        }
    }

    /// Next `(seq, event)`, or `Ok(None)` at the end of the valid records.
    /// After `Ok(None)` call [`WalReader::tail`]. Mid-file corruption returns
    /// [`WalError::Corrupt`] (again on every later call).
    #[allow(clippy::should_implement_trait)] // fallible, not an Iterator
    pub fn next(&mut self) -> Result<Option<(u64, Event)>, WalError> {
        match &self.state {
            ReaderState::Reading => {}
            ReaderState::Done(_) => return Ok(None),
            ReaderState::Failed { offset, why } => {
                return Err(WalError::Corrupt {
                    offset: *offset,
                    why,
                })
            }
            ReaderState::Errored => {
                return Err(WalError::Io(io::Error::other(
                    "wal reader stopped after an earlier i/o failure",
                )))
            }
        }
        let Some(sc) = self.scanner.as_mut() else {
            return Ok(None);
        };
        match sc.next() {
            Ok(Step::Record(seq, ev)) => Ok(Some((seq, *ev))),
            Ok(Step::End) => {
                self.state = ReaderState::Done(Tail::Clean);
                Ok(None)
            }
            Ok(Step::Torn {
                extra,
                damaged_record,
            }) => {
                self.state = ReaderState::Done(match damaged_record {
                    None => Tail::Torn {
                        valid_len: sc.offset,
                        extra_bytes: extra,
                    },
                    Some(record_len) => Tail::Damaged {
                        valid_len: sc.offset,
                        extra_bytes: extra,
                        record_len,
                    },
                });
                Ok(None)
            }
            Err(WalError::Corrupt { offset, why }) => {
                self.state = ReaderState::Failed { offset, why };
                Err(WalError::Corrupt { offset, why })
            }
            Err(e) => {
                self.state = ReaderState::Errored;
                Err(e)
            }
        }
    }

    /// State of the end of the file; `None` until `next` has returned `Ok(None)`.
    pub fn tail(&self) -> Option<Tail> {
        match &self.state {
            ReaderState::Done(t) => Some(*t),
            _ => None,
        }
    }

    /// Records returned so far.
    pub fn records(&self) -> u64 {
        self.scanner.as_ref().map_or(0, |s| s.next_seq)
    }

    /// Chain value after the records returned so far.
    pub fn chain_head(&self) -> Digest {
        self.scanner
            .as_ref()
            .map_or_else(genesis_chain, |s| s.chain)
    }

    /// File length when the reader was opened.
    pub fn file_len(&self) -> u64 {
        self.file_len
    }
}

/// Read and verify the whole log without modifying it. A torn or damaged tail
/// is reported in [`VerifyReport::torn_bytes`] and
/// [`VerifyReport::damaged_tail`]; mid-file corruption is an error.
pub fn verify(path: impl AsRef<Path>) -> Result<VerifyReport, WalError> {
    let mut r = WalReader::open(path)?;
    while r.next()?.is_some() {}
    let (torn_bytes, damaged_tail) = match r.tail() {
        Some(Tail::Torn { extra_bytes, .. }) => (extra_bytes, false),
        Some(Tail::Damaged { extra_bytes, .. }) => (extra_bytes, true),
        _ => (0, false),
    };
    let records = r.records();
    Ok(VerifyReport {
        records,
        last_seq: records.checked_sub(1),
        chain_head: r.chain_head(),
        file_len: r.file_len(),
        torn_bytes,
        damaged_tail,
    })
}

impl From<WalError> for PortError {
    fn from(e: WalError) -> PortError {
        let kind = match &e {
            WalError::Io(_) => PortErrorKind::Io,
            WalError::BadHeader | WalError::Corrupt { .. } | WalError::DamagedTail { .. } => {
                PortErrorKind::Corrupt
            }
            WalError::SeqMismatch { .. } | WalError::RecordTooLarge => PortErrorKind::Invalid,
            WalError::Full => PortErrorKind::Full,
        };
        PortError::new(kind, e.to_string())
    }
}

/// Stream the records with `seq >= from` of the log at `path` into `visit`,
/// verifying the whole file as it goes. `expected_records` is the record count
/// the caller believes the file has; a different count means the file changed
/// underneath the caller.
fn replay_file(
    path: &Path,
    from: u64,
    expected_records: u64,
    visit: &mut dyn FnMut(u64, Event),
) -> Result<(), PortError> {
    let mut reader = WalReader::open(path)?;
    while let Some((seq, ev)) = reader.next()? {
        if seq >= from {
            visit(seq, ev);
        }
    }
    if reader.records() != expected_records || reader.tail() != Some(Tail::Clean) {
        return Err(PortError::new(
            PortErrorKind::Corrupt,
            "log file changed while it was open",
        ));
    }
    Ok(())
}

/// File-backed implementation of the [`EventLog`] port: a [`Wal`] that was
/// opened (and repaired) for appending.
#[derive(Debug)]
pub struct FileEventLog {
    wal: Wal,
    repaired: u64,
    sidecar: Option<PathBuf>,
}

impl FileEventLog {
    /// Open or create the log at `path`; a torn tail is saved to a sidecar
    /// file, truncated away and reported through [`EventLog::repaired_bytes`].
    /// A complete but damaged final record is refused with
    /// [`WalError::DamagedTail`] unless `cfg.repair_damaged_tail` is set.
    pub fn open(path: impl AsRef<Path>, cfg: WalConfig) -> Result<FileEventLog, WalError> {
        let (wal, report) = Wal::open(path, cfg)?;
        Ok(FileEventLog {
            wal,
            repaired: report.truncated_bytes,
            sidecar: report.sidecar,
        })
    }

    /// Sidecar file holding the bytes dropped when the log was opened, if any.
    pub fn sidecar(&self) -> Option<&Path> {
        self.sidecar.as_deref()
    }

    /// The underlying writer (for its counters and file length).
    pub fn wal(&self) -> &Wal {
        &self.wal
    }
}

impl EventLog for FileEventLog {
    fn len(&self) -> u64 {
        self.wal.records()
    }

    fn append(&mut self, seq: u64, event: &Event) -> Result<(), PortError> {
        self.wal.append(seq, event).map_err(PortError::from)
    }

    fn sync(&mut self) -> Result<(), PortError> {
        self.wal.sync().map_err(PortError::from)
    }

    fn replay_from(
        &mut self,
        from_seq: u64,
        visit: &mut dyn FnMut(u64, Event),
    ) -> Result<(), PortError> {
        replay_file(self.wal.path(), from_seq, self.wal.records(), visit)
    }

    fn repaired_bytes(&self) -> u64 {
        self.repaired
    }

    fn head(&self) -> Digest {
        self.wal.chain_head()
    }
}

/// Read-only implementation of the [`EventLog`] port for inspection and replay.
/// It never modifies the file: a torn tail is ignored (and reported through
/// [`EventLog::repaired_bytes`], although nothing is repaired) and `append`
/// always fails.
#[derive(Debug)]
pub struct ReadOnlyFileLog {
    path: PathBuf,
    report: VerifyReport,
}

impl ReadOnlyFileLog {
    /// Verify and open the log at `path`. Mid-file corruption is an error.
    pub fn open(path: impl AsRef<Path>) -> Result<ReadOnlyFileLog, WalError> {
        let path = path.as_ref().to_path_buf();
        let report = verify(&path)?;
        Ok(ReadOnlyFileLog { path, report })
    }

    /// Result of the verification done at open time.
    pub fn report(&self) -> &VerifyReport {
        &self.report
    }
}

impl EventLog for ReadOnlyFileLog {
    fn len(&self) -> u64 {
        self.report.records
    }

    fn append(&mut self, _seq: u64, _event: &Event) -> Result<(), PortError> {
        Err(PortError::new(
            PortErrorKind::Invalid,
            "log was opened read-only",
        ))
    }

    fn sync(&mut self) -> Result<(), PortError> {
        Ok(())
    }

    fn replay_from(
        &mut self,
        from_seq: u64,
        visit: &mut dyn FnMut(u64, Event),
    ) -> Result<(), PortError> {
        // A torn tail was present at open time and is still there: the reader
        // stops before it, so only the record count is compared.
        let mut reader = WalReader::open(&self.path)?;
        while let Some((seq, ev)) = reader.next()? {
            if seq >= from_seq {
                visit(seq, ev);
            }
        }
        if reader.records() != self.report.records {
            return Err(PortError::new(
                PortErrorKind::Corrupt,
                "log file changed while it was open",
            ));
        }
        Ok(())
    }

    fn repaired_bytes(&self) -> u64 {
        self.report.torn_bytes
    }

    fn head(&self) -> Digest {
        self.report.chain_head
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
pub(crate) mod test_support {
    use perp_domain::fixed::Money;
    use perp_domain::types::{Event, EventBody};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Unique temporary directory, removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("perp-engine-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }

        pub fn join(&self, name: impl AsRef<Path>) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Deterministic sample events: a mix of event kinds, increasing timestamps.
    pub fn sample_events(n: u64) -> Vec<Event> {
        (0..n)
            .map(|i| {
                let body = match i % 4 {
                    0 => EventBody::Deposit {
                        account: i % 7,
                        amount: Money(1_000 + i128::from(i)),
                    },
                    1 => EventBody::Tick,
                    2 => EventBody::FundInsurance {
                        amount: Money(5 + i128::from(i)),
                    },
                    _ => EventBody::Cancel {
                        account: i,
                        order_id: i * 3,
                    },
                };
                Event { ts: i * 10, body }
            })
            .collect()
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
    use super::test_support::{sample_events, TempDir};
    use super::*;
    use perp_domain::types::EventBody;

    fn cfg(fsync: FsyncPolicy) -> WalConfig {
        WalConfig {
            fsync,
            max_file_bytes: 1 << 30,
            ..WalConfig::default()
        }
    }

    /// Write `n` sample events with fsync disabled; return the file bytes and
    /// the offsets at which each record ends (index 0 = end of header).
    fn build_log(path: &Path, n: u64) -> (Vec<u8>, Vec<u64>) {
        let (mut wal, _) = Wal::open(path, cfg(FsyncPolicy::Never)).unwrap();
        let mut bounds = vec![wal.len()];
        for (i, ev) in sample_events(n).iter().enumerate() {
            wal.append(i as u64, ev).unwrap();
            bounds.push(wal.len());
        }
        drop(wal);
        (std::fs::read(path).unwrap(), bounds)
    }

    /// One raw record (len | payload | chain) chained after `prev`.
    fn raw_record(prev: &Digest, payload: &[u8]) -> (Vec<u8>, Digest) {
        let len_le = (payload.len() as u32).to_le_bytes();
        let chain = sha256_concat(&[prev, &len_le, payload]);
        let mut v = Vec::new();
        v.extend_from_slice(&len_le);
        v.extend_from_slice(payload);
        v.extend_from_slice(&chain);
        (v, chain)
    }

    fn payload_for(seq: u64, ev: &Event) -> Vec<u8> {
        let mut w = Writer::new_buf();
        w.u64(seq);
        ev.encode(&mut w);
        w.into_bytes()
    }

    /// Header plus records with explicit sequence numbers (possibly wrong).
    fn log_with_seqs(seqs: &[u64]) -> Vec<u8> {
        let evs = sample_events(seqs.len() as u64);
        let mut out = header_bytes().to_vec();
        let mut prev = genesis_chain();
        for (seq, ev) in seqs.iter().zip(&evs) {
            let (rec, chain) = raw_record(&prev, &payload_for(*seq, ev));
            out.extend_from_slice(&rec);
            prev = chain;
        }
        out
    }

    fn read_all(path: &Path) -> Vec<(u64, Event)> {
        let mut r = WalReader::open(path).unwrap();
        let mut out = Vec::new();
        while let Some(x) = r.next().unwrap() {
            out.push(x);
        }
        out
    }

    #[test]
    fn header_layout_is_fixed() {
        let h = header_bytes();
        assert_eq!(&h[..8], b"PERPWAL1");
        assert_eq!(&h[8..12], &1u32.to_le_bytes());
        assert_eq!(&h[12..], &[0, 0, 0, 0]);
        assert_eq!(h.len() as u64, HEADER_LEN);
    }

    #[test]
    fn missing_file_is_created_with_header() {
        let dir = TempDir::new("wal-create");
        let p = dir.join("a.wal");
        let (wal, rep) = Wal::open(&p, WalConfig::default()).unwrap();
        assert_eq!(wal.records(), 0);
        assert!(wal.is_empty());
        assert_eq!(wal.len(), HEADER_LEN);
        assert_eq!(rep.records, 0);
        assert_eq!(rep.truncated_bytes, 0);
        assert_eq!(rep.chain_head, genesis_chain());
        assert_eq!(std::fs::read(&p).unwrap(), header_bytes().to_vec());
    }

    #[test]
    fn round_trip_preserves_events_and_order() {
        let dir = TempDir::new("wal-rt");
        let p = dir.join("a.wal");
        let evs = sample_events(40);
        let (mut wal, _) = Wal::open(&p, WalConfig::default()).unwrap();
        for (i, ev) in evs.iter().enumerate() {
            wal.append(i as u64, ev).unwrap();
        }
        assert_eq!(wal.records(), 40);
        drop(wal);
        let got = read_all(&p);
        assert_eq!(got.len(), 40);
        for (i, (seq, ev)) in got.iter().enumerate() {
            assert_eq!(*seq, i as u64);
            assert_eq!(ev, &evs[i]);
        }
    }

    #[test]
    fn chain_matches_independent_computation() {
        let dir = TempDir::new("wal-chain");
        let p = dir.join("a.wal");
        let (bytes, bounds) = build_log(&p, 12);
        let mut prev = sha256(b"perp-engine wal v1 genesis");
        for w in bounds.windows(2) {
            let rec = &bytes[w[0] as usize..w[1] as usize];
            let len = u32::from_le_bytes(rec[..4].try_into().unwrap()) as usize;
            assert_eq!(rec.len(), 4 + len + 32);
            let mut h = prev.to_vec();
            h.extend_from_slice(&rec[..4 + len]);
            let expect = sha256(&h);
            assert_eq!(&rec[4 + len..], &expect);
            prev = expect;
        }
        let rep = verify(&p).unwrap();
        assert_eq!(rep.chain_head, prev);
        let (wal, _) = Wal::open(&p, WalConfig::default()).unwrap();
        assert_eq!(wal.chain_head(), prev);
    }

    #[test]
    fn reopen_continues_the_chain() {
        let dir = TempDir::new("wal-reopen");
        let p = dir.join("a.wal");
        let evs = sample_events(10);
        let (mut wal, _) = Wal::open(&p, WalConfig::default()).unwrap();
        for (i, ev) in evs.iter().take(5).enumerate() {
            wal.append(i as u64, ev).unwrap();
        }
        let head5 = wal.chain_head();
        drop(wal);
        let (mut wal, rep) = Wal::open(&p, WalConfig::default()).unwrap();
        assert_eq!(rep.records, 5);
        assert_eq!(rep.chain_head, head5);
        assert_eq!(wal.records(), 5);
        for (i, ev) in evs.iter().enumerate().skip(5) {
            wal.append(i as u64, ev).unwrap();
        }
        drop(wal);
        // A log written in two sessions equals one written in one session.
        let dir2 = TempDir::new("wal-reopen2");
        let p2 = dir2.join("b.wal");
        let (mut wal2, _) = Wal::open(&p2, WalConfig::default()).unwrap();
        for (i, ev) in evs.iter().enumerate() {
            wal2.append(i as u64, ev).unwrap();
        }
        drop(wal2);
        assert_eq!(std::fs::read(&p).unwrap(), std::fs::read(&p2).unwrap());
    }

    #[test]
    fn torn_tail_at_every_byte_offset() {
        let dir = TempDir::new("wal-torn");
        let src = dir.join("src.wal");
        let (bytes, bounds) = build_log(&src, 40);
        let evs = sample_events(41);
        let work = dir.join("work.wal");
        for cut in 0..=bytes.len() {
            std::fs::write(&work, &bytes[..cut]).unwrap();
            let (mut wal, rep) = Wal::open(&work, cfg(FsyncPolicy::Never)).unwrap();
            let complete = bounds.iter().skip(1).filter(|b| **b <= cut as u64).count();
            let boundary = if cut < HEADER_LEN as usize {
                HEADER_LEN
            } else {
                bounds[complete]
            };
            assert_eq!(rep.records, complete as u64, "cut {cut}");
            assert_eq!(rep.valid_len, boundary, "cut {cut}");
            assert_eq!(wal.len(), boundary, "cut {cut}");
            let removed = if cut < HEADER_LEN as usize {
                cut as u64
            } else {
                cut as u64 - boundary
            };
            assert_eq!(rep.truncated_bytes, removed, "cut {cut}");
            assert_eq!(
                std::fs::metadata(&work).unwrap().len(),
                boundary,
                "cut {cut}"
            );
            // Recovered prefix is byte-identical to the original.
            if cut >= HEADER_LEN as usize {
                assert_eq!(std::fs::read(&work).unwrap(), &bytes[..boundary as usize]);
            }
            // The dropped bytes were saved first, exactly, next to the log.
            if cut >= HEADER_LEN as usize && cut as u64 > boundary {
                let side = rep.sidecar.clone().expect("sidecar");
                assert_eq!(side, sidecar_path(&work, boundary), "cut {cut}");
                assert_eq!(
                    std::fs::read(&side).unwrap(),
                    &bytes[boundary as usize..cut],
                    "cut {cut}"
                );
                std::fs::remove_file(side).unwrap();
            } else {
                assert_eq!(rep.sidecar, None, "cut {cut}");
            }
            // Appending continues with a valid chain.
            wal.append(complete as u64, &evs[complete]).unwrap();
            drop(wal);
            let v = verify(&work).unwrap();
            assert_eq!(v.records, complete as u64 + 1, "cut {cut}");
            assert_eq!(v.torn_bytes, 0, "cut {cut}");
            // A second reopen is clean.
            let (_, rep2) = Wal::open(&work, cfg(FsyncPolicy::Never)).unwrap();
            assert_eq!(rep2.truncated_bytes, 0, "cut {cut}");
            assert_eq!(rep2.records, complete as u64 + 1, "cut {cut}");
        }
    }

    /// Path of the first sidecar name for a tail dropped at `offset`.
    fn sidecar_path(log: &Path, offset: u64) -> PathBuf {
        let mut name = log.file_name().unwrap().to_os_string();
        name.push(format!(".torn-{offset}"));
        log.with_file_name(name)
    }

    fn repair_cfg() -> WalConfig {
        WalConfig {
            repair_damaged_tail: true,
            ..cfg(FsyncPolicy::Never)
        }
    }

    #[test]
    fn every_single_byte_flip_in_last_record_is_damaged_or_torn_and_never_lost() {
        let dir = TempDir::new("wal-flip-last");
        let src = dir.join("src.wal");
        let (bytes, bounds) = build_log(&src, 30);
        let last_start = bounds[bounds.len() - 2] as usize;
        let record = bytes.len() - last_start;
        let work = dir.join("work.wal");
        let (mut damaged, mut torn) = (0, 0);
        for mask in [0xFFu8, 0x01, 0x80] {
            for i in last_start..bytes.len() {
                let mut b = bytes.clone();
                b[i] ^= mask;
                std::fs::write(&work, &b).unwrap();
                // The record stays complete when its (possibly flipped) length
                // field is a valid length that fits in the file.
                let len =
                    u32::from_le_bytes(b[last_start..last_start + 4].try_into().unwrap()) as usize;
                let complete = (MIN_PAYLOAD..=MAX_RECORD_BYTES).contains(&len)
                    && LEN_FIELD + len + CHAIN_LEN <= record;
                let at = format!("byte {i} mask {mask:#x}");
                // Read-only view first: reported, not repaired.
                let v = verify(&work).unwrap();
                assert_eq!(v.records, 29, "{at}");
                assert_eq!(v.torn_bytes, record as u64, "{at}");
                assert_eq!(v.damaged_tail, complete, "{at}");
                assert_eq!(std::fs::read(&work).unwrap(), b, "{at}");
                let r = ReadOnlyFileLog::open(&work).unwrap();
                assert_eq!(r.report().damaged_tail, complete, "{at}");
                assert_eq!(std::fs::read(&work).unwrap(), b, "{at}");

                if complete {
                    damaged += 1;
                    // A complete but damaged record is refused and untouched.
                    match Wal::open(&work, cfg(FsyncPolicy::Never)) {
                        Err(WalError::DamagedTail { offset, record_len }) => {
                            assert_eq!(offset, last_start as u64, "{at}");
                            assert_eq!(record_len, (LEN_FIELD + len + CHAIN_LEN) as u64, "{at}");
                        }
                        other => panic!("{at}: {:?}", other.map(|_| ())),
                    }
                    assert_eq!(std::fs::read(&work).unwrap(), b, "{at}");
                    assert!(!sidecar_path(&work, last_start as u64).exists(), "{at}");
                    // The explicit repair option saves the bytes, then truncates.
                    let (wal, rep) = Wal::open(&work, repair_cfg()).unwrap();
                    assert_eq!(rep.records, 29, "{at}");
                    assert_eq!(rep.truncated_bytes, record as u64, "{at}");
                    assert_eq!(wal.len(), last_start as u64, "{at}");
                    let side = rep.sidecar.unwrap();
                    assert_eq!(std::fs::read(&side).unwrap(), &b[last_start..], "{at}");
                    std::fs::remove_file(side).unwrap();
                } else {
                    torn += 1;
                    // An incomplete record is repaired automatically, after
                    // saving it.
                    let (wal, rep) = Wal::open(&work, cfg(FsyncPolicy::Never)).unwrap();
                    assert_eq!(rep.records, 29, "{at}");
                    assert_eq!(rep.truncated_bytes, record as u64, "{at}");
                    assert_eq!(wal.len(), last_start as u64, "{at}");
                    let side = rep.sidecar.unwrap();
                    assert_eq!(std::fs::read(&side).unwrap(), &b[last_start..], "{at}");
                    std::fs::remove_file(side).unwrap();
                }
                assert_eq!(std::fs::read(&work).unwrap(), &bytes[..last_start], "{at}");
            }
        }
        assert!(damaged > 0 && torn > 0, "both outcomes are exercised");
    }

    #[test]
    fn damaged_tail_error_explains_the_repair_option_and_maps_to_corrupt() {
        let e = WalError::DamagedTail {
            offset: 77,
            record_len: 50,
        };
        let text = e.to_string();
        assert!(text.contains("77") && text.contains("repair"), "{text}");
        let p = PortError::from(e);
        assert_eq!(p.kind, PortErrorKind::Corrupt);
        assert!(!WalConfig::default().repair_damaged_tail);
    }

    #[test]
    fn file_event_log_refuses_a_damaged_tail_and_repairs_it_on_request() {
        let dir = TempDir::new("wal-fel-damaged");
        let p = dir.join("a.wal");
        let (mut bytes, bounds) = build_log(&p, 6);
        bytes[bounds[5] as usize + 9] ^= 0x20;
        std::fs::write(&p, &bytes).unwrap();
        let err = FileEventLog::open(&p, cfg(FsyncPolicy::Never)).unwrap_err();
        assert!(matches!(err, WalError::DamagedTail { .. }));
        assert_eq!(std::fs::read(&p).unwrap(), bytes);
        let mut log = FileEventLog::open(&p, repair_cfg()).unwrap();
        assert_eq!(log.len(), 5);
        assert_eq!(log.repaired_bytes(), bounds[6] - bounds[5]);
        assert_eq!(
            std::fs::read(log.sidecar().unwrap()).unwrap(),
            &bytes[bounds[5] as usize..]
        );
        log.append(5, &sample_events(6)[5]).unwrap();
        assert_eq!(log.head(), verify(&p).unwrap().chain_head);
    }

    #[test]
    fn sidecars_never_overwrite_and_get_numeric_suffixes() {
        let dir = TempDir::new("wal-sidecar-names");
        let p = dir.join("a.wal");
        let (bytes, bounds) = build_log(&p, 4);
        let start = bounds[3];
        let torn = bytes[..bytes.len() - 5].to_vec();
        // An unrelated file already owns the plain name.
        let first = sidecar_path(&p, start);
        std::fs::write(&first, b"precious").unwrap();
        std::fs::write(&p, &torn).unwrap();
        let (_, rep) = Wal::open(&p, cfg(FsyncPolicy::Never)).unwrap();
        let second = rep.sidecar.unwrap();
        assert_eq!(
            second.file_name().unwrap().to_str().unwrap(),
            format!("a.wal.torn-{start}.1")
        );
        assert_eq!(second.parent(), p.parent());
        assert_eq!(std::fs::read(&first).unwrap(), b"precious");
        assert_eq!(std::fs::read(&second).unwrap(), &torn[start as usize..]);
        // The same damage again takes the next free name.
        std::fs::write(&p, &torn).unwrap();
        let (_, rep) = Wal::open(&p, cfg(FsyncPolicy::Never)).unwrap();
        assert_eq!(
            rep.sidecar.unwrap().file_name().unwrap().to_str().unwrap(),
            format!("a.wal.torn-{start}.2")
        );
    }

    #[test]
    fn when_the_sidecar_cannot_be_created_the_log_is_left_untouched() {
        let dir = TempDir::new("wal-sidecar-exhausted");
        let p = dir.join("a.wal");
        let (bytes, bounds) = build_log(&p, 3);
        let torn = bytes[..bytes.len() - 5].to_vec();
        std::fs::write(&p, &torn).unwrap();
        let start = bounds[2];
        std::fs::write(sidecar_path(&p, start), b"x").unwrap();
        for n in 1..=MAX_SIDECAR_SUFFIX {
            let mut name = sidecar_path(&p, start).into_os_string();
            name.push(format!(".{n}"));
            std::fs::write(name, b"x").unwrap();
        }
        assert!(matches!(
            Wal::open(&p, cfg(FsyncPolicy::Never)),
            Err(WalError::Io(_))
        ));
        assert_eq!(std::fs::read(&p).unwrap(), torn);
    }

    #[test]
    fn single_byte_flip_in_a_middle_record_is_corruption_and_file_is_untouched() {
        let dir = TempDir::new("wal-flip-mid");
        let src = dir.join("src.wal");
        let (bytes, bounds) = build_log(&src, 30);
        let work = dir.join("work.wal");
        for rec in [0usize, 10, 28] {
            let (s, e) = (bounds[rec] as usize, bounds[rec + 1] as usize);
            for mask in [0xFFu8, 0x01] {
                for i in s..e {
                    let mut b = bytes.clone();
                    b[i] ^= mask;
                    std::fs::write(&work, &b).unwrap();
                    match Wal::open(&work, cfg(FsyncPolicy::Never)) {
                        Err(WalError::Corrupt { offset, .. }) => {
                            assert_eq!(offset, s as u64, "record {rec} byte {i}");
                        }
                        other => panic!("record {rec} byte {i}: {:?}", other.map(|_| ())),
                    }
                    assert_eq!(std::fs::read(&work).unwrap(), b, "file modified");
                    assert!(matches!(verify(&work), Err(WalError::Corrupt { .. })));
                }
            }
        }
    }

    #[test]
    fn zero_filled_tail_is_truncated() {
        let dir = TempDir::new("wal-zero");
        let src = dir.join("src.wal");
        let (bytes, _) = build_log(&src, 10);
        let work = dir.join("work.wal");
        for zeros in [1usize, 3, 4, 40, 4096, 200_000] {
            let mut b = bytes.clone();
            b.resize(bytes.len() + zeros, 0);
            std::fs::write(&work, &b).unwrap();
            let v = verify(&work).unwrap();
            assert_eq!(v.torn_bytes, zeros as u64);
            let (wal, rep) = Wal::open(&work, cfg(FsyncPolicy::Never)).unwrap();
            assert_eq!(rep.records, 10);
            assert_eq!(rep.truncated_bytes, zeros as u64);
            // A zero tail up to one maximum record is saved; a longer one holds
            // nothing worth keeping.
            if zeros as u64 <= MAX_TORN_TAIL {
                let side = rep.sidecar.clone().unwrap();
                assert_eq!(std::fs::read(&side).unwrap(), vec![0u8; zeros]);
                std::fs::remove_file(side).unwrap();
            } else {
                assert_eq!(rep.sidecar, None);
            }
            assert_eq!(wal.len(), bytes.len() as u64);
            assert_eq!(std::fs::read(&work).unwrap(), bytes);
        }
    }

    #[test]
    fn short_garbage_tail_is_truncated() {
        let dir = TempDir::new("wal-garbage");
        let src = dir.join("src.wal");
        let (bytes, _) = build_log(&src, 6);
        let work = dir.join("work.wal");
        for garbage in [vec![0xABu8; 7], vec![0x01, 0x02, 0x03], vec![0xFF; 500]] {
            let mut b = bytes.clone();
            b.extend_from_slice(&garbage);
            std::fs::write(&work, &b).unwrap();
            let (_, rep) = Wal::open(&work, cfg(FsyncPolicy::Never)).unwrap();
            assert_eq!(rep.records, 6);
            assert_eq!(rep.truncated_bytes, garbage.len() as u64);
            assert_eq!(std::fs::read(&work).unwrap(), bytes);
        }
    }

    #[test]
    fn huge_claimed_length_followed_by_valid_data_is_corruption() {
        let dir = TempDir::new("wal-huge-mid");
        let src = dir.join("src.wal");
        let (bytes, bounds) = build_log(&src, 20);
        let mut b = bytes.clone();
        let s = bounds[5] as usize;
        b[s..s + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let work = dir.join("work.wal");
        std::fs::write(&work, &b).unwrap();
        assert!(matches!(
            Wal::open(&work, WalConfig::default()),
            Err(WalError::Corrupt { offset, .. }) if offset == s as u64
        ));
        assert_eq!(std::fs::read(&work).unwrap(), b);
    }

    #[test]
    fn huge_claimed_length_with_long_nonzero_tail_is_corruption() {
        let dir = TempDir::new("wal-huge-long");
        let src = dir.join("src.wal");
        let (bytes, bounds) = build_log(&src, 5);
        let mut b = bytes.clone();
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        b.extend(std::iter::repeat_n(0x5Au8, 100_000));
        let work = dir.join("work.wal");
        std::fs::write(&work, &b).unwrap();
        match Wal::open(&work, WalConfig::default()) {
            Err(WalError::Corrupt { offset, why }) => {
                assert_eq!(offset, *bounds.last().unwrap());
                assert_eq!(why, "invalid record length");
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
        assert_eq!(std::fs::read(&work).unwrap(), b);
        assert!(matches!(verify(&work), Err(WalError::Corrupt { .. })));
    }

    #[test]
    fn huge_claimed_length_at_the_tail_is_torn() {
        let dir = TempDir::new("wal-huge-tail");
        let src = dir.join("src.wal");
        let (bytes, _) = build_log(&src, 5);
        let mut b = bytes.clone();
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        b.extend(std::iter::repeat_n(0x5Au8, 100));
        let work = dir.join("work.wal");
        std::fs::write(&work, &b).unwrap();
        let (_, rep) = Wal::open(&work, WalConfig::default()).unwrap();
        assert_eq!(rep.records, 5);
        assert_eq!(rep.truncated_bytes, 104);
        assert_eq!(std::fs::read(&work).unwrap(), bytes);
    }

    #[test]
    fn corruption_followed_by_a_torn_last_record_is_still_corruption() {
        // Record 3 damaged, record 4 valid, record 5 half written.
        let dir = TempDir::new("wal-mid-and-torn");
        let src = dir.join("src.wal");
        let (bytes, bounds) = build_log(&src, 6);
        let mut b = bytes[..bounds[5] as usize + 10].to_vec();
        b[bounds[3] as usize + 6] ^= 0x40;
        let work = dir.join("work.wal");
        std::fs::write(&work, &b).unwrap();
        assert!(matches!(
            Wal::open(&work, WalConfig::default()),
            Err(WalError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&work).unwrap(), b);
    }

    #[test]
    fn header_validation() {
        let dir = TempDir::new("wal-header");
        let work = dir.join("h.wal");
        let good = header_bytes();
        let mut cases: Vec<Vec<u8>> = Vec::new();
        let mut wrong_magic = good.to_vec();
        wrong_magic[0] ^= 1;
        cases.push(wrong_magic);
        let mut wrong_version = good.to_vec();
        wrong_version[8] = 2;
        cases.push(wrong_version);
        let mut reserved = good.to_vec();
        reserved[15] = 1;
        cases.push(reserved);
        cases.push(b"NOTAWAL".to_vec());
        cases.push(b"PERPWAL1\x02".to_vec());
        cases.push(vec![0xFF; 40]);
        for c in cases {
            std::fs::write(&work, &c).unwrap();
            assert!(matches!(
                Wal::open(&work, WalConfig::default()),
                Err(WalError::BadHeader)
            ));
            assert!(matches!(WalReader::open(&work), Err(WalError::BadHeader)));
            assert!(matches!(verify(&work), Err(WalError::BadHeader)));
            assert_eq!(std::fs::read(&work).unwrap(), c, "file modified");
        }
    }

    #[test]
    fn torn_header_creation_is_rewritten() {
        let dir = TempDir::new("wal-torn-header");
        let work = dir.join("h.wal");
        for n in 0..HEADER_LEN as usize {
            std::fs::write(&work, &header_bytes()[..n]).unwrap();
            let v = verify(&work).unwrap();
            assert_eq!(v.records, 0);
            assert_eq!(v.torn_bytes, n as u64);
            let (wal, rep) = Wal::open(&work, WalConfig::default()).unwrap();
            assert_eq!(rep.truncated_bytes, n as u64);
            assert_eq!(rep.valid_len, HEADER_LEN);
            assert_eq!(wal.records(), 0);
            assert_eq!(std::fs::read(&work).unwrap(), header_bytes().to_vec());
        }
    }

    #[test]
    fn header_only_file_is_an_empty_clean_log() {
        let dir = TempDir::new("wal-empty");
        let work = dir.join("h.wal");
        std::fs::write(&work, header_bytes()).unwrap();
        let mut r = WalReader::open(&work).unwrap();
        assert!(r.tail().is_none());
        assert!(r.next().unwrap().is_none());
        assert_eq!(r.tail(), Some(Tail::Clean));
        let v = verify(&work).unwrap();
        assert_eq!(v.records, 0);
        assert_eq!(v.last_seq, None);
        assert_eq!(v.chain_head, genesis_chain());
        assert_eq!(v.file_len, HEADER_LEN);
    }

    #[test]
    fn sequence_gap_or_duplicate_is_corruption() {
        let dir = TempDir::new("wal-seq");
        let work = dir.join("s.wal");
        for (seqs, bad_index) in [
            (vec![0u64, 1, 3], 2usize),
            (vec![0, 1, 1], 2),
            (vec![1, 2, 3], 0),
            (vec![0, 2], 1),
        ] {
            let bytes = log_with_seqs(&seqs);
            std::fs::write(&work, &bytes).unwrap();
            match Wal::open(&work, WalConfig::default()) {
                Err(WalError::Corrupt { why, offset }) => {
                    assert_eq!(why, "sequence number out of order");
                    let valid = log_with_seqs(&(0..bad_index as u64).collect::<Vec<_>>());
                    assert_eq!(offset, valid.len() as u64, "{seqs:?}");
                }
                other => panic!("{seqs:?}: {:?}", other.map(|_| ())),
            }
            assert_eq!(std::fs::read(&work).unwrap(), bytes);
        }
    }

    #[test]
    fn valid_chain_with_undecodable_payload_is_corruption_even_at_eof() {
        let dir = TempDir::new("wal-undecodable");
        let work = dir.join("u.wal");
        let ev = sample_events(1).remove(0);
        let good = payload_for(0, &ev);
        let mut trailing = good.clone();
        trailing.push(0);
        let mut bad_tag = 0u64.to_le_bytes().to_vec();
        bad_tag.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0xEE]);
        let short = vec![1u8; MIN_PAYLOAD]; // seq decodes, event does not
        let max = vec![0u8; MAX_RECORD_BYTES];
        for (payload, why) in [
            (trailing, "payload has trailing bytes"),
            (bad_tag, "payload does not decode"),
            (short, "payload does not decode"),
            (max, "payload has trailing bytes"),
        ] {
            let mut bytes = header_bytes().to_vec();
            bytes.extend_from_slice(&raw_record(&genesis_chain(), &payload).0);
            std::fs::write(&work, &bytes).unwrap();
            match Wal::open(&work, WalConfig::default()) {
                Err(WalError::Corrupt { offset, why: w }) => {
                    assert_eq!(offset, HEADER_LEN);
                    assert_eq!(w, why);
                }
                other => panic!("{:?}", other.map(|_| ())),
            }
            assert_eq!(std::fs::read(&work).unwrap(), bytes);
        }
    }

    #[test]
    fn payload_length_bounds_are_enforced() {
        let dir = TempDir::new("wal-len-bounds");
        let work = dir.join("l.wal");
        // Below the minimum: not a valid record; as the last record it is a
        // torn tail.
        for n in [0usize, 8] {
            let mut bytes = header_bytes().to_vec();
            bytes.extend_from_slice(&raw_record(&genesis_chain(), &vec![7u8; n]).0);
            std::fs::write(&work, &bytes).unwrap();
            let (_, rep) = Wal::open(&work, WalConfig::default()).unwrap();
            assert_eq!(rep.records, 0, "payload {n}");
            assert_eq!(rep.valid_len, HEADER_LEN);
        }
        // One byte above the maximum: the damaged region is longer than any
        // single valid record could be, so it is not accepted as a torn tail.
        let mut bytes = header_bytes().to_vec();
        bytes.extend_from_slice(&raw_record(&genesis_chain(), &vec![7u8; MAX_RECORD_BYTES + 1]).0);
        std::fs::write(&work, &bytes).unwrap();
        assert!(matches!(
            Wal::open(&work, WalConfig::default()),
            Err(WalError::Corrupt {
                offset: 16,
                why: "invalid record length"
            })
        ));
        assert_eq!(std::fs::read(&work).unwrap(), bytes);
    }

    #[test]
    fn frame_len_rejects_oversize_payloads() {
        assert_eq!(frame_len(MIN_PAYLOAD).unwrap(), MIN_RECORD);
        assert_eq!(frame_len(MAX_RECORD_BYTES).unwrap(), MAX_RECORD_BYTES + 36);
        assert!(matches!(
            frame_len(MAX_RECORD_BYTES + 1),
            Err(WalError::RecordTooLarge)
        ));
    }

    #[test]
    fn append_requires_the_next_sequence_number() {
        let dir = TempDir::new("wal-seqmismatch");
        let p = dir.join("a.wal");
        let evs = sample_events(3);
        let (mut wal, _) = Wal::open(&p, WalConfig::default()).unwrap();
        for bad in [1u64, 5, u64::MAX] {
            match wal.append(bad, &evs[0]) {
                Err(WalError::SeqMismatch { expected, got }) => {
                    assert_eq!(expected, 0);
                    assert_eq!(got, bad);
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(wal.records(), 0);
        assert_eq!(wal.len(), HEADER_LEN);
        wal.append(0, &evs[0]).unwrap();
        assert!(matches!(
            wal.append(0, &evs[1]),
            Err(WalError::SeqMismatch {
                expected: 1,
                got: 0
            })
        ));
        wal.append(1, &evs[1]).unwrap();
        assert_eq!(verify(&p).unwrap().records, 2);
    }

    #[test]
    fn fsync_policy_always_syncs_every_append() {
        let dir = TempDir::new("wal-fsync-always");
        let (mut wal, _) = Wal::open(dir.join("a.wal"), cfg(FsyncPolicy::Always)).unwrap();
        assert_eq!(wal.syncs(), 0);
        for (i, ev) in sample_events(5).iter().enumerate() {
            wal.append(i as u64, ev).unwrap();
            assert_eq!(wal.syncs(), i as u64 + 1);
        }
        wal.sync().unwrap(); // nothing pending
        assert_eq!(wal.syncs(), 5);
    }

    #[test]
    fn fsync_policy_every_n_syncs_in_batches() {
        let dir = TempDir::new("wal-fsync-n");
        let (mut wal, _) = Wal::open(dir.join("a.wal"), cfg(FsyncPolicy::EveryN(3))).unwrap();
        let evs = sample_events(7);
        for (i, ev) in evs.iter().enumerate() {
            wal.append(i as u64, ev).unwrap();
        }
        assert_eq!(wal.syncs(), 2);
        wal.sync().unwrap(); // one record pending
        assert_eq!(wal.syncs(), 3);
        wal.sync().unwrap();
        assert_eq!(wal.syncs(), 3);
    }

    #[test]
    fn fsync_policy_every_zero_behaves_like_always() {
        let dir = TempDir::new("wal-fsync-0");
        let (mut wal, _) = Wal::open(dir.join("a.wal"), cfg(FsyncPolicy::EveryN(0))).unwrap();
        for (i, ev) in sample_events(4).iter().enumerate() {
            wal.append(i as u64, ev).unwrap();
        }
        assert_eq!(wal.syncs(), 4);
    }

    #[test]
    fn fsync_policy_never_syncs_only_on_request() {
        let dir = TempDir::new("wal-fsync-never");
        let (mut wal, _) = Wal::open(dir.join("a.wal"), cfg(FsyncPolicy::Never)).unwrap();
        for (i, ev) in sample_events(6).iter().enumerate() {
            wal.append(i as u64, ev).unwrap();
        }
        assert_eq!(wal.syncs(), 0);
        wal.sync().unwrap();
        assert_eq!(wal.syncs(), 1);
    }

    #[test]
    fn max_file_bytes_is_enforced_without_side_effects() {
        let dir = TempDir::new("wal-full");
        let p = dir.join("a.wal");
        let evs = sample_events(4);
        // Size of the first two records.
        let probe = dir.join("probe.wal");
        let (bytes, bounds) = build_log(&probe, 4);
        drop(bytes);
        let limit = bounds[2];
        let (mut wal, _) = Wal::open(
            &p,
            WalConfig {
                fsync: FsyncPolicy::Never,
                max_file_bytes: limit,
                ..WalConfig::default()
            },
        )
        .unwrap();
        wal.append(0, &evs[0]).unwrap();
        wal.append(1, &evs[1]).unwrap();
        let head = wal.chain_head();
        assert_eq!(wal.len(), limit);
        assert!(matches!(wal.append(2, &evs[2]), Err(WalError::Full)));
        assert_eq!(wal.records(), 2);
        assert_eq!(wal.chain_head(), head);
        assert_eq!(std::fs::metadata(&p).unwrap().len(), limit);
        // The same call fails the same way; the log is still valid.
        assert!(matches!(wal.append(2, &evs[2]), Err(WalError::Full)));
        assert_eq!(verify(&p).unwrap().records, 2);
    }

    #[test]
    fn max_file_bytes_below_header_refuses_everything() {
        let dir = TempDir::new("wal-full-tiny");
        let (mut wal, _) = Wal::open(
            dir.join("a.wal"),
            WalConfig {
                fsync: FsyncPolicy::Never,
                max_file_bytes: 10,
                ..WalConfig::default()
            },
        )
        .unwrap();
        assert!(matches!(
            wal.append(0, &sample_events(1)[0]),
            Err(WalError::Full)
        ));
        assert_eq!(wal.records(), 0);
    }

    #[test]
    fn verify_and_reader_never_modify_the_file() {
        let dir = TempDir::new("wal-readonly");
        let src = dir.join("src.wal");
        let (bytes, bounds) = build_log(&src, 12);
        let work = dir.join("work.wal");
        // Clean, torn (partial record), and zero-padded files.
        let mut torn = bytes[..bytes.len() - 7].to_vec();
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0u8; 33]);
        for content in [bytes.clone(), std::mem::take(&mut torn), padded] {
            std::fs::write(&work, &content).unwrap();
            let before = std::fs::read(&work).unwrap();
            let v = verify(&work).unwrap();
            let mut r = WalReader::open(&work).unwrap();
            while r.next().unwrap().is_some() {}
            assert_eq!(std::fs::read(&work).unwrap(), before);
            assert_eq!(v.file_len, before.len() as u64);
        }
        // Torn: 11 complete records, 7 bytes missing from the 12th.
        std::fs::write(&work, &bytes[..bytes.len() - 7]).unwrap();
        let v = verify(&work).unwrap();
        assert_eq!(v.records, 11);
        assert_eq!(v.last_seq, Some(10));
        assert_eq!(v.torn_bytes, bounds[12] - bounds[11] - 7);
    }

    #[test]
    fn reader_and_verify_report_a_damaged_tail_without_touching_it() {
        let dir = TempDir::new("wal-reader-damaged");
        let src = dir.join("src.wal");
        let (bytes, bounds) = build_log(&src, 8);
        let work = dir.join("work.wal");
        let mut b = bytes.clone();
        b[bounds[7] as usize + 12] ^= 0x04;
        std::fs::write(&work, &b).unwrap();
        let mut r = WalReader::open(&work).unwrap();
        while r.next().unwrap().is_some() {}
        assert_eq!(
            r.tail(),
            Some(Tail::Damaged {
                valid_len: bounds[7],
                extra_bytes: bounds[8] - bounds[7],
                record_len: bounds[8] - bounds[7],
            })
        );
        let v = verify(&work).unwrap();
        assert!(v.damaged_tail);
        assert_eq!(v.torn_bytes, bounds[8] - bounds[7]);
        let mut log = ReadOnlyFileLog::open(&work).unwrap();
        assert_eq!(log.len(), 7);
        assert_eq!(log.repaired_bytes(), bounds[8] - bounds[7]);
        log.replay_from(0, &mut |_, _| {}).unwrap();
        assert_eq!(std::fs::read(&work).unwrap(), b, "file untouched");
        // A clean or merely torn log is not reported as damaged.
        assert!(!verify(&src).unwrap().damaged_tail);
        std::fs::write(&work, &bytes[..bytes.len() - 2]).unwrap();
        assert!(!verify(&work).unwrap().damaged_tail);
    }

    #[test]
    fn reader_reports_tail_and_sticky_corruption() {
        let dir = TempDir::new("wal-reader");
        let src = dir.join("src.wal");
        let (bytes, bounds) = build_log(&src, 8);
        let work = dir.join("work.wal");
        std::fs::write(&work, &bytes[..bytes.len() - 1]).unwrap();
        let mut r = WalReader::open(&work).unwrap();
        let mut n = 0;
        while r.next().unwrap().is_some() {
            n += 1;
            assert!(r.tail().is_none());
        }
        assert_eq!(n, 7);
        assert_eq!(r.records(), 7);
        assert_eq!(
            r.tail(),
            Some(Tail::Torn {
                valid_len: bounds[7],
                extra_bytes: bounds[8] - bounds[7] - 1
            })
        );
        assert!(r.next().unwrap().is_none());

        let mut b = bytes.clone();
        b[bounds[2] as usize + 5] ^= 1;
        std::fs::write(&work, &b).unwrap();
        let mut r = WalReader::open(&work).unwrap();
        assert!(r.next().unwrap().is_some());
        assert!(r.next().unwrap().is_some());
        for _ in 0..3 {
            assert!(matches!(
                r.next(),
                Err(WalError::Corrupt { offset, .. }) if offset == bounds[2]
            ));
        }
        assert_eq!(r.records(), 2);
        assert!(r.tail().is_none());
    }

    #[test]
    fn missing_file_and_directory_paths_are_io_errors() {
        let dir = TempDir::new("wal-io");
        assert!(matches!(
            WalReader::open(dir.join("nope.wal")),
            Err(WalError::Io(e)) if e.kind() == io::ErrorKind::NotFound
        ));
        assert!(matches!(verify(dir.join("nope.wal")), Err(WalError::Io(_))));
        assert!(matches!(
            Wal::open(dir.path(), WalConfig::default()),
            Err(WalError::Io(_))
        ));
        assert!(matches!(
            Wal::open(dir.join("no-such-dir/x.wal"), WalConfig::default()),
            Err(WalError::Io(_))
        ));
    }

    #[test]
    fn repeated_open_append_cycles_keep_one_valid_chain() {
        let dir = TempDir::new("wal-cycles");
        let p = dir.join("a.wal");
        let evs = sample_events(60);
        let mut next = 0usize;
        for cycle in 0..20 {
            let (mut wal, rep) = Wal::open(&p, cfg(FsyncPolicy::EveryN(2))).unwrap();
            assert_eq!(rep.records, next as u64, "cycle {cycle}");
            assert_eq!(rep.truncated_bytes, 0);
            for _ in 0..3 {
                wal.append(next as u64, &evs[next]).unwrap();
                next += 1;
            }
            wal.sync().unwrap();
        }
        let v = verify(&p).unwrap();
        assert_eq!(v.records, 60);
        assert_eq!(v.last_seq, Some(59));
        let got = read_all(&p);
        for (i, (seq, ev)) in got.iter().enumerate() {
            assert_eq!(*seq, i as u64);
            assert_eq!(ev, &evs[i]);
        }
    }

    #[test]
    fn crash_between_cycles_with_partial_append_recovers() {
        let dir = TempDir::new("wal-crash-cycles");
        let p = dir.join("a.wal");
        let evs = sample_events(30);
        let mut next = 0usize;
        for cycle in 0..10usize {
            let (mut wal, rep) = Wal::open(&p, cfg(FsyncPolicy::Never)).unwrap();
            assert_eq!(rep.records, next as u64);
            for _ in 0..3 {
                wal.append(next as u64, &evs[next]).unwrap();
                next += 1;
            }
            drop(wal);
            // Simulate a crash during the next append: a partial record.
            let mut b = std::fs::read(&p).unwrap();
            b.extend_from_slice(&[0x20, 0x00, 0x00, 0x00, cycle as u8, 1, 2]);
            std::fs::write(&p, &b).unwrap();
        }
        let (wal, rep) = Wal::open(&p, WalConfig::default()).unwrap();
        assert_eq!(rep.records, 30);
        assert_eq!(wal.records(), 30);
        assert_eq!(read_all(&p).len(), 30);
    }

    #[test]
    fn all_event_kinds_survive_a_round_trip() {
        use perp_domain::fixed::{Price, Qty};
        use perp_domain::types::{
            EngineConfig, MarketConfig, OrderKind, PlaceOrder, Side, StpMode, Tif,
        };
        let dir = TempDir::new("wal-kinds");
        let p = dir.join("a.wal");
        let bodies = vec![
            EventBody::Configure(EngineConfig::default()),
            EventBody::AddMarket(MarketConfig::example(1)),
            EventBody::SetFeeTier {
                account: 1,
                tier: 1,
            },
            EventBody::Place(PlaceOrder {
                market: 1,
                account: 1,
                client_id: 9,
                side: Side::Buy,
                qty: Qty(10_000),
                kind: OrderKind::Limit {
                    price: Price(1_000_000),
                    tif: Tif::Gtc,
                },
                reduce_only: false,
                stp: StpMode::CancelNewest,
            }),
            EventBody::CancelAll {
                account: 1,
                market: Some(1),
            },
            EventBody::Oracle {
                market: 1,
                index_price: Price(5),
                source_ts: 3,
            },
            EventBody::Tick,
        ];
        let (mut wal, _) = Wal::open(&p, WalConfig::default()).unwrap();
        let evs: Vec<Event> = bodies
            .into_iter()
            .enumerate()
            .map(|(i, body)| Event { ts: i as u64, body })
            .collect();
        for (i, ev) in evs.iter().enumerate() {
            wal.append(i as u64, ev).unwrap();
        }
        drop(wal);
        let got: Vec<Event> = read_all(&p).into_iter().map(|(_, e)| e).collect();
        assert_eq!(got, evs);
    }

    #[test]
    fn error_display_and_conversion() {
        let e: WalError = io::Error::other("boom").into();
        assert!(e.to_string().contains("boom"));
        assert!(std::error::Error::source(&e).is_some());
        let c = WalError::Corrupt {
            offset: 12,
            why: "chain mismatch",
        };
        assert_eq!(c.to_string(), "wal corrupt at byte 12: chain mismatch");
        assert!(std::error::Error::source(&c).is_none());
        assert!(WalError::SeqMismatch {
            expected: 1,
            got: 2
        }
        .to_string()
        .contains("expected 1, got 2"));
        assert!(!WalError::BadHeader.to_string().is_empty());
        assert!(!WalError::RecordTooLarge.to_string().is_empty());
        assert!(!WalError::Full.to_string().is_empty());
    }

    #[test]
    fn default_config_values() {
        let c = WalConfig::default();
        assert_eq!(c.fsync, FsyncPolicy::Always);
        assert_eq!(c.max_file_bytes, 1 << 30);
    }

    #[test]
    fn wal_errors_map_to_port_error_kinds() {
        let kind = |e: WalError| PortError::from(e).kind;
        assert_eq!(kind(WalError::Io(io::Error::other("x"))), PortErrorKind::Io);
        assert_eq!(kind(WalError::BadHeader), PortErrorKind::Corrupt);
        assert_eq!(
            kind(WalError::Corrupt {
                offset: 1,
                why: "x"
            }),
            PortErrorKind::Corrupt
        );
        assert_eq!(
            kind(WalError::SeqMismatch {
                expected: 1,
                got: 2
            }),
            PortErrorKind::Invalid
        );
        assert_eq!(kind(WalError::RecordTooLarge), PortErrorKind::Invalid);
        assert_eq!(kind(WalError::Full), PortErrorKind::Full);
        assert_eq!(
            kind(WalError::DamagedTail {
                offset: 1,
                record_len: 2
            }),
            PortErrorKind::Corrupt
        );
        let p = PortError::from(WalError::Corrupt {
            offset: 9,
            why: "chain mismatch",
        });
        assert!(p.message.contains("byte 9") && p.message.contains("chain mismatch"));
    }

    #[test]
    fn file_event_log_reports_the_repair_and_continues() {
        let dir = TempDir::new("wal-fel");
        let p = dir.join("a.wal");
        let (bytes, bounds) = build_log(&p, 6);
        std::fs::write(&p, &bytes[..bytes.len() - 5]).unwrap();
        let mut log = FileEventLog::open(&p, cfg(FsyncPolicy::Never)).unwrap();
        assert_eq!(log.len(), 5);
        assert_eq!(log.repaired_bytes(), bounds[6] - bounds[5] - 5);
        assert_eq!(log.wal().len(), bounds[5]);
        assert_eq!(log.wal().path(), p.as_path());
        log.append(5, &sample_events(6)[5]).unwrap();
        assert_eq!(log.head(), verify(&p).unwrap().chain_head);
        let mut seen = Vec::new();
        log.replay_from(4, &mut |seq, _| seen.push(seq)).unwrap();
        assert_eq!(seen, vec![4, 5]);
    }

    #[test]
    fn replay_notices_a_log_file_that_changed_underneath() {
        let dir = TempDir::new("wal-changed");
        let p = dir.join("a.wal");
        let (bytes, bounds) = build_log(&p, 8);
        let mut writable = FileEventLog::open(&p, cfg(FsyncPolicy::Never)).unwrap();
        let mut read_only = ReadOnlyFileLog::open(&p).unwrap();
        assert_eq!(read_only.report().records, 8);
        std::fs::write(&p, &bytes[..bounds[5] as usize]).unwrap();
        for r in [
            writable.replay_from(0, &mut |_, _| {}),
            read_only.replay_from(0, &mut |_, _| {}),
        ] {
            let e = r.unwrap_err();
            assert_eq!(e.kind, PortErrorKind::Corrupt);
            assert!(e.message.contains("changed"));
        }
    }

    #[test]
    fn read_only_log_reports_a_torn_tail_without_repairing_it() {
        let dir = TempDir::new("wal-ro");
        let p = dir.join("a.wal");
        let (bytes, bounds) = build_log(&p, 4);
        let torn = &bytes[..bytes.len() - 3];
        std::fs::write(&p, torn).unwrap();
        let mut log = ReadOnlyFileLog::open(&p).unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log.repaired_bytes(), bounds[4] - bounds[3] - 3);
        let mut n = 0;
        log.replay_from(0, &mut |_, _| n += 1).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            log.append(3, &sample_events(4)[3]).unwrap_err().kind,
            PortErrorKind::Invalid
        );
        assert!(log.sync().is_ok());
        assert_eq!(std::fs::read(&p).unwrap(), torn);
        assert_eq!(log.head(), verify(&p).unwrap().chain_head);
    }
}
