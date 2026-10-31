# 11. The insurance fund pays only realised shortfall

Status: accepted (security review finding MONEY-002)

## Context
When an insolvent account was liquidated, the engine sold one chunk (25%) of its
position and then credited the **whole** remaining deficit, including the
unrealised loss on the 75% still open, from the insurance fund. The account was
left at equity zero with an open leveraged position: if the price recovered it
kept the upside, and if it fell further the fund paid again. The fund was
underwriting a free option.

## Decision
* After a liquidation chunk the fund pays exactly the **realised** shortfall:
  the negative collateral that the chunk's fills created. It is paid when the
  account is flat, or still insolvent with no position in a stale-oracle market.
  Unrealised losses of positions that remain open are never credited; the account
  stays insolvent and is liquidated again.
* When the fund cannot cover the deficit of the position (plus worst-case
  slippage), the auto-deleveraging path pays what the fund has and closes the
  **whole** position in that market against opposite positions at the bankruptcy
  price, so no zombie position survives. The deficit used there belongs to that
  position (`-(collateral + unrealised PnL of that position)`), not to the whole
  cross-margin account.
* Anything nobody can cover is recorded as `bad_debt`.

## Consequences
Property tests assert that after an `InsurancePaid` the collateral of the account
is exactly zero unless a later fill in the same event involves it. With several
open markets the fund still pays market by market, in order of the lowest fresh
market, which can take more events than one global payout.
