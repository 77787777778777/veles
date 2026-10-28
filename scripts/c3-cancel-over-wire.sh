#!/bin/sh
# c3-cancel-over-wire.sh — the real-binary check for C3: a remote task
# is cancelable ACROSS THE WIRE, and the task methods are peer-scoped.
# Scratch home under /dev/shm; nothing touches the live config or the
# live :9900 (the b3a/b4/b5 discipline).
#
#   sh scripts/c3-cancel-over-wire.sh
#
# What is REAL: the receiver gateway (this build's binary), the A2A
# wire with real bearer auth, the ask encode/decode (hand-built here in
# the exact remotetask.rs wire shape: marker + ```b64 fence + sha256 +
# base64), the child process spawn + kill, and the tasks/get +
# tasks/cancel methods. What is STUBBED: the child's model call (a
# shell script that blocks until killed) and the SENDER process (this
# script plays it — its cancel POST is byte-for-byte the JSON-RPC
# A2AClient::cancel_task sends; the sender-side watch racing is pinned
# in remotetask::watch_pins against a raw loopback peer).
#
# Properties checked:
#   1. WORKING MID-RUN — an ask with a sender-minted wire_task_id
#      registers a task under that id, state WORKING, while the child
#      still blocks. (Pre-C3 the record existed only after the run.)
#   2. CANCEL KILLS THE CHILD — tasks/cancel answers CANCELED and the
#      child PROCESS is dead within ~2s, not riding out its sleep.
#   3. THE BLOCKED SEND RETURNS CANCELED — the sender's in-flight
#      message/send completes with the terminal CANCELED state.
#   4. PEER SCOPING — a different authenticated peer gets "task not
#      found" for the same id (get AND cancel): sender-minted ids are
#      known outside the node, so the gates are the fence.
set -e
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/debug/veles"
ROOT=$(mktemp -d /dev/shm/c3-XXXXXX)
B="$ROOT/b"
mkdir -p "$B"

cleanup() {
    [ -n "${GB:-}" ] && kill "$GB" 2>/dev/null || true
    sleep 0.3
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

B_ID="bbbbbbbb-0000-4000-8000-000000000003"
cat > "$B/node.toml" <<EOF
id = "$B_ID"
name = "c3-node-b"
EOF

# The blocking child: marks its start + pid, then BECOMES the sleep
# (exec — the kill lands on the process holding the pipes; a shell
# that merely spawned sleep would die and orphan it, and property 2
# would measure the fake, not the cancel path).
cat > "$B/fake-child" <<EOF
#!/bin/sh
echo \$\$ > "$B/child.pid"
touch "$B/child-started"
exec sleep 30
EOF
chmod +x "$B/fake-child"

cat > "$B/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19911"
[a2a_agents.a]
url = "http://127.0.0.1:19909"
auth = { type = "bearer", token = "tok-a" }
[cluster]
task_peers = ["a"]
gossip_interval_secs = 3
stale_after_secs = 60
EOF

# The script presents tok-a → identity "a" (the task peer); tok-c →
# identity "c" (authenticated, but NOT the task's owner — the scoping
# probe). B's own shared bearer is tok-b.
cat > "$B/.env" <<EOF
A2A_PEER_TOKENS=a:tok-a,c:tok-c
A2A_BEARER_TOKEN=tok-b
EOF

B_URL="http://127.0.0.1:19911"
A2A_PORT=19911 VELES_HOME="$B" HOME="$B" VELES_BINARY="$B/fake-child" \
    "$BIN" gateway run >"$ROOT/b.log" 2>&1 &
GB=$!

echo "== gateway b started ($GB); waiting for health"
ok=""
for i in $(seq 1 40); do
    sleep 0.25
    kill -0 "$GB" 2>/dev/null || { echo "FAIL: gateway b died"; tail -8 "$ROOT/b.log"; exit 1; }
    curl -sf -o /dev/null "$B_URL/.well-known/agent-card.json" \
        -H "Authorization: Bearer tok-b" 2>/dev/null && { ok=1; break; }
done
[ -n "$ok" ] || { echo "FAIL: b never came up"; tail -8 "$ROOT/b.log"; exit 1; }
echo "PASS 0: gateway b serves its card"

# ── the ask, hand-built in the exact remotetask.rs wire shape ──────
# marker line, a human line, the ```b64 fence, sha256:<hex of the JSON
# bytes>, the base64 of those same bytes. decode_ask re-hashes the
# decoded bytes, so the sha must be of the JSON as encoded.
WIRE_ID="wire-c3-e2e-1"
JSON="{\"from_node\":\"aaaaaaaa-0000-4000-8000-000000000009\",\"goal\":\"c3-slow-child\",\"context\":\"c3 e2e\",\"wire_task_id\":\"$WIRE_ID\"}"
SHA=$(printf '%s' "$JSON" | sha256sum | cut -d' ' -f1)
B64=$(printf '%s' "$JSON" | base64 | tr -d '\n')
ASK="[veles remote task]
remote subagent over the cluster link
\`\`\`b64
sha256:$SHA
$B64"
# JSON-escape the ask text for the message/send body. The ask has
# newlines, backticks and double-quotes; JSON needs \n, and the " and
# \ escaped. Pure shell (awk), no helper binary.
ASK_JSON=$(printf '%s' "$ASK" | awk '
    BEGIN { ORS="" }
    {
        gsub(/\\/, "\\\\");   # backslash first
        gsub(/"/, "\\\"");    # double-quote
        if (NR > 1) print "\\n";
        print
    }
')

SEND_BODY="{\"jsonrpc\":\"2.0\",\"id\":\"c3-send\",\"method\":\"message/send\",\"params\":{\"message\":{\"role\":\"ROLE_USER\",\"parts\":[{\"kind\":\"text\",\"text\":\"$ASK_JSON\"}],\"messageId\":\"c3-msg-1\",\"contextId\":\"c3-ctx-1\"}}}"

# Fire the send in the background — it blocks until the child ends or
# the cancel kills it (property 3 reads what it returns).
curl -s -X POST "$B_URL/" -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer tok-a' -d "$SEND_BODY" > "$ROOT/send-reply.json" &
SEND_PID=$!

# ── 1. WORKING mid-run under the sender's wire id ──────────────────
ok=""
for i in $(seq 1 60); do
    sleep 0.25
    [ -f "$B/child-started" ] || continue
    GET=$(curl -s -X POST "$B_URL/" -H 'Content-Type: application/json' \
        -H 'Authorization: Bearer tok-a' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":\"c3-get\",\"method\":\"tasks/get\",\"params\":{\"id\":\"$WIRE_ID\"}}")
    case "$GET" in *TASK_STATE_WORKING*) ok=1; break;; esac
done
[ -n "$ok" ] || { echo "FAIL: no WORKING record under the wire id while the child runs"; echo "$GET"; exit 1; }
echo "PASS 1: the receiver registered WORKING under the sender-minted id, mid-run"

# ── 4. peer scoping (before the cancel, while the record is live) ──
GETC=$(curl -s -X POST "$B_URL/" -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer tok-c' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":\"c3-getc\",\"method\":\"tasks/get\",\"params\":{\"id\":\"$WIRE_ID\"}}")
case "$GETC" in *"task not found"*) ;; *) echo "FAIL: a foreign peer read the task: $GETC"; exit 1;; esac
CANC=$(curl -s -X POST "$B_URL/" -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer tok-c' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":\"c3-canc\",\"method\":\"tasks/cancel\",\"params\":{\"id\":\"$WIRE_ID\"}}")
case "$CANC" in *"task not found"*) ;; *) echo "FAIL: a foreign peer reached tasks/cancel: $CANC"; exit 1;; esac
echo "PASS 4: tasks/get and tasks/cancel are peer-scoped (identity c sees nothing)"

# ── 2. the owner cancels; the child process dies ───────────────────
CANCEL=$(curl -s -X POST "$B_URL/" -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer tok-a' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":\"c3-cancel\",\"method\":\"tasks/cancel\",\"params\":{\"id\":\"$WIRE_ID\"}}")
case "$CANCEL" in *TASK_STATE_CANCELED*) ;; *) echo "FAIL: cancel did not answer CANCELED: $CANCEL"; exit 1;; esac
CPID=$(cat "$B/child.pid")
dead=""
for i in $(seq 1 20); do
    sleep 0.25
    kill -0 "$CPID" 2>/dev/null || { dead=1; break; }
done
[ -n "$dead" ] || { echo "FAIL: the child process ($CPID) survived the cancel"; exit 1; }
echo "PASS 2: tasks/cancel answered CANCELED and the child process died"

# ── 3. the blocked send returns the terminal state ─────────────────
ok=""
for i in $(seq 1 20); do
    kill -0 "$SEND_PID" 2>/dev/null || { ok=1; break; }
    sleep 0.25
done
[ -n "$ok" ] || { echo "FAIL: the sender's message/send never returned"; exit 1; }
wait "$SEND_PID" || true
case "$(cat "$ROOT/send-reply.json")" in
    *TASK_STATE_CANCELED*) ;;
    *) echo "FAIL: the send reply is not CANCELED:"; cat "$ROOT/send-reply.json"; exit 1;;
esac
echo "PASS 3: the sender's blocked message/send returned TASK_STATE_CANCELED"

echo ""
echo "PASS: C3 cancel-over-wire verified against a real gateway (4 properties)"
exit 0
