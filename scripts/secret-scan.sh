#!/bin/sh
# Dependency-free secret scanner used by the git hooks and CI.
#   secret-scan.sh --staged    scan what is about to be committed (pre-commit)
#   secret-scan.sh --tree      scan every tracked file in the working tree (pre-push, CI)
#   secret-scan.sh --history   scan the added lines of every commit reachable from HEAD (CI)
# Exit status: 0 clean, 1 findings, 2 usage error. Findings never print the secret itself,
# only the file, the line number and the rule name.
# Allow a known-safe line with the marker: secret-scan: allow
set -eu

mode=${1:-}
case "$mode" in --staged|--tree|--history) ;; *) echo "usage: secret-scan.sh --staged|--tree|--history" >&2; exit 2 ;; esac

# rule name <TAB> extended regular expression
rules='private-key-block	-----BEGIN [A-Z ]*PRIVATE KEY-----
aws-access-key-id	(^|[^A-Za-z0-9])(AKIA|ASIA)[0-9A-Z]{16}([^A-Za-z0-9]|$)
github-token	(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}
github-fine-grained-token	github_pat_[A-Za-z0-9_]{30,}
stripe-key	[sr]k_(live|test)_[A-Za-z0-9]{16,}
stripe-webhook-secret	whsec_[A-Za-z0-9]{16,}
slack-token	xox[abprs]-[A-Za-z0-9-]{10,}
google-api-key	AIza[0-9A-Za-z_-]{35}
npm-token	npm_[A-Za-z0-9]{36}
sk-style-key	sk-[A-Za-z0-9_-]{32,}
jwt	eyJ[A-Za-z0-9_-]{15,}\.eyJ[A-Za-z0-9_-]{15,}\.[A-Za-z0-9_-]{10,}
url-with-password	[a-z][a-z0-9+.-]*://[^/[:space:]:@]+:[^/[:space:]@]{6,}@[^/[:space:]]+
quoted-secret-assignment	(api[_-]?key|secret|token|passwd|password|private[_-]?key)["'"'"']?[[:space:]]*[:=][[:space:]]*["'"'"'][A-Za-z0-9+/_=-]{24,}["'"'"']
raw-private-hex	(^|[^0-9A-Fa-f])0x[0-9a-fA-F]{64}([^0-9A-Fa-f]|$)'

# file names that must never be committed (matched against the basename)
bad_name() {
  case "$1" in
    .env|.env.*|*.env) case "$1" in .env.example|.env.sample|.env.template|*.env.example) return 1 ;; esac; return 0 ;;
    *.pem|*.key|*.p12|*.pfx|*.jks|*.keystore|id_rsa|id_rsa.*|id_ed25519|id_ed25519.*|id_ecdsa|id_ecdsa.*|.netrc|credentials.json|service-account*.json|*.tfstate|*.tfstate.*|secrets.json|secrets.yml|secrets.yaml|secrets.toml) return 0 ;;
  esac
  return 1
}

found=0
allow_file=$(git rev-parse --show-toplevel)/.secret-scan-allow
allowed() { # path rule
  [ -f "$allow_file" ] || return 1
  base=${1%% @*}
  awk -v p="$base" -v r="$2" '$0 !~ /^#/ && $1 == p && $2 == r { ok=1 } END { exit ok ? 0 : 1 }' "$allow_file"
}
report() { printf 'secret-scan: %s:%s: %s\n' "$1" "$2" "$3" >&2; found=1; }

scan_stream() {
  # reads "path<TAB>lineno<TAB>content" lines on stdin; one grep per rule over the whole stream
  stream=$(mktemp)
  grep -v -F 'secret-scan: allow' > "$stream" || true
  printf '%s\n' "$rules" | while IFS="$(printf '\t')" read -r name re; do
    grep -a -E -- "$re" "$stream" | awk -F'\t' -v n="$name" '{ printf "%s\t%s\t%s\n", $1, $2, n }' || true
  done
  rm -f "$stream"
}

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT HUP INT TERM

case "$mode" in
  --staged)
    files=$(git diff --cached --name-only --diff-filter=ACMR)
    for f in $files; do if bad_name "$(basename "$f")" && ! allowed "$f" forbidden-file-name; then report "$f" 0 "forbidden-file-name"; fi; done
    git diff --cached -U0 --no-color --diff-filter=ACMR | awk '
      /^\+\+\+ b\// { path=substr($0,7); next }
      /^@@/ { match($0,/\+[0-9]+/); line=substr($0,RSTART+1,RLENGTH-1)+0; next }
      /^\+/ && !/^\+\+\+/ { sub(/^\+/,""); printf "%s\t%d\t%s\n", path, line, $0; line++ }
    ' | scan_stream > "$tmp"
    ;;
  --tree)
    for f in $(git ls-files); do if bad_name "$(basename "$f")" && ! allowed "$f" forbidden-file-name; then report "$f" 0 "forbidden-file-name"; fi; done
    git grep -I -n --no-color -e '' -- . ':!package-lock.json' ':!Cargo.lock' ':!pnpm-lock.yaml' ':!yarn.lock' \
      | awk -F: '{ path=$1; line=$2; sub(/^[^:]*:[^:]*:/,""); printf "%s\t%s\t%s\n", path, line, $0 }' | scan_stream > "$tmp"
    ;;
  --history)
    for c in $(git rev-list HEAD); do
      git show --no-color -U0 --format= "$c" -- . ':!package-lock.json' ':!Cargo.lock' | awk -v c="$(printf '%.8s' "$c")" '
        /^\+\+\+ b\// { path=substr($0,7) " @" c; next }
        /^@@/ { match($0,/\+[0-9]+/); line=substr($0,RSTART+1,RLENGTH-1)+0; next }
        /^\+/ && !/^\+\+\+/ { sub(/^\+/,""); printf "%s\t%d\t%s\n", path, line, $0; line++ }
      ' | scan_stream
    done > "$tmp"
    ;;
esac

# Reviewed allowlist: .secret-scan-allow in the repo root, one entry per line:
#   <path> <rule>   # reason
# An entry silences one rule for one file (fixtures only; every entry needs a reason).
# Rule names: the names printed by the scanner, and forbidden-file-name for tracked key/env files.
while IFS="$(printf '\t')" read -r path lineno name; do
  [ -n "$path" ] || continue
  allowed "$path" "$name" && continue
  report "$path" "$lineno" "$name"
done < "$tmp"

if [ "$found" -ne 0 ]; then
  echo "secret-scan: blocked. Remove the secret (and rotate it if it was ever pushed), or add 'secret-scan: allow' on a line that is a known-safe fixture." >&2
  exit 1
fi
echo "secret-scan: clean ($mode)"
