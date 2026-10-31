# 12. Read-only mode, pre-validation and bounded time steps

Status: accepted (security review findings INPUT-001, DOS-003)

## Decision
* **Log full is not fatal.** When the event log reports that it is full, the
  service enters a degraded read-only mode: queries work, state-changing events
  are refused with `ReadOnly` (the TCP server answers `read_only`), nothing is
  half-applied. Recovery is operator action: free space or raise `--max-log-mib`
  and restart. Other storage failures stay fatal.
* **Cheap rejections are not logged.** `Engine::precheck` rejects what needs only
  a look-up (unknown account/market/order, zero amount, bad quantity, time
  regression, ...), so a flood of junk requests cannot grow the log. Rejections
  that depend on market state (margin, liquidity, bands) are still logged and
  replay identically.
* **Time moves at most one step per event.** The service stamps events with the
  clock but never more than `max_time_step_ms` (default one hour) beyond the
  previous event, so a single wild clock reading cannot freeze every oracle.
