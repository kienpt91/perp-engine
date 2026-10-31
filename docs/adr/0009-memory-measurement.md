# 9. Memory is measured with VmRSS, not an allocator hook

Status: accepted

## Decision
Tests sample `VmRSS` from `/proc/self/status` through a `ResourceProbe` port. A
counting global allocator would need `unsafe`, which every crate forbids.

## Consequences
Resident memory includes allocator slack, so the check allows a small band. It
runs on Linux only.
