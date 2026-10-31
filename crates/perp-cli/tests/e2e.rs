//! End-to-end tests: run the real `perp-engine` binary.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("perp-e2e-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_perp-engine"))
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Out {
    let o = bin().args(args).output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn field(text: &str, label: &str) -> String {
    text.lines()
        .find(|l| l.starts_with(label))
        .map(|l| l[label.len()..].trim().to_string())
        .unwrap_or_else(|| panic!("no '{label}' in:\n{text}"))
}

fn simulate_into(dir: &Path, seed: &str, events: &str) -> Out {
    run(&[
        "simulate",
        "--dir",
        dir.to_str().unwrap(),
        "--seed",
        seed,
        "--events",
        events,
        "--accounts",
        "60",
        "--fsync",
        "never",
    ])
}

#[test]
fn help_and_usage_errors_have_the_documented_exit_codes() {
    let h = run(&["help"]);
    assert_eq!(h.code, 0);
    assert!(h.stdout.contains("NOT for real funds"));
    assert_eq!(run(&["--help"]).code, 0);
    assert_eq!(run(&[]).code, 0, "no arguments prints usage");
    let bad = run(&["frobnicate"]);
    assert_eq!(bad.code, 2);
    assert!(bad.stderr.contains("unknown command"));
    assert_eq!(run(&["simulate", "--bogus", "1"]).code, 2);
    assert_eq!(run(&["simulate", "--events", "x"]).code, 2);
    assert_eq!(run(&["simulate", "--accounts", "0"]).code, 2);
    assert_eq!(run(&["simulate", "--fsync", "sometimes"]).code, 2);
    assert_eq!(run(&["replay"]).code, 2);
    assert_eq!(run(&["replay", "a", "b"]).code, 2);
    assert_eq!(run(&["serve"]).code, 2);
}

#[test]
fn in_memory_simulation_is_deterministic_and_reports_activity() {
    let a = run(&[
        "simulate",
        "--seed",
        "5",
        "--events",
        "20000",
        "--accounts",
        "50",
        "--check-every",
        "2000",
    ]);
    let b = run(&[
        "simulate",
        "--seed",
        "5",
        "--events",
        "20000",
        "--accounts",
        "50",
    ]);
    assert_eq!(a.code, 0, "{}", a.stderr);
    assert_eq!(
        field(&a.stdout, "state hash"),
        field(&b.stdout, "state hash")
    );
    assert!(a.stdout.contains("log: in memory"));
    assert!(field(&a.stdout, "fills").parse::<u64>().unwrap() > 0);
    assert!(field(&a.stdout, "generated events").starts_with("20000 "));
    let c = run(&[
        "simulate",
        "--seed",
        "6",
        "--events",
        "20000",
        "--accounts",
        "50",
    ]);
    assert_ne!(
        field(&a.stdout, "state hash"),
        field(&c.stdout, "state hash")
    );
}

#[test]
fn simulate_replay_verify_and_dumps_agree() {
    let t = TempDir::new("flow");
    let sim = simulate_into(t.path(), "9", "15000");
    assert_eq!(sim.code, 0, "{}", sim.stderr);
    let wal = t.path().join("engine.wal");
    let wal = wal.to_str().unwrap();

    let replay = run(&["replay", wal]);
    assert_eq!(replay.code, 0, "{}", replay.stderr);
    assert_eq!(
        field(&replay.stdout, "state hash"),
        field(&sim.stdout, "state hash")
    );
    assert_eq!(field(&replay.stdout, "torn tail bytes"), "0");

    let verify = run(&["verify", wal, "--strict"]);
    assert_eq!(verify.code, 0, "{}{}", verify.stdout, verify.stderr);
    assert!(verify.stdout.contains("OK: chain valid"));
    assert_eq!(
        field(&verify.stdout, "state hash"),
        field(&sim.stdout, "state hash")
    );

    let book = run(&["book", wal, "--depth", "3"]);
    assert_eq!(book.code, 0);
    assert!(
        book.stdout.contains("market 1")
            && (book.stdout.contains("bid")
                || book.stdout.contains("ask")
                || book.stdout.contains("(empty)"))
    );
    assert_eq!(run(&["book", wal, "--market", "99"]).code, 1);

    let pos = run(&["positions", wal]);
    assert_eq!(pos.code, 0);
    assert!(pos.stdout.contains("account ") && pos.stdout.contains("size"));
    let one = run(&["positions", wal, "--account", "1"]);
    assert!(one.stdout.starts_with("account 1:"));
    assert_eq!(run(&["positions", wal, "--account", "999999"]).code, 1);
}

#[test]
fn a_torn_tail_is_reported_and_strict_verification_fails() {
    let t = TempDir::new("torn");
    assert_eq!(simulate_into(t.path(), "3", "2000").code, 0);
    let wal = t.path().join("engine.wal");
    let mut bytes = fs::read(&wal).unwrap();
    bytes.truncate(bytes.len() - 7);
    let torn = t.path().join("torn.wal");
    fs::write(&torn, &bytes).unwrap();
    let torn = torn.to_str().unwrap();

    let v = run(&["verify", torn]);
    assert_eq!(v.code, 0, "{}", v.stderr);
    assert!(!field(&v.stdout, "torn tail").starts_with("0 "));
    let strict = run(&["verify", torn, "--strict"]);
    assert_eq!(strict.code, 1);
    assert!(strict.stderr.contains("torn"));
    let r = run(&["replay", torn]);
    assert_eq!(r.code, 0);
    assert_ne!(field(&r.stdout, "torn tail bytes"), "0");
}

#[test]
fn corruption_in_the_middle_of_the_log_fails_verification() {
    let t = TempDir::new("corrupt");
    assert_eq!(simulate_into(t.path(), "4", "3000").code, 0);
    let wal = t.path().join("engine.wal");
    let mut bytes = fs::read(&wal).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x40;
    let bad = t.path().join("bad.wal");
    fs::write(&bad, &bytes).unwrap();
    let v = run(&["verify", bad.to_str().unwrap()]);
    assert_eq!(v.code, 1);
    assert!(v.stderr.contains("error:"));
    assert_eq!(run(&["replay", bad.to_str().unwrap()]).code, 1);
    assert_eq!(
        run(&["verify", t.path().join("missing.wal").to_str().unwrap()]).code,
        1
    );
}

#[test]
fn simulate_refuses_a_directory_that_already_has_a_log() {
    let t = TempDir::new("reuse");
    assert_eq!(simulate_into(t.path(), "1", "200").code, 0);
    let again = simulate_into(t.path(), "1", "200");
    assert_eq!(again.code, 1);
    assert!(again.stderr.contains("already contains events"));
}

#[test]
fn snapshots_are_written_when_requested() {
    let t = TempDir::new("snap");
    let out = run(&[
        "simulate",
        "--dir",
        t.path().to_str().unwrap(),
        "--events",
        "3000",
        "--accounts",
        "30",
        "--snapshot-every",
        "1000",
        "--fsync",
        "every:100",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let snaps: Vec<_> = fs::read_dir(t.path().join("snapshots")).unwrap().collect();
    assert!(
        !snaps.is_empty() && snaps.len() <= 3,
        "kept {} snapshots",
        snaps.len()
    );
}

#[test]
fn checkpoints_report_resident_memory_on_linux() {
    let t = TempDir::new("rss");
    let out = run(&[
        "simulate",
        "--dir",
        t.path().to_str().unwrap(),
        "--events",
        "4000",
        "--accounts",
        "30",
        "--checkpoints",
        "2000",
        "--fsync",
        "never",
    ]);
    assert_eq!(out.code, 0);
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| l.starts_with("checkpoint"))
        .collect();
    assert_eq!(lines.len(), 2);
    if cfg!(target_os = "linux") {
        assert!(lines.iter().all(|l| l.contains("KiB")), "{lines:?}");
    }
}

#[test]
fn gen_token_refuses_to_print_into_a_pipe_and_writes_a_private_file() {
    let piped = run(&["gen-token"]);
    assert_eq!(
        piped.code, 1,
        "stdout is a pipe here, so the token must not be printed"
    );
    assert!(piped.stdout.is_empty());
    assert!(piped.stderr.contains("--out"));

    let t = TempDir::new("tok");
    let file = t.path().join("token");
    let ok = run(&["gen-token", "--out", file.to_str().unwrap()]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    let secret = fs::read_to_string(&file).unwrap();
    let secret = secret.trim();
    assert_eq!(secret.len(), 64);
    assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(
        !ok.stdout.contains(secret) && !ok.stderr.contains(secret),
        "token must never be printed"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(
        run(&["gen-token", "--out", file.to_str().unwrap()]).code,
        1,
        "never overwrite"
    );
}

// --- serve ---------------------------------------------------------------

struct Server {
    child: Child,
    addr: String,
    _stderr: BufReader<std::process::ChildStderr>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_server(dir: &Path, token_file: &Path, extra: &[&str]) -> Server {
    let mut child = bin()
        .args([
            "serve",
            "--dir",
            dir.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--token-file",
            token_file.to_str().unwrap(),
        ])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stderr.take().unwrap());
    let mut addr = None;
    let mut seen = String::new();
    for _ in 0..50 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        seen.push_str(&line);
        if let Some(rest) = line.strip_prefix("listening on ") {
            addr = rest.split_whitespace().next().map(str::to_string);
            break;
        }
    }
    let addr = addr.unwrap_or_else(|| panic!("server did not start:\n{seen}"));
    Server {
        child,
        addr,
        _stderr: reader,
    }
}

struct Client {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Client {
    fn connect(addr: &str) -> Client {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        Client { stream, reader }
    }
    fn call(&mut self, line: &str) -> String {
        writeln!(self.stream, "{line}").unwrap();
        let mut resp = String::new();
        self.reader.read_line(&mut resp).unwrap();
        resp
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn make_token(dir: &Path) -> (PathBuf, String) {
    let file = dir.join("token");
    assert_eq!(run(&["gen-token", "--out", file.to_str().unwrap()]).code, 0);
    let secret = fs::read_to_string(&file).unwrap().trim().to_string();
    (file, secret)
}

#[test]
fn server_requires_init_for_an_empty_log_and_rejects_bad_options() {
    let t = TempDir::new("noinit");
    let (token, _) = make_token(t.path());
    let data = t.path().join("data");
    let out = run(&[
        "serve",
        "--dir",
        data.to_str().unwrap(),
        "--token-file",
        token.to_str().unwrap(),
    ]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("--init"));
    let both = run(&[
        "serve",
        "--dir",
        "x",
        "--token-env",
        "A",
        "--token-file",
        "b",
    ]);
    assert_eq!(both.code, 2);
    let nonloop = run(&[
        "serve",
        "--dir",
        data.to_str().unwrap(),
        "--listen",
        "0.0.0.0:0",
        "--token-file",
        token.to_str().unwrap(),
        "--init",
    ]);
    assert_eq!(
        nonloop.code, 1,
        "non-loopback bind needs --allow-non-loopback: {}",
        nonloop.stderr
    );
    let missing = run(&[
        "serve",
        "--dir",
        data.to_str().unwrap(),
        "--token-file",
        t.path().join("nope").to_str().unwrap(),
    ]);
    assert_eq!(missing.code, 1);
}

#[test]
fn server_trades_over_tcp_and_survives_a_restart() {
    let t = TempDir::new("serve");
    let (token_file, secret) = make_token(t.path());
    let data = t.path().join("data");

    let srv = spawn_server(&data, &token_file, &["--init", "--fsync", "always"]);
    let mut c = Client::connect(&srv.addr);
    // nothing works before authentication
    assert!(c.call(r#"{"op":"summary"}"#).contains("\"ok\":false"));
    let mut wrong = Client::connect(&srv.addr);
    let bad = wrong.call(&format!(r#"{{"op":"auth","token":"{}"}}"#, "x".repeat(64)));
    assert!(bad.contains("auth_failed"), "{bad}");
    let mut c = Client::connect(&srv.addr);
    assert!(c
        .call(&format!(r#"{{"op":"auth","token":"{secret}"}}"#))
        .contains("\"ok\":true"));
    assert!(c.call(r#"{"op":"ping"}"#).contains("\"ok\":true"));

    let ts = now_ms();
    let r = c.call(&format!(
        r#"{{"op":"oracle","market":1,"index_price":50000000000,"source_ts":{ts}}}"#
    ));
    assert!(
        r.contains("mark_updated") || r.contains("MarkUpdated") || r.contains("\"ok\":true"),
        "{r}"
    );
    for acct in [1, 2] {
        let d = c.call(&format!(
            r#"{{"op":"deposit","account":{acct},"amount":100000000000}}"#
        ));
        assert!(d.contains("\"ok\":true"), "{d}");
    }
    let maker = c.call(r#"{"op":"place","market":1,"account":2,"side":"sell","type":"limit","price":50000000000,"tif":"gtc","qty":10000,"client_id":1}"#);
    assert!(maker.contains("\"ok\":true"), "{maker}");
    let taker = c.call(r#"{"op":"place","market":1,"account":1,"side":"buy","type":"limit","price":50000000000,"tif":"ioc","qty":10000,"client_id":1}"#);
    assert!(
        taker.contains("\"ok\":true") && taker.contains("fill"),
        "{taker}"
    );
    let rejected = c.call(r#"{"op":"place","market":1,"account":1,"side":"buy","type":"limit","price":50000000001,"tif":"ioc","qty":10000,"client_id":2}"#);
    assert!(
        rejected.contains("\"ok\":false") && rejected.contains("invalid_price"),
        "{rejected}"
    );
    let acct = c.call(r#"{"op":"account","account":1}"#);
    assert!(acct.contains("\"size\":10000"), "{acct}");
    let book = c.call(r#"{"op":"book","market":1,"depth":5}"#);
    assert!(book.contains("\"ok\":true"));
    let summary = c.call(r#"{"op":"summary"}"#);
    assert!(summary.contains("\"fills\":1"), "{summary}");
    assert!(!summary.contains(&secret) && !acct.contains(&secret));
    drop(c);
    drop(srv); // SIGKILL: no clean shutdown

    // the server never prints the secret; the on-disk log must not contain it either
    let wal_bytes = fs::read(data.join("engine.wal")).unwrap();
    assert!(!wal_bytes
        .windows(secret.len())
        .any(|w| w == secret.as_bytes()));

    // the log verifies and replays to the same trade
    let wal = data.join("engine.wal");
    let verify = run(&["verify", wal.to_str().unwrap()]);
    assert_eq!(verify.code, 0, "{}{}", verify.stdout, verify.stderr);
    let positions = run(&["positions", wal.to_str().unwrap(), "--account", "2"]);
    assert!(positions.stdout.contains("-1.0000"), "{}", positions.stdout);

    // restart without --init recovers the state
    let srv2 = spawn_server(&data, &token_file, &[]);
    let mut c2 = Client::connect(&srv2.addr);
    assert!(c2
        .call(&format!(r#"{{"op":"auth","token":"{secret}"}}"#))
        .contains("\"ok\":true"));
    let acct2 = c2.call(r#"{"op":"account","account":1}"#);
    assert!(
        acct2.contains("\"size\":10000"),
        "state lost after restart: {acct2}"
    );
}

#[test]
fn server_accepts_the_token_from_an_environment_variable_only() {
    let t = TempDir::new("env");
    let data = t.path().join("data");
    let secret = "ab".repeat(32);
    let mut child = bin()
        .args([
            "serve",
            "--dir",
            data.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--init",
            "--token-env",
            "PERP_E2E_TOKEN",
        ])
        .env("PERP_E2E_TOKEN", &secret)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    let mut addr = String::new();
    for _ in 0..50 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        seen.push_str(&line);
        if let Some(rest) = line.strip_prefix("listening on ") {
            addr = rest.split_whitespace().next().unwrap().to_string();
            break;
        }
    }
    assert!(!addr.is_empty(), "{seen}");
    let mut c = Client::connect(&addr);
    assert!(c
        .call(&format!(r#"{{"op":"auth","token":"{secret}"}}"#))
        .contains("\"ok\":true"));
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        !seen.contains(&secret),
        "server output must never contain the token"
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_argument_is_a_usage_error_not_a_panic() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let out = bin()
        .arg("replay")
        .arg(OsString::from_vec(vec![0x66, 0xff, 0xfe]))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("argument 2 is not valid UTF-8"), "{err}");
    assert!(!err.contains("panicked"), "{err}");
}
