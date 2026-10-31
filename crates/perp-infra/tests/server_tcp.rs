//! Integration tests for the JSON-lines TCP server over real loopback sockets,
//! using a mock implementation of the application's `ExchangeApi` port.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use perp_app::api::{ApiError, ExchangeApi};
use perp_domain::error::Error;
use perp_domain::fixed::{Money, Price, Qty};
use perp_domain::types::{
    AccountId, EventBody, MarketId, OrderKind, Output, PlaceOrder, Side, StpMode, Tif,
};
use perp_domain::views::{AccountView, BookView, LevelView, PositionView, Summary};
use perp_infra::json::{self, Json, Limits};
use perp_infra::server::{Server, ServerConfig, ServerStats, StatsHandle};
use perp_infra::token::Token;

// ---------------------------------------------------------------------------
// Mock application
// ---------------------------------------------------------------------------

struct State {
    submitted: Mutex<Vec<EventBody>>,
    applied: AtomicU64,
    gate_closed: Mutex<bool>,
    gate_cv: Condvar,
    delay_ms: AtomicU64,
}

impl State {
    fn new() -> Arc<State> {
        Arc::new(State {
            submitted: Mutex::new(Vec::new()),
            applied: AtomicU64::new(0),
            gate_closed: Mutex::new(false),
            gate_cv: Condvar::new(),
            delay_ms: AtomicU64::new(0),
        })
    }

    fn close_gate(&self) {
        *self.gate_closed.lock().unwrap() = true;
    }

    fn open_gate(&self) {
        *self.gate_closed.lock().unwrap() = false;
        self.gate_cv.notify_all();
    }

    fn applied(&self) -> u64 {
        self.applied.load(Ordering::SeqCst)
    }

    fn bodies(&self) -> Vec<EventBody> {
        self.submitted.lock().unwrap().clone()
    }
}

struct Mock(Arc<State>);

impl ExchangeApi for Mock {
    fn submit(&mut self, body: EventBody) -> Result<Vec<Output>, ApiError> {
        {
            let mut closed = self.0.gate_closed.lock().unwrap();
            let give_up = Instant::now() + Duration::from_secs(20);
            while *closed && Instant::now() < give_up {
                closed = self
                    .0
                    .gate_cv
                    .wait_timeout(closed, Duration::from_millis(50))
                    .unwrap()
                    .0;
            }
        }
        let delay = self.0.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            thread::sleep(Duration::from_millis(delay));
        }
        self.0.submitted.lock().unwrap().push(body.clone());
        self.0.applied.fetch_add(1, Ordering::SeqCst);
        match body {
            EventBody::Deposit { account: 999, .. } => Err(ApiError::Engine(Error::UnknownAccount)),
            EventBody::Deposit { account: 777, .. } => Ok(vec![Output::Configured; 20_000]),
            EventBody::Deposit { account: 555, .. } => panic!("mock backend panic"),
            EventBody::FundInsurance { amount: Money(666) } => {
                Err(ApiError::Storage("disk full".to_string()))
            }
            EventBody::Deposit { account, amount } => Ok(vec![Output::Deposited {
                account,
                amount,
                balance: amount,
            }]),
            _ => Ok(vec![]),
        }
    }

    fn book(&self, market: MarketId, _depth: usize) -> Option<BookView> {
        (market == 1).then(|| BookView {
            market,
            bids: vec![LevelView {
                price: Price(100),
                qty: Qty(5),
                orders: 2,
            }],
            asks: vec![],
        })
    }

    fn account(&self, id: AccountId) -> Option<AccountView> {
        (id == 7).then(|| AccountView {
            id,
            collateral: Money(1000),
            equity: Money(1005),
            initial_margin: Money(10),
            maintenance_margin: Money(5),
            open_orders: 1,
            fee_tier: 0,
            positions: vec![PositionView {
                market: 1,
                size: 3,
                entry_price: Some(Price(100)),
                mark: None,
                unrealized: Money(5),
            }],
        })
    }

    fn summary(&self) -> Summary {
        Summary {
            events_applied: self.0.applied(),
            clock: 0,
            accounts: 1,
            markets: 1,
            open_orders: 0,
            total_collateral: Money(0),
            insurance_fund: Money(0),
            fee_pool: Money(0),
            bad_debt: Money(0),
            accounts_with_positions: 0,
            liquidation_steps: 0,
            adl_fills: 0,
            fills: 0,
            halted: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn secret() -> String {
    format!("{}{}", "tT9_".repeat(10), "-end")
}

fn base_cfg() -> ServerConfig {
    let mut c = ServerConfig::new(Token::new(&secret()).unwrap());
    c.listen = "127.0.0.1:0".parse().unwrap();
    c.auth_failure_delay = Duration::from_millis(5);
    c.auth_timeout = Duration::from_secs(5);
    c.idle_timeout = Duration::from_secs(30);
    c
}

type RunResult = io::Result<(ServerStats, Mock)>;

struct Running {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    stats: StatsHandle,
    state: Arc<State>,
    join: Option<JoinHandle<RunResult>>,
}

fn start(cfg: ServerConfig) -> Running {
    start_with(cfg, State::new())
}

fn start_with(cfg: ServerConfig, state: Arc<State>) -> Running {
    let server = Server::bind(cfg).unwrap();
    let addr = server.local_addr().unwrap();
    let stats = server.stats_handle();
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let mock = Mock(Arc::clone(&state));
    let join = thread::spawn(move || server.run_returning(mock, sd));
    Running {
        addr,
        shutdown,
        stats,
        state,
        join: Some(join),
    }
}

impl Running {
    fn client(&self) -> Client {
        Client::connect(self.addr)
    }

    fn authed(&self) -> Client {
        let mut c = self.client();
        assert_eq!(c.request(&auth_line(&secret())), r#"{"ok":true}"#);
        c
    }

    fn snapshot(&self) -> ServerStats {
        self.stats.snapshot()
    }

    fn finish(mut self) -> RunResult {
        self.shutdown.store(true, Ordering::SeqCst);
        self.state.open_gate();
        self.join.take().unwrap().join().unwrap()
    }

    /// Wait for the server to stop by itself (without requesting shutdown).
    fn wait_for_exit(mut self) -> RunResult {
        self.join.take().unwrap().join().unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.state.open_gate();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn auth_line(token: &str) -> String {
    format!(r#"{{"op":"auth","token":"{token}"}}"#)
}

#[derive(Debug, PartialEq, Eq)]
enum Recv {
    Line(String),
    Closed,
    TimedOut,
}

struct Client {
    reader: BufReader<TcpStream>,
}

impl Client {
    fn connect(addr: SocketAddr) -> Client {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.set_nodelay(true).unwrap();
        Client {
            reader: BufReader::new(s),
        }
    }

    fn send_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.reader.get_mut().write_all(bytes)
    }

    fn send(&mut self, line: &str) {
        self.send_raw(format!("{line}\n").as_bytes()).unwrap();
    }

    fn recv_state(&mut self) -> Recv {
        let mut s = String::new();
        match self.reader.read_line(&mut s) {
            Ok(0) => Recv::Closed,
            Ok(_) => Recv::Line(s.trim_end().to_string()),
            Err(e) => match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Recv::TimedOut,
                _ => Recv::Closed,
            },
        }
    }

    fn recv(&mut self) -> String {
        match self.recv_state() {
            Recv::Line(l) => l,
            other => panic!("expected a line, got {other:?}"),
        }
    }

    fn request(&mut self, line: &str) -> String {
        self.send(line);
        self.recv()
    }

    fn expect_closed(&mut self) {
        assert_eq!(self.recv_state(), Recv::Closed);
    }
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn parse(line: &str) -> Json {
    let lim = Limits {
        max_array: 100_000,
        max_bytes: 1 << 22,
        ..Limits::default()
    };
    json::parse_with(line, &lim).unwrap()
}

const AUTH_FAILED: &str = r#"{"ok":false,"error":"auth_failed"}"#;
const RATE_LIMITED: &str = r#"{"ok":false,"error":"rate_limited"}"#;

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[test]
fn first_request_must_be_auth_and_nothing_else_runs_before() {
    let r = start(base_cfg());
    let mut c = r.client();
    let resp = c.request(r#"{"op":"deposit","account":1,"amount":100}"#);
    assert_eq!(resp, AUTH_FAILED);
    assert_eq!(r.state.applied(), 0);
    drop(c);
    // A ping before auth is not answered either.
    let mut c = r.client();
    assert_eq!(c.request(r#"{"op":"ping"}"#), AUTH_FAILED);
    assert_eq!(r.state.bodies().len(), 0);
}

#[test]
fn wrong_token_is_rejected_and_correct_token_accepted() {
    let r = start(base_cfg());
    let mut c = r.client();
    assert_eq!(c.request(&auth_line(&"x".repeat(40))), AUTH_FAILED);
    assert_eq!(c.request(&auth_line(&secret())), r#"{"ok":true}"#);
    assert_eq!(c.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
    let s = r.snapshot();
    assert_eq!(s.auth_failures, 1);
    assert_eq!(s.auth_successes, 1);
}

#[test]
fn auth_failures_are_byte_identical_whatever_the_reason() {
    let mut cfg = base_cfg();
    cfg.max_auth_failures = 100;
    let r = start(cfg);
    let mut c = r.client();
    let mut off_by_one = secret().into_bytes();
    let last = off_by_one.len() - 1;
    off_by_one[last] = b'X';
    let off_by_one = String::from_utf8(off_by_one).unwrap();
    let attempts = [
        auth_line(&"x".repeat(40)),
        auth_line(&off_by_one),
        auth_line("short"),
        auth_line(""),
        auth_line(&"y".repeat(1000)),
        r#"{"op":"ping"}"#.to_string(),
        r#"{"op":"auth"}"#.to_string(),
        r#"{"op":"auth","token":5}"#.to_string(),
        format!(r#"{{"op":"auth","token":"{}","extra":1}}"#, secret()),
        "not json at all".to_string(),
        "[]".to_string(),
        String::new(),
        "{\"op\":\"auth\",\"token\":".to_string(),
    ];
    for a in &attempts {
        assert_eq!(c.request(a), AUTH_FAILED, "attempt {a:.30}");
    }
    // Binary garbage (invalid UTF-8) gets the same answer.
    c.send_raw(&[0xff, 0xfe, 0x00, b'\n']).unwrap();
    assert_eq!(c.recv(), AUTH_FAILED);
    assert_eq!(r.snapshot().auth_failures, attempts.len() as u64 + 1);
    // The right token still works afterwards.
    assert_eq!(c.request(&auth_line(&secret())), r#"{"ok":true}"#);
}

#[test]
fn disconnects_after_max_auth_failures() {
    let r = start(base_cfg()); // max_auth_failures = 3
    let mut c = r.client();
    for _ in 0..3 {
        assert_eq!(c.request(&auth_line(&"z".repeat(40))), AUTH_FAILED);
    }
    c.expect_closed();
    wait_until("connection to close", || {
        r.snapshot().active_connections == 0
    });
    assert_eq!(r.snapshot().auth_failures, 3);
}

#[test]
fn every_auth_failure_takes_the_same_fixed_pause() {
    let mut cfg = base_cfg();
    cfg.auth_failure_delay = Duration::from_millis(80);
    cfg.max_auth_failures = 10;
    let r = start(cfg);
    let mut c = r.client();
    let reasons = [
        auth_line(&"q".repeat(40)),
        "garbage".to_string(),
        r#"{"op":"ping"}"#.to_string(),
        auth_line("a"),
    ];
    for line in &reasons {
        let t = Instant::now();
        assert_eq!(c.request(line), AUTH_FAILED);
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(75), "{took:?}");
        assert!(took < Duration::from_secs(2), "{took:?}");
    }
}

#[test]
fn authenticating_twice_is_reported_and_harmless() {
    let r = start(base_cfg());
    let mut c = r.authed();
    let resp = c.request(&auth_line(&secret()));
    assert!(resp.contains("already_authenticated"));
    assert_eq!(c.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
}

#[test]
fn token_is_never_echoed_in_any_response() {
    let mut cfg = base_cfg();
    cfg.max_auth_failures = 50;
    let r = start(cfg);
    let token = secret();
    let mut c = r.client();
    let mut all = Vec::new();
    all.push(c.request(&auth_line(&token[..39])));
    all.push(c.request(&format!(r#"{{"op":"auth","token":"{token}","x":1}}"#)));
    all.push(c.request(&format!(r#"{{"op":"auth","token":{token}}}"#)));
    all.push(c.request(&auth_line(&token)));
    all.push(c.request(&format!(r#"{{"op":"{token}"}}"#)));
    all.push(c.request(&format!(r#"{{"{token}":1}}"#)));
    for resp in all {
        assert!(!resp.contains(&token), "{resp}");
    }
}

#[test]
fn auth_timeout_closes_silent_connections() {
    let mut cfg = base_cfg();
    cfg.auth_timeout = Duration::from_millis(300);
    let r = start(cfg);
    let mut c = r.client();
    let t = Instant::now();
    assert_eq!(c.recv(), r#"{"ok":false,"error":"timeout"}"#);
    c.expect_closed();
    assert!(t.elapsed() < Duration::from_secs(3));
    assert_eq!(r.snapshot().timeouts, 1);
}

#[test]
fn auth_deadline_is_not_extended_by_failed_attempts() {
    let mut cfg = base_cfg();
    cfg.auth_timeout = Duration::from_millis(400);
    cfg.auth_failure_delay = Duration::ZERO;
    cfg.max_auth_failures = 1000;
    let r = start(cfg);
    let mut c = r.client();
    let t = Instant::now();
    // Keep failing until the deadline fires.
    loop {
        c.send(r#"{"op":"ping"}"#);
        match c.recv_state() {
            Recv::Line(l) if l == AUTH_FAILED => thread::sleep(Duration::from_millis(50)),
            Recv::Line(l) => {
                assert!(l.contains("timeout"), "{l}");
                break;
            }
            _ => break,
        }
        assert!(t.elapsed() < Duration::from_secs(3), "deadline never fired");
    }
    assert!(t.elapsed() < Duration::from_secs(3));
}

// ---------------------------------------------------------------------------
// Requests and responses
// ---------------------------------------------------------------------------

#[test]
fn every_command_reaches_the_backend_in_order_with_typed_fields() {
    let r = start(base_cfg());
    let mut c = r.authed();
    let lines = [
        r#"{"op":"deposit","account":1,"amount":1000}"#,
        r#"{"op":"withdraw","account":1,"amount":5}"#,
        r#"{"op":"fund_insurance","amount":42}"#,
        r#"{"op":"place","market":1,"account":1,"side":"buy","type":"limit","price":100,"tif":"post_only","qty":10,"client_id":7}"#,
        r#"{"op":"place","market":1,"account":2,"side":"sell","type":"market","max_slippage_bps":50,"qty":3,"client_id":8,"reduce_only":true,"stp":"cancel_both"}"#,
        r#"{"op":"cancel","account":1,"order_id":9}"#,
        r#"{"op":"cancel_all","account":1,"market":1}"#,
        r#"{"op":"replace","account":1,"order_id":9,"price":101,"qty":4}"#,
        r#"{"op":"oracle","market":1,"index_price":100,"source_ts":123}"#,
        r#"{"op":"tick"}"#,
    ];
    for l in lines {
        let resp = c.request(l);
        assert!(resp.starts_with(r#"{"ok":true"#), "{l}: {resp}");
    }
    let expected = vec![
        EventBody::Deposit {
            account: 1,
            amount: Money(1000),
        },
        EventBody::Withdraw {
            account: 1,
            amount: Money(5),
        },
        EventBody::FundInsurance { amount: Money(42) },
        EventBody::Place(PlaceOrder {
            market: 1,
            account: 1,
            side: Side::Buy,
            kind: OrderKind::Limit {
                price: Price(100),
                tif: Tif::PostOnly,
            },
            qty: Qty(10),
            client_id: 7,
            reduce_only: false,
            stp: StpMode::CancelNewest,
        }),
        EventBody::Place(PlaceOrder {
            market: 1,
            account: 2,
            side: Side::Sell,
            kind: OrderKind::Market {
                max_slippage_bps: 50,
            },
            qty: Qty(3),
            client_id: 8,
            reduce_only: true,
            stp: StpMode::CancelBoth,
        }),
        EventBody::Cancel {
            account: 1,
            order_id: 9,
        },
        EventBody::CancelAll {
            account: 1,
            market: Some(1),
        },
        EventBody::Replace {
            account: 1,
            order_id: 9,
            price: Some(Price(101)),
            qty: Some(Qty(4)),
        },
        EventBody::Oracle {
            market: 1,
            index_price: Price(100),
            source_ts: 123,
        },
        EventBody::Tick,
    ];
    assert_eq!(r.state.bodies(), expected);
}

#[test]
fn deposit_response_carries_the_outputs() {
    let r = start(base_cfg());
    let mut c = r.authed();
    let resp = c.request(r#"{"op":"deposit","account":3,"amount":250}"#);
    let d = parse(&resp);
    assert_eq!(d.get("ok"), Some(&Json::Bool(true)));
    let outs = d.get("outputs").and_then(Json::as_array).unwrap();
    assert_eq!(outs.len(), 1);
    assert_eq!(
        outs[0].get("type").and_then(Json::as_str),
        Some("deposited")
    );
    assert_eq!(outs[0].get("account").and_then(Json::as_int), Some(3));
    assert_eq!(outs[0].get("amount").and_then(Json::as_int), Some(250));
}

#[test]
fn configure_and_add_market_are_not_reachable_over_tcp() {
    let r = start(base_cfg());
    let mut c = r.authed();
    for op in ["configure", "add_market", "set_fee_tier"] {
        let resp = c.request(&format!(r#"{{"op":"{op}"}}"#));
        assert_eq!(
            resp,
            r#"{"ok":false,"error":"unknown_op","message":"unknown op"}"#
        );
    }
    assert_eq!(r.state.applied(), 0);
}

#[test]
fn engine_rejections_are_relayed_with_their_code() {
    let r = start(base_cfg());
    let mut c = r.authed();
    let resp = c.request(r#"{"op":"deposit","account":999,"amount":1}"#);
    assert_eq!(resp, r#"{"ok":false,"error":"unknown_account"}"#);
    // The connection and the server keep working.
    assert!(c
        .request(r#"{"op":"deposit","account":1,"amount":1}"#)
        .starts_with(r#"{"ok":true"#));
}

#[test]
fn queries_are_answered_from_the_backend() {
    let r = start(base_cfg());
    let mut c = r.authed();
    let book = parse(&c.request(r#"{"op":"book","market":1,"depth":5}"#));
    let b = book.get("book").unwrap();
    assert_eq!(b.get("market").and_then(Json::as_int), Some(1));
    assert_eq!(b.get("bids").and_then(Json::as_array).unwrap().len(), 1);
    assert_eq!(
        c.request(r#"{"op":"book","market":2}"#),
        r#"{"ok":false,"error":"unknown_market"}"#
    );
    let acct = parse(&c.request(r#"{"op":"account","account":7}"#));
    let a = acct.get("account").unwrap();
    assert_eq!(a.get("equity").and_then(Json::as_int), Some(1005));
    assert_eq!(
        c.request(r#"{"op":"account","account":8}"#),
        r#"{"ok":false,"error":"unknown_account"}"#
    );
    c.request(r#"{"op":"deposit","account":1,"amount":1}"#);
    let summary = parse(&c.request(r#"{"op":"summary"}"#));
    assert_eq!(
        summary
            .get("summary")
            .and_then(|s| s.get("events_applied"))
            .and_then(Json::as_int),
        Some(1)
    );
}

#[test]
fn invalid_requests_get_errors_and_keep_the_connection() {
    let r = start(base_cfg());
    let mut c = r.authed();
    let cases = [
        (r#"{"op":"deposit","account":1}"#, "bad_request"),
        (r#"{"op":"deposit","account":-1,"amount":1}"#, "bad_request"),
        (r#"{"op":"deposit","account":1.5,"amount":1}"#, "bad_json"),
        (r#"{"op":"nope"}"#, "unknown_op"),
        ("[1,2]", "bad_request"),
        ("{", "bad_json"),
        (r#"{"op":"ping","op":"ping"}"#, "bad_json"),
        (r#"{"op":"place","type":"limit"}"#, "bad_request"),
    ];
    for (line, code) in cases {
        let resp = c.request(line);
        assert!(
            resp.contains(&format!("\"error\":\"{code}\"")),
            "{line} -> {resp}"
        );
    }
    assert_eq!(r.state.applied(), 0);
    assert_eq!(r.snapshot().invalid_requests, cases.len() as u64);
    assert_eq!(c.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
}

#[test]
fn oversized_engine_responses_are_replaced_by_an_error() {
    let r = start(base_cfg());
    let mut c = r.authed();
    let resp = c.request(r#"{"op":"deposit","account":777,"amount":1}"#);
    assert!(
        resp.contains("\"error\":\"response_too_large\""),
        "{resp:.80}"
    );
    assert!(resp.len() < 300);
    assert_eq!(c.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
}

// ---------------------------------------------------------------------------
// Line handling
// ---------------------------------------------------------------------------

#[test]
fn line_at_the_limit_is_served_and_one_byte_more_is_not() {
    let mut cfg = base_cfg();
    cfg.max_line_bytes = 128;
    let r = start(cfg);
    let mut c = r.authed();
    let body = r#"{"op":"ping"}"#;
    let exact = format!("{body}{}", " ".repeat(128 - body.len()));
    assert_eq!(exact.len(), 128);
    assert_eq!(c.request(&exact), r#"{"ok":true,"pong":true}"#);
    c.send(&format!("{exact} "));
    assert!(c.recv().contains("line_too_long"));
    c.expect_closed();
    assert_eq!(r.snapshot().oversize_lines, 1);
}

#[test]
fn oversize_line_without_newline_is_cut_off() {
    let mut cfg = base_cfg();
    cfg.max_line_bytes = 128;
    let r = start(cfg);
    let mut c = r.authed();
    c.send_raw(&[b'a'; 200]).unwrap();
    assert!(c.recv().contains("line_too_long"));
    c.expect_closed();
    wait_until("connection to close", || {
        r.snapshot().active_connections == 0
    });
}

#[test]
fn oversize_line_before_auth_is_refused_too() {
    let mut cfg = base_cfg();
    cfg.max_line_bytes = 128;
    let r = start(cfg);
    let mut c = r.client();
    c.send_raw(&[b'{'; 300]).unwrap();
    assert!(c.recv().contains("line_too_long"));
    c.expect_closed();
    assert_eq!(r.snapshot().auth_successes, 0);
}

#[test]
fn huge_stream_without_newline_does_not_hurt_the_server() {
    let mut cfg = base_cfg();
    cfg.max_line_bytes = 1024;
    let r = start(cfg);
    let mut c = r.authed();
    let chunk = vec![b'z'; 64 * 1024];
    for _ in 0..64 {
        if c.send_raw(&chunk).is_err() {
            break; // the server already hung up
        }
    }
    drop(c);
    wait_until("oversize to be counted", || {
        r.snapshot().oversize_lines == 1
    });
    wait_until("connection to close", || {
        r.snapshot().active_connections == 0
    });
    // The server still serves new clients.
    let mut c2 = r.authed();
    assert_eq!(c2.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
}

#[test]
fn invalid_utf8_after_auth_is_an_error_not_a_disconnect() {
    let r = start(base_cfg());
    let mut c = r.authed();
    c.send_raw(b"{\"op\":\"\xff\xfe\"}\n").unwrap();
    assert_eq!(
        c.recv(),
        r#"{"ok":false,"error":"invalid_utf8","message":"request is not valid UTF-8"}"#
    );
    assert_eq!(c.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
    assert_eq!(r.snapshot().invalid_utf8, 1);
}

#[test]
fn random_binary_garbage_never_breaks_the_server() {
    let r = start(base_cfg());
    let mut c = r.authed();
    // Deterministic pseudo-random bytes, newlines placed explicitly.
    let mut x: u32 = 0x1234_5678;
    let mut lines = 0;
    let mut payload = Vec::new();
    for _ in 0..40 {
        for _ in 0..50 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let b = (x >> 24) as u8;
            payload.push(if b == b'\n' { 1 } else { b });
        }
        payload.push(b'\n');
        lines += 1;
    }
    c.send_raw(&payload).unwrap();
    for _ in 0..lines {
        let resp = c.recv();
        assert!(resp.starts_with(r#"{"ok":false,"error":""#), "{resp}");
    }
    assert_eq!(c.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
    assert_eq!(r.state.applied(), 0);
}

#[test]
fn crlf_line_endings_are_accepted() {
    let r = start(base_cfg());
    let mut c = r.client();
    c.send_raw(format!("{}\r\n", auth_line(&secret())).as_bytes())
        .unwrap();
    assert_eq!(c.recv(), r#"{"ok":true}"#);
    c.send_raw(b"{\"op\":\"ping\"}\r\n").unwrap();
    assert_eq!(c.recv(), r#"{"ok":true,"pong":true}"#);
}

#[test]
fn empty_lines_are_invalid_requests() {
    let r = start(base_cfg());
    let mut c = r.authed();
    let resp = c.request("");
    assert!(resp.contains("bad_json"), "{resp}");
}

#[test]
fn half_closed_socket_still_gets_all_replies_including_unterminated_last_line() {
    let r = start(base_cfg());
    let mut c = r.client();
    let payload = format!(
        "{}\n{{\"op\":\"ping\"}}\n{{\"op\":\"ping\"}}",
        auth_line(&secret())
    );
    c.send_raw(payload.as_bytes()).unwrap();
    c.reader.get_ref().shutdown(Shutdown::Write).unwrap();
    assert_eq!(c.recv(), r#"{"ok":true}"#);
    assert_eq!(c.recv(), r#"{"ok":true,"pong":true}"#);
    assert_eq!(c.recv(), r#"{"ok":true,"pong":true}"#);
    c.expect_closed();
}

#[test]
fn abrupt_disconnect_in_the_middle_of_a_line_is_harmless() {
    let r = start(base_cfg());
    {
        let mut c = r.authed();
        c.send_raw(b"{\"op\":\"depo").unwrap();
    }
    wait_until("connection to close", || {
        r.snapshot().active_connections == 0
    });
    assert_eq!(r.state.applied(), 0);
    let mut c = r.authed();
    assert_eq!(c.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
}

#[test]
fn client_that_never_reads_cannot_hold_a_connection_forever() {
    // The server's writes are small; this only checks that a client which
    // sends and then vanishes without reading does not leave the slot used.
    let r = start(base_cfg());
    {
        let mut c = r.authed();
        for _ in 0..20 {
            c.send(r#"{"op":"ping"}"#);
        }
    }
    wait_until("slot to be freed", || r.snapshot().active_connections == 0);
}

// ---------------------------------------------------------------------------
// Limits: connections, rate, idle, back-pressure
// ---------------------------------------------------------------------------

#[test]
fn surplus_connections_get_an_error_line_and_are_closed() {
    let mut cfg = base_cfg();
    cfg.max_connections = 2;
    let r = start(cfg);
    let mut a = r.authed();
    let b = r.authed();
    wait_until("two active", || r.snapshot().active_connections == 2);
    let mut third = r.client();
    assert_eq!(
        third.recv(),
        r#"{"ok":false,"error":"too_many_connections","message":"the server is full"}"#
    );
    third.expect_closed();
    assert_eq!(r.snapshot().connections_rejected, 1);
    // Existing connections are unaffected.
    assert_eq!(a.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
    // Freeing a slot lets the next client in.
    let _ = b.reader.get_ref().shutdown(Shutdown::Both);
    drop(b);
    wait_until("one active", || r.snapshot().active_connections == 1);
    let mut again = r.authed();
    assert_eq!(
        again.request(r#"{"op":"ping"}"#),
        r#"{"ok":true,"pong":true}"#
    );
    let s = r.snapshot();
    assert_eq!(s.connections_accepted, 3);
    assert_eq!(s.peak_connections, 2);
}

#[test]
fn rate_limit_answers_rate_limited_after_the_burst() {
    let mut cfg = base_cfg();
    cfg.rate_per_sec = 1;
    cfg.burst = 5;
    cfg.rate_limit_disconnect_after = 1000;
    let r = start(cfg);
    let mut c = r.authed(); // uses one token
    let mut batch = String::new();
    for _ in 0..30 {
        batch.push_str("{\"op\":\"ping\"}\n");
    }
    c.send_raw(batch.as_bytes()).unwrap();
    let (mut pongs, mut limited) = (0, 0);
    for _ in 0..30 {
        match c.recv().as_str() {
            r#"{"ok":true,"pong":true}"# => pongs += 1,
            RATE_LIMITED => limited += 1,
            other => panic!("unexpected {other}"),
        }
    }
    assert!((3..=6).contains(&pongs), "pongs = {pongs}");
    assert_eq!(pongs + limited, 30);
    assert_eq!(r.snapshot().rate_limited, limited);
}

#[test]
fn sustained_rate_limit_abuse_disconnects() {
    let mut cfg = base_cfg();
    cfg.rate_per_sec = 1;
    cfg.burst = 1;
    cfg.rate_limit_disconnect_after = 5;
    let r = start(cfg);
    let mut c = r.authed();
    let mut batch = String::new();
    for _ in 0..40 {
        batch.push_str("{\"op\":\"ping\"}\n");
    }
    c.send_raw(batch.as_bytes()).unwrap();
    let mut limited = 0;
    loop {
        match c.recv_state() {
            Recv::Line(l) => {
                assert_eq!(l, RATE_LIMITED);
                limited += 1;
            }
            Recv::Closed => break,
            Recv::TimedOut => panic!("server never hung up"),
        }
    }
    // Disconnect happens on the 6th consecutive limited request.
    assert_eq!(limited, 6);
    wait_until("connection to close", || {
        r.snapshot().active_connections == 0
    });
}

#[test]
fn rate_limit_is_per_connection() {
    let mut cfg = base_cfg();
    cfg.rate_per_sec = 1;
    cfg.burst = 3;
    cfg.rate_limit_disconnect_after = 1000;
    let r = start(cfg);
    let mut hog = r.authed();
    let mut batch = String::new();
    for _ in 0..20 {
        batch.push_str("{\"op\":\"ping\"}\n");
    }
    hog.send_raw(batch.as_bytes()).unwrap();
    for _ in 0..20 {
        hog.recv();
    }
    // A fresh connection has its own bucket.
    let mut other = r.authed();
    assert_eq!(
        other.request(r#"{"op":"ping"}"#),
        r#"{"ok":true,"pong":true}"#
    );
}

#[test]
fn idle_authenticated_connections_time_out() {
    let mut cfg = base_cfg();
    cfg.idle_timeout = Duration::from_millis(300);
    let r = start(cfg);
    let mut c = r.authed();
    let t = Instant::now();
    assert_eq!(c.recv(), r#"{"ok":false,"error":"timeout"}"#);
    c.expect_closed();
    assert!(t.elapsed() >= Duration::from_millis(250));
    assert!(t.elapsed() < Duration::from_secs(3));
    assert_eq!(r.snapshot().timeouts, 1);
}

#[test]
fn activity_resets_the_idle_timer() {
    let mut cfg = base_cfg();
    cfg.idle_timeout = Duration::from_millis(500);
    let r = start(cfg);
    let mut c = r.authed();
    for _ in 0..6 {
        thread::sleep(Duration::from_millis(200));
        assert_eq!(c.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
    }
}

#[test]
fn trickling_bytes_does_not_extend_the_deadline() {
    let mut cfg = base_cfg();
    cfg.idle_timeout = Duration::from_millis(400);
    let r = start(cfg);
    let mut c = r.authed();
    let t = Instant::now();
    let msg = b"{\"op\":\"ping\"}";
    for b in msg.iter().take(msg.len() - 1) {
        if c.send_raw(&[*b]).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    // The line never completed in time: the server cut the connection.
    assert_eq!(c.recv(), r#"{"ok":false,"error":"timeout"}"#);
    c.expect_closed();
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn queue_bound_holds_when_the_backend_is_stalled() {
    let mut cfg = base_cfg();
    cfg.queue_depth = 2;
    cfg.max_connections = 16;
    let state = State::new();
    state.close_gate();
    let r = start_with(cfg, Arc::clone(&state));
    let addr = r.addr;
    let clients: Vec<JoinHandle<String>> = (0..8)
        .map(|i| {
            thread::spawn(move || {
                let mut c = Client::connect(addr);
                assert_eq!(c.request(&auth_line(&secret())), r#"{"ok":true}"#);
                c.request(&format!(
                    r#"{{"op":"deposit","account":1,"amount":{}}}"#,
                    i + 1
                ))
            })
        })
        .collect();
    // One job is held by the backend and two sit in the queue; the other
    // five connection threads are blocked and enqueue nothing.
    wait_until("queue to fill", || r.snapshot().backend_enqueued == 3);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(r.snapshot().backend_enqueued, 3);
    assert_eq!(state.applied(), 0);
    state.open_gate();
    for c in clients {
        let resp = c.join().unwrap();
        assert!(resp.starts_with(r#"{"ok":true"#), "{resp}");
    }
    assert_eq!(state.applied(), 8);
    assert_eq!(r.snapshot().backend_enqueued, 8);
}

#[test]
fn a_connection_has_one_request_in_flight_and_answers_in_order() {
    let r = start(base_cfg());
    r.state.close_gate();
    let mut c = r.authed();
    let mut batch = String::new();
    for i in 1..=50 {
        batch.push_str(&format!(
            "{{\"op\":\"deposit\",\"account\":1,\"amount\":{i}}}\n"
        ));
    }
    c.send_raw(batch.as_bytes()).unwrap();
    wait_until("first job", || r.snapshot().backend_enqueued == 1);
    thread::sleep(Duration::from_millis(300));
    // The connection did not read ahead while its request was pending.
    assert_eq!(r.snapshot().backend_enqueued, 1);
    assert_eq!(r.state.applied(), 0);
    r.state.open_gate();
    for i in 1..=50i128 {
        let d = parse(&c.recv());
        let outs = d.get("outputs").and_then(Json::as_array).unwrap();
        assert_eq!(outs[0].get("amount").and_then(Json::as_int), Some(i));
    }
    assert_eq!(r.state.applied(), 50);
}

#[test]
fn slow_backend_slows_clients_down_without_losing_requests() {
    let mut cfg = base_cfg();
    cfg.queue_depth = 1;
    let state = State::new();
    state.delay_ms.store(20, Ordering::SeqCst);
    let r = start_with(cfg, Arc::clone(&state));
    let addr = r.addr;
    let t = Instant::now();
    let workers: Vec<JoinHandle<()>> = (0..4)
        .map(|_| {
            thread::spawn(move || {
                let mut c = Client::connect(addr);
                c.request(&auth_line(&secret()));
                for _ in 0..10 {
                    let resp = c.request(r#"{"op":"deposit","account":1,"amount":1}"#);
                    assert!(resp.starts_with(r#"{"ok":true"#));
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    assert_eq!(state.applied(), 40);
    // 40 sequential 20 ms applies cannot finish faster than ~800 ms.
    assert!(
        t.elapsed() >= Duration::from_millis(700),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn many_concurrent_clients_are_all_served() {
    let r = start(base_cfg());
    let addr = r.addr;
    let workers: Vec<JoinHandle<()>> = (0..8)
        .map(|w| {
            thread::spawn(move || {
                let mut c = Client::connect(addr);
                assert_eq!(c.request(&auth_line(&secret())), r#"{"ok":true}"#);
                for i in 0..25 {
                    let resp = c.request(&format!(
                        r#"{{"op":"deposit","account":{w},"amount":{}}}"#,
                        i + 1
                    ));
                    assert!(resp.starts_with(r#"{"ok":true"#), "{resp}");
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    assert_eq!(r.state.applied(), 200);
    assert_eq!(r.snapshot().requests, 8 * 26);
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[test]
fn many_sequential_connections_do_not_leak() {
    let r = start(base_cfg());
    for _ in 0..100 {
        let mut c = r.authed();
        assert_eq!(c.request(r#"{"op":"ping"}"#), r#"{"ok":true,"pong":true}"#);
    }
    wait_until("all connections to close", || {
        r.snapshot().active_connections == 0
    });
    let s = r.snapshot();
    assert_eq!(s.connections_accepted, 100);
    assert_eq!(s.connections_rejected, 0);
    assert_eq!(s.auth_successes, 100);
    assert_eq!(s.requests, 200);
    // And the server shuts down cleanly afterwards.
    let (final_stats, _) = r.finish().unwrap();
    assert_eq!(final_stats.active_connections, 0);
}

#[test]
fn shutdown_joins_threads_promptly_and_closes_idle_clients() {
    let r = start(base_cfg());
    let mut clients: Vec<Client> = (0..5).map(|_| r.authed()).collect();
    let unauthenticated = r.client();
    wait_until("six active", || r.snapshot().active_connections == 6);
    let t = Instant::now();
    let (stats, _) = r.finish().unwrap();
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(stats.active_connections, 0);
    assert_eq!(stats.connections_accepted, 6);
    for c in &mut clients {
        c.expect_closed();
    }
    drop(unauthenticated);
}

#[test]
fn run_returning_hands_the_backend_back() {
    let r = start(base_cfg());
    let mut c = r.authed();
    c.request(r#"{"op":"deposit","account":1,"amount":9}"#);
    let state = Arc::clone(&r.state);
    let (_, mock) = r.finish().unwrap();
    assert!(Arc::ptr_eq(&mock.0, &state));
    assert_eq!(mock.0.applied(), 1);
}

#[test]
fn events_in_flight_at_shutdown_are_answered() {
    let r = start(base_cfg());
    r.state.close_gate();
    let mut c = r.authed();
    c.send(r#"{"op":"deposit","account":1,"amount":5}"#);
    wait_until("job queued", || r.snapshot().backend_enqueued == 1);
    r.shutdown.store(true, Ordering::SeqCst);
    thread::sleep(Duration::from_millis(100));
    r.state.open_gate();
    assert!(c.recv().starts_with(r#"{"ok":true"#));
    let (stats, _) = r.finish().unwrap();
    assert_eq!(stats.backend_enqueued, 1);
}

#[test]
fn storage_failure_is_reported_and_stops_the_server() {
    let r = start(base_cfg());
    let mut c = r.authed();
    let resp = c.request(r#"{"op":"fund_insurance","amount":666}"#);
    assert!(resp.contains("\"error\":\"storage_error\""), "{resp}");
    assert!(!resp.contains("disk full"));
    let err = r.wait_for_exit().err().expect("run must fail");
    assert!(err.to_string().contains("storage"), "{err}");
    c.expect_closed();
}

#[test]
fn backend_panic_fails_the_server_instead_of_hanging() {
    let r = start(base_cfg());
    let mut c = r.authed();
    let resp = c.request(r#"{"op":"deposit","account":555,"amount":1}"#);
    assert!(resp.contains("\"error\":\"unavailable\""), "{resp}");
    c.expect_closed();
    let err = r.wait_for_exit().err().expect("run must fail");
    assert!(err.to_string().contains("panicked"), "{err}");
}

// ---------------------------------------------------------------------------
// Binding
// ---------------------------------------------------------------------------

#[test]
fn binding_a_non_loopback_address_is_refused_by_default() {
    let mut cfg = base_cfg();
    cfg.listen = "0.0.0.0:0".parse().unwrap();
    let err = Server::bind(cfg).err().expect("must refuse");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let msg = err.to_string();
    assert!(
        msg.contains("non-loopback") && msg.contains("allow_non_loopback"),
        "{msg}"
    );
    assert!(!msg.contains(&secret()));
}

#[test]
fn non_loopback_binding_works_when_explicitly_allowed() {
    let mut cfg = base_cfg();
    cfg.listen = "0.0.0.0:0".parse().unwrap();
    cfg.allow_non_loopback = true;
    let server = Server::bind(cfg).unwrap();
    assert!(!server.local_addr().unwrap().ip().is_loopback());
}

#[test]
fn invalid_configuration_is_refused_at_bind() {
    let mut cfg = base_cfg();
    cfg.max_connections = 0;
    assert_eq!(
        Server::bind(cfg).err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn binding_a_port_that_is_in_use_fails() {
    let first = Server::bind(base_cfg()).unwrap();
    let mut cfg = base_cfg();
    cfg.listen = first.local_addr().unwrap();
    assert!(Server::bind(cfg).is_err());
}

#[test]
fn connection_churn_never_leaks_slots() {
    let mut cfg = base_cfg();
    cfg.max_connections = 4;
    let r = start(cfg);
    let addr = r.addr;
    let connected = Arc::new(AtomicU64::new(0));
    let hs: Vec<_> = (0..6)
        .map(|_| {
            let connected = Arc::clone(&connected);
            thread::spawn(move || {
                for _ in 0..100 {
                    if let Ok(s) = TcpStream::connect(addr) {
                        connected.fetch_add(1, Ordering::SeqCst);
                        let _ = s.shutdown(Shutdown::Both);
                    }
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    // Connections still waiting in the listen backlog are admitted (or refused)
    // after the clients are gone: wait until every successful connect has been
    // handled and all slots are back, otherwise a zero reading can be transient.
    let total = connected.load(Ordering::SeqCst);
    wait_until("every connection handled and all slots released", || {
        let s = r.snapshot();
        s.connections_accepted + s.connections_rejected >= total && s.active_connections == 0
    });
    assert!(r.snapshot().peak_connections <= 4);
    let c = r.authed(); // a new client is still admitted
    drop(c);
}
