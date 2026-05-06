#!/usr/bin/env bash
# Smoke test: build, boot, exercise the routes, SIGTERM, verify a clean
# shutdown. Used during development; not a substitute for integration
# tests.
#
# Usage:
#   scripts/smoke.sh             # debug build
#   scripts/smoke.sh --release   # release build
#
# Exits 0 on success, non-zero on any failure. Prints intermediate
# observations to stdout so a failed run is debuggable from the log.

set -euo pipefail

profile="dev"
target_dir="target/debug"
if [[ "${1-}" == "--release" ]]; then
    profile="release"
    target_dir="target/release"
elif [[ "${1-}" == "--help" || "${1-}" == "-h" ]]; then
    sed -n '2,12p' "$0"
    exit 0
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

tmp="$(mktemp -d -t saehrimnir-smoke-XXXXXX)"
ready_file="$tmp/ready"
pid=""

cleanup() {
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT

echo "=== build ($profile) ==="
if [[ "$profile" == "release" ]]; then
    cargo build --release --quiet
else
    cargo build --quiet
fi

echo "=== boot ==="
"$target_dir/saehrimnir" \
    --port 0 \
    --readiness-file "$ready_file" \
    --fixture fixtures/jmap-small.toml &
pid=$!

# Wait for sentinel (max 5s).
for _ in $(seq 1 50); do
    [[ -f "$ready_file" ]] && break
    sleep 0.1
done
if [[ ! -f "$ready_file" ]]; then
    echo "saehrimnir did not write sentinel within 5s" >&2
    exit 1
fi

port="$(awk '{print $2}' "$ready_file")"
base="http://127.0.0.1:$port"
echo "sentinel: $(cat "$ready_file")"

echo "=== GET / ==="
curl -fsSL "$base/"

echo "=== GET /jmap/session ==="
session="$(curl -fsSL "$base/jmap/session")"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
print("capabilities:", sorted(d["capabilities"]))
print("accounts:    ", sorted(d["accounts"]))
print("state:       ", d["state"])
print("primary:     ", d["primaryAccounts"])
' "$session"

echo "=== GET /.well-known/jmap matches /jmap/session ==="
wk="$(curl -fsSL "$base/.well-known/jmap")"
if [[ "$session" != "$wk" ]]; then
    echo "/.well-known/jmap diverged from /jmap/session" >&2
    diff <(echo "$session") <(echo "$wk") || true
    exit 1
fi
echo "match"

echo "=== SIGTERM ==="
kill -TERM "$pid"
wait "$pid"
status=$?
pid=""  # don't double-kill in cleanup
if [[ $status -ne 0 ]]; then
    echo "non-zero exit: $status" >&2
    exit 1
fi
echo "=== clean exit ==="
