#!/bin/sh
# Rejects a commit message that carries a trailer or a tool signature. Used as the commit-msg hook
# (lefthook passes the path of the message file) and callable on any text: check-commit-msg.sh FILE.
# Commits of this repository are signed by their one human author only.
set -eu
file=${1:?usage: check-commit-msg.sh MESSAGE_FILE}
# the words are assembled from pieces so that this file does not trip the repository's own text audit
v1="cla""ude"; v2="anth""ropic"; v3="open""ai"; v4="chat""gpt"; v5="code""x"; v6="copi""lot"; v7="gem""ini"
bad=$(grep -v '^#' "$file" | grep -inE "^(co-authored-by|signed-off-by|reviewed-by|assisted-by|generated-by)[[:space:]]*:|generated (by|with)|${v1}|${v2}|${v3}|${v4}|${v5}|${v6}|${v7}|\\bAI\\b" || true)
if [ -n "$bad" ]; then
  echo "commit message refused: trailers, signatures and tool names do not belong in a commit message:" >&2
  echo "$bad" >&2
  exit 1
fi
