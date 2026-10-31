# 6. Liquidation, insurance fund, auto-deleveraging and bad debt

Status: accepted

## Decision
Solvent accounts are liquidated in chunks through the book with a price bounded
by the bankruptcy price. Insolvent accounts are sold into the book only if the
insurance fund can cover deficit plus worst-case slippage; otherwise the fund pays
what it has and the position is closed against opposite positions at the
bankruptcy price. Anything still uncovered is recorded as `bad_debt`.

## Consequences
In simplified form this mirrors what perpetual venues must handle. It is not a
claim about any real venue. ADL ranking is deliberately simple. `bad_debt` makes
insolvency visible instead of hiding it in a negative balance.

## Update (security review findings MONEY-002, DOS-002, DOS-003)
* The fund pays only the realised shortfall of a liquidation chunk (ADR 11).
* The liquidation queue is served **round-robin** from a persistent cursor
  instead of lowest account id first. Accounts that cannot be filled (nobody
  quotes the other side) still use budget, but every pending account is reached
  within `ceil(pending / max_liquidations_per_event)` events, so low-id decoy
  accounts cannot starve a high-id insolvent account.
* Maintenance is assessed on the markets with a **fresh** mark only (equity and
  requirement both): a dead oracle on one market no longer shields large losses on
  a live market, and nothing is paid from the fund while a position in a stale
  market remains.
