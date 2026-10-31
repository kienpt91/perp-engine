# 3. Positions carry a money cost basis, not an entry price

Status: accepted

## Context
With integer arithmetic, `size * (exit - entry)` rounds on every close, and the
two counterparties of a trade would round differently, so money would leak.

## Decision
A position stores the signed money paid or received for its open size. Each
fill's notional is computed once and used for both sides. Closing part of a
position releases a proportional part of the basis (rounded against the closer).

## Consequences
For every trade the change of `collateral - basis` over both sides is exactly
zero, which gives the money identity checked after every event. Unrealised PnL
is `floor(size * mark) - basis`. Entry price is derived for display only.
