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
