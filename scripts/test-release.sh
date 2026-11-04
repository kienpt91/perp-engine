#!/bin/sh
# Shell tests of the release scripts (manifest-version.sh, check-release-tag.sh, release-notes.sh) against
# throwaway git repositories. Needs only git and a POSIX shell. usage: sh scripts/test-release.sh
set -eu
scripts=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
# nothing from the user's or the system's git configuration (hooks, signing, templates) applies to the probes
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=probe GIT_AUTHOR_EMAIL=probe@example.invalid GIT_COMMITTER_NAME=probe GIT_COMMITTER_EMAIL=probe@example.invalid

passed=0
failed=0
ok() { passed=$((passed + 1)); }
bad() { failed=$((failed + 1)); echo "FAIL: $1" >&2; }
expect_ok() { desc=$1; shift; if "$@" > "$work/out" 2>&1; then ok; else bad "$desc"; sed 's/^/  /' "$work/out" >&2; fi; }
expect_fail() { desc=$1; shift; if "$@" > "$work/out" 2>&1; then bad "$desc (accepted)"; else ok; fi; }
expect_in() { desc=$1; needle=$2; if grep -qF -- "$needle" "$work/out"; then ok; else bad "$desc (missing: $needle)"; fi; }
expect_not_in() { desc=$1; needle=$2; if grep -qF -- "$needle" "$work/out"; then bad "$desc (unexpected: $needle)"; else ok; fi; }

n=0
newrepo() { # a fresh repository on main; its directory is left in $r
  n=$((n + 1))
  r="$work/r$n"
  mkdir "$r"
  git -C "$r" init -q -b main
}
commit() { git -C "$1" add -A && git -C "$1" commit -q -m "$2"; }
publish_main() { git -C "$1" update-ref refs/remotes/origin/main refs/heads/main; }
check() { (cd "$1" && sh "$scripts/check-release-tag.sh" "$2"); }
notes() { (cd "$1" && sh "$scripts/release-notes.sh" "$2"); }
version() { (cd "$1" && sh "$scripts/manifest-version.sh"); }

# package.json: the version at the top level counts, a nested "version" does not
newrepo
printf '{\n  "name": "probe",\n  "version": "1.2.3",\n  "dependencies": {\n    "x": {\n      "version": "9.9.9"\n    }\n  }\n}\n' > "$r/package.json"
commit "$r" "Add the package"
publish_main "$r"
git -C "$r" tag -a -m annotated v1.2.3
git -C "$r" tag v1.2.4
expect_ok "reads the npm version" test "$(version "$r")" = "1.2.3"
expect_ok "accepts the (annotated) tag that names the version" check "$r" v1.2.3
expect_fail "refuses a tag that names another version" check "$r" v1.2.4
expect_fail "refuses a tag without the v prefix" check "$r" 1.2.3
expect_fail "refuses a tag that is not a version" check "$r" vnext
expect_fail "refuses a tag that does not exist" check "$r" v1.2.3-missing

# the tag has to be in the history of the default branch
git -C "$r" checkout -q -b side
printf '{\n  "name": "probe",\n  "version": "1.2.3-side"\n}\n' > "$r/package.json"
commit "$r" "Name the side version"
git -C "$r" tag v1.2.3-side
expect_fail "refuses a tag that is not in the default branch" check "$r" v1.2.3-side
expect_fail "refuses when the default branch ref is missing" env DEFAULT_BRANCH=trunk sh -c "cd '$r' && sh '$scripts/check-release-tag.sh' v1.2.3-side"
expect_ok "compares with the ref of the environment" env RELEASE_BASE_REF=refs/heads/side sh -c "cd '$r' && sh '$scripts/check-release-tag.sh' v1.2.3-side"
git -C "$r" update-ref refs/remotes/origin/main refs/heads/side
expect_ok "accepts it once the default branch contains the tag" check "$r" v1.2.3-side

# Cargo.toml: workspace version and single-crate version; dependency versions do not count
newrepo
printf '[workspace]\nmembers = ["a"]\n\n[workspace.dependencies]\nx = { version = "7.7.7" }\n\n[workspace.package]\nversion = "0.4.0"\nedition = "2021"\n\n[dependencies.y]\nversion = "8.8.8"\n' > "$r/Cargo.toml"
commit "$r" "Add the workspace"
publish_main "$r"
git -C "$r" tag v0.4.0
git -C "$r" tag v7.7.7
git -C "$r" tag v8.8.8
expect_ok "reads the workspace version" test "$(version "$r")" = "0.4.0"
expect_ok "accepts the tag of the workspace version" check "$r" v0.4.0
expect_fail "ignores the version of a workspace dependency" check "$r" v7.7.7
expect_fail "ignores the version of a dependency table" check "$r" v8.8.8

newrepo
printf '[package]\nname = "one"\nversion = "2.0.1"\n\n[dependencies]\nz = { version = "3.0.0" }\n' > "$r/Cargo.toml"
commit "$r" "Add the crate"
publish_main "$r"
git -C "$r" tag v2.0.1
expect_ok "reads the single crate version" test "$(version "$r")" = "2.0.1"
expect_ok "accepts the tag of the single crate" check "$r" v2.0.1

# no manifest, no version
newrepo
printf 'x\n' > "$r/file"
commit "$r" "Add a file"
publish_main "$r"
git -C "$r" tag v1.0.0
expect_fail "refuses without a manifest" check "$r" v1.0.0
newrepo
printf '{\n  "name": "probe"\n}\n' > "$r/package.json"
commit "$r" "Add a package without a version"
publish_main "$r"
git -C "$r" tag v1.0.0
expect_fail "refuses a manifest without a version" check "$r" v1.0.0

# release notes: subjects since the previous tag, no merge commits, nothing from before it
newrepo
printf '1\n' > "$r/f"
commit "$r" "First change"
printf '2\n' > "$r/f"
commit "$r" "Second change"
git -C "$r" tag v0.1.0
printf '3\n' > "$r/f"
commit "$r" "Third change"
git -C "$r" checkout -q -b feature
printf 'g\n' > "$r/g"
commit "$r" "Feature change"
git -C "$r" checkout -q main
printf '4\n' > "$r/f"
commit "$r" "Fourth change"
git -C "$r" merge -q --no-ff -m "Merge feature" feature
git -C "$r" tag -a -m "second release" v0.2.0
notes "$r" v0.1.0 > "$work/out"
expect_in "first release names itself" "Changes in v0.1.0"
expect_in "first release lists its commits" "- First change"
expect_in "first release lists every commit" "- Second change"
expect_not_in "first release has nothing later" "- Third change"
notes "$r" v0.2.0 > "$work/out"
expect_in "later release names the previous tag" "Changes since v0.1.0"
expect_in "later release lists its commits" "- Third change"
expect_in "later release lists a merged commit" "- Feature change"
expect_in "later release lists the last commit" "- Fourth change"
expect_not_in "later release leaves out merges" "Merge feature"
expect_not_in "later release leaves out earlier commits" "- Second change"
expect_fail "release notes for a missing tag fail" notes "$r" v9.9.9

if [ "$failed" -ne 0 ]; then
  echo "$failed of $((passed + failed)) release script checks failed" >&2
  exit 1
fi
echo "release scripts: $passed checks passed"
