# 1. No third-party dependencies

Status: accepted

## Context
Every dependency is code that runs at build time (build scripts, proc macros)
and at run time, and each one needs vetting against a release-age cutoff. The
engine needs SHA-256, a binary codec, a small JSON parser and a TCP server.

## Decision
Use only `std`. SHA-256 is implemented from FIPS 180-4 and checked against the
NIST vectors; the JSON parser is strict, integer-only and bounded; the server is
`std::net` plus threads. `Cargo.lock` lists only the four workspace crates.

## Consequences
No build scripts, no proc macros, nothing to audit but this repository. The
price is more code here (hash, JSON), covered by tests. A bug in the hash would
weaken the log's integrity chain, not any secrecy property.
