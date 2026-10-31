//! Shared-secret token handling for the TCP server.
//!
//! A [`Token`] keeps only the SHA-256 digest of the secret, so the plaintext is
//! not retained by the server after start-up and cannot be printed by `Debug`.
//! Candidates are verified by hashing them and comparing digests in constant
//! time, which also removes any dependence on the candidate's length.
//!
//! Limits, stated plainly:
//!
//! * One token grants full privileges. There are no users, roles or expiry.
//! * Plaintext is briefly held in ordinary heap memory while loading (the file
//!   buffer is overwritten afterwards, best effort). Safe Rust cannot
//!   guarantee that no copies remain.
//! * [`generate_token`] reads `/dev/urandom`; it is available on Unix only.
//! * The file-permission check (`mode & 0o077 == 0`) exists on Unix only.
//!
//! Error values never contain token contents.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;

use perp_domain::sha256::{ct_eq, sha256, to_hex, Digest};

/// Shortest accepted token, in characters.
pub const MIN_TOKEN_LEN: usize = 32;
/// Longest accepted token, in characters.
pub const MAX_TOKEN_LEN: usize = 256;
/// Largest token file that is read, in bytes.
pub const MAX_TOKEN_FILE_BYTES: usize = 1024;
/// Random bytes behind a generated token (64 hex characters).
const GENERATED_TOKEN_BYTES: usize = 32;

/// Why a token could not be created, loaded or generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// Shorter than [`MIN_TOKEN_LEN`].
    TooShort,
    /// Longer than [`MAX_TOKEN_LEN`].
    TooLong,
    /// Contains characters outside `[A-Za-z0-9_-]`.
    InvalidChars,
    /// Environment variable not set.
    EnvNotSet(String),
    /// Environment variable is not valid Unicode.
    EnvNotUnicode(String),
    /// Token file is not a regular file.
    NotRegularFile,
    /// Token file grants access to group or other (Unix mode bits shown).
    InsecurePermissions {
        /// Permission bits of the file (`mode & 0o777`).
        mode: u32,
    },
    /// Token file is larger than [`MAX_TOKEN_FILE_BYTES`].
    FileTooLarge,
    /// Token file is not valid UTF-8.
    FileNotUtf8,
    /// An I/O operation failed.
    Io {
        /// What was being done.
        op: &'static str,
        /// The error kind reported by the operating system.
        kind: io::ErrorKind,
    },
    /// Refusing to print a token to something that is not a terminal.
    RefusedToPrint,
    /// Not available on this platform.
    Unsupported,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::TooShort => write!(f, "token must be at least {MIN_TOKEN_LEN} characters"),
            TokenError::TooLong => write!(f, "token must be at most {MAX_TOKEN_LEN} characters"),
            TokenError::InvalidChars => {
                write!(f, "token may only contain the characters A-Z a-z 0-9 _ -")
            }
            TokenError::EnvNotSet(var) => write!(f, "environment variable {var} is not set"),
            TokenError::EnvNotUnicode(var) => {
                write!(f, "environment variable {var} is not valid unicode")
            }
            TokenError::NotRegularFile => write!(f, "token file is not a regular file"),
            TokenError::InsecurePermissions { mode } => write!(
                f,
                "token file mode {mode:03o} allows group/other access; run chmod 600 on it"
            ),
            TokenError::FileTooLarge => {
                write!(f, "token file is larger than {MAX_TOKEN_FILE_BYTES} bytes")
            }
            TokenError::FileNotUtf8 => write!(f, "token file is not valid UTF-8"),
            TokenError::Io { op, kind } => write!(f, "{op} failed: {kind}"),
            TokenError::RefusedToPrint => write!(
                f,
                "refusing to print a token because stdout is not a terminal; \
                 use --out <file> to write it to a private file instead"
            ),
            TokenError::Unsupported => write!(f, "not supported on this platform"),
        }
    }
}

impl std::error::Error for TokenError {}

fn io_err(op: &'static str, e: &io::Error) -> TokenError {
    TokenError::Io { op, kind: e.kind() }
}

/// SHA-256 digest of an accepted token. The plaintext is not stored.
#[derive(Clone)]
pub struct Token {
    digest: Digest,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Validate `secret` (32..=256 characters of `[A-Za-z0-9_-]`) and keep its
    /// digest.
    pub fn new(secret: &str) -> Result<Token, TokenError> {
        let n = secret.len();
        if n < MIN_TOKEN_LEN {
            return Err(TokenError::TooShort);
        }
        if n > MAX_TOKEN_LEN {
            return Err(TokenError::TooLong);
        }
        if !secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(TokenError::InvalidChars);
        }
        Ok(Token {
            digest: sha256(secret.as_bytes()),
        })
    }

    /// Constant-time check of `candidate` against the stored digest.
    ///
    /// The candidate is hashed first, so the comparison always covers 32 bytes
    /// whatever its length.
    pub fn verify(&self, candidate: &str) -> bool {
        ct_eq(&self.digest, &sha256(candidate.as_bytes()))
    }
}

/// Read 32 bytes from the operating system's random source and return them as
/// 64 lower-case hex characters.
///
/// Unix only (reads `/dev/urandom`); other platforms get an
/// [`io::ErrorKind::Unsupported`] error.
pub fn generate_token() -> io::Result<String> {
    #[cfg(unix)]
    {
        let mut f = File::open("/dev/urandom")?;
        let mut raw = [0u8; GENERATED_TOKEN_BYTES];
        f.read_exact(&mut raw)?;
        let hex = to_hex(&raw);
        raw.fill(0);
        Ok(hex)
    }
    #[cfg(not(unix))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "token generation needs /dev/urandom (Unix only)",
        ))
    }
}

/// Load a token from the environment variable `var_name` (surrounding
/// whitespace is trimmed).
pub fn load_from_env(var_name: &str) -> Result<Token, TokenError> {
    match std::env::var(var_name) {
        Ok(value) => Token::new(value.trim()),
        Err(std::env::VarError::NotPresent) => Err(TokenError::EnvNotSet(var_name.to_string())),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(TokenError::EnvNotUnicode(var_name.to_string()))
        }
    }
}

/// Load a token from a file.
///
/// On Unix the file must be a regular file with no group/other permission
/// bits. At most [`MAX_TOKEN_FILE_BYTES`] are read; surrounding whitespace is
/// trimmed.
pub fn load_from_file(path: &Path) -> Result<Token, TokenError> {
    let file = File::open(path).map_err(|e| io_err("open token file", &e))?;
    let meta = file.metadata().map_err(|e| io_err("stat token file", &e))?;
    if !meta.is_file() {
        return Err(TokenError::NotRegularFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(TokenError::InsecurePermissions { mode: mode & 0o777 });
        }
    }
    let limit = u64::try_from(MAX_TOKEN_FILE_BYTES).map_or(u64::MAX, |n| n.saturating_add(1));
    let mut buf = Vec::with_capacity(MAX_TOKEN_FILE_BYTES + 1);
    let read = file.take(limit).read_to_end(&mut buf);
    let result = match read {
        Err(e) => Err(io_err("read token file", &e)),
        Ok(_) if buf.len() > MAX_TOKEN_FILE_BYTES => Err(TokenError::FileTooLarge),
        Ok(_) => match std::str::from_utf8(&buf) {
            Ok(text) => Token::new(text.trim()),
            Err(_) => Err(TokenError::FileNotUtf8),
        },
    };
    buf.fill(0);
    result
}

/// Implementation of the `gen-token` command.
///
/// With `out`: creates the file exclusively (it must not exist) with mode 0600
/// on Unix, writes the token and a newline, and prints only the path to
/// stdout. Without `out`: prints the token to stdout only when stdout is a
/// terminal (with a note on stderr); otherwise refuses, so a token cannot end
/// up in a pipe or a CI log by accident.
pub fn cmd_gen_token(out: Option<&Path>) -> Result<(), TokenError> {
    let stdout = io::stdout();
    let is_terminal = stdout.is_terminal();
    gen_token_to(
        out,
        is_terminal,
        &mut stdout.lock(),
        &mut io::stderr().lock(),
    )
}

fn gen_token_to(
    out: Option<&Path>,
    stdout_is_terminal: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), TokenError> {
    match out {
        Some(path) => {
            let token = generate_token().map_err(|e| {
                if e.kind() == io::ErrorKind::Unsupported {
                    TokenError::Unsupported
                } else {
                    io_err("read random bytes", &e)
                }
            })?;
            write_new_private_file(path, &token)?;
            writeln!(stdout, "{}", path.display()).map_err(|e| io_err("write stdout", &e))
        }
        None => {
            if !stdout_is_terminal {
                return Err(TokenError::RefusedToPrint);
            }
            let token = generate_token().map_err(|e| {
                if e.kind() == io::ErrorKind::Unsupported {
                    TokenError::Unsupported
                } else {
                    io_err("read random bytes", &e)
                }
            })?;
            writeln!(stdout, "{token}").map_err(|e| io_err("write stdout", &e))?;
            writeln!(
                stderr,
                "note: this token is shown once and is not stored; save it now"
            )
            .map_err(|e| io_err("write stderr", &e))
        }
    }
}

fn write_new_private_file(path: &Path, token: &str) -> Result<(), TokenError> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|e| io_err("create token file", &e))?;
    let written = file
        .write_all(token.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all());
    if let Err(e) = written {
        drop(file);
        // Do not leave a half-written secret behind.
        let _ = std::fs::remove_file(path);
        return Err(io_err("write token file", &e));
    }
    Ok(())
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!(
                "perp-engine-token-test-{}-{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn good() -> String {
        "a1B2_c3-".repeat(5) // 40 chars
    }

    #[cfg(unix)]
    fn write_with_mode(path: &Path, content: &[u8], mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, content).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn generated_tokens_differ_and_are_hex() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert_eq!(t.len(), 64);
            assert!(t
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
            Token::new(t).unwrap();
        }
    }

    #[test]
    fn generated_tokens_are_not_constant_across_many_draws() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            assert!(seen.insert(generate_token().unwrap()));
        }
    }

    #[test]
    fn debug_is_redacted() {
        let secret = good();
        let t = Token::new(&secret).unwrap();
        let s = format!("{t:?}");
        assert_eq!(s, "Token(<redacted>)");
        assert!(!s.contains(&secret));
        assert_eq!(format!("{t:#?}"), "Token(<redacted>)");
    }

    #[test]
    fn verify_true_and_false() {
        let secret = good();
        let t = Token::new(&secret).unwrap();
        assert!(t.verify(&secret));
        assert!(!t.verify(""));
        assert!(!t.verify("wrong"));
        assert!(!t.verify(&secret.to_uppercase()));
    }

    #[test]
    fn verify_rejects_off_by_one_char() {
        let secret = good();
        let t = Token::new(&secret).unwrap();
        let mut changed = secret.clone().into_bytes();
        let last = changed.len() - 1;
        changed[last] = if changed[last] == b'x' { b'y' } else { b'x' };
        assert!(!t.verify(std::str::from_utf8(&changed).unwrap()));
        let mut first = secret.clone().into_bytes();
        first[0] = if first[0] == b'x' { b'y' } else { b'x' };
        assert!(!t.verify(std::str::from_utf8(&first).unwrap()));
        // Prefix, suffix and padded variants.
        assert!(!t.verify(&secret[..secret.len() - 1]));
        assert!(!t.verify(&format!("{secret}x")));
        assert!(!t.verify(&format!("{secret}\n")));
        assert!(!t.verify(&format!(" {secret}")));
    }

    #[test]
    fn verify_handles_huge_and_odd_candidates() {
        let t = Token::new(&good()).unwrap();
        assert!(!t.verify(&"a".repeat(1_000_000)));
        assert!(!t.verify("\u{0}"));
        assert!(!t.verify("\u{1f600}"));
    }

    #[test]
    fn length_extremes() {
        assert_eq!(
            Token::new(&"a".repeat(MIN_TOKEN_LEN - 1)).unwrap_err(),
            TokenError::TooShort
        );
        assert!(Token::new(&"a".repeat(MIN_TOKEN_LEN)).is_ok());
        assert!(Token::new(&"a".repeat(MAX_TOKEN_LEN)).is_ok());
        assert_eq!(
            Token::new(&"a".repeat(MAX_TOKEN_LEN + 1)).unwrap_err(),
            TokenError::TooLong
        );
        assert_eq!(Token::new("").unwrap_err(), TokenError::TooShort);
        assert_eq!(
            Token::new(&"a".repeat(1_000_000)).unwrap_err(),
            TokenError::TooLong
        );
    }

    #[test]
    fn character_set_is_enforced() {
        for bad in [" ", "+", "/", "=", ".", "\n", "\u{e9}", "\u{0}", "$"] {
            let s = format!("{}{}", "a".repeat(40), bad);
            assert_eq!(
                Token::new(&s).unwrap_err(),
                TokenError::InvalidChars,
                "{bad:?}"
            );
        }
        assert!(Token::new(&"-_Az09".repeat(7)).is_ok());
    }

    #[test]
    fn token_errors_do_not_contain_the_token() {
        let secret = format!("{}!", "Zq9".repeat(12));
        let e = Token::new(&secret).unwrap_err();
        assert!(!e.to_string().contains(&secret));
        assert!(!format!("{e:?}").contains(&secret));
        let short = "ShortSecret123";
        let e = Token::new(short).unwrap_err();
        assert!(!e.to_string().contains(short));
        assert!(!format!("{e:?}").contains(short));
    }

    #[test]
    fn env_loading() {
        let var = format!("PERP_ENGINE_TEST_TOKEN_OK_{}", std::process::id());
        let secret = good();
        std::env::set_var(&var, format!("  {secret}\n"));
        let t = load_from_env(&var).unwrap();
        assert!(t.verify(&secret));
        std::env::remove_var(&var);
    }

    #[test]
    fn env_missing_and_invalid() {
        let var = format!("PERP_ENGINE_TEST_TOKEN_MISSING_{}", std::process::id());
        assert_eq!(
            load_from_env(&var).unwrap_err(),
            TokenError::EnvNotSet(var.clone())
        );
        let var2 = format!("PERP_ENGINE_TEST_TOKEN_SHORT_{}", std::process::id());
        let short = "tooshortvalue";
        std::env::set_var(&var2, short);
        let e = load_from_env(&var2).unwrap_err();
        assert_eq!(e, TokenError::TooShort);
        assert!(!e.to_string().contains(short));
        std::env::remove_var(&var2);
    }

    #[cfg(unix)]
    #[test]
    fn env_not_unicode() {
        use std::os::unix::ffi::OsStrExt;
        let var = format!("PERP_ENGINE_TEST_TOKEN_BYTES_{}", std::process::id());
        std::env::set_var(&var, std::ffi::OsStr::from_bytes(&[0xff, 0xfe, 0xfd]));
        assert_eq!(
            load_from_env(&var).unwrap_err(),
            TokenError::EnvNotUnicode(var.clone())
        );
        std::env::remove_var(&var);
    }

    #[cfg(unix)]
    #[test]
    fn file_with_mode_0600_loads_and_trims() {
        let dir = TempDir::new();
        let p = dir.path("tok");
        let secret = good();
        write_with_mode(&p, format!("{secret}\r\n\n").as_bytes(), 0o600);
        let t = load_from_file(&p).unwrap();
        assert!(t.verify(&secret));
        write_with_mode(&p, secret.as_bytes(), 0o400);
        assert!(load_from_file(&p).unwrap().verify(&secret));
    }

    #[cfg(unix)]
    #[test]
    fn file_with_group_or_other_bits_is_refused() {
        let dir = TempDir::new();
        let p = dir.path("tok");
        let secret = good();
        for mode in [0o644, 0o640, 0o604, 0o660, 0o666, 0o601, 0o610] {
            write_with_mode(&p, secret.as_bytes(), mode);
            let e = load_from_file(&p).unwrap_err();
            assert_eq!(e, TokenError::InsecurePermissions { mode }, "{mode:o}");
            assert!(!e.to_string().contains(&secret));
        }
    }

    #[cfg(unix)]
    #[test]
    fn oversize_file_is_refused() {
        let dir = TempDir::new();
        let p = dir.path("tok");
        write_with_mode(&p, "a".repeat(MAX_TOKEN_FILE_BYTES + 1).as_bytes(), 0o600);
        assert_eq!(load_from_file(&p).unwrap_err(), TokenError::FileTooLarge);
        // Exactly at the limit is read, then rejected by the length rule.
        write_with_mode(&p, "a".repeat(MAX_TOKEN_FILE_BYTES).as_bytes(), 0o600);
        assert_eq!(load_from_file(&p).unwrap_err(), TokenError::TooLong);
    }

    #[cfg(unix)]
    #[test]
    fn file_not_utf8_and_empty() {
        let dir = TempDir::new();
        let p = dir.path("tok");
        write_with_mode(&p, &[0xff; 40], 0o600);
        assert_eq!(load_from_file(&p).unwrap_err(), TokenError::FileNotUtf8);
        write_with_mode(&p, b"", 0o600);
        assert_eq!(load_from_file(&p).unwrap_err(), TokenError::TooShort);
        write_with_mode(&p, b"\n\n", 0o600);
        assert_eq!(load_from_file(&p).unwrap_err(), TokenError::TooShort);
    }

    #[test]
    fn file_missing_and_directory() {
        let dir = TempDir::new();
        let e = load_from_file(&dir.path("nope")).unwrap_err();
        assert!(matches!(
            e,
            TokenError::Io {
                kind: io::ErrorKind::NotFound,
                ..
            }
        ));
        let e = load_from_file(&dir.0).unwrap_err();
        assert!(matches!(
            e,
            TokenError::NotRegularFile | TokenError::Io { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn gen_token_with_out_writes_private_file_and_prints_only_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let p = dir.path("new.token");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        gen_token_to(Some(&p), false, &mut stdout, &mut stderr).unwrap();
        let content = std::fs::read_to_string(&p).unwrap();
        assert_eq!(content.len(), 65);
        assert!(content.ends_with('\n'));
        let token = content.trim_end();
        assert_eq!(token.len(), 64);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let out = String::from_utf8(stdout).unwrap();
        assert_eq!(out.trim_end(), p.display().to_string());
        assert!(!out.contains(token));
        assert!(stderr.is_empty());
        // The file loads back as a valid token.
        assert!(load_from_file(&p).unwrap().verify(token));
    }

    #[test]
    fn gen_token_refuses_to_overwrite() {
        let dir = TempDir::new();
        let p = dir.path("exists.token");
        std::fs::write(&p, b"keep me").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let e = gen_token_to(Some(&p), false, &mut stdout, &mut stderr).unwrap_err();
        assert_eq!(
            e,
            TokenError::Io {
                op: "create token file",
                kind: io::ErrorKind::AlreadyExists
            }
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"keep me");
        assert!(stdout.is_empty());
    }

    #[test]
    fn gen_token_without_out_refuses_when_not_a_terminal() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let e = gen_token_to(None, false, &mut stdout, &mut stderr).unwrap_err();
        assert_eq!(e, TokenError::RefusedToPrint);
        assert!(e.to_string().contains("--out"));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn gen_token_without_out_prints_once_on_terminal() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        gen_token_to(None, true, &mut stdout, &mut stderr).unwrap();
        let out = String::from_utf8(stdout).unwrap();
        assert_eq!(out.lines().count(), 1);
        let token = out.trim_end();
        assert_eq!(token.len(), 64);
        Token::new(token).unwrap();
        let note = String::from_utf8(stderr).unwrap();
        assert_eq!(note.lines().count(), 1);
        assert!(!note.contains(token));
    }

    #[test]
    fn io_errors_have_no_token_text() {
        let e = TokenError::Io {
            op: "open token file",
            kind: io::ErrorKind::PermissionDenied,
        };
        assert_eq!(e.to_string(), "open token file failed: permission denied");
    }
}
