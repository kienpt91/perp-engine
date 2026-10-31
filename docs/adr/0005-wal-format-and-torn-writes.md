# 5. Log format and torn-write rule

Status: accepted

## Context
After a crash the last record may be partial. Mid-file damage must not be
silently dropped.

## Decision
Records are `len | seq + event | SHA-256(prev_chain, len, payload)`. On open, an
incomplete or unverifiable tail with no valid record after it is a torn write and
is truncated; damage followed by a valid record is corruption and is an error.
Lengths are capped before anything is allocated.

## Consequences
Torn-write recovery is tested at every byte offset of a fixture. Edge case: a
damaged tail record cannot be told from a torn one, so it is truncated.

## Update (security review finding WAL-001)
Truncating a damaged tail is irreversible, so it is now done carefully:
* An incomplete or all-zero tail is a torn write. It is repaired on open, but the
  dropped bytes are first copied to a sidecar file `<log>.torn-<offset>`.
* A **complete but damaged** final record (valid length, all bytes present, wrong
  hash) is never removed automatically: `open` fails with `DamagedTail` and
  leaves the file alone; `serve --repair` saves it to the sidecar and truncates.
  `verify --strict` fails on it.
* Residual: an invalid or oversized length field in the final record cannot be
  told from a torn write and is repaired (with the sidecar).
