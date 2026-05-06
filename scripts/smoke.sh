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

# Per CLAUDE.md bash rules: data lives in the project, not /tmp.
tmp="$repo_root/.smoke"
rm -rf "$tmp"
mkdir -p "$tmp"
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

echo "=== POST /jmap/api Mailbox/get ==="
mbx="$(curl -fsSL -H 'Content-Type: application/json' -X POST "$base/jmap/api" --data '{
  "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
  "methodCalls": [["Mailbox/get", {"accountId": "account-1"}, "c0"]]
}')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
assert d["sessionState"] == "fixture-state", d
mr = d["methodResponses"]
assert len(mr) == 1, mr
assert mr[0][0] == "Mailbox/get", mr
assert mr[0][2] == "c0", mr
result = mr[0][1]
assert result["accountId"] == "account-1", result
ids = [m["id"] for m in result["list"]]
assert ids == ["mbx-inbox", "mbx-archive"], ids
inbox = result["list"][0]
assert inbox["totalEmails"] == 2, inbox
assert inbox["unreadEmails"] == 2, inbox
print("Mailbox/get: ok ({} mailboxes)".format(len(ids)))
' "$mbx"

echo "=== POST /jmap/api Email/query ==="
eq="$(curl -fsSL -H 'Content-Type: application/json' -X POST "$base/jmap/api" --data '{
  "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
  "methodCalls": [["Email/query", {"accountId": "account-1", "calculateTotal": true}, "q0"]]
}')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
mr = d["methodResponses"][0]
assert mr[0] == "Email/query", mr
result = mr[1]
# Both fixture emails share 2026-01-15 received_at; "email-002" is newer
# (11:00 vs 10:00) so it should be first.
assert result["ids"] == ["email-002", "email-001"], result
assert result["total"] == 2, result
assert result["canCalculateChanges"] is False
assert result["queryState"] == "fixture-state"
print("Email/query: ok ({} ids)".format(len(result["ids"])))
' "$eq"

echo "=== POST /jmap/api Email/get ==="
eg="$(curl -fsSL -H 'Content-Type: application/json' -X POST "$base/jmap/api" --data '{
  "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
  "methodCalls": [
    ["Email/get", {"accountId": "account-1", "ids": ["email-001"], "fetchTextBodyValues": true}, "g0"],
    ["Email/get", {"accountId": "account-1", "ids": []}, "g1"]
  ]
}')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
mr = d["methodResponses"]
assert len(mr) == 2, mr

# First call: full email shape.
r0 = mr[0][1]
assert mr[0][0] == "Email/get" and mr[0][2] == "g0"
item = r0["list"][0]
assert item["id"] == "email-001"
assert item["blobId"] == "blob-email-001"
assert item["threadId"] == "email-001"
assert isinstance(item["receivedAt"], int)
assert item["mailboxIds"] == {"mbx-inbox": True}
assert item["from"] == [{"name": None, "email": "alice@example.com"}]
assert item["textBody"][0]["partId"] == "email-001:text"
assert item["bodyValues"]["email-001:text"]["value"] == "First message body."
assert item["attachments"] == []
for k in ("header:List-Unsubscribe:asText",
         "header:List-Unsubscribe-Post:asText",
         "header:Disposition-Notification-To:asText"):
    assert k in item and item[k] is None, k

# Second call: empty ids -> state-token-only response.
r1 = mr[1][1]
assert mr[1][2] == "g1"
assert r1["state"] == "fixture-state"
assert r1["list"] == []
print("Email/get: ok")
' "$eg"

echo "=== POST /jmap/api unknownMethod ==="
unk="$(curl -fsSL -H 'Content-Type: application/json' -X POST "$base/jmap/api" --data '{
  "using": [],
  "methodCalls": [["Email/import", {}, "c1"]]
}')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
mr = d["methodResponses"][0]
assert mr[0] == "error", mr
assert mr[1]["type"] == "unknownMethod", mr
assert mr[2] == "c1", mr
print("unknownMethod: ok")
' "$unk"

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
