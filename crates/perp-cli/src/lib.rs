//! Command line front end of the educational perpetual-futures engine.
//!
//! `run` takes the arguments and two output streams and returns the process
//! exit code, so the commands can be tested without spawning processes.
//! **Not for real funds.**
#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]
#![warn(missing_docs)]

pub mod args;
mod commands;
pub mod render;

use args::Args;
use std::io::Write;

/// Usage text.
pub const USAGE: &str = "\
perp-engine: educational perpetual-futures exchange core. NOT for real funds.

USAGE:
  perp-engine simulate [--seed N] [--events N] [--accounts N] [--markets N]
                       [--dir DIR] [--fsync always|every:N|never]
                       [--snapshot-every N] [--checkpoints N] [--check-every N]
  perp-engine replay <wal>                  replay a log and print the final state
  perp-engine verify <wal> [--strict]       check the hash chain, replay, check invariants
  perp-engine book <wal> [--market N] [--depth N]
  perp-engine positions <wal> [--account N]
  perp-engine serve --dir DIR [--listen ADDR] [--token-env NAME | --token-file PATH]
                    [--allow-non-loopback] [--init] [--markets N]
                    [--fsync always|every:N|never] [--snapshot-every N]
  perp-engine gen-token [--out FILE]        create a random auth token (never logged)
  perp-engine help

Exit codes: 0 success, 1 failure (including failed verification), 2 usage error.
";

/// Exit code for success.
pub const EXIT_OK: i32 = 0;
/// Exit code for runtime failures.
pub const EXIT_FAILURE: i32 = 1;
/// Exit code for usage errors.
pub const EXIT_USAGE: i32 = 2;

/// Failure of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// Bad arguments.
    Usage(String),
    /// The command ran and failed.
    Failure(String),
}

/// Run the command line. Returns the process exit code.
pub fn run(argv: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let args = match Args::parse(argv) {
        Ok(a) => a,
        Err(e) => return usage_error(err, &e),
    };
    if args.has("help") {
        let _ = out.write_all(USAGE.as_bytes());
        return EXIT_OK;
    }
    match commands::dispatch(&args, out, err) {
        Ok(()) => EXIT_OK,
        Err(CliError::Usage(m)) => usage_error(err, &m),
        Err(CliError::Failure(m)) => {
            let _ = writeln!(err, "error: {m}");
            EXIT_FAILURE
        }
    }
}

fn usage_error(err: &mut dyn Write, msg: &str) -> i32 {
    let _ = writeln!(err, "error: {msg}\n");
    let _ = err.write_all(USAGE.as_bytes());
    EXIT_USAGE
}
