# 2. Layers as workspace crates

Status: accepted

## Context
The model must stay free of I/O, clocks and randomness so it can be replayed,
and the dependency direction must not erode.

## Decision
Four crates: `perp-domain` (no dependencies), `perp-app` (depends on domain),
`perp-infra` (domain, app), `perp-cli` (all). The application declares ports; the
infrastructure and in-memory fakes implement them. The domain receives time in
events and generates ids from its own counters.

## Consequences
The compiler enforces the rule; `architecture.rs` checks manifests too. Tests
split by layer: scenario and property tests in the domain, use cases against
fakes in the application, shared contract suites against fake and real adapters,
end-to-end tests on the binary.
