#!/bin/sh
# Prints plain-text release notes for a tag: the subjects of the commits since the previous v* tag
# (every commit up to the tag when there is no earlier tag). Merge commits are left out.
# usage: release-notes.sh TAG
set -eu
tag=${1:?usage: release-notes.sh TAG}
ref="refs/tags/$tag"
git rev-parse --verify --quiet "$ref^{commit}" > /dev/null || { echo "no such tag: $tag" >&2; exit 1; }
prev=$(git describe --tags --abbrev=0 --match 'v[0-9]*' "$ref^" 2> /dev/null || true)
if [ -n "$prev" ]; then
  echo "Changes since $prev"
  range="$prev..$ref"
else
  echo "Changes in $tag"
  range=$ref
fi
echo
git log --no-merges --format='- %s' "$range"
