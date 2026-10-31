# 8. TCP server: threads, one token, bounded everything

Status: accepted

## Decision
`std::net` with one thread per connection (bounded) and one backend thread that
owns the application service. One shared token authenticates (compared in
constant time against a stored digest). Line size, connection count and request
rate are limited; a bounded queue gives backpressure. Loopback by default.

## Consequences
Simple and reviewable. There is no TLS and no per-user authorisation, so it must
not face an untrusted network (see SECURITY.md).
