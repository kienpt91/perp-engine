#!/bin/sh
# Run a command in a throwaway Rust container with the repository mounted.
# The container runs as your user, with all capabilities dropped; the cargo
# registry and build output live in docker volumes, not in your home directory.
#
#   scripts/container.sh scripts/run-layers.sh
#   scripts/container.sh scripts/coverage.sh
#   scripts/container.sh cargo test --locked
#
# The first run downloads the toolchain pinned in rust-toolchain.toml (network
# needed); set PERP_NET=none afterwards to run fully offline.
set -eu
image="${PERP_IMAGE:-rust:1.95}"
repo=$(cd "$(dirname "$0")/.." && pwd)
docker volume create perp-engine-cargo >/dev/null
docker volume create perp-engine-target >/dev/null
docker run --rm -u 0 -v perp-engine-cargo:/c -v perp-engine-target:/t "$image" \
  chown "$(id -u):$(id -g)" /c /t >/dev/null
exec docker run --rm \
  --user "$(id -u):$(id -g)" --cap-drop ALL --security-opt no-new-privileges \
  --network "${PERP_NET:-bridge}" \
  -e HOME=/tmp/home -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=never \
  -v "$repo:/work" -v perp-engine-cargo:/cargo -v perp-engine-target:/target -w /work \
  "$image" "$@"
