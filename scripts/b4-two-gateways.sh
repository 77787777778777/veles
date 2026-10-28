#!/bin/sh
# b4-two-gateways.sh — the two-gateway real-binary check for the B4
# round: work placement (delegate_task backend) over the real A2A
# wire. Scratch homes under /dev/shm; nothing touches the live config
# or the live :9900 (the b3a round's discipline).
#
#   sh scripts/b4-two-gateways.sh
#
# What is REAL: both gateway processes (this build's binary), the A2A
# wire between them, the remote-task interceptor, gossip carrying the
# B4 load/served columns, and the placement resolver reading A's live
# registry. What is STUBBED: only the leaf child's MODEL call — B's
# gateway spawns VELES_BINARY (a script that records its argv and
# prints a canned summary) instead of an LLM turn. Everything between
# "A's tool call" and "B's child process" is production code.
#
# Properties checked:
#   1. REMOTE EXECUTION — delegate_task backend=<B's name> from A runs
#      the child ON B (goal in B's child argv log, NOT in A's), and
#      the verdict names B's node id + both child sessions.
#   2. METRICS ON THE WIRE — A's registry shows B with load=<0-100>
#      and served=<n>: the B4 columns propagate through real gossip.
#   3. GPU CONSTRAINT — backend=gpu lands on B even though A is the
#      machine with a real RTX (A's caps opt out with gpu = false —
#      the override that B1's review pinned).
#   4. ANY = LEAST-LOADED, SELF INCLUDED — backend=any runs the child
#      on exactly ONE of the two nodes, and the verdict says which.
#   5. UNREACHABLE = HONEST ERROR — with B's gateway dead (but not yet
#      stale), backend=<B> from A errors naming the node; the work
#      does NOT silently run locally.
set -e
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/debug/veles"
ROOT=$(mktemp -d /dev/shm/b4-XXXXXX)
A="$ROOT/a"; B="$ROOT/b"
mkdir -p "$A" "$B"

cleanup() {
    for p in ${GA:-} ${GB:-}; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

A_ID="aaaaaaaa-0000-4000-8000-000000000001"
B_ID="bbbbbbbb-0000-4000-8000-000000000002"

# Pre-seeded identities (the b3a pattern): registry rows come from
# node.toml, and backend=<name> resolution + gossip need stable ids.
cat > "$A/node.toml" <<EOF
id = "$A_ID"
name = "b4-node-a"
EOF
cat > "$B/node.toml" <<EOF
id = "$B_ID"
name = "b4-node-b"
EOF

# The fake child: records the argv it was handed (the goal rides in
# the prompt after -q) and prints a canned summary. Distinct output
# per side so "ran here, not there" is decidable from the verdict too.
for side in a b; do
    dir=$(eval echo \$$([ $side = a ] && echo A || echo B))
    cat > "$dir/fake-child" <<EOF
#!/bin/sh
for arg in "\$@"; do printf '%s\n' "\$arg" >> "$dir/child-argv.log"; done
case "\$*" in *b4-slow*) sleep 2 ;; esac
echo "ran-on-$side"
EOF
    chmod +x "$dir/fake-child"
done

# Configs: the b3a two-gateway shape (loopback A2A, distinct ports,
# cross-authenticated peer names) plus the B4 bits — [node] endpoint
# (how remote_target_for dials: endpoint match against [a2a_agents]),
# A's gpu opt-out (this laptop HAS a real RTX 3080; the probe would
# advertise it and `gpu` placement could legitimately pick A — the
# caps override is the deterministic off-switch B1 pinned), and B's
# declared fake GPU.
cat > "$A/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19899"
[node.caps]
gpu = false
[a2a_agents.b]
url = "http://127.0.0.1:19901"
auth = { type = "bearer", token = "tok-a" }
[cluster]
memsync_peers = ["b"]
gossip_peers = ["b"]
gossip_interval_secs = 3
stale_after_secs = 60
EOF
cat > "$B/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19901"
[node.caps]
gpu = { name = "FakeTest GPU", memory_total_mb = 1024 }
[a2a_agents.a]
url = "http://127.0.0.1:19899"
auth = { type = "bearer", token = "tok-b" }
[cluster]
memsync_peers = ["a"]
gossip_peers = ["a"]
# Remote work has its own allowlist (2026-09-28 review): a sync peer
# is not a task peer.
task_peers = ["a"]
gossip_interval_secs = 3
stale_after_secs = 60
EOF

# Cross-auth: each side maps the OTHER's presented token to its peer
# name (the memsync_peers spelling). Per-process A2A_PORT beats the
# ambient A2A_PORT=9900 on this host (the b3a port discipline).
echo 'A2A_PEER_TOKENS=b:tok-b' > "$A/.env"
echo 'A2A_BEARER_TOKEN=tok-a' >> "$A/.env"
echo 'A2A_PEER_TOKENS=a:tok-a' > "$B/.env"
echo 'A2A_BEARER_TOKEN=tok-b' >> "$B/.env"

echo "== homes under $ROOT"

A2A_PORT=19899 VELES_HOME="$A" HOME="$A" VELES_BINARY="$A/fake-child" \
    "$BIN" gateway run >"$ROOT/a.log" 2>&1 &
GA=$!
A2A_PORT=19901 VELES_HOME="$B" HOME="$B" VELES_BINARY="$B/fake-child" \
    "$BIN" gateway run >"$ROOT/b.log" 2>&1 &
GB=$!

echo "== gateways started (a=$GA b=$GB); waiting for gossip convergence"

# A must see B's row (with its endpoint) before any placement can
# resolve — gossip runs on the 3s cadence.
#
# The CLI prints tracing lines to STDOUT with ANSI escapes; strip the
# escapes first so the timestamp filter (`^20`) can see them, then
# drop the log lines. RUST_LOG=error keeps most of them from existing.
nodes_a() {
    RUST_LOG=error VELES_HOME="$A" HOME="$A" "$BIN" nodes list 2>/dev/null \
        | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true
}
ok=""
for i in $(seq 1 30); do
    sleep 0.5
    kill -0 "$GA" 2>/dev/null || { echo "FAIL: gateway a died"; tail -8 "$ROOT/a.log"; exit 1; }
    kill -0 "$GB" 2>/dev/null || { echo "FAIL: gateway b died"; tail -8 "$ROOT/b.log"; exit 1; }
    ANODES="$(nodes_a)"
    if echo "$ANODES" | grep -q "b4-node-b"; then ok=1; break; fi
done
[ -n "$ok" ] || {
    echo "FAIL: a never learned b over gossip"
    echo "-- a's registry:"; nodes_a
    echo "-- a log tail:"; tail -8 "$ROOT/a.log"
    exit 1
}
echo "PASS 0: a's registry carries b (gossip converged)"
echo "$ANODES" | sed 's/^/  a> /'

# ── 1. remote execution: the child runs ON B ────────────────────────
# The tool call runs through `veles tool --full` (the escalation to
# the session registry; base_registry deliberately has no
# delegate_task). RUST_LOG=error keeps the CLI's tracing off the
# stdout the assertions grep; the sed strips any ANSI that survives.
tool_a() {
    RUST_LOG=error VELES_HOME="$A" HOME="$A" VELES_BINARY="$A/fake-child" \
        "$BIN" tool --name delegate_task --full --args "$1" 2>/dev/null \
        | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true
}
OUT="$(tool_a '{"goal":"b4-wire-goal-one","backend":"b4-node-b"}')"
echo "$OUT" | grep -q '"status":"ok"' \
    || { echo "FAIL: remote delegate verdict not ok: $OUT"; exit 1; }
echo "$OUT" | grep -q '"summary":"ran-on-b"' \
    || { echo "FAIL: the summary is not B's child's: $OUT"; exit 1; }
echo "$OUT" | grep -q "$B_ID" \
    || { echo "FAIL: the verdict does not name B's node id: $OUT"; exit 1; }
echo "$OUT" | grep -q 'remote_child_session' \
    || { echo "FAIL: no remote child session in the verdict: $OUT"; exit 1; }
sleep 0.3
grep -q "b4-wire-goal-one" "$B/child-argv.log" \
    || { echo "FAIL: the goal never reached B's child process"; cat "$B/child-argv.log" 2>/dev/null; exit 1; }
if [ -f "$A/child-argv.log" ] && grep -q "b4-wire-goal-one" "$A/child-argv.log"; then
    echo "FAIL: the work ALSO ran locally on A — placement leaked"
    exit 1
fi
echo "PASS 1: backend=b4-node-b ran the child ON B (goal in B's argv log, summary ran-on-b, A did not run it)"

# ── 2. the B4 metrics propagate over the real wire ─────────────────
# B's heartbeat samples its real load; gossip carries load1 +
# sessions_served to A's registry. B registers at boot with load NULL
# and only fills it on its first heartbeat tick, so A can learn B's
# ROW (PASS 0) before B's LOAD arrives — poll rather than read once.
ok=""
for i in $(seq 1 30); do
    ANODES="$(nodes_a)"
    BROW="$(echo "$ANODES" | grep "b4-node-b" || true)"
    if echo "$BROW" | grep -qE 'load=[0-9]+' && echo "$BROW" | grep -qE 'served=[0-9]+'; then
        ok=1; break
    fi
    sleep 0.5
done
[ -n "$ok" ] || {
    echo "FAIL: b's row on a never carried both load= and served=:"
    echo "$ANODES"
    exit 1
}
echo "PASS 2: load + served ride the real gossip wire"
echo "$ANODES" | sed 's/^/  a> /'

# ── 3. the gpu constraint picks the GPU node ───────────────────────
OUT="$(tool_a '{"goal":"b4-wire-goal-gpu","backend":"gpu"}')"
echo "$OUT" | grep -q '"summary":"ran-on-b"' \
    || { echo "FAIL: backend=gpu did not land on B (the only GPU node): $OUT"; exit 1; }
echo "$OUT" | grep -q 'FakeTest GPU' \
    || { echo "FAIL: the gpu verdict does not name the card: $OUT"; exit 1; }
grep -q "b4-wire-goal-gpu" "$B/child-argv.log" \
    || { echo "FAIL: the gpu goal never reached B's child"; exit 1; }
echo "PASS 3: backend=gpu resolved to B and its card, over the wire"

# ── 4. any = least-loaded, exactly one node runs it ────────────────
OUT="$(tool_a '{"goal":"b4-wire-goal-any","backend":"any"}')"
echo "$OUT" | grep -q '"status":"ok"' \
    || { echo "FAIL: backend=any verdict not ok: $OUT"; exit 1; }
sleep 0.3
ON_A=0; ON_B=0
[ -f "$A/child-argv.log" ] && grep -q "b4-wire-goal-any" "$A/child-argv.log" && ON_A=1
grep -q "b4-wire-goal-any" "$B/child-argv.log" 2>/dev/null && ON_B=1
[ $((ON_A + ON_B)) -eq 1 ] \
    || { echo "FAIL: backend=any ran the child on $((ON_A + ON_B)) nodes (want exactly 1)"; exit 1; }
if [ "$ON_A" = 1 ]; then
    echo "$OUT" | grep -q '"summary":"ran-on-a"' \
        || { echo "FAIL: any landed local but the verdict is not the local child's: $OUT"; exit 1; }
    echo "PASS 4: backend=any chose THIS node (self included, as designed)"
else
    echo "$OUT" | grep -q '"summary":"ran-on-b"' \
        || { echo "FAIL: any landed on b but the verdict disagrees: $OUT"; exit 1; }
    echo "PASS 4: backend=any chose b (least-loaded at call time)"
fi

# ── 6. a remote batch runs in PARALLEL ─────────────────────────────
# (2026-09-28 review: B4 awaited batch members one by one, so three
# children ran in series.) Each slow child sleeps 2 s on B; in series
# the batch takes >= 6 s.
T0=$(date +%s.%N)
OUT="$(tool_a '{"backend":"b4-node-b","tasks":[{"goal":"b4-slow-1"},{"goal":"b4-slow-2"},{"goal":"b4-slow-3"}]}')"
T1=$(date +%s.%N)
TOOK=$(awk "BEGIN { printf \"%.2f\", $T1 - $T0 }")
[ "$(echo "$OUT" | grep -o '"summary":"ran-on-b"' | wc -l)" -eq 3 ] \
    || { echo "FAIL: the remote batch did not run 3 children on B: $OUT"; exit 1; }
awk "BEGIN { exit !($TOOK < 5) }" \
    || { echo "FAIL: three 2 s remote children took ${TOOK}s — they ran in series"; exit 1; }
echo "PASS 6: a remote batch of three 2 s children took ${TOOK}s (parallel)"

# ── 7. a SYNC peer is not a TASK peer ──────────────────────────────
# (USER, 2026-09-28: remote work has its own allowlist.) B reads its
# config live per ask; drop task_peers, ask, restore.
cp "$B/config.toml" "$B/config.toml.keep"
grep -v '^task_peers' "$B/config.toml.keep" > "$B/config.toml"
OUT="$(tool_a '{"goal":"b4-wire-goal-syncpeer","backend":"b4-node-b"}')"
mv -f "$B/config.toml.keep" "$B/config.toml"
echo "$OUT" | grep -q 'task_peers' \
    || { echo "FAIL: a memsync-only peer was not refused by name: $OUT"; exit 1; }
grep -q "b4-wire-goal-syncpeer" "$B/child-argv.log" \
    && { echo "FAIL: B ran work for a peer outside task_peers"; exit 1; }
echo "PASS 7: a peer in memsync_peers but not task_peers is refused, nothing runs"

# ── 5. unreachable = honest error, never a silent local run ────────
kill "$GB"; GB=""; sleep 0.5
OUT="$(tool_a '{"goal":"b4-wire-goal-dead","backend":"b4-node-b"}')"
echo "$OUT" | grep -q '^error:' \
    || { echo "FAIL: a dead backend must be an error, got: $OUT"; exit 1; }
echo "$OUT" | grep -q 'b4-node-b' \
    || { echo "FAIL: the error does not name the node: $OUT"; exit 1; }
if [ -f "$A/child-argv.log" ] && grep -q "b4-wire-goal-dead" "$A/child-argv.log"; then
    echo "FAIL: the unreachable node's work silently ran locally"
    exit 1
fi
echo "PASS 5: dead backend = honest error naming the node, no local fallback"
echo "  error was: $(echo "$OUT" | head -c 200)"

echo ""
echo "PASS: two-gateway B4 verification complete (7 properties)"
exit 0
