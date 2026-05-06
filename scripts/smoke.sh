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

jmap_port="$(awk '/^JMAP /{print $2}' "$ready_file")"
imap_port="$(awk '/^IMAP /{print $2}' "$ready_file")"
smtp_port="$(awk '/^SMTP /{print $2}' "$ready_file")"
graph_port="$(awk '/^GRAPH /{print $2}' "$ready_file")"
gmail_port="$(awk '/^GMAIL /{print $2}' "$ready_file")"
base="http://127.0.0.1:$jmap_port"
graph_base="http://127.0.0.1:$graph_port"
gmail_base="http://127.0.0.1:$gmail_port"
echo "sentinel:"
sed 's/^/  /' "$ready_file"

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

echo "=== IMAP full bootstrap + LIST + STATUS + SELECT + UID SEARCH ==="
imap_out="$(printf 'a1 CAPABILITY\r\nb1 LOGIN "alice" "hunter2"\r\nc1 ENABLE QRESYNC\r\nd1 LIST "" "*"\r\ne1 STATUS "INBOX" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY HIGHESTMODSEQ)\r\nf1 SELECT "INBOX"\r\ng1 UID SEARCH ALL\r\nh1 UID SEARCH 2:*\r\nq LOGOUT\r\n' | nc -w 2 127.0.0.1 "$imap_port")"
python3 -c '
import sys
out = sys.argv[1]
assert "* OK saehrimnir IMAP4rev1 ready" in out, out
assert "* CAPABILITY IMAP4REV1 CONDSTORE QRESYNC" in out, out
assert "a1 OK CAPABILITY completed" in out, out
assert "b1 OK [CAPABILITY IMAP4REV1 CONDSTORE QRESYNC] LOGIN completed" in out, out
assert "* ENABLED QRESYNC" in out, out
assert "c1 OK ENABLE completed" in out, out
assert "* LIST (\\Inbox) \"/\" \"INBOX\"" in out, out
assert "* LIST (\\Archive) \"/\" \"Archive\"" in out, out
assert "d1 OK LIST completed" in out, out
assert "* STATUS \"INBOX\" (MESSAGES 2 UNSEEN 2 UIDNEXT 3 UIDVALIDITY 1 HIGHESTMODSEQ 1)" in out, out
assert "e1 OK STATUS completed" in out, out
# SELECT - should emit the canonical untagged set.
assert "* 2 EXISTS" in out, out
assert "* 0 RECENT" in out, out
assert "* OK [UIDVALIDITY 1]" in out, out
assert "* OK [UIDNEXT 3]" in out, out
assert "* OK [HIGHESTMODSEQ 1]" in out, out
assert "f1 OK [READ-WRITE] SELECT completed" in out, out
# UID SEARCH ALL with two emails -> "* SEARCH 1 2".
assert "* SEARCH 1 2" in out, out
assert "g1 OK UID SEARCH completed" in out, out
# UID SEARCH 2:* on a 2-email folder -> "* SEARCH 2".
assert "* SEARCH 2\r\n" in out, out
assert "h1 OK UID SEARCH completed" in out, out
print("IMAP through UID SEARCH: ok (continuing)")
' "$imap_out"

echo "=== IMAP UID FETCH ==="
imap_out2="$(printf 'a LOGIN "u" "p"\r\nb SELECT "INBOX"\r\nc UID FETCH 1:* (UID FLAGS INTERNALDATE BODY.PEEK[])\r\nq LOGOUT\r\n' | nc -w 2 127.0.0.1 "$imap_port")"
python3 -c '
import sys, re
out = sys.argv[1]
# Two FETCH responses, one per fixture email.
assert out.count("* 1 FETCH (") == 1, out
assert out.count("* 2 FETCH (") == 1, out
# UIDs echoed.
assert "UID 1" in out and "UID 2" in out
# INTERNALDATE in IMAP wire format.
assert "INTERNALDATE \"15-Jan-2026 10:00:00 +0000\"" in out, out
# BODY[] literal block: {N}\r\n then exactly N bytes.
m = re.search(r"BODY\[\] \{(\d+)\}\r\n", out)
assert m, out
size = int(m.group(1))
start = m.end()
# Verify the reported size matches the bytes that follow up to the
# closing paren of the FETCH item (which is followed by the next
# FETCH or the tagged OK).
body = out[start:start+size]
assert "Subject:" in body, body
assert "MIME-Version: 1.0" in body, body
assert "Content-Type: text/plain; charset=utf-8" in body, body
assert "First message body." in body or "Reply body." in body, body
assert "c OK UID FETCH completed" in out, out
print("IMAP UID FETCH: ok")
' "$imap_out2"

echo "=== SMTP submission ==="
smtp_out="$(printf 'EHLO me.local\r\nAUTH PLAIN AGFsaWNlAGh1bnRlcg==\r\nMAIL FROM:<alice@example.com>\r\nRCPT TO:<bob@example.com>\r\nDATA\r\nFrom: <alice@example.com>\r\nTo: <bob@example.com>\r\nSubject: smoke\r\n\r\nbody\r\n.\r\nQUIT\r\n' | nc -w 2 127.0.0.1 "$smtp_port")"
python3 -c '
import sys
out = sys.argv[1]
assert "220 saehrimnir ESMTP ready" in out, out
assert "250-saehrimnir greets you" in out, out
assert "250 AUTH PLAIN LOGIN XOAUTH2" in out, out
assert "235 authentication accepted" in out, out
assert "354 send data" in out, out
assert "250 OK queued" in out, out
assert "221 saehrimnir bye" in out, out
print("SMTP submission: ok")
' "$smtp_out"

echo "=== Graph mailFolders + messages + delta ==="
folders="$(curl -fsSL "$graph_base/v1.0/me/mailFolders" -H 'Authorization: Bearer x')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
v = d["value"]
ids = [f["id"] for f in v]
assert ids == ["mbx-inbox", "mbx-archive"], ids
assert v[0]["wellKnownName"] == "inbox"
assert v[0]["totalItemCount"] == 2
print("Graph mailFolders: ok")
' "$folders"

inbox_msgs="$(curl -fsSL "$graph_base/v1.0/me/mailFolders/inbox/messages" -H 'Authorization: Bearer x')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
ids = [m["id"] for m in d["value"]]
assert ids == ["email-002", "email-001"], ids
m0 = d["value"][0]
assert m0["body"]["contentType"] == "text"
assert m0["body"]["content"] == "Reply body."
assert m0["from"]["emailAddress"]["address"] == "carol@example.com"
assert m0["receivedDateTime"].endswith("Z")
print("Graph messages: ok")
' "$inbox_msgs"

delta="$(curl -fsSL "$graph_base/v1.0/me/mailFolders/inbox/messages/delta" -H 'Authorization: Bearer x')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
assert len(d["value"]) == 2
assert "@odata.deltaLink" in d
assert "$deltatoken=" in d["@odata.deltaLink"]
print("Graph delta: ok")
' "$delta"

echo "=== Gmail profile + labels + threads + history ==="
profile="$(curl -fsSL "$gmail_base/gmail/v1/users/me/profile" -H 'Authorization: Bearer x')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
assert d["emailAddress"] == "test@example.com", d
assert d["historyId"] == "1", d
print("Gmail profile: ok")
' "$profile"

threads="$(curl -fsSL "$gmail_base/gmail/v1/users/me/threads" -H 'Authorization: Bearer x')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
ids = [t["id"] for t in d["threads"]]
assert ids == ["email-002", "email-001"], ids
print("Gmail threads: ok")
' "$threads"

thread="$(curl -fsSL "$gmail_base/gmail/v1/users/me/threads/email-001?format=full" -H 'Authorization: Bearer x')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
assert d["id"] == "email-001"
m = d["messages"][0]
assert "INBOX" in m["labelIds"]
assert "UNREAD" in m["labelIds"]
assert m["payload"]["body"]["data"] == "Rmlyc3QgbWVzc2FnZSBib2R5Lg"
print("Gmail thread fetch: ok")
' "$thread"

history="$(curl -fsSL "$gmail_base/gmail/v1/users/me/history?startHistoryId=1" -H 'Authorization: Bearer x')"
python3 -c '
import json, sys
d = json.loads(sys.argv[1])
assert d["history"] == []
assert d["historyId"] == "1"
print("Gmail history: ok")
' "$history"

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
