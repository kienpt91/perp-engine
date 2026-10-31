#!/bin/sh
# Run the test suites layer by layer. Needs a Rust toolchain (see rust-toolchain.toml);
# use scripts/container.sh to run it in a throwaway container.
#
#   scripts/run-layers.sh            all layers
#   scripts/run-layers.sh domain     one layer: domain | application | adapters | e2e
set -eu
layer="${1:-all}"

run() {
  echo
  echo "=== $1 ==="
  shift
  "$@"
}

if [ "$layer" = all ] || [ "$layer" = domain ]; then
  # Pure unit tests, scenario tests of the aggregate, property tests, codec tests.
  run "domain layer (perp-domain)" cargo test --locked -p perp-domain
fi
if [ "$layer" = all ] || [ "$layer" = application ]; then
  # Use cases driven through in-memory fakes; port contract suites against the fakes.
  run "application layer (perp-app)" cargo test --locked -p perp-app
fi
if [ "$layer" = all ] || [ "$layer" = adapters ]; then
  # Adapter tests: the same port contract suites against the real file adapters,
  # plus WAL recovery, TCP server and protocol tests, in temp dirs / on loopback.
  run "infrastructure adapters (perp-infra)" cargo test --locked -p perp-infra
fi
if [ "$layer" = all ] || [ "$layer" = e2e ]; then
  # The real binary: simulate, replay, verify, serve over TCP, restart, memory plateau.
  run "end to end (perp-cli)" cargo test --locked -p perp-cli
fi
