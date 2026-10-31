//! JSON-lines-over-TCP front end, generic over the application's
//! [`ExchangeApi`] driving port.
//!
//! # Threat model (read this first)
//!
//! * The transport is **plaintext TCP**. There is no TLS. Anyone who can read
//!   the network path can read the token and every request.
//! * Authentication is **one shared token** with full privileges (it can
//!   deposit, withdraw and place orders for any account). There are no users,
//!   roles, per-account permissions or token expiry.
//! * By default the server refuses to bind anything but a loopback address.
//!   [`ServerConfig::allow_non_loopback`] lifts that; do not do so on an
//!   untrusted network. This server is a teaching tool and is **not** meant to
//!   face untrusted networks.
//! * What it does defend against, within those limits: unauthenticated clients
//!   (nothing is processed before a valid `auth`; failures are identical,
//!   delayed and counted), oversized lines (bounded reads), slow or silent
//!   clients (deadlines per line, not per byte), request floods (per-connection
//!   token bucket, disconnect on sustained abuse), connection floods (hard
//!   connection cap) and a slow engine (bounded queue; connection threads stop
//!   reading their sockets while the queue is full). There is no per-IP limit,
//!   so one host can occupy every connection slot.
//! * The token is compared by digest in constant time and is never logged,
//!   echoed or placed in an error. Log lines contain peer addresses and counts
//!   only, never request contents.
//!
//! # Architecture
//!
//! `std::net` and threads only; no async runtime.
//!
//! * one accept thread (the caller of [`Server::run`]), non-blocking accept
//!   plus a short sleep so shutdown is prompt;
//! * one thread per connection, at most [`ServerConfig::max_connections`];
//! * one backend thread that owns the [`ExchangeApi`] and receives jobs over a
//!   bounded `sync_channel`. A connection has at most one request in flight
//!   and does not read its next line before it has written the reply, so a
//!   slow backend back-pressures connections, then sockets, then clients.
//!
//! The server does not stamp events: the application owns the clock and
//! assigns each submitted body its timestamp, so clients cannot supply one.
//!
//! An [`ApiError::Storage`] is treated as fatal: the backend thread stops,
//! pending and later requests get `unavailable`, and [`Server::run`] returns
//! an error. The engine's in-memory state and its log may disagree after a
//! failed write, so continuing would be unsafe.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use perp_app::api::{ApiError, ExchangeApi};
use perp_domain::types::{AccountId, EventBody, MarketId, Output};
use perp_domain::views::{AccountView, BookView, Summary};

use crate::protocol::{self, Request};
use crate::token::Token;

/// Granularity at which blocked reads wake up to check deadlines and shutdown.
const POLL: Duration = Duration::from_millis(50);
/// Sleep between non-blocking accept attempts.
const ACCEPT_POLL: Duration = Duration::from_millis(10);
/// Size of the per-connection read buffer.
const READ_CHUNK: usize = 4096;
/// Stack size of connection threads.
const CONN_STACK: usize = 512 * 1024;
/// Time allowed for the one-line rejection sent to surplus connections.
const REJECT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// Smallest accepted `max_line_bytes`.
pub const MIN_LINE_BYTES: usize = 64;
/// Largest accepted `max_line_bytes`.
pub const MAX_LINE_BYTES_LIMIT: usize = 1 << 20;
/// Largest accepted `max_connections`.
pub const MAX_CONNECTIONS_LIMIT: usize = 4096;
/// Largest accepted `queue_depth`.
pub const MAX_QUEUE_DEPTH: usize = 65_536;
/// Largest accepted timeout or delay.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// Server settings. Build with [`ServerConfig::new`] and adjust fields.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to listen on (default `127.0.0.1:7878`).
    pub listen: SocketAddr,
    /// Allow binding a non-loopback address (default `false`).
    pub allow_non_loopback: bool,
    /// The shared secret clients must present.
    pub token: Token,
    /// Maximum simultaneous connections (default 32). Surplus connections get
    /// one error line and are closed.
    pub max_connections: usize,
    /// Maximum request line length in bytes, excluding the newline (default
    /// 4096).
    pub max_line_bytes: usize,
    /// Sustained requests per second allowed per connection (default 200).
    pub rate_per_sec: u32,
    /// Burst size of the per-connection token bucket (default 400).
    pub burst: u32,
    /// Disconnect after this many consecutive rate-limited requests (default
    /// 50).
    pub rate_limit_disconnect_after: u32,
    /// Time a new connection has to authenticate (default 5 s).
    pub auth_timeout: Duration,
    /// Time an authenticated connection may stay silent between requests
    /// (default 60 s). Applies to the whole line, so trickling bytes does not
    /// extend it.
    pub idle_timeout: Duration,
    /// Failed `auth` attempts before disconnecting (default 3).
    pub max_auth_failures: u32,
    /// Fixed pause before every auth failure response (default 200 ms).
    pub auth_failure_delay: Duration,
    /// Total time allowed to write one response (default 5 s).
    pub write_timeout: Duration,
    /// Capacity of the queue in front of the backend thread (default 256).
    pub queue_depth: usize,
}

impl ServerConfig {
    /// Defaults for everything except the token.
    pub fn new(token: Token) -> ServerConfig {
        ServerConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 7878)),
            allow_non_loopback: false,
            token,
            max_connections: 32,
            max_line_bytes: 4096,
            rate_per_sec: 200,
            burst: 400,
            rate_limit_disconnect_after: 50,
            auth_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(60),
            max_auth_failures: 3,
            auth_failure_delay: Duration::from_millis(200),
            write_timeout: Duration::from_secs(5),
            queue_depth: 256,
        }
    }

    fn validate(&self) -> io::Result<()> {
        let invalid = |msg: &str| Err(io::Error::new(io::ErrorKind::InvalidInput, msg.to_string()));
        if !self.allow_non_loopback && !self.listen.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "refusing to listen on non-loopback address {}: traffic is plaintext and one \
                     shared token grants full control; set allow_non_loopback (--allow-non-loopback) \
                     only on a network you trust",
                    self.listen
                ),
            ));
        }
        if self.max_connections == 0 || self.max_connections > MAX_CONNECTIONS_LIMIT {
            return invalid("max_connections must be between 1 and 4096");
        }
        if self.max_line_bytes < MIN_LINE_BYTES || self.max_line_bytes > MAX_LINE_BYTES_LIMIT {
            return invalid("max_line_bytes must be between 64 and 1048576");
        }
        if self.rate_per_sec == 0 || self.rate_per_sec > 1_000_000 {
            return invalid("rate_per_sec must be between 1 and 1000000");
        }
        if self.burst == 0 || self.burst > 1_000_000 {
            return invalid("burst must be between 1 and 1000000");
        }
        if self.rate_limit_disconnect_after == 0 {
            return invalid("rate_limit_disconnect_after must be at least 1");
        }
        if self.max_auth_failures == 0 {
            return invalid("max_auth_failures must be at least 1");
        }
        if self.queue_depth == 0 || self.queue_depth > MAX_QUEUE_DEPTH {
            return invalid("queue_depth must be between 1 and 65536");
        }
        for (name, d) in [
            ("auth_timeout", self.auth_timeout),
            ("idle_timeout", self.idle_timeout),
            ("write_timeout", self.write_timeout),
        ] {
            if d.is_zero() || d > MAX_TIMEOUT {
                return invalid(&format!("{name} must be between 1 ns and 24 h"));
            }
        }
        if self.auth_failure_delay > MAX_TIMEOUT {
            return invalid("auth_failure_delay must be at most 24 h");
        }
        Ok(())
    }
}

/// Snapshot of the server's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServerStats {
    /// Connections admitted.
    pub connections_accepted: u64,
    /// Connections turned away because the cap was reached (or a thread could
    /// not be started).
    pub connections_rejected: u64,
    /// Connections currently open.
    pub active_connections: u64,
    /// Highest number of simultaneously open connections.
    pub peak_connections: u64,
    /// Lines that were processed (not rate limited), authenticated or not.
    pub requests: u64,
    /// Successful `auth` requests.
    pub auth_successes: u64,
    /// Failed authentication attempts of any kind.
    pub auth_failures: u64,
    /// Requests answered with `rate_limited`.
    pub rate_limited: u64,
    /// Lines longer than the limit.
    pub oversize_lines: u64,
    /// Lines that were not valid UTF-8.
    pub invalid_utf8: u64,
    /// Authenticated requests that failed to decode.
    pub invalid_requests: u64,
    /// Connections closed by the auth or idle deadline.
    pub timeouts: u64,
    /// Jobs placed on the backend queue.
    pub backend_enqueued: u64,
}

#[derive(Default)]
struct SharedStats {
    accepted: AtomicU64,
    rejected: AtomicU64,
    active: AtomicU64,
    peak: AtomicU64,
    requests: AtomicU64,
    auth_successes: AtomicU64,
    auth_failures: AtomicU64,
    rate_limited: AtomicU64,
    oversize: AtomicU64,
    invalid_utf8: AtomicU64,
    invalid_requests: AtomicU64,
    timeouts: AtomicU64,
    enqueued: AtomicU64,
}

fn inc(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

impl SharedStats {
    fn snapshot(&self) -> ServerStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        ServerStats {
            connections_accepted: get(&self.accepted),
            connections_rejected: get(&self.rejected),
            active_connections: get(&self.active),
            peak_connections: get(&self.peak),
            requests: get(&self.requests),
            auth_successes: get(&self.auth_successes),
            auth_failures: get(&self.auth_failures),
            rate_limited: get(&self.rate_limited),
            oversize_lines: get(&self.oversize),
            invalid_utf8: get(&self.invalid_utf8),
            invalid_requests: get(&self.invalid_requests),
            timeouts: get(&self.timeouts),
            backend_enqueued: get(&self.enqueued),
        }
    }
}

/// Cloneable handle for reading the counters of a running server.
#[derive(Clone)]
pub struct StatsHandle(Arc<SharedStats>);

impl StatsHandle {
    /// Current counters.
    pub fn snapshot(&self) -> ServerStats {
        self.0.snapshot()
    }
}

// ---------------------------------------------------------------------------
// Small pure helpers (unit tested below)
// ---------------------------------------------------------------------------

/// Per-connection token bucket in integer micro-tokens (no floating point).
#[derive(Debug)]
struct TokenBucket {
    capacity: u64,
    tokens: u64,
    rate_per_us: u64,
    last: Instant,
}

const MICRO: u64 = 1_000_000;

impl TokenBucket {
    fn new(rate_per_sec: u32, burst: u32, now: Instant) -> TokenBucket {
        let capacity = u64::from(burst).saturating_mul(MICRO);
        TokenBucket {
            capacity,
            tokens: capacity,
            // `rate` tokens per second = `rate` micro-tokens per microsecond.
            rate_per_us: u64::from(rate_per_sec),
            last: now,
        }
    }

    /// Take one token if available.
    fn try_take(&mut self, now: Instant) -> bool {
        let elapsed_us =
            u64::try_from(now.saturating_duration_since(self.last).as_micros()).unwrap_or(u64::MAX);
        if now > self.last {
            self.last = now;
        }
        let added = elapsed_us.saturating_mul(self.rate_per_us);
        self.tokens = self.tokens.saturating_add(added).min(self.capacity);
        if self.tokens >= MICRO {
            self.tokens -= MICRO;
            true
        } else {
            false
        }
    }
}

enum Absorb {
    /// No newline in the chunk; this many bytes were taken.
    Partial(usize),
    /// A newline was found; this many bytes (including it) were taken.
    Line(usize),
    /// The line would exceed the limit.
    TooLong,
}

/// Move the part of `chunk` up to the first newline into `buf`, refusing to
/// let `buf` grow beyond `max` bytes.
fn absorb(buf: &mut Vec<u8>, chunk: &[u8], max: usize) -> Absorb {
    let newline = chunk.iter().position(|b| *b == b'\n');
    let data_len = newline.unwrap_or(chunk.len());
    let data = chunk.get(..data_len).unwrap_or(&[]);
    if buf.len().saturating_add(data.len()) > max {
        return Absorb::TooLong;
    }
    buf.extend_from_slice(data);
    match newline {
        Some(i) => Absorb::Line(i.saturating_add(1)),
        None => Absorb::Partial(chunk.len()),
    }
}

fn write_all_deadline(stream: &mut TcpStream, data: &[u8], total: Duration) -> io::Result<()> {
    let deadline = Instant::now().checked_add(total);
    let mut rest = data;
    while !rest.is_empty() {
        let remaining = match deadline {
            Some(d) => d
                .checked_duration_since(Instant::now())
                .filter(|r| !r.is_zero())
                .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?,
            None => total,
        };
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(rest) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => rest = rest.get(n..).unwrap_or(&[]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn send_line(stream: &mut TcpStream, mut line: String, timeout: Duration) -> io::Result<()> {
    line.push('\n');
    write_all_deadline(stream, line.as_bytes(), timeout)
}

fn log(msg: &str) {
    eprintln!("perp-engine server: {msg}");
}

// ---------------------------------------------------------------------------
// Backend thread
// ---------------------------------------------------------------------------

enum Command {
    Apply(EventBody),
    Book { market: MarketId, depth: usize },
    Account(AccountId),
    Summary,
}

enum Reply {
    Applied(Result<Vec<Output>, ApiError>),
    Book(Option<BookView>),
    Account(Option<AccountView>),
    Summary(Summary),
}

struct Job {
    cmd: Command,
    reply: SyncSender<Reply>,
}

/// Run the backend until every sender is gone. Returns the backend and
/// whether it stopped because of a fatal storage error.
fn backend_loop<B: ExchangeApi>(mut backend: B, rx: &Receiver<Job>) -> (B, bool) {
    while let Ok(job) = rx.recv() {
        let reply = match job.cmd {
            Command::Apply(body) => Reply::Applied(backend.submit(body)),
            Command::Book { market, depth } => Reply::Book(backend.book(market, depth)),
            Command::Account(id) => Reply::Account(backend.account(id)),
            Command::Summary => Reply::Summary(backend.summary()),
        };
        let fatal = matches!(reply, Reply::Applied(Err(ApiError::Storage(_))));
        // The reply channel has room for one message and the connection is
        // waiting for it; an error only means the connection is gone.
        let _ = job.reply.send(reply);
        if fatal {
            log("backend reported a storage failure; stopping");
            return (backend, true);
        }
    }
    (backend, false)
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

struct Shared {
    cfg: ServerConfig,
    stats: Arc<SharedStats>,
    shutdown: Arc<AtomicBool>,
    tx: SyncSender<Job>,
}

/// Decrements the active-connection gauge when the connection thread ends (or
/// fails to start).
struct ActiveGuard(Arc<SharedStats>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

enum ReadOutcome {
    /// A line is in the buffer. `last` is set when it ended at EOF without a
    /// newline.
    Line {
        last: bool,
    },
    Eof,
    TooLong,
    Timeout,
    Shutdown,
    Io,
}

fn read_line(
    reader: &mut BufReader<TcpStream>,
    buf: &mut Vec<u8>,
    max: usize,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> ReadOutcome {
    buf.clear();
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return ReadOutcome::Shutdown;
        }
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            return ReadOutcome::Timeout;
        };
        if reader
            .get_ref()
            .set_read_timeout(Some(remaining.min(POLL)))
            .is_err()
        {
            return ReadOutcome::Io;
        }
        match reader.fill_buf() {
            Ok([]) => {
                return if buf.is_empty() {
                    ReadOutcome::Eof
                } else {
                    ReadOutcome::Line { last: true }
                };
            }
            Ok(chunk) => match absorb(buf, chunk, max) {
                Absorb::TooLong => return ReadOutcome::TooLong,
                Absorb::Line(n) => {
                    reader.consume(n);
                    return ReadOutcome::Line { last: false };
                }
                Absorb::Partial(n) => reader.consume(n),
            },
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return ReadOutcome::Io,
        }
    }
}

struct Outcome {
    response: String,
    close: Option<&'static str>,
}

impl Outcome {
    fn reply(response: String) -> Outcome {
        Outcome {
            response,
            close: None,
        }
    }
}

struct Session<'a> {
    sh: &'a Shared,
    authed: bool,
    failures: u32,
}

impl Session<'_> {
    fn auth_failure(&mut self) -> Outcome {
        inc(&self.sh.stats.auth_failures);
        self.failures = self.failures.saturating_add(1);
        // The same pause and the same bytes whatever the reason.
        thread::sleep(self.sh.cfg.auth_failure_delay);
        Outcome {
            response: protocol::err("auth_failed", ""),
            close: (self.failures >= self.sh.cfg.max_auth_failures)
                .then_some("too many authentication failures"),
        }
    }

    fn call(&self, cmd: Command) -> Option<Reply> {
        let (reply_tx, reply_rx) = sync_channel(1);
        self.sh
            .tx
            .send(Job {
                cmd,
                reply: reply_tx,
            })
            .ok()?;
        inc(&self.sh.stats.enqueued);
        reply_rx.recv().ok()
    }

    fn backend(&self, cmd: Command) -> Outcome {
        match self.call(cmd) {
            Some(reply) => Outcome::reply(render(reply)),
            None => Outcome {
                response: protocol::err("unavailable", "the engine is not running"),
                close: Some("backend unavailable"),
            },
        }
    }

    fn handle(&mut self, line: &[u8]) -> Outcome {
        let Ok(text) = std::str::from_utf8(line) else {
            inc(&self.sh.stats.invalid_utf8);
            return if self.authed {
                Outcome::reply(protocol::err("invalid_utf8", "request is not valid UTF-8"))
            } else {
                self.auth_failure()
            };
        };
        let parsed = protocol::parse_request(text, self.sh.cfg.max_line_bytes);
        if !self.authed {
            return match parsed {
                Ok(Request::Auth { token }) if self.sh.cfg.token.verify(&token) => {
                    self.authed = true;
                    inc(&self.sh.stats.auth_successes);
                    Outcome::reply(protocol::ok_empty())
                }
                _ => self.auth_failure(),
            };
        }
        match parsed {
            Err(e) => {
                inc(&self.sh.stats.invalid_requests);
                Outcome::reply(protocol::err(e.code, &e.message))
            }
            Ok(Request::Auth { .. }) => Outcome::reply(protocol::err(
                "already_authenticated",
                "this connection is already authenticated",
            )),
            Ok(Request::Ping) => Outcome::reply(protocol::pong()),
            Ok(Request::Command(body)) => self.backend(Command::Apply(body)),
            Ok(Request::Book { market, depth }) => self.backend(Command::Book { market, depth }),
            Ok(Request::Account { id }) => self.backend(Command::Account(id)),
            Ok(Request::Summary) => self.backend(Command::Summary),
        }
    }
}

fn render(reply: Reply) -> String {
    match reply {
        Reply::Applied(Ok(outputs)) => protocol::ok(&outputs),
        Reply::Applied(Err(ApiError::Engine(e))) => {
            let text = e.to_string();
            let message = if text == e.code() { "" } else { text.as_str() };
            protocol::err(e.code(), message)
        }
        Reply::Applied(Err(ApiError::Storage(_))) => protocol::err(
            "storage_error",
            "the event could not be persisted; the server is stopping",
        ),
        Reply::Book(Some(view)) => protocol::book_response(&view),
        Reply::Book(None) => protocol::err("unknown_market", ""),
        Reply::Account(Some(view)) => protocol::account_response(&view),
        Reply::Account(None) => protocol::err("unknown_account", ""),
        Reply::Summary(s) => protocol::summary_response(&s),
    }
}

/// Serve one connection until it ends. Returns the reason and the number of
/// processed requests.
fn serve(stream: TcpStream, sh: &Shared) -> (&'static str, u64) {
    let cfg = &sh.cfg;
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::with_capacity(READ_CHUNK, stream);
    let mut buf: Vec<u8> = Vec::with_capacity(READ_CHUNK.min(cfg.max_line_bytes));
    let mut bucket = TokenBucket::new(cfg.rate_per_sec, cfg.burst, Instant::now());
    let mut session = Session {
        sh,
        authed: false,
        failures: 0,
    };
    let mut streak: u32 = 0;
    let mut processed: u64 = 0;
    let start = Instant::now();
    let after = |d: Duration| start.checked_add(d).unwrap_or(start);
    let auth_deadline = after(cfg.auth_timeout);
    let mut deadline = auth_deadline;
    let write = |reader: &mut BufReader<TcpStream>, line: String| {
        send_line(reader.get_mut(), line, cfg.write_timeout)
    };

    loop {
        let last = match read_line(
            &mut reader,
            &mut buf,
            cfg.max_line_bytes,
            deadline,
            &sh.shutdown,
        ) {
            ReadOutcome::Line { last } => last,
            ReadOutcome::Eof => return ("client closed", processed),
            ReadOutcome::TooLong => {
                inc(&sh.stats.oversize);
                let _ = write(
                    &mut reader,
                    protocol::err("line_too_long", "request line exceeds the limit"),
                );
                return ("line too long", processed);
            }
            ReadOutcome::Timeout => {
                inc(&sh.stats.timeouts);
                let _ = write(&mut reader, protocol::err("timeout", ""));
                return (
                    if session.authed {
                        "idle timeout"
                    } else {
                        "authentication timeout"
                    },
                    processed,
                );
            }
            ReadOutcome::Shutdown => return ("server shutdown", processed),
            ReadOutcome::Io => return ("read error", processed),
        };
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }

        if bucket.try_take(Instant::now()) {
            streak = 0;
        } else {
            inc(&sh.stats.rate_limited);
            streak = streak.saturating_add(1);
            if write(&mut reader, protocol::err("rate_limited", "")).is_err() {
                return ("write error", processed);
            }
            if streak > cfg.rate_limit_disconnect_after {
                return ("sustained rate limiting", processed);
            }
            if last {
                return ("client closed", processed);
            }
            if session.authed {
                deadline = after_now(cfg.idle_timeout);
            }
            continue;
        }

        inc(&sh.stats.requests);
        processed = processed.saturating_add(1);
        let outcome = session.handle(&buf);
        if write(&mut reader, outcome.response).is_err() {
            return ("write error", processed);
        }
        if let Some(reason) = outcome.close {
            return (reason, processed);
        }
        if last {
            return ("client closed", processed);
        }
        if session.authed {
            deadline = after_now(cfg.idle_timeout);
        }
    }
}

fn after_now(d: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(d).unwrap_or(now)
}

fn run_connection(stream: TcpStream, peer: Option<SocketAddr>, sh: &Shared) {
    let peer = peer.map_or_else(|| "unknown".to_string(), |p| p.to_string());
    log(&format!("connection opened peer={peer}"));
    let (reason, requests) = serve(stream, sh);
    log(&format!(
        "connection closed peer={peer} reason=\"{reason}\" requests={requests}"
    ));
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// A bound listener plus configuration. Call [`Server::run`] to serve.
pub struct Server {
    listener: TcpListener,
    cfg: ServerConfig,
    stats: Arc<SharedStats>,
}

impl Server {
    /// Validate the configuration and bind the listening socket.
    ///
    /// Refuses non-loopback addresses unless
    /// [`ServerConfig::allow_non_loopback`] is set.
    pub fn bind(cfg: ServerConfig) -> io::Result<Server> {
        cfg.validate()?;
        let listener = TcpListener::bind(cfg.listen)?;
        listener.set_nonblocking(true)?;
        if !cfg.listen.ip().is_loopback() {
            log("warning: listening on a non-loopback address; traffic is plaintext");
        }
        Ok(Server {
            listener,
            cfg,
            stats: Arc::new(SharedStats::default()),
        })
    }

    /// The address actually bound (useful with port 0).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Handle for reading counters while the server runs.
    pub fn stats_handle(&self) -> StatsHandle {
        StatsHandle(Arc::clone(&self.stats))
    }

    /// Serve until `shutdown` becomes true, then stop accepting, wait for all
    /// connection threads and the backend thread, and return the counters.
    ///
    /// Blocks the calling thread. Returns an error if the backend thread
    /// panicked or reported a fatal storage failure.
    pub fn run<B: ExchangeApi>(
        self,
        backend: B,
        shutdown: Arc<AtomicBool>,
    ) -> io::Result<ServerStats> {
        self.run_returning(backend, shutdown)
            .map(|(stats, _)| stats)
    }

    /// Like [`Server::run`], but also hands the backend back after a clean
    /// shutdown so the caller can flush or close it.
    pub fn run_returning<B: ExchangeApi>(
        self,
        backend: B,
        shutdown: Arc<AtomicBool>,
    ) -> io::Result<(ServerStats, B)> {
        let Server {
            listener,
            cfg,
            stats,
        } = self;
        let (tx, rx) = sync_channel::<Job>(cfg.queue_depth);
        let backend_handle = thread::Builder::new()
            .name("perp-backend".to_string())
            .spawn(move || backend_loop(backend, &rx))?;
        let shared = Arc::new(Shared {
            cfg,
            stats: Arc::clone(&stats),
            shutdown: Arc::clone(&shutdown),
            tx,
        });
        let mut handles: Vec<JoinHandle<()>> = Vec::new();

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            if backend_handle.is_finished() {
                log("backend thread stopped; shutting down");
                shutdown.store(true, Ordering::Relaxed);
                break;
            }
            reap(&mut handles);
            match listener.accept() {
                Ok((stream, peer)) => admit(&shared, stream, Some(peer), &mut handles),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    log(&format!("accept failed: {}", e.kind()));
                    thread::sleep(POLL);
                }
            }
        }

        drop(listener);
        for h in handles {
            if h.join().is_err() {
                log("a connection thread panicked");
            }
        }
        // Connection threads held the last other references to `shared`;
        // dropping it closes the queue and lets the backend thread finish.
        drop(shared);
        match backend_handle.join() {
            Ok((backend, false)) => Ok((stats.snapshot(), backend)),
            Ok((_, true)) => Err(io::Error::other(
                "backend reported a storage failure; the server stopped",
            )),
            Err(_) => Err(io::Error::other("backend thread panicked")),
        }
    }
}

fn reap(handles: &mut Vec<JoinHandle<()>>) {
    if handles.is_empty() {
        return;
    }
    let (done, running): (Vec<_>, Vec<_>) = std::mem::take(handles)
        .into_iter()
        .partition(JoinHandle::is_finished);
    *handles = running;
    for h in done {
        if h.join().is_err() {
            log("a connection thread panicked");
        }
    }
}

fn admit(
    shared: &Arc<Shared>,
    mut stream: TcpStream,
    peer: Option<SocketAddr>,
    handles: &mut Vec<JoinHandle<()>>,
) {
    let stats = &shared.stats;
    // Accepted sockets may inherit the listener's non-blocking mode.
    if stream.set_nonblocking(false).is_err() {
        inc(&stats.rejected);
        return;
    }
    let active = stats.active.load(Ordering::Relaxed);
    let cap = u64::try_from(shared.cfg.max_connections).unwrap_or(u64::MAX);
    if active >= cap {
        inc(&stats.rejected);
        let _ = send_line(
            &mut stream,
            protocol::err("too_many_connections", "the server is full"),
            REJECT_WRITE_TIMEOUT,
        );
        return;
    }
    let now_active = active.saturating_add(1);
    stats.active.store(now_active, Ordering::Relaxed);
    stats.peak.fetch_max(now_active, Ordering::Relaxed);
    let guard = ActiveGuard(Arc::clone(stats));
    let sh = Arc::clone(shared);
    let spawned = thread::Builder::new()
        .name("perp-conn".to_string())
        .stack_size(CONN_STACK)
        .spawn(move || {
            let _guard = guard;
            run_connection(stream, peer, &sh);
        });
    match spawned {
        Ok(h) => {
            inc(&stats.accepted);
            handles.push(h);
        }
        Err(e) => {
            // The closure (and its guard) was dropped, undoing the increment.
            inc(&stats.rejected);
            log(&format!(
                "could not start a connection thread: {}",
                e.kind()
            ));
        }
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
    use perp_domain::error::Error;

    fn token() -> Token {
        Token::new(&format!("u{}", "n1t".repeat(14))).unwrap()
    }

    fn cfg() -> ServerConfig {
        let mut c = ServerConfig::new(token());
        c.listen = "127.0.0.1:0".parse().unwrap();
        c
    }

    #[test]
    fn bucket_allows_burst_then_limits() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(10, 5, t0);
        for _ in 0..5 {
            assert!(b.try_take(t0));
        }
        assert!(!b.try_take(t0));
        assert!(!b.try_take(t0));
    }

    #[test]
    fn bucket_refills_at_the_configured_rate() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(10, 5, t0);
        for _ in 0..5 {
            assert!(b.try_take(t0));
        }
        // 10 tokens/s: after 100 ms exactly one token is back.
        assert!(!b.try_take(t0 + Duration::from_millis(99)));
        assert!(b.try_take(t0 + Duration::from_millis(100)));
        assert!(!b.try_take(t0 + Duration::from_millis(100)));
        // After a long pause the bucket is capped at the burst size.
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..5 {
            assert!(b.try_take(later));
        }
        assert!(!b.try_take(later));
    }

    #[test]
    fn bucket_tolerates_time_going_backwards() {
        let t0 = Instant::now() + Duration::from_secs(10);
        let mut b = TokenBucket::new(1, 1, t0);
        assert!(b.try_take(t0));
        assert!(!b.try_take(t0 - Duration::from_secs(5)));
        assert!(!b.try_take(t0));
    }

    #[test]
    fn bucket_with_extreme_parameters_does_not_overflow() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(u32::MAX, u32::MAX, t0);
        assert!(b.try_take(t0 + Duration::from_secs(86_400 * 365)));
    }

    #[test]
    fn absorb_splits_at_newline() {
        let mut buf = Vec::new();
        match absorb(&mut buf, b"abc\ndef", 10) {
            Absorb::Line(n) => assert_eq!(n, 4),
            _ => panic!("expected line"),
        }
        assert_eq!(buf, b"abc");
    }

    #[test]
    fn absorb_accumulates_partial_chunks() {
        let mut buf = Vec::new();
        assert!(matches!(absorb(&mut buf, b"ab", 5), Absorb::Partial(2)));
        assert!(matches!(absorb(&mut buf, b"cd", 5), Absorb::Partial(2)));
        assert!(matches!(absorb(&mut buf, b"e\n", 5), Absorb::Line(2)));
        assert_eq!(buf, b"abcde");
    }

    #[test]
    fn absorb_enforces_the_limit_exactly() {
        let mut buf = Vec::new();
        assert!(matches!(absorb(&mut buf, b"abcde\n", 5), Absorb::Line(6)));
        let mut buf = Vec::new();
        assert!(matches!(absorb(&mut buf, b"abcdef\n", 5), Absorb::TooLong));
        let mut buf = Vec::new();
        assert!(matches!(absorb(&mut buf, b"abcdef", 5), Absorb::TooLong));
        let mut buf = Vec::new();
        assert!(matches!(absorb(&mut buf, b"abc", 5), Absorb::Partial(3)));
        assert!(matches!(absorb(&mut buf, b"def", 5), Absorb::TooLong));
        // The buffer never grew past the limit.
        assert!(buf.len() <= 5);
    }

    #[test]
    fn absorb_empty_line() {
        let mut buf = Vec::new();
        assert!(matches!(absorb(&mut buf, b"\nrest", 5), Absorb::Line(1)));
        assert!(buf.is_empty());
    }

    #[test]
    fn default_config_matches_the_documented_values() {
        let c = ServerConfig::new(token());
        assert_eq!(c.listen, "127.0.0.1:7878".parse::<SocketAddr>().unwrap());
        assert!(!c.allow_non_loopback);
        assert_eq!(c.max_connections, 32);
        assert_eq!(c.max_line_bytes, 4096);
        assert_eq!(c.rate_per_sec, 200);
        assert_eq!(c.burst, 400);
        assert_eq!(c.rate_limit_disconnect_after, 50);
        assert_eq!(c.auth_timeout, Duration::from_secs(5));
        assert_eq!(c.idle_timeout, Duration::from_secs(60));
        assert_eq!(c.max_auth_failures, 3);
        assert_eq!(c.queue_depth, 256);
        assert!(c.validate().is_ok());
        // Debug output does not contain the token.
        assert!(format!("{c:?}").contains("<redacted>"));
    }

    #[test]
    fn validation_rejects_out_of_range_settings() {
        type Mutation = Box<dyn Fn(&mut ServerConfig)>;
        let cases: Vec<Mutation> = vec![
            Box::new(|c| c.max_connections = 0),
            Box::new(|c| c.max_connections = MAX_CONNECTIONS_LIMIT + 1),
            Box::new(|c| c.max_line_bytes = MIN_LINE_BYTES - 1),
            Box::new(|c| c.max_line_bytes = MAX_LINE_BYTES_LIMIT + 1),
            Box::new(|c| c.rate_per_sec = 0),
            Box::new(|c| c.burst = 0),
            Box::new(|c| c.rate_limit_disconnect_after = 0),
            Box::new(|c| c.max_auth_failures = 0),
            Box::new(|c| c.queue_depth = 0),
            Box::new(|c| c.queue_depth = MAX_QUEUE_DEPTH + 1),
            Box::new(|c| c.auth_timeout = Duration::ZERO),
            Box::new(|c| c.idle_timeout = Duration::ZERO),
            Box::new(|c| c.write_timeout = Duration::ZERO),
            Box::new(|c| c.idle_timeout = Duration::MAX),
            Box::new(|c| c.auth_failure_delay = Duration::MAX),
        ];
        for (i, mutate) in cases.iter().enumerate() {
            let mut c = cfg();
            mutate(&mut c);
            let e = c.validate().unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "case {i}");
        }
    }

    #[test]
    fn validation_accepts_the_boundaries() {
        let mut c = cfg();
        c.max_connections = MAX_CONNECTIONS_LIMIT;
        c.max_line_bytes = MIN_LINE_BYTES;
        c.queue_depth = MAX_QUEUE_DEPTH;
        c.auth_failure_delay = Duration::ZERO;
        c.idle_timeout = MAX_TIMEOUT;
        assert!(c.validate().is_ok());
        c.max_line_bytes = MAX_LINE_BYTES_LIMIT;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn non_loopback_is_refused_unless_allowed() {
        for addr in [
            "0.0.0.0:0",
            "192.0.2.1:7878",
            "[::]:0",
            "[2001:db8::1]:7878",
        ] {
            let mut c = cfg();
            c.listen = addr.parse().unwrap();
            let e = c.validate().unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{addr}");
            assert!(e.to_string().contains("non-loopback"));
            c.allow_non_loopback = true;
            assert!(c.validate().is_ok(), "{addr}");
        }
        for addr in ["127.0.0.1:0", "127.9.9.9:0", "[::1]:0"] {
            let mut c = cfg();
            c.listen = addr.parse().unwrap();
            assert!(c.validate().is_ok(), "{addr}");
        }
    }

    #[test]
    fn bind_reports_the_chosen_port() {
        let s = Server::bind(cfg()).unwrap();
        let addr = s.local_addr().unwrap();
        assert!(addr.ip().is_loopback());
        assert_ne!(addr.port(), 0);
        assert_eq!(s.stats_handle().snapshot(), ServerStats::default());
    }

    #[test]
    fn render_maps_every_reply_kind() {
        assert_eq!(
            render(Reply::Applied(Err(ApiError::Engine(Error::UnknownOrder)))),
            r#"{"ok":false,"error":"unknown_order"}"#
        );
        let with_msg = render(Reply::Applied(Err(ApiError::Engine(Error::InvalidConfig(
            "tick",
        )))));
        assert!(with_msg.contains("\"error\":\"invalid_config\""));
        assert!(with_msg.contains("message"));
        let storage = render(Reply::Applied(Err(ApiError::Storage(
            "secret path /x".into(),
        ))));
        assert!(storage.contains("storage_error"));
        assert!(!storage.contains("secret path"));
        assert_eq!(
            render(Reply::Applied(Ok(vec![]))),
            r#"{"ok":true,"outputs":[]}"#
        );
        assert!(render(Reply::Book(None)).contains("unknown_market"));
        assert!(render(Reply::Account(None)).contains("unknown_account"));
    }
}
