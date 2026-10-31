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
