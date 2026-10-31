# 7. Exact funding with an explicit carry

Status: accepted

## Decision
The integer total of a funding payment is split among payers and, separately,
among receivers with the largest-remainder method; the fraction of a unit that
remains is stored per market and added to the next interval.

## Consequences
`sum(paid) == sum(received)` exactly; funding never changes total collateral.
