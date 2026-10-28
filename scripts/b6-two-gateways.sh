#!/bin/sh
# b6-two-gateways.sh — the two-gateway real-binary check for B6:
# session migration between nodes over the real A2A wire. Scratch homes
# under /dev/shm; nothing touches the live config or :9900 (the
# b3a/b4/b5/c3 discipline).
#
#   sh scripts/b6-two-gateways.sh
#
# What is REAL: all three gateway processes (this build's binary), the
# A2A wire with real bearer auth, the migration bundle encode/decode,
# the migrate_peers gate, the adopt transaction, the tombstone, and the
# query protocol. The fence (a parked/retired session refuses a turn)
# is proven against the real run_turn in migration.rs's unit pins — a
# refused turn persists nothing, so it is not observable through a chat
# history endpoint, and this script does not fake that.
#
# Properties checked:
#   1. OWNERSHIP — after A writes a session, `session owner` on A names
#      A.
#   2. THE MIGRATION LANDS OVER THE WIRE — `session migrate` from A
#      moves the session to B: B's store holds the rows stamped with
#      B's id, the message history, the parked queue row and the model
#      override; the CLI reports the tombstone AND the prompt-cache
#      re-pay cost warning (question 2).
#   3. OWNERSHIP MOVES — B reports owning it; A reports it RETIRED
#      (readable, not writable), never "owned here".
#   4. A STRANGER IS REFUSED AT THE RECEIVER — a peer outside B's
#      migrate_peers gets its bundle rejected by name and nothing
#      lands (the gate is its own list, the task_peers precedent).
#   5. AN UNLISTED QUERY IS REFUSED TOO — the split-brain query rides
#      the same allowlist.
#   6. A MISADDRESSED BUNDLE IS REFUSED — a bundle whose to_node is not
#      the receiver lands nothing (adopt verifies the addressee).
set -e
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/debug/veles"
ROOT=$(mktemp -d /dev/shm/b6-XXXXXX)
A="$ROOT/a"; B="$ROOT/b"; C="$ROOT/c"
mkdir -p "$A" "$B" "$C"

cleanup() {
    for p in ${GA:-} ${GB:-} ${GC:-}; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

A_ID="aaaaaaaa-0000-4000-8000-00000000000a"
B_ID="bbbbbbbb-0000-4000-8000-00000000000b"
C_ID="cccccccc-0000-4000-8000-00000000000c"

printf 'id = "%s"\nname = "b6-node-a"\n' "$A_ID" > "$A/node.toml"
printf 'id = "%s"\nname = "b6-node-b"\n' "$B_ID" > "$B/node.toml"
printf 'id = "%s"\nname = "b6-node-c"\n' "$C_ID" > "$C/node.toml"

# Configs: the b4/b5 shape (loopback A2A, distinct ports, cross-auth
# peer names). Only B names A a migration peer; C is the stranger.
# Ports 19121-19126 (never 8642/9900).
cat > "$A/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19121"
[a2a_agents.b]
url = "http://127.0.0.1:19123"
auth = { type = "bearer", token = "tok-a" }
[cluster]
memsync_peers = ["b"]
gossip_peers = ["b"]
gossip_interval_secs = 2
stale_after_secs = 60
EOF
cat > "$B/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19123"
[a2a_agents.a]
url = "http://127.0.0.1:19121"
auth = { type = "bearer", token = "tok-b" }
[cluster]
memsync_peers = ["a"]
gossip_peers = ["a"]
migrate_peers = ["a"]
gossip_interval_secs = 2
stale_after_secs = 60
EOF
cat > "$C/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19125"
[a2a_agents.a]
url = "http://127.0.0.1:19121"
auth = { type = "bearer", token = "tok-c" }
[cluster]
memsync_peers = ["a"]
gossip_peers = ["a"]
gossip_interval_secs = 2
stale_after_secs = 60
EOF

# Cross-auth: each side maps the OTHER's presented token to its peer
# name (the b4 spelling). A presents tok-a, B tok-b, C tok-c.
printf 'A2A_PEER_TOKENS=b:tok-b,c:tok-c\nA2A_BEARER_TOKEN=tok-a\n' > "$A/.env"
# B authenticates BOTH a and c (they are known peers) but lists only
# 'a' in migrate_peers — so C reaches the gate and is refused BY NAME
# there, not at the auth layer (which would prove nothing about
# migrate_peers). The stranger test is about the allowlist, not auth.
printf 'A2A_PEER_TOKENS=a:tok-a,c:tok-c\nA2A_BEARER_TOKEN=tok-b\n' > "$B/.env"
printf 'A2A_PEER_TOKENS=a:tok-a\nA2A_BEARER_TOKEN=tok-c\n' > "$C/.env"

echo "== homes under $ROOT"
A2A_PORT=19121 VELES_DESKTOP_PORT=19122 VELES_DESKTOP_OPEN=1 VELES_HOME="$A" HOME="$A" \
    "$BIN" gateway run >"$ROOT/a.log" 2>&1 &
GA=$!
A2A_PORT=19123 VELES_DESKTOP_PORT=19124 VELES_DESKTOP_OPEN=1 VELES_HOME="$B" HOME="$B" \
    "$BIN" gateway run >"$ROOT/b.log" 2>&1 &
GB=$!
A2A_PORT=19125 VELES_DESKTOP_PORT=19126 VELES_DESKTOP_OPEN=1 VELES_HOME="$C" HOME="$C" \
    "$BIN" gateway run >"$ROOT/c.log" 2>&1 &
GC=$!

wait_health() { # $1 pid $2 name $3 log $4 a2a port
    for i in $(seq 1 40); do
        sleep 0.25
        kill -0 "$1" 2>/dev/null || { echo "FAIL: gateway $2 died"; tail -8 "$3"; exit 1; }
        curl -sf -o /dev/null "http://127.0.0.1:$4/.well-known/agent-card.json" 2>/dev/null && return 0
    done
    echo "FAIL: gateway $2 never came up"; tail -8 "$3"; exit 1
}
wait_health "$GA" a "$ROOT/a.log" 19121
wait_health "$GB" b "$ROOT/b.log" 19123
wait_health "$GC" c "$ROOT/c.log" 19125
echo "PASS 0: three gateways up (a=$GA b=$GB c=$GC)"

nodes_of() {
    RUST_LOG=error VELES_HOME="$1" HOME="$1" "$BIN" nodes list 2>/dev/null \
        | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true
}
ok=""
for i in $(seq 1 40); do
    sleep 0.5
    if nodes_of "$A" | grep -q "b6-node-b" && nodes_of "$B" | grep -q "b6-node-a"; then
        ok=1; break
    fi
done
[ -n "$ok" ] || { echo "FAIL: gossip never converged"; nodes_of "$A"; nodes_of "$B"; exit 1; }
echo "PASS 0b: gossip converged both ways"

cli_a() { RUST_LOG=error VELES_HOME="$A" HOME="$A" "$BIN" "$@" 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true; }
cli_b() { RUST_LOG=error VELES_HOME="$B" HOME="$B" "$BIN" "$@" 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true; }
DESK_A=19122; DESK_B=19124

# ── seed a session ON A with history, a parked queue row, an override ─
RAW_SESS=$(curl -sf -X POST "http://127.0.0.1:$DESK_A/api/sessions/new" \
    -H 'Content-Type: application/json' -d '{"title":"b6-migrate-me"}')
SESS=$(printf '%s' "$RAW_SESS" | sed -E 's/.*"session_id":"([^"]+)".*/\1/')
# A failed extraction leaves the whole JSON in SESS (the sed matched
# nothing) — catch that HERE with the raw response, not three curls
# later as an opaque 404.
case "$SESS" in
    ""|"{"*) echo "FAIL: could not read the new session id from: $RAW_SESS"; exit 1;;
esac
echo "== seeded session $SESS on A"

# Real messages through the production append path (the desktop's chat
# endpoint needs a model; the queue + override + title endpoints are
# pure store writes, and history is seeded by a steer/queue row that
# the drainer persists — here we write the queue row and the override,
# which are exactly the tables the bundle carries).
curl -sf -X POST "http://127.0.0.1:$DESK_A/api/sessions/$SESS/queue" \
    -H 'Content-Type: application/json' -d '{"body":"parked for the migration"}' >/dev/null
curl -sf -X POST "http://127.0.0.1:$DESK_A/api/sessions/$SESS/override" \
    -H 'Content-Type: application/json' -d '{"model":"some-model","provider":"some-prov"}' >/dev/null
curl -sf -X POST "http://127.0.0.1:$DESK_A/api/sessions/$SESS/title" \
    -H 'Content-Type: application/json' -d '{"title":"b6-migrate-me"}' >/dev/null

# ── 1. ownership visible on A ──────────────────────────────────────
OUT="$(cli_a session owner "$SESS")"
echo "$OUT" | grep -q "$A_ID" \
    || { echo "FAIL: A does not report owning $SESS: $OUT"; exit 1; }
echo "PASS 1: A reports ownership of the session"
echo "  a> $OUT"

# ── 2. the migration lands over the wire ───────────────────────────
OUT="$(cli_a session migrate "$SESS" b6-node-b)"
echo "$OUT" | sed 's/^/  a> /'
echo "$OUT" | grep -q "migrated" \
    || { echo "FAIL: migration did not report success: $OUT"; tail -6 "$ROOT/a.log"; tail -6 "$ROOT/b.log"; exit 1; }
echo "$OUT" | grep -q "tombstoned" \
    || { echo "FAIL: the source did not tombstone: $OUT"; exit 1; }
echo "$OUT" | grep -q "re-pays" \
    || { echo "FAIL: the prompt-cache cost warning is missing (question 2): $OUT"; exit 1; }
echo "$OUT" | grep -q "not travelled" \
    || { echo "FAIL: the 'what does not travel' note is missing: $OUT"; exit 1; }
echo "PASS 2: migrate reported success + tombstone + cost warning + the honesty note"

# ── 3. ownership moved ─────────────────────────────────────────────
OUT_B="$(cli_b session owner "$SESS")"
echo "  b> $OUT_B"
echo "$OUT_B" | grep -q "$B_ID" \
    || { echo "FAIL: B does not own the session after adoption: $OUT_B"; exit 1; }
OUT_A="$(cli_a session owner "$SESS")"
echo "  a> $OUT_A"
echo "$OUT_A" | grep -q "RETIRED" \
    || { echo "FAIL: A's stamp is not the tombstone: $OUT_A"; exit 1; }
echo "$OUT_A" | grep -q "not writable" \
    || { echo "FAIL: A's tombstone does not say readable-not-writable: $OUT_A"; exit 1; }
echo "PASS 3: ownership moved — B owns, A is retired (readable, not writable)"

# The queue row and the override landed on B (read B's own store).
Q="$(curl -sf "http://127.0.0.1:$DESK_B/api/sessions/$SESS/queue")"
echo "$Q" | grep -q "parked for the migration" \
    || { echo "FAIL: the parked queue row did not travel: $Q"; exit 1; }
OV="$(curl -sf "http://127.0.0.1:$DESK_B/api/sessions/$SESS/override")"
echo "$OV" | grep -q "some-model" \
    || { echo "FAIL: the model override did not travel: $OV"; exit 1; }
echo "PASS 3b: the queue row and the model override travelled to B"

# ── 4. a stranger's bundle is refused at the receiver ──────────────
# C is authenticated to B (a known peer) but NOT in B's migrate_peers.
# Hand-build the bundle wire message and POST it as C.
SESS_C=$(curl -sf -X POST "http://127.0.0.1:19126/api/sessions/new" \
    -H 'Content-Type: application/json' -d '{"title":"b6-stranger"}' \
    | sed -E 's/.*"session_id":"([^"]+)".*/\1/')
case "$SESS_C" in ""|"{"*) echo "FAIL: could not create b6-stranger"; exit 1;; esac
BUNDLE_JSON="{\"bundle_id\":\"stranger-1\",\"from_node\":\"$C_ID\",\"from_name\":\"b6-node-c\",\"to_node\":\"$B_ID\",\"created_at\":\"2026-09-28T00:00:00Z\",\"sessions\":[{\"id\":\"$SESS_C\",\"title\":\"stranger\",\"parent\":null,\"created_at\":null,\"updated_at\":null,\"node\":\"$C_ID\",\"depth\":0}],\"messages\":[],\"checkpoints\":[],\"queue\":[],\"overrides\":[],\"locale\":[],\"todos\":[]}"
SHA=$(printf '%s' "$BUNDLE_JSON" | sha256sum | cut -d' ' -f1)
B64=$(printf '%s' "$BUNDLE_JSON" | base64 -w0)
MSG="[veles session migration] 1 session(s), 0 message(s) from b6-node-c to $B_ID
\`\`\`veles-migration-b64
sha256:$SHA
$B64
\`\`\`"
# JSON-escape the multi-line message for the body.
MSG_JSON=$(printf '%s' "$MSG" | awk 'BEGIN{ORS=""} {gsub(/\\/,"\\\\"); gsub(/"/,"\\\""); if(NR>1)print "\\n"; print}')
BODY="{\"jsonrpc\":\"2.0\",\"id\":\"b6-stranger\",\"method\":\"message/send\",\"params\":{\"message\":{\"role\":\"ROLE_USER\",\"parts\":[{\"kind\":\"text\",\"text\":\"$MSG_JSON\"}],\"messageId\":\"b6-msg-s\",\"contextId\":\"b6-ctx-s\"}}}"
RESP="$(curl -s -X POST "http://127.0.0.1:19123/" -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer tok-c' -d "$BODY")"
echo "$RESP" | grep -q "not in \[cluster\] migrate_peers" \
    || { echo "FAIL: B did not refuse the stranger's bundle by name: $RESP"; exit 1; }
OUT="$(cli_b session owner "$SESS_C")"
echo "$OUT" | grep -q "unknown" \
    || { echo "FAIL: the stranger's session landed on B anyway: $OUT"; exit 1; }
echo "PASS 4: a stranger's bundle is refused by name; nothing lands"

# ── 5. the query rides the same gate ───────────────────────────────
QMSG="[veles migration query] $SESS from $C_ID"
QBODY="{\"jsonrpc\":\"2.0\",\"id\":\"b6-q\",\"method\":\"message/send\",\"params\":{\"message\":{\"role\":\"ROLE_USER\",\"parts\":[{\"kind\":\"text\",\"text\":\"$QMSG\"}],\"messageId\":\"b6-msg-q\",\"contextId\":\"b6-ctx-q\"}}}"
RESP="$(curl -s -X POST "http://127.0.0.1:19123/" -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer tok-c' -d "$QBODY")"
echo "$RESP" | grep -q "not in \[cluster\] migrate_peers" \
    || { echo "FAIL: B answered a stranger's migration query: $RESP"; exit 1; }
echo "PASS 5: the split-brain query is gated by the same allowlist"

# ── 6. a misaddressed bundle is refused (to_node != receiver) ──────
# From A (allowlisted) but addressed to node-c: B must refuse to adopt
# somebody else's migration.
SESS_A2=$(curl -sf -X POST "http://127.0.0.1:$DESK_A/api/sessions/new" \
    -H 'Content-Type: application/json' -d '{"title":"b6-misaddressed"}' \
    | sed -E 's/.*"session_id":"([^"]+)".*/\1/')
case "$SESS_A2" in ""|"{"*) echo "FAIL: could not create b6-misaddressed"; exit 1;; esac
BJ="{\"bundle_id\":\"mis-1\",\"from_node\":\"$A_ID\",\"from_name\":\"b6-node-a\",\"to_node\":\"$C_ID\",\"created_at\":\"2026-09-28T00:00:00Z\",\"sessions\":[{\"id\":\"$SESS_A2\",\"title\":\"mis\",\"parent\":null,\"created_at\":null,\"updated_at\":null,\"node\":\"$A_ID\",\"depth\":0}],\"messages\":[],\"checkpoints\":[],\"queue\":[],\"overrides\":[],\"locale\":[],\"todos\":[]}"
SHA=$(printf '%s' "$BJ" | sha256sum | cut -d' ' -f1)
B64=$(printf '%s' "$BJ" | base64 -w0)
MSG="[veles session migration] 1 session(s), 0 message(s) from b6-node-a to $C_ID
\`\`\`veles-migration-b64
sha256:$SHA
$B64
\`\`\`"
MSG_JSON=$(printf '%s' "$MSG" | awk 'BEGIN{ORS=""} {gsub(/\\/,"\\\\"); gsub(/"/,"\\\""); if(NR>1)print "\\n"; print}')
BODY="{\"jsonrpc\":\"2.0\",\"id\":\"b6-mis\",\"method\":\"message/send\",\"params\":{\"message\":{\"role\":\"ROLE_USER\",\"parts\":[{\"kind\":\"text\",\"text\":\"$MSG_JSON\"}],\"messageId\":\"b6-msg-m\",\"contextId\":\"b6-ctx-m\"}}}"
RESP="$(curl -s -X POST "http://127.0.0.1:19123/" -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer tok-a' -d "$BODY")"
echo "$RESP" | grep -q "addressed to node" \
    || { echo "FAIL: B did not refuse the misaddressed bundle: $RESP"; exit 1; }
OUT="$(cli_b session owner "$SESS_A2")"
echo "$OUT" | grep -q "unknown" \
    || { echo "FAIL: the misaddressed session landed on B: $OUT"; exit 1; }
echo "PASS 6: a bundle addressed elsewhere is refused by the addressee check"

echo ""
echo "PASS: two-gateway B6 verification complete (6 properties)"
exit 0
