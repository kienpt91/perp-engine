#!/bin/sh
# Prints the version of the package in the current directory: package.json (npm) or Cargo.toml (Rust workspace
# or single crate). No tools beyond sed and awk, so it runs the same in the CI runner and in a bare container.
# usage: manifest-version.sh
set -eu
if [ -f package.json ]; then
  # npm writes the top-level keys with two spaces of indent; the first "version" at that depth is the package's
  version=$(sed -n 's/^  "version": "\([^"]*\)".*/\1/p' package.json | sed -n 1p)
  manifest=package.json
elif [ -f Cargo.toml ]; then
  version=$(awk '/^\[/ { s = ($0 == "[workspace.package]" || $0 == "[package]") } s && /^version[ \t]*=/ { gsub(/^[^"]*"|".*$/, ""); print; exit }' Cargo.toml)
  manifest=Cargo.toml
else
  echo "no package.json or Cargo.toml in $(pwd)" >&2
  exit 1
fi
if [ -z "$version" ]; then
  echo "no version found in $manifest" >&2
  exit 1
fi
printf '%s\n' "$version"
