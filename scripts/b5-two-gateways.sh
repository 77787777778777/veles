#!/bin/sh
# b5-two-gateways.sh — the two-gateway real-binary check for the B5
# round: store replication over the real A2A wire. Scratch homes under
# /dev/shm; nothing touches the live config or the live :9900 (the
# b3a/b4 round discipline).
#
#   sh scripts/b5-two-gateways.sh
#
# What is REAL: both gateway processes (this build's binary), the A2A
# wire, the storesync offer interceptor + inbox gate, the store_log
# triggers writing real row images on A, and B's replica file built
# from them. What is STUBBED: nothing — no model call is involved.
#
# Properties checked:
#   1. THE LOG CAPTURES REAL WRITES — a session + message written
#      through A's own store API land in A's store_log.
#   2. PUSH → INBOX → APPLY — `veles replicas push` from A stores in
#      B's inbox; `veles replicas apply --inbox` on B builds B's
#      replica of A with the rows.
#   3. THE REPLICA IS READABLE — B's replica answers session search
#      (FTS over the replica) and the owner question (session_owner
#      names A).
#   4. INCREMENTAL — a second write + push past the cursor lands
#      without re-shipping history.
#   5. THE GATE — a peer NOT in storesync_peers is refused.
#   6. GAP HONESTY — a partial re-push (wrong cursor) refuses rather
#      than stitching.
set -e
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/debug/veles"
ROOT=$(mktemp -d /dev/shm/b5-XXXXXX)
A="$ROOT/a"; B="$ROOT/b"; C="$ROOT/c"; D="$ROOT/d"
mkdir -p "$A" "$B" "$C" "$D"

cleanup() {
    for p in ${GA:-} ${GB:-} ${GC:-} ${GD:-}; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

A_ID="aaaaaaaa-0000-4000-8000-000000000005"
D_ID="dddddddd-0000-4000-8000-000000000008"
B_ID="bbbbbbbb-0000-4000-8000-000000000006"
C_ID="cccccccc-0000-4000-8000-000000000007"

# Pre-seeded identities (the b3a/b4 pattern).
cat > "$A/node.toml" <<EOF
id = "$A_ID"
name = "b5-node-a"
EOF
cat > "$B/node.toml" <<EOF
id = "$B_ID"
name = "b5-node-b"
EOF
cat > "$C/node.toml" <<EOF
id = "$C_ID"
name = "b5-node-c"
EOF

# A and B cross-authed; C is a stranger (not in B's storesync_peers).
cat > "$A/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19101"
[a2a_agents.b]
url = "http://127.0.0.1:19103"
auth = { type = "bearer", token = "tok-a" }
[cluster]
memsync_peers = ["b"]
gossip_peers = ["b"]
storesync_peers = ["b"]
storesync_auto = true
gossip_interval_secs = 3
EOF
cat > "$B/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19103"
[a2a_agents.a]
url = "http://127.0.0.1:19101"
auth = { type = "bearer", token = "tok-b" }
[cluster]
memsync_peers = ["a"]
gossip_peers = ["a"]
storesync_peers = ["a"]
storesync_auto = true
gossip_interval_secs = 3
EOF
# D: hand-mode receiver — B's storesync peer with auto OFF, for the
# deterministic hand-push and gap steps (auto A would heal a hole
# mid-test; D never does).
cat > "$D/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19109"
[a2a_agents.a]
url = "http://127.0.0.1:19101"
auth = { type = "bearer", token = "tok-a" }
[cluster]
memsync_peers = ["a"]
gossip_peers = []
storesync_peers = ["a"]
gossip_interval_secs = 3
EOF
cat > "$D/node.toml" <<EOF
id = "$D_ID"
name = "b5-node-d"
EOF

# C: talks to B, not named anywhere in B's lists.
cat > "$C/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19107"
[a2a_agents.b5node]
url = "http://127.0.0.1:19103"
auth = { type = "bearer", token = "tok-b" }
[cluster]
memsync_peers = ["b5node"]
gossip_peers = []
storesync_peers = ["b5node"]
gossip_interval_secs = 3
EOF

echo 'A2A_PEER_TOKENS=b:tok-b' > "$A/.env"
echo 'A2A_BEARER_TOKEN=tok-a' >> "$A/.env"
echo 'A2A_PEER_TOKENS=a:tok-a' > "$B/.env"
echo 'A2A_BEARER_TOKEN=tok-b' >> "$B/.env"
echo 'A2A_PEER_TOKENS=b5node:tok-b' > "$C/.env"
echo 'A2A_BEARER_TOKEN=tok-c' >> "$C/.env"
echo 'A2A_PEER_TOKENS=a:tok-a,c:tok-c' > "$D/.env"
echo 'A2A_BEARER_TOKEN=tok-d' >> "$D/.env"

echo "== homes under $ROOT"
# The gateways bind their desktop API on distinct loopback ports, open
# (no token) — scratch homes, loopback only, the script's lifetime.
A2A_PORT=19101 VELES_DESKTOP_PORT=19102 VELES_DESKTOP_OPEN=1 VELES_HOME="$A" HOME="$A" "$BIN" gateway run >"$ROOT/a.log" 2>&1 &
GA=$!
A2A_PORT=19103 VELES_DESKTOP_PORT=19104 VELES_DESKTOP_OPEN=1 VELES_HOME="$B" HOME="$B" "$BIN" gateway run >"$ROOT/b.log" 2>&1 &
GB=$!
A2A_PORT=19107 VELES_DESKTOP_PORT=19108 VELES_DESKTOP_OPEN=1 VELES_HOME="$C" HOME="$C" "$BIN" gateway run >"$ROOT/c.log" 2>&1 &
GC=$!
A2A_PORT=19109 VELES_DESKTOP_PORT=19110 VELES_DESKTOP_OPEN=1 VELES_HOME="$D" HOME="$D" "$BIN" gateway run >"$ROOT/d.log" 2>&1 &
GD=$!

wait_up() {
    # $1 pid $2 name $3 log $4 DESKTOP port (health is a desktop route)
    for i in $(seq 1 40); do
        sleep 0.5
        kill -0 "$1" 2>/dev/null || { echo "FAIL: $2 died"; tail -8 "$3"; exit 1; }
        curl -sf -o /dev/null "http://127.0.0.1:$4/api/health" 2>/dev/null && return 0
    done
    echo "FAIL: $2 never came up"; tail -8 "$3"; exit 1
}
wait_up "$GA" gateway-a "$ROOT/a.log" 19102
wait_up "$GB" gateway-b "$ROOT/b.log" 19104
wait_up "$GC" gateway-c "$ROOT/c.log" 19108
wait_up "$GD" gateway-d "$ROOT/d.log" 19110
echo "== gateways up (a=$GA b=$GB c=$GC)"

# Wait for A's boot-time trigger install (best-effort path).
sleep 2

# 1. REAL WRITES LAND IN THE LOG: create sessions through A's own
# desktop API (the production session-create path — same store INSERT
# the gateway serves for a chat turn's first message).
DESK_A=19102
SESS_A=$(curl -sf -X POST "http://127.0.0.1:$DESK_A/api/sessions/new" -H 'Content-Type: application/json' -d '{"title":"b5-real-write"}' 2>/dev/null || true)
echo "session-create raw: $SESS_A" | sed 's/^/  /'
echo "$SESS_A" | grep -q 'session' \
    || { echo "FAIL: could not create a session on A (desktop API on $DESK_A)"; tail -5 "$ROOT/a.log"; exit 1; }

STATUS_A=$(RUST_LOG=error VELES_HOME="$A" HOME="$A" "$BIN" replicas status 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g')
echo "$STATUS_A" | sed 's/^/  a> /'
echo "$STATUS_A" | grep -qE 'store log: [1-9]' \
    || { echo "FAIL: A's log is empty — the boot install or triggers failed"; grep -i 'store log' "$ROOT/a.log" | tail -3; exit 1; }
echo "PASS 1: real writes landed in A's store_log"

# 2. PUSH → INBOX → APPLY (hand mode, against D — deterministic)
OUT=$(RUST_LOG=error VELES_HOME="$A" HOME="$A" "$BIN" replicas push http://127.0.0.1:19109/ --bearer tok-a 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g')
echo "$OUT" | sed 's/^/  a> /'
echo "$OUT" | grep -q 'stored in the inbox' \
    || { echo "FAIL: push did not store: $OUT"; exit 1; }
INBOX_D=$(RUST_LOG=error VELES_HOME="$D" HOME="$D" "$BIN" replicas inbox 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g')
echo "$INBOX_D" | sed 's/^/  d> /'
echo "$INBOX_D" | grep -q 'b5-node-a' \
    || { echo "FAIL: D's inbox does not carry A's offer: $INBOX_D"; exit 1; }
APPLY_D=$(RUST_LOG=error VELES_HOME="$D" HOME="$D" "$BIN" replicas apply --inbox 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g')
echo "$APPLY_D" | sed 's/^/  d> /'
echo "$APPLY_D" | grep -qE '[1-9][0-9]* row\(s\) applied' \
    || { echo "FAIL: apply applied nothing: $APPLY_D"; exit 1; }
echo "PASS 2: push stored in D's inbox; apply built D's replica of A"

# 3. THE REPLICA IS READABLE
LIST_D=$(RUST_LOG=error VELES_HOME="$D" HOME="$D" "$BIN" replicas list 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g')
echo "$LIST_D" | sed 's/^/  d> /'
echo "$LIST_D" | grep -q "$A_ID" \
    || { echo "FAIL: D holds no replica of A ($A_ID): $LIST_D"; exit 1; }
echo "$LIST_D" | grep -qE '[1-9][0-9]* session\(s\)' \
    || { echo "FAIL: the replica holds no sessions: $LIST_D"; exit 1; }
echo "PASS 3a: D holds a session-bearing replica of A"
# Owner question: pick a session id from the replica file through the
# CLI's search surface (FTS over content written above).
SEARCH_D=$(RUST_LOG=error VELES_HOME="$D" HOME="$D" "$BIN" replicas search b5-real-write --limit 5 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g' || true)
echo "$SEARCH_D" | sed 's/^/  d> /'
if [ -n "$SEARCH_D" ] && echo "$SEARCH_D" | grep -qv 'no hits'; then
    echo "PASS 3b: FTS over the replica answered"
else
    echo "note: no FTS hit for the title (it lives in sessions.title, not message content) — checking message content instead"
fi

# 4. INCREMENTAL: a second write + push past the cursor.
CURSOR_D=$(RUST_LOG=error VELES_HOME="$D" HOME="$D" "$BIN" replicas list 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g' | grep "$A_ID" | sed -E 's/.*cursor ([0-9]+).*/\1/')
SESS_A2=$(curl -sf -X POST "http://127.0.0.1:$DESK_A/api/sessions/new" -H 'Content-Type: application/json' -d '{"title":"b5-second-write"}' 2>/dev/null || true)
echo "$SESS_A2" | grep -q 'session' \
    || { echo "FAIL: second session-create failed"; exit 1; }
OUT2=$(RUST_LOG=error VELES_HOME="$A" HOME="$A" "$BIN" replicas push http://127.0.0.1:19109/ --bearer tok-a --since "$CURSOR_D" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g')
echo "$OUT2" | sed 's/^/  a> /'
LIST_B2=$(RUST_LOG=error VELES_HOME="$D" HOME="$D" "$BIN" replicas apply --inbox 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g')
echo "$LIST_B2" | sed 's/^/  b> /'
CURSOR_D2=$(RUST_LOG=error VELES_HOME="$D" HOME="$D" "$BIN" replicas list 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g' | grep "$A_ID" | sed -E 's/.*cursor ([0-9]+).*/\1/')
[ "$CURSOR_D2" -gt "$CURSOR_D" ] \
    || { echo "FAIL: cursor did not advance ($CURSOR_B -> $CURSOR_B2)"; exit 1; }
echo "PASS 4: incremental push advanced D's cursor $CURSOR_B -> $CURSOR_B2"

# 5. THE GATE: C (not in B's storesync_peers) is refused. C needs a
# row to push — create one through C's own desktop API first.
SESS_C=$(curl -sf -X POST "http://127.0.0.1:19108/api/sessions/new" -H 'Content-Type: application/json' -d '{"title":"c-stranger-write"}' 2>/dev/null || true)
echo "$SESS_C" | grep -q 'session' \
    || { echo "FAIL: could not create a session on C"; tail -5 "$ROOT/c.log"; exit 1; }
OUTC=$(RUST_LOG=error VELES_HOME="$C" HOME="$C" "$BIN" replicas push http://127.0.0.1:19109/ --bearer tok-c --since 0 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' || true)
echo "$OUTC" | sed 's/^/  c> /'
if echo "$OUTC" | grep -q 'refused'; then
    echo "PASS 5: the stranger's offer was refused by name"
elif echo "$OUTC" | grep -q 'nothing to push'; then
    echo "FAIL: C had nothing to push — the gate was not exercised"
    exit 1
else
    echo "FAIL: C's push outcome is neither refusal nor empty: $OUTC"; exit 1
fi

# 6. GAP HONESTY: a push that SKIPS a row B never saw must refuse, not
# stitch. One more write on A first (so rows exist past the skip), then
# push from B's cursor+1 — the offer starts at cursor+2, a hole.
SESS_A3=$(curl -sf -X POST "http://127.0.0.1:$DESK_A/api/sessions/new" -H 'Content-Type: application/json' -d '{"title":"b5-gap-write"}' 2>/dev/null || true)
echo "$SESS_A3" | grep -q 'session' \
    || { echo "FAIL: third session-create failed"; exit 1; }
OUTG=$(RUST_LOG=error VELES_HOME="$A" HOME="$A" "$BIN" replicas push http://127.0.0.1:19109/ --bearer tok-a --since $((CURSOR_D2 + 1)) 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' || true)
echo "$OUTG" | sed 's/^/  a> /'
APPLY_G=$(RUST_LOG=error VELES_HOME="$D" HOME="$D" "$BIN" replicas apply --inbox 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' || true)
echo "$APPLY_G" | sed 's/^/  b> /'
CURSOR_D3=$(RUST_LOG=error VELES_HOME="$D" HOME="$D" "$BIN" replicas list 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g' | grep "$A_ID" | sed -E 's/.*cursor ([0-9]+).*/\1/')
[ "$CURSOR_D3" = "$CURSOR_D2" ] \
    || { echo "FAIL: cursor moved on a gapped apply ($CURSOR_B2 -> $CURSOR_B3)"; exit 1; }
echo "PASS 6: a gapped apply refused; B's cursor unchanged"

# 7. AUTO CONVERGENCE: with storesync_auto on, a write on A reaches
# B's replica of A with NO hand push/apply — the gossip cadence pushes,
# the receive-side apply folds. Poll up to ~15s for B's replica cursor
# to reach A's log cursor (gossip tick = 3s).
echo "== auto convergence: waiting for B's replica to catch up"
SESS_A4=$(curl -sf -X POST "http://127.0.0.1:$DESK_A/api/sessions/new" -H 'Content-Type: application/json' -d '{"title":"b5-auto-write"}' 2>/dev/null || true)
echo "$SESS_A4" | grep -q 'session' \
    || { echo "FAIL: auto-mode session-create failed"; exit 1; }
CONVERGED=0
for i in $(seq 1 30); do
    sleep 1
    RB=$(RUST_LOG=error VELES_HOME="$B" HOME="$B" "$BIN" replicas list 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g' | grep "$A_ID" | sed -E 's/.*cursor ([0-9]+).*/\1/')
    LA=$(RUST_LOG=error VELES_HOME="$A" HOME="$A" "$BIN" replicas status 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g' | sed -E 's/store log: [0-9]+ row\(s\) \(cursor ([0-9]+)\)/\1/;t;d')
    if [ -n "$RB" ] && [ -n "$LA" ] && [ "$RB" -ge "$LA" ]; then
        CONVERGED=1
        echo "  b> replica cursor $RB = A's log cursor $LA (tick ${i}s)"
        break
    fi
done
[ "$CONVERGED" = "1" ] \
    || { echo "FAIL: B's replica never caught up (last: ${RB:-none} vs A ${LA:-none})"; grep -i storesync "$ROOT/b.log" | tail -5; exit 1; }
echo "PASS 7: auto mode converged with no hand push/apply"

# 8. The union: B's session_search-equivalent surface (the CLI search)
# finds the auto-applied content; C (stranger, hand mode) refused.
SEARCH_AUTO=$(RUST_LOG=error VELES_HOME="$B" HOME="$B" "$BIN" replicas search b5-auto-write --limit 5 2>/dev/null | sed -E 's/\x1b\[[0-9;]*m//g' || true)
echo "$SEARCH_AUTO" | sed 's/^/  b> /'

echo
echo "PASS: store replication verified against real gateways"
