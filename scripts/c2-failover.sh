#!/bin/sh
# c2-failover.sh — the two-gateway real-binary check for C2 of
# PLAN-pool-round.md: a session taken over from a replica when its node
# dies, and handed over when that node comes back. Scratch homes under
# /dev/shm; nothing touches the live config, :8642 or :9900.
#
#   sh scripts/c2-failover.sh        (~4 minutes: a real node-lease TTL)
#
# The pool: a owns the session; b is the arbiter AND the survivor. Both
# run `[cluster] failover`, so each holds a node lease at b; both push
# their store logs to each other (storesync auto).
#
# Properties:
#   1. a HOLDS ITS NODE LEASE once it knows the arbiter (its first boot
#      cannot — it has not heard of b yet — so it is restarted once, the
#      b3a pattern).
#   2. THE REPLICA CARRIES THE SESSION — b's replica of a has the
#      history and the frozen prompt, with no hand push.
#   3. NO TAKEOVER WHILE THE OWNER LIVES — the arbiter refuses by name.
#   4. TAKEOVER AFTER DEATH — kill a; once its node lease runs out, b
#      takes the session over: the history and a's frozen prompt, byte
#      for byte (the provider's cache is keyed on it).
#   5. THE OWNER YIELDS ON RETURN — a message written on a after b's
#      copy (a partition's tail, written into a's store while it is
#      down) survives as a FORK on a; a's own copy is tombstoned to b.
#   6. ONE WRITER — b owns the session; a reports it retired.
#   7. LAZY TAKEOVER (C3) — a second session, never taken over by hand:
#      while a lives, a desktop turn on b for it is answered with where it
#      lives (409, a's name); once a is dead, the same request takes it
#      over on b by itself (USER 2026-09-29: automatic while the arbiter
#      is alive).
set -e
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/debug/veles"
ROOT=$(mktemp -d /dev/shm/c2-XXXXXX)
A="$ROOT/a"; B="$ROOT/b"
mkdir -p "$A" "$B"
A_ID="aaaaaaaa-0000-4000-8000-0000000000c2"
B_ID="bbbbbbbb-0000-4000-8000-0000000000c2"

cleanup() {
    for p in ${GA:-} ${GB:-}; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM
fail() { echo "FAIL: $*"; echo "--- a"; tail -15 "$ROOT/a.log" 2>/dev/null; echo "--- b"; tail -15 "$ROOT/b.log"; exit 1; }

printf 'id = "%s"\nname = "c2-node-a"\n' "$A_ID" > "$A/node.toml"
printf 'id = "%s"\nname = "c2-node-b"\n' "$B_ID" > "$B/node.toml"

# Ports 19141-19144 (never 8642/9900).
cat > "$A/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19141"
[a2a_agents.b]
url = "http://127.0.0.1:19143"
auth = { type = "bearer", token = "tok-a" }
[cluster]
arbiter = "bbbbbbbb"
memsync_peers = ["b"]
gossip_peers = ["b"]
storesync_peers = ["b"]
storesync_auto = true
failover = true
gossip_interval_secs = 2
stale_after_secs = 60
EOF
cat > "$B/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19143"
[a2a_agents.a]
url = "http://127.0.0.1:19141"
auth = { type = "bearer", token = "tok-b" }
[cluster]
arbiter = "bbbbbbbb"
memsync_peers = ["a"]
gossip_peers = ["a"]
storesync_peers = ["a"]
storesync_auto = true
failover = true
gossip_interval_secs = 2
stale_after_secs = 60
EOF
printf 'A2A_PEER_TOKENS=b:tok-b\nA2A_BEARER_TOKEN=tok-a\n' > "$A/.env"
printf 'A2A_PEER_TOKENS=a:tok-a\nA2A_BEARER_TOKEN=tok-b\n' > "$B/.env"

run_a() {
    A2A_PORT=19141 VELES_DESKTOP_PORT=19142 VELES_DESKTOP_OPEN=1 VELES_HOME="$A" HOME="$A" \
        "$BIN" gateway run >"$1" 2>&1 &
    GA=$!
}
run_b() {
    A2A_PORT=19143 VELES_DESKTOP_PORT=19144 VELES_DESKTOP_OPEN=1 VELES_HOME="$B" HOME="$B" \
        "$BIN" gateway run >"$ROOT/b.log" 2>&1 &
    GB=$!
}
wait_health() { # $1 pid $2 name $3 log $4 a2a port
    for i in $(seq 1 60); do
        sleep 0.25
        kill -0 "$1" 2>/dev/null || fail "gateway $2 died"
        curl -sf -o /dev/null "http://127.0.0.1:$4/.well-known/agent-card.json" 2>/dev/null && return 0
    done
    fail "gateway $2 never came up"
}
cli_a() { RUST_LOG=error VELES_HOME="$A" HOME="$A" "$BIN" "$@" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true; }
cli_b() { RUST_LOG=error VELES_HOME="$B" HOME="$B" "$BIN" "$@" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true; }
sql_a() { sqlite3 "$A/state.db" "$@"; }
sql_b() { sqlite3 "$B/state.db" "$@"; }

echo "== homes under $ROOT"
run_b; wait_health "$GB" b "$ROOT/b.log" 19143
run_a "$ROOT/a.log"; wait_health "$GA" a "$ROOT/a.log" 19141

ok=""
for i in $(seq 1 60); do
    sleep 0.5
    if cli_a nodes list | grep -q c2-node-b && cli_b nodes list | grep -q c2-node-a; then ok=1; break; fi
done
[ -n "$ok" ] || fail "gossip never converged"
echo "PASS 0: two gateways up, gossip converged"

# ── 1. a restarts knowing the arbiter, and holds its node lease ────
kill "$GA"; wait "$GA" 2>/dev/null || true; GA=""
run_a "$ROOT/a.log"; wait_health "$GA" a "$ROOT/a.log" 19141
ok=""
for i in $(seq 1 40); do
    sleep 0.5
    grep -q "holding the node lease node:$A_ID" "$ROOT/a.log" && { ok=1; break; }
done
[ -n "$ok" ] || fail "a does not hold its node lease"
echo "PASS 1: a holds node:$A_ID at the arbiter"

# ── 2. a session on a reaches b's replica on its own ───────────────
RAW=$(curl -sf -X POST "http://127.0.0.1:19142/api/sessions/new" \
    -H 'Content-Type: application/json' -d '{"title":"c2-take-me"}')
SESS=$(printf '%s' "$RAW" | sed -E 's/.*"session_id":"([^"]+)".*/\1/')
case "$SESS" in ""|"{"*) fail "no session id in: $RAW";; esac
sql_a "INSERT INTO messages(session_id, role, content) VALUES('$SESS', 'user', 'c2 hello');
       INSERT INTO messages(session_id, role, content) VALUES('$SESS', 'assistant', 'c2 hi');
       INSERT OR REPLACE INTO session_prompt(session_id, system_prompt, tools_json, format, node)
           VALUES('$SESS', 'the c2 frozen prompt', '[]', 1, '$A_ID');"
RAW2=$(curl -sf -X POST "http://127.0.0.1:19142/api/sessions/new" \
    -H 'Content-Type: application/json' -d '{"title":"c2-lazy"}')
LAZY=$(printf '%s' "$RAW2" | sed -E 's/.*"session_id":"([^"]+)".*/\1/')
case "$LAZY" in ""|"{"*) fail "no session id in: $RAW2";; esac
sql_a "INSERT INTO messages(session_id, role, content) VALUES('$LAZY', 'user', 'lazy hello');"
REP="$B/replicas/$A_ID.db"
ok=""
for i in $(seq 1 60); do
    sleep 0.5
    n=$(sqlite3 "$REP" "SELECT COUNT(*) FROM messages WHERE session_id='$SESS'" 2>/dev/null || echo 0)
    p=$(sqlite3 "$REP" "SELECT COUNT(*) FROM session_prompt WHERE session_id='$SESS'" 2>/dev/null || echo 0)
    l=$(sqlite3 "$REP" "SELECT COUNT(*) FROM messages WHERE session_id='$LAZY'" 2>/dev/null || echo 0)
    [ "$n" = 2 ] && [ "$p" = 1 ] && [ "$l" = 1 ] && { ok=1; break; }
done
[ -n "$ok" ] || fail "b's replica never got the session (messages=$n prompt=$p)"
echo "PASS 2: b's replica holds the session's history and frozen prompt"

# ── 3. no takeover while the owner lives ───────────────────────────
OUT="$(cli_b session takeover "$SESS")"
echo "$OUT" | grep -q "is alive" || fail "a takeover of a live node's session was not refused as alive: $OUT"
echo "PASS 3: refused while a lives — $(echo "$OUT" | head -1)"
chat_b() { # $1 session → the HTTP status and body of a desktop turn on b
    curl -s -o "$ROOT/chat.json" -w '%{http_code}' -X POST "http://127.0.0.1:19144/api/chat" \
        -H 'Content-Type: application/json' -d "{\"session_id\":\"$1\",\"text\":\"hi\"}"
}
CODE=$(chat_b "$LAZY")
[ "$CODE" = 409 ] || fail "a desktop turn on b for a's live session answered $CODE: $(cat "$ROOT/chat.json")"
grep -q "c2-node-a" "$ROOT/chat.json" || fail "the 409 does not name a: $(cat "$ROOT/chat.json")"
echo "PASS 3b: a desktop turn on b for a's session names a ($CODE)"

# ── 4. kill a; b takes over once a's node lease runs out ───────────
kill "$GA"; wait "$GA" 2>/dev/null || true; GA=""
# The partition's tail: written on a after b's copy was made.
sql_a "INSERT INTO messages(session_id, role, content) VALUES('$SESS', 'user', 'written while apart');"
ok=""
for i in $(seq 1 30); do
    sleep 5
    OUT="$(cli_b session takeover "$SESS")"
    echo "$OUT" | grep -q "took $SESS over" && { ok=1; break; }
    echo "$OUT" | grep -q "is alive" || fail "unexpected takeover answer: $OUT"
done
[ -n "$ok" ] || fail "b never took the session over: $OUT"
echo "$OUT" | sed 's/^/  b> /'
P="$(sql_b "SELECT system_prompt FROM session_prompt WHERE session_id='$SESS'")"
[ "$P" = "the c2 frozen prompt" ] || fail "the frozen prompt did not come along: '$P'"
N="$(sql_b "SELECT COUNT(*) FROM messages WHERE session_id='$SESS' AND injection IS NULL")"
[ "$N" = 2 ] || fail "b holds $N messages of the session, expected 2"
echo "PASS 4: b took the session over after a's lease ran out, with a's frozen prompt"

# ── 5. a comes back and yields; its tail survives as a fork ────────
run_a "$ROOT/a2.log"; wait_health "$GA" a "$ROOT/a2.log" 19141
ok=""
for i in $(seq 1 60); do
    sleep 1
    grep -q "failover: $SESS: yielded" "$ROOT/a2.log" && { ok=1; break; }
done
[ -n "$ok" ] || { cp "$ROOT/a2.log" "$ROOT/a.log"; fail "a never yielded the session"; }
grep "failover: $SESS" "$ROOT/a2.log" | sed 's/^.*failover: /  a> /'
FORK="$(sql_a "SELECT id FROM sessions WHERE title = 'c2-take-me (fork)'")"
[ -n "$FORK" ] || fail "no fork session on a"
T="$(sql_a "SELECT COUNT(*) FROM messages WHERE session_id='$FORK' AND content='written while apart'")"
[ "$T" = 1 ] || fail "the fork does not carry what a wrote while apart"
echo "PASS 5: a yielded; what it wrote while apart continues in the fork $FORK"

# ── 6. one writer ──────────────────────────────────────────────────
OUT_A="$(cli_a session owner "$SESS")"
echo "$OUT_A" | grep -q "RETIRED" || fail "a still claims the session: $OUT_A"
echo "$OUT_A" | grep -q "$B_ID" || fail "a's tombstone does not name b: $OUT_A"
OUT_B="$(cli_b session owner "$SESS")"
echo "$OUT_B" | grep -q "$B_ID" || fail "b does not own the session: $OUT_B"
echo "PASS 6: one writer — b owns $SESS, a has it retired to b"

# ── 7. lazy takeover: a turn on b takes a's session over by itself ──
# a is back, so its lease is live again: kill it once more and wait for
# the lease to lapse; nobody runs a takeover command this time.
kill "$GA"; wait "$GA" 2>/dev/null || true; GA=""
ok=""
for i in $(seq 1 30); do
    sleep 5
    CODE=$(chat_b "$LAZY")
    # Anything but 409 means b stopped pointing at a: it took the session
    # over (the turn itself may still fail — these homes have no model).
    case "$CODE" in 409) ;; *) ok=1; break;; esac
done
[ -n "$ok" ] || fail "b never took the lazy session over: $(cat "$ROOT/chat.json")"
N="$(sql_b "SELECT COUNT(*) FROM messages WHERE session_id='$LAZY' AND content='lazy hello'")"
[ "$N" = 1 ] || fail "the lazy session's history did not come along ($N)"
grep -q "failover: c2-node-a stopped answering; this node took session $LAZY over" "$ROOT/b.log" \
    || fail "b's log does not say it took $LAZY over"
echo "PASS 7: a desktop turn on b took a's session over once a was gone ($CODE)"

echo ""
echo "PASS: failover verified against real gateways (7 properties)"
exit 0
