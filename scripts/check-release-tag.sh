#!/bin/sh
# Refuses a release unless the tag names the version in the manifest (package.json or Cargo.toml) and the tagged
# commit is in the history of the default branch. Run it in a checkout that has every branch and the tag.
# usage: check-release-tag.sh TAG
#   DEFAULT_BRANCH    name of the default branch (default: main)
#   RELEASE_BASE_REF  full ref to compare against (default: refs/remotes/origin/$DEFAULT_BRANCH)
set -eu
tag=${1:?usage: check-release-tag.sh TAG}
branch=${DEFAULT_BRANCH:-main}
base=${RELEASE_BASE_REF:-refs/remotes/origin/$branch}
here=$(dirname "$0")

refuse() {
  echo "release refused: $*" >&2
  exit 1
}

case $tag in
  v[0-9]*) ;;
  *) refuse "tag '$tag' does not look like vMAJOR.MINOR.PATCH" ;;
esac

version=$(sh "$here/manifest-version.sh") || refuse "cannot read the version of the manifest"
[ "$tag" = "v$version" ] || refuse "tag '$tag' does not match the manifest version '$version' (expected v$version)"

commit=$(git rev-parse --verify --quiet "refs/tags/$tag^{commit}") || refuse "tag '$tag' does not exist in this checkout"
git rev-parse --verify --quiet "$base^{commit}" > /dev/null || refuse "default branch ref '$base' is not in this checkout (fetch full history)"
git merge-base --is-ancestor "$commit" "$base" || refuse "tag '$tag' is not in the history of the default branch '$branch'"

echo "release ok: $tag is v$version and is in the history of $branch"
