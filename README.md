# perp-engine

An **educational** perpetual-futures exchange core in safe Rust: a deterministic
matching engine, cross-margin accounting, funding, liquidation with an insurance
fund and auto-deleveraging, and a crash-safe event log with exact replay.

> **Not for real funds.** This is a learning project by one person. It has not
> been audited, has no users, no customers and no production deployment, and is
> not affiliated with any exchange. It is **not a clone** of any venue, uses no
> code or proprietary material from any venue, and has no consensus, L1, wallet
> or oracle-security layer. It models the parts of a perp venue that are easy to
> get subtly wrong (integer money, priority rules, margin, funding, liquidation,
> replay) and tests them hard. Read [Limitations](#limitations) before using any
> of it for anything.

## What is and is not modelled

| Modelled | Not modelled |
|----------|--------------|
| Fixed-point price, size and money types, checked arithmetic, documented rounding | Floating point (deliberately absent), multiple collateral assets |
| Order book with price-time priority, limit GTC/IOC/FOK/post-only, market orders with a slippage limit, reduce-only, cancel, cancel-replace, self-trade prevention, tick/lot/min-notional | Iceberg, stop and trigger orders, order modification of side/market, batch auctions |
| Price band around the mark (limit prices, market-order slippage, resting orders swept when the mark moves) and execution-price loss in the margin test | Circuit breakers, auctions, price protection against a malicious oracle |
| Cross-margin accounts, tiered margin schedule, max leverage, maker/taker fees with rebates, deposits and margin-checked withdrawals | Isolated margin, portfolio margin, sub-accounts, fee tiers driven by volume |
| Oracle events with staleness checks and a bounded change filter, mark price, premium-based funding with a clamp and an exact zero-sum split | Oracle security, multiple sources, TWAP windows |
| Liquidation by IOC orders at a documented price rule, insurance fund, simple deterministic auto-deleveraging, bad-debt reporting | Liquidation auctions, partial fills by external liquidators, real ADL ranking by leverage |
| Event-sourced state machine, write-ahead log with SHA-256 hash chain and torn-write repair, snapshots, state hash | Replication, consensus, multi-writer logs, log rotation |
| CLI, deterministic traffic simulator, JSON-lines TCP server on loopback with a token | TLS, per-user permissions, a public API, a UI |

## Architecture

The code follows a layered design (domain-driven): a pure domain, an application
layer of use cases and ports, and adapters. Each layer is a Cargo crate, so the
dependency rule is enforced by the compiler; an architecture test also checks the
manifests. See [docs/domain.md](docs/domain.md) for the glossary and the context
map and [docs/adr](docs/adr) for the decisions.

```mermaid
flowchart TB
    subgraph cli["perp-cli (binary)"]
        CMD["simulate / replay / verify / book / positions / serve / gen-token"]
    end
    subgraph infra["perp-infra (adapters)"]
        WAL["file WAL<br/>length-prefixed, SHA-256 chain"]
        SNAP["snapshot files"]
        TCP["JSON-lines TCP server<br/>token, limits, backpressure"]
        CLK["system clock, VmRSS probe"]
    end
    subgraph app["perp-app (use cases and ports)"]
        SVC["ExchangeService<br/>recover, log-then-apply, snapshot"]
        SIM["run_simulation"]
        PORTS["ports: EventLog, SnapshotStore, Clock, ResourceProbe, ExchangeApi"]
        FAKES["in-memory fakes + contract suites"]
    end
    subgraph dom["perp-domain (pure: no I/O, clock or randomness)"]
        ENG["Engine aggregate<br/>apply(Event) -> Outputs"]
        BOOK["Book"]
        MARGIN["margin tiers"]
        CODEC["canonical codec, SHA-256"]
        GEN["seeded Rng, TrafficGenerator"]
    end
    CMD --> SVC
    CMD --> TCP
    TCP --> PORTS
    WAL -. implements .-> PORTS
    SNAP -. implements .-> PORTS
    CLK -. implements .-> PORTS
    FAKES -. implements .-> PORTS
    SVC --> PORTS
    SVC --> ENG
    SIM --> SVC
    SIM --> GEN
    ENG --> BOOK
    ENG --> MARGIN
    ENG --> CODEC
```

```
perp-cli  ->  perp-infra  ->  perp-app  ->  perp-domain
```

| Crate | Layer | Contains | Third-party deps |
|-------|-------|----------|------------------|
| `perp-domain` | domain | fixed-point values, `Engine` aggregate, book, margin, funding, liquidation, codec, SHA-256, seeded RNG, traffic generator | none (not even `perp-*`) |
| `perp-app` | application | ports, `ExchangeService`, simulation use case, in-memory fakes, shared contract suites | none |
| `perp-infra` | adapters | file WAL, snapshot files, strict JSON, protocol, TCP server, token, clock, memory probe | none |
| `perp-cli` | adapters | the `perp-engine` binary, benchmark, end-to-end and architecture tests | none |

`Cargo.lock` lists exactly these four crates: no third-party crates, no build
scripts, no proc macros, and `#![forbid(unsafe_code)]` in every crate.

### How an event flows

1. `ExchangeService::submit` stamps the event with a non-decreasing time (the
   clock is a port; the domain never reads one).
2. The event is appended to the log **first** (`fsync` per policy).
3. `Engine::apply` validates, then mutates. A rejected event changes only the
   applied-event counter and is still in the log, so replay reproduces rejections
   too. An error after mutation began halts the engine (fail-stop) instead of
   continuing on corrupt state.
4. Outputs (fills, cancels, funding, liquidation, ...) are returned.
5. Every N events a snapshot is written; recovery = newest valid snapshot + replay
   of the log tail.

## Quick start

Everything is built and tested in a throwaway container; no Rust is needed on the
host. Docker pulls the pinned toolchain on first use.

```sh
scripts/container.sh cargo fetch --locked
scripts/container.sh scripts/run-layers.sh          # all layers, one after the other
scripts/container.sh scripts/coverage.sh            # coverage table (see below)
scripts/container.sh cargo build --release --locked
scripts/container.sh /target/release/perp-engine simulate --events 200000
```

With a local toolchain the same commands work without the wrapper
(`rust-toolchain.toml` pins 1.95.0 with rustfmt, clippy and llvm-tools).

```text
perp-engine simulate [--seed N] [--events N] [--accounts N] [--markets N]
                     [--dir DIR] [--fsync always|every:N|never]
                     [--snapshot-every N] [--checkpoints N] [--check-every N]
perp-engine replay <wal>                 replay a log and print the final state
perp-engine verify <wal> [--strict]      hash chain + replay + invariants
perp-engine book <wal> [--market N] [--depth N]
perp-engine positions <wal> [--account N]
perp-engine serve --dir DIR [--listen ADDR] [--token-env NAME | --token-file PATH]
                  [--allow-non-loopback] [--init] [--markets N]
                  [--fsync always|every:N|never] [--snapshot-every N]
                  [--max-log-mib N] [--repair]
perp-engine gen-token [--out FILE]
```

### A real simulation run

`simulate` generates seeded order flow from five trader behaviours (market maker,
noise taker, trend follower, leveraged whale, churner), pushes every event through
the durable service into a real file log, then `verify` re-reads the log. Output
of `simulate --dir /tmp/simdir --seed 1 --events 200000 --accounts 200 --checkpoints 50000 --fsync never --snapshot-every 50000`
followed by `verify /tmp/simdir/engine.wal --strict` (release build, container):

```text
log: /tmp/simdir/engine.wal
simulation: seed 1 accounts 200 markets 1 events 200000
bootstrap events      270
generated events      200000 (accepted 183111, rejected 16889)
rejections            fok_not_fillable=7531 invalid_price=364 margin_breach=6295 no_liquidity=1395 unknown_account=423 unknown_order=388 would_cross=125 zero_qty=368
orders rested         49843
fills                 65247
self-trade cancels    64
funding settlements   68
liquidation steps     622
auto-deleveraging     0
insurance fund        45707.883001
fee pool              58647.470215
bad debt              0.000000
checkpoint     50000 events: resting orders     22, accounts with positions  196, rss 2464 KiB
checkpoint    100000 events: resting orders     37, accounts with positions  197, rss 2464 KiB
checkpoint    150000 events: resting orders     26, accounts with positions  192, rss 2464 KiB
checkpoint    200000 events: resting orders     29, accounts with positions  186, rss 2464 KiB
state hash            c1c3400fa37f1e084b79a1e72d128638d79bb65db5614d2d9281cab12af092c7

records               199091
file length           17914553 bytes
chain head            a77724d75bb4e3252accdf402b7e5abc0cef016fe52dda2b610155dd338dcffc
torn tail             0 bytes
damaged tail          no
state hash            c1c3400fa37f1e084b79a1e72d128638d79bb65db5614d2d9281cab12af092c7
OK: chain valid, replay deterministic, invariants hold
```

The state hash printed by `simulate` equals the one `verify` recomputes from the
file by replay. 16 889 of the 200 000 generated events (8.4%) are rejected: the
generator deliberately emits about 0.8% invalid events (zero quantity, off-tick
prices, unknown accounts and orders), and the rest are margin, liquidity and
fill-or-kill outcomes. The log holds 199 091 records, not 200 270: 1 179 events
(unknown account, unknown order, zero quantity) are refused by `Engine::precheck`
before they are logged. Margin rejections are frequent (6 295) because the margin
test now reserves the execution-price loss of each order (ADR 10).

In this flow the insurance fund started at 50 000 and ended at 45 707.88 after
paying 59 realised shortfalls of liquidated whales (about 5 195 in total) and
receiving liquidation fees; bad debt is zero and auto-deleveraging does not
occur. The liquidation tests build the cases that exhaust the fund.

## Money conservation, by test

The engine keeps one exact identity after **every** event (checked by
`Engine::check_invariants`, which runs in the property tests, after snapshot
decode and in `verify`):

```
sum(collateral) + insurance + fee_pool - sum(cost basis) == net_deposits + bad_debt
```

`net_deposits` is deposits minus withdrawals plus insurance funding. When all
positions are flat the cost basis is zero, so collateral plus the two pools equals
the money that came in. Positions carry a *money* cost basis, so each trade moves
`collateral - basis` by exactly zero over its two counterparties (ADR 3).
`bad_debt` is zero unless a loss was uncovered; then it is reported, not hidden.

| Property | Where it is checked |
|----------|---------------------|
| Money identity holds after random flow (300k+ events, three seeds, checked every 500 events) | `properties.rs::invariants_hold_over_300k_events_across_seeds`, `money_in_equals_money_out_plus_fees_and_insurance` |
| Money identity after scenarios: fees, rebates, flips through zero, withdrawals | `accounts_margin.rs` (`assert_conserved`), `order_matching.rs` |
| Money identity through liquidation, insurance payout and ADL (`bad_debt` stays 0 in every scenario the public API can build; a non-zero `bad_debt` is accepted by snapshot decode and covered there) | `liquidation.rs` (`identity`), `liquidation_fairness.rs` |
| Funding is zero-sum, remainder carried explicitly | `oracle_funding.rs::funding_is_zero_sum_with_several_accounts_and_odd_amounts`, `engine::funding` unit tests |
| Order book never crossed | `properties.rs` (every event), `order_matching.rs::book_is_never_crossed_under_a_deterministic_mixed_sequence` |
| Position sizes sum to zero per market | `properties.rs::position_sizes_sum_to_zero_in_every_market`, enforced on decode |
| No underwater account without liquidation having run | `properties.rs` (`underwater_accounts` vs `LiquidationStarted`), `liquidation.rs` |
| No fill executes outside the price band of the mark in force (non-liquidation fills) | `price_band.rs` (seeded flow), `review_regressions.rs` (colluding-accounts attack at 10x, 3x, 2x, 1.2x, 1.05x and the mirror cases) |
| The insurance fund never drops while accounts trade off-mark (4 seeded fuzz runs of 20 000 events) and never pays unrealised losses: after an `InsurancePaid` the collateral is exactly 0 | `review_regressions.rs`, `liquidation.rs`, `liquidation_fairness.rs` |
| Replay of the log gives the identical state hash | `properties.rs::replay_of_the_log_reproduces_the_state_hash`, `perp-infra/tests/wal_recovery.rs` |
| Snapshot + tail equals full replay | `properties.rs::snapshot_plus_tail_equals_full_replay`, `wal_recovery.rs::snapshot_recovery_equals_full_replay` |
| Rejected events change nothing but the counter | `properties.rs::rejected_events_change_only_the_counter` |
| Torn-write recovery at every byte offset of a 30-event log | `wal_recovery.rs::recovery_after_a_crash_at_every_byte_offset_of_the_log` (plus `perp-infra` unit tests on a 40-record fixture) |
| Hostile inputs (overflow, zero sizes, cancel storms, extreme timestamps, 200k garbage events) | `adversarial.rs` |
| Memory plateaus | `perp-cli/tests/memory.rs` |

## Tests by layer

834 tests, all deterministic, no network beyond loopback, all in temp dirs (counts from
`cargo test --workspace` in a fresh container; `scripts/run-layers.sh` runs them per layer).

| Layer | Kind | Tests |
|-------|------|-------|
| Domain (`perp-domain`) | unit tests of values, book, margin, codec, hashing, errors | 76 |
| | aggregate scenarios: matching (82), accounts and margin (42), oracle and funding (38), liquidation and insurance (45), liquidation fairness (16), price band (45), precheck (28) | 296 |
| | state codec and hash (35), adversarial and fuzz (43), property tests (7), review regressions (28), smoke (1) | 114 |
| Application (`perp-app`) | use cases on in-memory fakes (service, recovery, read-only mode, clock steps, simulation) and the port contract suites against the fakes | 69 |
| Adapters (`perp-infra`) | unit tests (JSON, protocol, token, WAL, snapshot, clock, probe) 168, TCP server 49, WAL recovery 26 | 243 |
| CLI (`perp-cli`) | argument and rendering units (10), architecture (6), end-to-end on the real binary (13), WAL repair (5), memory (2) | 36 |

`review_regressions.rs` holds the tests written first for the findings of an
independent security review (see [SECURITY.md](SECURITY.md)); 22 of its 28 tests
failed on the engine as it was before the fixes, the other 6 are controls.

Domain tests exercise the aggregate through its public API: every rule has a
passing and a rejecting case with an exact `Error` asserted, and every domain
event (`Output`) is checked to be emitted exactly when expected. Application
tests use only in-memory fakes. The same *contract suite* (`perp_app::contract`)
runs against the fakes and the real file adapters, so a fake cannot drift from
the real thing. End-to-end tests run the real binary: simulate, replay, verify,
corrupt and truncate logs, `serve` over TCP, kill the process and restart.

`scripts/run-layers.sh [domain|application|adapters|e2e]` runs one layer at a time.

### Coverage

Measured with rustc's source-based instrumentation (`-C instrument-coverage`) and
`llvm-profdata`/`llvm-cov` from the `llvm-tools` rustup component; no third-party
crate. `scripts/coverage.sh` runs the whole workspace instrumented and prints line
coverage per layer. Inline `#[cfg(test)]` modules are excluded from the numbers
(everything from the first `#[cfg(test)]` line to the end of a file), and `tests/`
and `benches/` are never counted.

```text
layer               lines  covered  percent
domain               3866     3791   98.06%
application           707      693   98.02%
infrastructure       2472     2350   95.06%
cli                   524      488   93.13%
all                  7569     7322   96.74%
```

Targets were 95% (domain), 90% (application), 85% (overall). What is **not**
covered, honestly (run `scripts/coverage.sh --uncovered` for line numbers):

* `Display`/`Debug` formatting of a few error types and the redaction of the
  `Auth` request in `Debug`.
* Defensive branches that return `Error::Internal` or halt the engine
  (`Halt::Funding`, `Halt::Adl`, "fee pool went negative"): they guard
  invariants that the other code establishes, so no input reaches them.
* WAL "poisoned after a failed fsync" and mid-write rollback paths: forcing a
  write or fsync failure portably needs fault injection, which this repository
  does not have.
* Server paths for a panicking backend thread and a few socket-error branches.
* CLI branches for failed writes to stdout/stderr.

Line coverage says the code ran, not that the assertions were strong; the
invariant and property tests carry the weight.

## Benchmarks

`cargo bench --bench throughput` (std only, `Instant`; no criterion). It records a
seeded event stream once (400 270 events: 200 accounts, one market, 342 035 order
placements) and replays it for each measurement, so every variant does identical
work. Latency is the time around one `Engine::apply` call (the `Instant` pair
itself costs tens of nanoseconds). Every recorded event, including the
deliberately invalid ones, goes straight to `Engine::apply`; `Engine::precheck` and
the service layer are not part of the "engine only" rows.

Host: a Linux aarch64 container (Docker on an Apple-silicon Mac, 12 vCPUs, 24 GB
RAM visible), release build with `overflow-checks = true`, single thread, machine
mostly idle (load average 8 at the start of the runs; an earlier set of runs on a
loaded machine was 3x slower and much noisier, which is why numbers from a busy
host are not reported here). Three consecutive runs:

| Measurement | Run 1 | Run 2 | Run 3 |
|-------------|-------|-------|-------|
| engine only, events/s | 974 722 | 941 891 | 951 706 |
| engine only, order placements/s | 832 910 | 804 856 | 813 243 |
| `apply` latency, order placements p50 / p99 / p99.9 | 250 ns / 3.1 us / 6.8 us | 250 ns / 3.2 us / 7.2 us | 250 ns / 3.3 us / 7.7 us |
| `apply` latency, other events p50 / p99 / p99.9 | 167 ns / 18.9 us / 49 us | 167 ns / 19.6 us / 49 us | 167 ns / 21.4 us / 51.5 us |
| engine + file WAL, fsync never, events/s | 511 119 | 521 397 | 481 871 |
| engine + file WAL, fsync every 1000, events/s | 322 606 | 377 017 | 362 990 |
| engine + file WAL, fsync always, events/s (3000 events) | 1 355 | 1 863 | 2 239 |

Oracle and tick events are the slow "other events" (they scan all holders and
settle funding). Max latencies (48 us to 447 us) are scheduler noise on a shared VM.
Caveats: synthetic flow with shallow books, one machine, a Linux VM on macOS (so
`fsync` timing reflects the VM's virtual disk, not hardware), no warm-up beyond the
recorded pass. Treat these as order-of-magnitude figures, not a claim of
production capacity.

## Bounded memory

All growth is capped by configuration: accounts, orders per account, orders and
price levels per book, fills per incoming order, record and snapshot sizes,
connections, line length. `crates/perp-cli/tests/memory.rs` samples `VmRSS` from
`/proc/self/status` (no allocator hooks: a counting allocator needs `unsafe`,
which every crate forbids; ADR 9). Resident memory includes allocator slack, so
the assertion allows a band of 8 MiB or 10%; the observed movement is far smaller.
Release build, container, one run with 3 000 000 events (`PERP_MEMORY_EVENTS=3000000`):

```text
after    300000 events: rss    2592 KiB, resting orders 40
after    600000 events: rss    2720 KiB, resting orders 25
after    900000 events: rss    2720 KiB, resting orders 22
...                                (2720 KiB through 2 700 000)
after   3000000 events: rss    2720 KiB, resting orders 25
```

Because that flow keeps the books shallow, a second test fills every book to its
limit (200 accounts x 64 orders = 12 800 resting orders that never trade), then
cancels and re-places them for 14 rounds:

```text
round  0: resting orders  12800, rss    5280 KiB
round  1: resting orders  12800, rss    5408 KiB
round  2..13: resting orders  12800, rss    5408 KiB
```

Memory grows while the books fill and then stays flat.

## What Rust does and does not give you here

**Gives:** no use-after-free, buffer overflow or data race in safe code (all crates
forbid `unsafe`); integer overflow is a panic, not silent wrap-around
(`overflow-checks = true` even in release), and the engine uses explicit checked
arithmetic on every path that sees external values; exhaustive `match` on events
and errors; deterministic collections (`BTreeMap`) so iteration order is a
function of keys.

**Does not give:** correctness of the margin, matching or funding rules (a logic
bug is still a bug; the invariants and tests are what protect them); freedom from
deadlocks or livelocks; bounded memory (that comes from explicit limits and is
measured, not guaranteed by the language); protection against a malicious or
buggy sequencer, oracle or operator; timing side channels beyond the constant-time
token comparison; a proof of anything. A panic in the engine thread would still
stop the server, which is why hostile input is turned into `Result` errors first.

## The TCP mode

`perp-engine serve --dir DIR --init` runs the durable service behind a
JSON-lines-over-TCP server (`std::net` and threads, no async runtime). The wire
format is documented in `crates/perp-infra/src/protocol.rs`: one JSON object per
line, integers in raw units (price 1e-6, quantity 1e-4, money 1e-6), first message
`{"op":"auth","token":"..."}`.

* **Token.** From the environment variable `PERP_ENGINE_TOKEN` (or `--token-env NAME`)
  or a file (`--token-file`, refused if group or others have any permission bits). It is at least 32 characters, only
  its SHA-256 digest is kept in memory, it is compared in constant time and it is
  never printed, logged or returned in an error. `perp-engine gen-token` prints a
  random token once, and only to an interactive terminal; into a pipe or CI log
  it refuses. `gen-token --out FILE` writes a new 0600 file and prints only the path.
* **Loopback by default.** A non-loopback `--listen` address is refused unless
  `--allow-non-loopback` is given (then a warning is printed).
* **Limits.** Line length (4 KiB), connection count (32), per-connection token
  bucket (200/s, burst 400) with disconnect on sustained abuse, auth deadline (5 s),
  idle timeout (60 s), three auth failures with a delay each, write timeout. One
  request in flight per connection and a bounded queue to a single backend thread
  give backpressure all the way to the client's TCP window. Connection slots are
  reserved with an atomic compare-and-swap, so connection churn cannot leak slots.
* **Log full is not fatal.** When the log reaches `--max-log-mib` (default 1024) the
  service becomes read-only: queries still work and state-changing requests get
  `{"ok":false,"error":"read_only"}`. Free space or raise the limit and restart.
  Requests that `Engine::precheck` can refuse from a look-up alone (unknown
  account, order or market, zero amount, ...) are rejected before they are logged,
  so junk requests do not grow the log. Any other storage error stops the server.

Threat model for this mode (also in [SECURITY.md](SECURITY.md)):

| Threat | Handled | Not handled |
|--------|---------|-------------|
| Unauthenticated client | token required before any other request; failures delayed and limited | no per-IP limits |
| Oversized or malformed input | bounded line reads, strict bounded JSON, errors without echoing input | |
| Flooding and slow clients | rate limit, connection cap (atomic slot reservation), timeouts, bounded queue; junk requests are not logged | many token-holding clients can still saturate the single engine thread; valid-looking requests that fail later (for example a margin rejection) are logged and consume log space until `--max-log-mib` is reached, after which the service is read-only |
| Eavesdropping and tampering | none | no TLS: the token and all traffic are plaintext on the wire |
| A token holder acting maliciously | none | one shared token with full rights: any account, any oracle price |
| Token theft from the host | file mode 0600, environment not logged | anyone who can read the process environment or memory |

## Limitations

* Educational model; **not audited**; never use with real funds.
* **Single-threaded engine.** One aggregate applies events one at a time. There is
  no sharding, replication or failover, and no network consensus.
* **No oracle security.** Prices are whatever the sequencer or a token holder sends;
  the staleness check and the bounded change filter limit blast radius, not trust.
* **Simplified liquidation and ADL.** Fixed chunk size and band; ADL ranking is only
  profit ratio, size and account id. The liquidation queue is served round-robin,
  so an account can wait up to `ceil(pending / max_liquidations_per_event)` events
  behind accounts that cannot be filled; a stuck account (nobody quotes the other
  side) waits for liquidity or the next oracle/tick. Positions in a market whose
  oracle is stale are ignored when judging the rest of the account, and nothing is
  paid from the fund while one remains. Orders only reduce risk through a
  worst-case exposure rule; the reduce-only quantity is clamped, not rejected.
* **Price band and gap risk.** No fill (other than liquidation/ADL) executes
  further from the mark than `price_band_bps` (default 2.5%), but one oracle update
  may move the mark by up to 5% (the default change filter), more than an account's
  margin. That gap loss is covered by the insurance fund and then by ADL; it can
  exhaust them. After a large move, resting orders now more aggressive than the
  band are cancelled, which also removes liquidity that liquidations would need.
  A malicious oracle source is out of scope.
* **Log repair.** An incomplete tail is repaired automatically after its bytes are
  saved to a `.torn-<offset>` sidecar. A complete-but-damaged final record is not
  removed without `serve --repair`. A damaged length field in the last record
  looks like a torn write and is repaired (with the sidecar). The log is not
  authenticated: whoever can write the file can rebuild the hash chain.
* **Time.** The service never advances event time by more than one hour
  (`max_time_step_ms`) per event; after a longer downtime it catches up one step per
  event, and oracle updates stamped with the real time are rejected as coming from
  the future until it has.
* **Bad debt can exist.** If the insurance fund and ADL cannot cover a loss, the
  amount is recorded in `bad_debt` and the identity still holds, but the venue is
  then insolvent by that amount.
* Funding intervals that pass without an `Oracle` or `Tick` event are not back-filled.
* Timestamps come from the sequencer and must not go backwards; a hostile timestamp
  can stop funding for a market (the boundary saturates) but not halt the engine.
* Single log file, no rotation; the log is not cryptographically bound to snapshots;
  one writer, no lock file.
* The TCP mode is plaintext and uses one shared token.
* Benchmarks and memory numbers come from one machine and a synthetic flow.
* Windows is not supported (the token helpers use Unix file modes and `/dev/urandom`).

## Documentation

* [docs/domain.md](docs/domain.md): ubiquitous language and context map.
* [docs/adr](docs/adr): short decision records.
* [SECURITY.md](SECURITY.md): reporting, threat model, dependency policy.

## Secrets and hooks

Never commit `.env` files or credentials. Keep real values in a local, ignored `.env` and commit only a `.env.example` with placeholders; `.gitignore` already blocks common secret-like files.

Install [lefthook](https://github.com/evilmartians/lefthook) and run `lefthook install` once per clone. This enables two hooks:

- `pre-commit` runs `sh scripts/secret-scan.sh --staged` on what you are about to commit.
- `pre-push` runs `sh scripts/secret-scan.sh --tree` on the whole working tree.

CI additionally scans the full history. The scanner is a dependency-free shell script that looks for forbidden file names and key-shaped values, and it never prints the matched value. A reviewed false positive can be allowed in `.secret-scan-allow` with one line per file and rule (`<path> <rule>   # reason`), or on a single line with an inline `secret-scan: allow` marker.

If a secret is ever committed, rotate it first (treat it as compromised), then remove it from history.

## Releasing

Releases are cut by pushing a `vMAJOR.MINOR.PATCH` tag. The `release` workflow then verifies the code, builds the files, writes `SHA256SUMS`, attests where they were built and creates the GitHub release. See [RELEASING.md](RELEASING.md) for how to cut a release and how to verify a download with `gh attestation verify`.

## License

MIT, see [LICENSE](LICENSE).
