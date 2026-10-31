# Security policy

This repository is an **educational** example. It is not audited, has no users
and must never hold real funds or face an untrusted network. The policy below
describes what it tries to get right and how to report problems.

## Reporting a vulnerability

Please report privately: use GitHub's "Report a vulnerability" (private
vulnerability reporting) on this repository, or write to kienphan.dev@gmail.com.
Do not open a public issue for a vulnerability. This is a hobby-scale project;
expect a best-effort reply, not an SLA.

## Threat model

In scope (inputs the code is written to survive without panicking, without
unbounded allocation and without corrupting state):

| Surface | Hostile input considered | Defence |
|---------|--------------------------|---------|
| Engine events | overflowing sizes/prices, zero sizes, huge cancel storms, hostile timestamps | validated before mutation, checked arithmetic (`overflow-checks = true` in release), size caps, `Result` everywhere; fail-stop halt if an internal invariant breaks |
| Log file | truncated, bit-flipped, zero-filled or oversized records | length cap before allocation, SHA-256 chain, torn-tail repair only when no valid record follows, mid-file damage is an error |
| Snapshot file | forged lengths, truncation, wrong hash | size limit checked against file size, trailer hash, strict decode with counts bounded by remaining bytes, invariant check after decode |
| TCP server (loopback by default) | unauthenticated clients, oversized lines, request floods, slow clients, token guessing | token required first, constant-time comparison of SHA-256 digests, line/connection/rate limits, bounded queue (backpressure), auth deadline and failure limit |

Out of scope, stated plainly:

* **Memory safety is not correctness.** Safe Rust rules out memory-unsafety; it does
  not rule out logic bugs, deadlocks, or unbounded growth by design. The tests
  check bounds, they do not prove them.
* **No TLS.** The TCP mode is plaintext. The token and all traffic can be read
  by anyone on the path. Do not bind a non-loopback address on a hostile network.
* **One shared token, full privileges.** There are no per-account permissions.
  Anyone with the token can deposit, withdraw, trade for any account and feed
  oracle prices.
* **No oracle security.** Prices are whatever a token holder sends.
* **Not audited**, simplified liquidation and ADL, single-threaded engine.
* The token is read from an environment variable or a file with mode 0600 and is
  never printed or logged; a local attacker who can read the process environment
  or memory is out of scope.

## Dependency policy

* Third-party crates: **none**. `Cargo.lock` lists only the four workspace
  crates (`perp-domain`, `perp-app`, `perp-infra`, `perp-cli`); an architecture
  test fails if that changes. Every crate version in the lock file was therefore
  published by this repository itself.
* Crates with a `build.rs`: **none**. Proc macros: **none**.
* Dependabot is configured for `cargo` and `github-actions` with a seven-day
  cooldown. GitHub Actions are pinned to full commit SHAs; only the first-party
  checkout action is used.
* `#![forbid(unsafe_code)]` in every crate, including tests and benches.

## Secrets

No keys, tokens or credentials are committed. Tests build any secret-looking
strings at run time. `.gitignore` excludes `.env*`, `*.pem`, `*.key`, `*.token`
and `.agent-undo`. The `gen-token` command refuses to print a token into a pipe or
log; it prints to an interactive terminal once, or writes a 0600 file.
