//! End-to-end tests of the log-safety switches of `serve` and `verify`: a
//! complete but damaged final record is never removed without `--repair`
//! (and is saved to a sidecar when it is), and a log at its size limit makes the
//! server read-only instead of killing it.
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
use std::time::Duration;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("perp-repair-{tag}-{}-{n}", std::process::id()));
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

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Out {
    let o = Command::new(env!("CARGO_BIN_EXE_perp-engine"))
        .args(args)
        .output()
        .unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn simulate_into(dir: &Path, events: &str) {
    let out = run(&[
        "simulate",
        "--dir",
        dir.to_str().unwrap(),
        "--seed",
        "5",
        "--events",
        events,
        "--accounts",
        "60",
        "--fsync",
        "never",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
}

fn make_token(dir: &Path) -> (PathBuf, String) {
    let file = dir.join("token");
    assert_eq!(run(&["gen-token", "--out", file.to_str().unwrap()]).code, 0);
    let secret = fs::read_to_string(&file).unwrap().trim().to_string();
    (file, secret)
}

/// Start offset of the last record of a log image.
fn last_record_start(bytes: &[u8]) -> usize {
    let mut pos = 16;
    let mut last = pos;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        last = pos;
        pos += 4 + len + 32;
    }
    assert_eq!(pos, bytes.len(), "image ends on a record boundary");
    last
}

struct Server {
    child: Child,
    addr: String,
    seen: String,
    _stderr: BufReader<std::process::ChildStderr>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_server(dir: &Path, token_file: &Path, extra: &[&str]) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_perp-engine"))
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
        seen,
        _stderr: reader,
    }
}

struct Client {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Client {
    fn connect(addr: &str, secret: &str) -> Client {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let mut c = Client { stream, reader };
        let r = c.call(&format!(r#"{{"op":"auth","token":"{secret}"}}"#));
        assert!(r.contains("\"ok\":true"), "{r}");
        c
    }
    fn call(&mut self, line: &str) -> String {
        writeln!(self.stream, "{line}").unwrap();
        let mut resp = String::new();
        self.reader.read_line(&mut resp).unwrap();
        resp
    }
}

#[test]
fn verify_reports_a_damaged_tail_and_strict_verification_fails() {
    let t = TempDir::new("verify");
    simulate_into(t.path(), "1500");
    let mut bytes = fs::read(t.path().join("engine.wal")).unwrap();
    let last = last_record_start(&bytes);
    bytes[last + 8] ^= 0x01;
    let bad = t.path().join("bad.wal");
    fs::write(&bad, &bytes).unwrap();
    let bad = bad.to_str().unwrap();

    let v = run(&["verify", bad]);
    assert_eq!(v.code, 0, "{}", v.stderr);
    assert!(
        v.stdout.contains("damaged tail          YES"),
        "{}",
        v.stdout
    );
    let strict = run(&["verify", bad, "--strict"]);
    assert_eq!(strict.code, 1);
    assert!(strict.stderr.contains("damaged"), "{}", strict.stderr);
    assert!(strict.stderr.contains("--repair"), "{}", strict.stderr);
    assert_eq!(
        fs::read(bad).unwrap(),
        bytes,
        "verify never modifies the file"
    );

    // An intact log reports no damage.
    let ok = run(&[
        "verify",
        t.path().join("engine.wal").to_str().unwrap(),
        "--strict",
    ]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    assert!(ok.stdout.contains("damaged tail          no"));
}

#[test]
fn serve_refuses_a_damaged_tail_without_repair_and_saves_it_with_repair() {
    let t = TempDir::new("serve-repair");
    let (token, _) = make_token(t.path());
    let data = t.path().join("data");
    simulate_into(&data, "1500");
    let wal = data.join("engine.wal");
    let mut bytes = fs::read(&wal).unwrap();
    let last = last_record_start(&bytes);
    bytes[last + 8] ^= 0x01;
    fs::write(&wal, &bytes).unwrap();
    let sidecar = data.join(format!("engine.wal.torn-{last}"));

    let refused = run(&[
        "serve",
        "--dir",
        data.to_str().unwrap(),
        "--token-file",
        token.to_str().unwrap(),
    ]);
    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(refused.stderr.contains("repair"), "{}", refused.stderr);
    assert_eq!(fs::read(&wal).unwrap(), bytes, "file untouched");
    assert!(!sidecar.exists());

    let srv = spawn_server(&data, &token, &["--repair"]);
    assert!(srv.seen.contains("saved to"), "{}", srv.seen);
    drop(srv);
    assert_eq!(fs::read(&sidecar).unwrap(), &bytes[last..]);
    assert_eq!(fs::read(&wal).unwrap(), &bytes[..last]);
    let v = run(&["verify", wal.to_str().unwrap(), "--strict"]);
    assert_eq!(v.code, 0, "{}{}", v.stdout, v.stderr);
}

#[test]
fn a_torn_tail_is_still_repaired_automatically_and_saved() {
    let t = TempDir::new("serve-torn");
    let (token, _) = make_token(t.path());
    let data = t.path().join("data");
    simulate_into(&data, "1500");
    let wal = data.join("engine.wal");
    let bytes = fs::read(&wal).unwrap();
    let last = last_record_start(&bytes);
    let torn = &bytes[..bytes.len() - 5];
    fs::write(&wal, torn).unwrap();
    let srv = spawn_server(&data, &token, &[]);
    drop(srv);
    let sidecar = data.join(format!("engine.wal.torn-{last}"));
    assert_eq!(fs::read(sidecar).unwrap(), &torn[last..]);
    assert_eq!(fs::read(&wal).unwrap(), &bytes[..last]);
}

#[test]
fn max_log_mib_is_validated() {
    let t = TempDir::new("maxlog-usage");
    let (token, _) = make_token(t.path());
    let data = t.path().join("data");
    for bad in ["0", "abc", "-1", "18446744073709551615"] {
        let out = run(&[
            "serve",
            "--dir",
            data.to_str().unwrap(),
            "--token-file",
            token.to_str().unwrap(),
            "--max-log-mib",
            bad,
            "--init",
        ]);
        assert_eq!(out.code, 2, "--max-log-mib {bad}: {}", out.stderr);
    }
    assert!(
        !data.join("engine.wal").exists(),
        "nothing was created for a bad option"
    );
    // --repair is a switch.
    let out = run(&["serve", "--dir", "x", "--repair=yes"]);
    assert_eq!(out.code, 2);
}

#[test]
fn a_log_at_its_size_limit_makes_the_server_read_only_and_a_bigger_limit_resumes() {
    let t = TempDir::new("maxlog");
    let (token, secret) = make_token(t.path());
    let data = t.path().join("data");
    simulate_into(&data, "30000");
    let wal = data.join("engine.wal");
    assert!(
        fs::metadata(&wal).unwrap().len() > 1 << 20,
        "the log must exceed the 1 MiB limit used below"
    );

    let srv = spawn_server(&data, &token, &["--max-log-mib", "1"]);
    let mut c = Client::connect(&srv.addr, &secret);
    let before = c.call(r#"{"op":"summary"}"#);
    assert!(before.contains("\"ok\":true"), "{before}");
    for _ in 0..3 {
        let r = c.call(r#"{"op":"deposit","account":1,"amount":1000000}"#);
        assert!(r.contains("\"ok\":false") && r.contains("read_only"), "{r}");
    }
    // The server is alive and queries work.
    assert!(c.call(r#"{"op":"ping"}"#).contains("\"ok\":true"));
    assert!(c
        .call(r#"{"op":"account","account":1}"#)
        .contains("\"ok\":true"));
    assert_eq!(c.call(r#"{"op":"summary"}"#), before, "nothing was applied");
    drop(c);
    drop(srv);

    // Restart with the default limit: writes resume.
    let srv = spawn_server(&data, &token, &[]);
    let mut c = Client::connect(&srv.addr, &secret);
    let r = c.call(r#"{"op":"deposit","account":1,"amount":1000000}"#);
    assert!(r.contains("\"ok\":true"), "{r}");
}
