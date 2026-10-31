# 4. Deterministic event-sourced aggregate with fail-stop

Status: accepted

## Context
Replay must reproduce the state bit for bit, including after rejected input.

## Decision
`Engine::apply(&Event)` is a pure function of state and event. Handlers validate
first and mutate second; a rejection changes only the applied-event counter, and
rejected events are still logged. An error after mutation began halts the engine
(the flag is part of the state hash). All collections are ordered maps.

## Consequences
The service can log first and apply second, and replay ignores business errors.
A halted engine refuses everything, so a bug cannot silently corrupt money. The
cost is an O(state) hash for checkpoints.
