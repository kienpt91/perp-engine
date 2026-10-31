#!/bin/sh
# Source-based coverage with rustc's instrumentation and the llvm-tools rustup
# component (no third-party crate). Prints line coverage per layer.
#
# Test-only code is excluded: files under tests/ and benches/ are never listed,
# and in every source file everything from the first `#[cfg(test)]` line to the
# end of the file (the inline unit-test module) is dropped from the numbers.
#
#   scripts/coverage.sh                 run all suites instrumented, print the table
#   scripts/coverage.sh --uncovered     also list uncovered line ranges per file
set -eu

target="${CARGO_TARGET_DIR:-target}/coverage"
profraw="$target/profraw"
rm -rf "$target"
mkdir -p "$profraw"

export RUSTFLAGS="-C instrument-coverage"
export LLVM_PROFILE_FILE="$PWD/$profraw/%p-%m.profraw"
export CARGO_TARGET_DIR="$target/build"

cargo test --locked --workspace -- --test-threads=4 >"$target/test.log" 2>&1 || {
  tail -40 "$target/test.log"
  echo "tests failed under instrumentation" >&2
  exit 1
}
grep -E "^test result" "$target/test.log" | awk '{p+=$4; f+=$6} END {print "tests passed:", p, "failed:", f}'

sysroot=$(rustc --print sysroot)
host=$(rustc -vV | sed -n 's/^host: //p')
bin="$sysroot/lib/rustlib/$host/bin"
"$bin/llvm-profdata" merge -sparse "$profraw"/*.profraw -o "$target/merged.profdata"

# every instrumented test binary and the real executable
objects=""
for f in "$target"/build/debug/deps/*; do
  [ -f "$f" ] && [ -x "$f" ] || continue
  case "$f" in *.so|*.rlib|*.d|*.rmeta) continue ;; esac
  objects="$objects --object $f"
done

# shellcheck disable=SC2086
"$bin/llvm-cov" show --use-color=0 --show-instantiations=false \
  --instr-profile="$target/merged.profdata" $objects \
  --ignore-filename-regex='(/\.cargo/|/rustc/|/tests/|/benches/|/\.rustup/)' \
  >"$target/show.txt"

awk -v uncovered="${1:-}" '
function flush() {
  if (file == "") return
  layer = "other"
  if (file ~ /crates\/perp-domain\/src\//) layer = "domain"
  else if (file ~ /crates\/perp-app\/src\//) layer = "application"
  else if (file ~ /crates\/perp-infra\/src\//) layer = "infrastructure"
  else if (file ~ /crates\/perp-cli\/src\//) layer = "cli"
  tot[layer] += ftot; cov[layer] += fcov
  ftot_all[file] = ftot; fcov_all[file] = fcov
  if (uncovered == "--uncovered" && miss != "") print "  uncovered in " file ": " miss
  file = ""
}
/^\/.*:$/ { flush(); file = substr($0, 1, length($0) - 1); sub(/.*\/work\//, "", file); ftot = 0; fcov = 0; skip = 0; miss = ""; next }
{
  n = index($0, "|"); if (n == 0) next
  rest = substr($0, n + 1); m = index(rest, "|"); if (m == 0) next
  line = substr($0, 1, n - 1) + 0; count = substr(rest, 1, m - 1); code = substr(rest, m + 1)
  if (code ~ /#\[cfg\(test\)\]/) skip = 1
  if (skip) next
  gsub(/ /, "", count)
  if (count == "") next
  ftot++
  if (count != "0") fcov++
  else miss = miss (miss == "" ? "" : ",") line
}
END {
  flush()
  printf "%-16s %8s %8s %8s\n", "layer", "lines", "covered", "percent"
  order[1] = "domain"; order[2] = "application"; order[3] = "infrastructure"; order[4] = "cli"
  for (i = 1; i <= 4; i++) {
    l = order[i]; if (tot[l] == 0) continue
    printf "%-16s %8d %8d %7.2f%%\n", l, tot[l], cov[l], 100 * cov[l] / tot[l]
    T += tot[l]; C += cov[l]
  }
  printf "%-16s %8d %8d %7.2f%%\n", "all", T, C, 100 * C / T
}' "$target/show.txt"
