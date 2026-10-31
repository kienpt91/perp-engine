# 10. Price band around the mark, execution-price loss in the margin test

Status: accepted (supersedes the "fills execute anywhere" behaviour found by the security review, finding MONEY-001)

## Context
Orders were validated against tick size and the global maximum price only, margin
was computed on the notional at the **mark**, and fills executed at the maker's
price. Two colluding accounts could therefore pass margin checks and trade at
a price far from the mark: a thinly funded buyer lifts a maker's ask at ten
times the mark, ends with a deeply negative equity, and the insurance fund pays
the deficit to the account holding the mirror-image profit. The money identity
still balanced, so no invariant test noticed.

## Decision
1. **Price band.** `MarketConfig::price_band_bps` (default 250, at most the first
   maintenance margin rate). A limit buy above `floor(mark * (1 + band))` or a
   limit sell below `ceil(mark * (1 - band))` is rejected with
   `PriceOutsideBand`. Passive prices (a buy far below the mark, a sell far
   above) stay allowed: they cannot hurt their owner.
2. **Market orders** measure their protection price from the **mark**, never
   from the best opposite quote, and never wider than the band.
3. **Execution-price loss in the margin test.** At order entry the engine
   dry-runs the matching loop and reserves the net loss of trading at the resting
   prices instead of the mark (plus the same loss on the part that would rest).
   The reserve is added to the fee reserve in the initial-margin test; for every
   order, including reduce-only, `equity - reserve` must stay non-negative.
4. **Sweep on mark change.** When an oracle update moves the mark, resting orders
   that have become more aggressive than the band (bids above, asks below) are
   cancelled in the same event with `CancelReason::PriceBand`, so no fill
   executes outside the band of the mark in force.
5. The band may not exceed the first maintenance margin rate, so a fill at the
   band edge cannot take an account that held initial margin below zero.

## Consequences
* Liquidity that sits more aggressive than the band after a large oracle move is
  cancelled, so after a gap move there is less liquidity for liquidations; the
  insurance fund and ADL are then used more often in gap scenarios. This is the
  price of making stale quotes unusable.
* Liquidation and ADL orders are exempt from the band (they have their own
  documented price rules).
* Gap risk remains: an oracle update may move the mark by more than the margin
  an account holds. That is market risk, not an exploit, and is what the
  insurance fund is for. A malicious oracle source is out of scope (README).
* Cost: one extra walk over at most 256 resting orders per order that crosses.
