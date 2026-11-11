#!/bin/sh
# compute-aware-two-gateways.sh — the two-gateway real-binary check for
# owner ask 306787.b (docs/pool.md "How `any` and `gpu` choose";
# docs/issues/open-items/pool-platform.md "Compute-aware division of
# work"). Scratch homes under /dev/shm, loopback ports 19931-19934 (or
# the four from CW_PORT_BASE); nothing touches the live config, the live
# :9900 or the desktop :8642.
#
#   sh scripts/compute-aware-two-gateways.sh            # target/debug/veles
#   VELES_BIN=/path/to/veles sh scripts/compute-aware-two-gateways.sh
#   CW_PORT_BASE=19140 sh scripts/compute-aware-two-gateways.sh   # ports 19140-19143
#
# What is REAL: both gateway processes, their heartbeats (this
# machine's cores and /proc/pressure/io), the A2A wire, gossip, the
# placement work table, the boot reconciliation and the live auth
# probe. What is STUBBED: only the leaf child's model call (VELES_BINARY
# is a script that sleeps for a "slow" goal and prints a canned
# summary), as in b4-two-gateways.sh. Needs jq.
#
# Properties checked:
#   1. METRICS ON THE WIRE — A's registry shows B's core count and IO
#      pressure (or `?` where this kernel has no PSI), from B's own
#      heartbeat through real gossip.
#   2. CROSS-NODE WORK — while a child A placed runs on B, B's heartbeat
#      reports it (children=1, by origin A), A's registry carries that,
#      and A's `nodes` tool counts it ONCE (B's report, minus nothing
#      unreported from A).
#   3. RESTART RECONCILIATION — B's gateway is killed (SIGKILL: a crash,
#      or a stop that outlived its timeout; a SIGTERM lets the running
#      child finish first) while the child runs, and started again: the
#      boot reconciliation keeps a child whose process is still there
#      (or drops it if it is gone) and logs which; B's count follows the
#      child, and returns to 0 after it.
#   4. LIVE AUTH PROBE — with A's token for B made wrong, A's `nodes`
#      tool reports the probe refused (HTTP 401) and not routable, and
#      backend=any runs on A. Restored, the probe passes again.
#   5. EARLY PUBLISH — B restarts with a 60 s gossip interval, so its
#      first regular heartbeat is a minute after its boot. A child A
#      places on B, and that child's end, still reach A's registry
#      within seconds: a change to a node's own work is published at
#      once (placement::work_changed), not at the next tick.
set -e
BIN="${VELES_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/veles}"
[ -x "$BIN" ] || { echo "FAIL: no veles binary at $BIN"; exit 1; }
BASE="${CW_PORT_BASE:-19931}"
PA=$BASE; PA_DESK=$((BASE + 1)); PB=$((BASE + 2)); PB_DESK=$((BASE + 3))
command -v jq >/dev/null || { echo "FAIL: jq is needed"; exit 1; }
ROOT=$(mktemp -d /dev/shm/cw-XXXXXX)
A="$ROOT/a"; B="$ROOT/b"
mkdir -p "$A" "$B"

cleanup() {
    for p in ${GA:-} ${GB:-} ${TA:-}; do kill "$p" 2>/dev/null || true; done
    pkill -f "$ROOT/b/fake-child" 2>/dev/null || true
    sleep 0.3
    if [ -n "${KEEP_ROOT:-}" ]; then echo "kept $ROOT (logs a.log, b.log)"; else rm -rf "$ROOT"; fi
}
trap cleanup EXIT INT TERM

A_ID="aaaaaaaa-0000-4000-8000-0000000003a1"
B_ID="bbbbbbbb-0000-4000-8000-0000000003b2"
printf 'id = "%s"\nname = "cw-node-a"\n' "$A_ID" > "$A/node.toml"
printf 'id = "%s"\nname = "cw-node-b"\n' "$B_ID" > "$B/node.toml"

for side in a b; do
    dir="$ROOT/$side"
    cat > "$dir/fake-child" <<EOF
#!/bin/sh
for arg in "\$@"; do printf '%s\n' "\$arg" >> "$dir/child-argv.log"; done
# A killed slow child takes its sleep with it: an orphaned sleep would
# hold the gateway's pipes open, and the child would count until it ends.
case "\$*" in *cw-slow*)
    sleep 25 & SL=\$!
    trap 'kill \$SL 2>/dev/null; exit 143' TERM
    wait \$SL ;;
esac
echo "ran-on-$side"
EOF
    chmod +x "$dir/fake-child"
done

cat > "$A/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:$PA"
[node.caps]
gpu = false
[a2a_agents.b]
url = "http://127.0.0.1:$PB"
auth = { type = "bearer", token = "tok-a" }
[cluster]
# A gossip exchange is refused unless the caller is a memsync peer
# (gossip::receive_gossip); memsync_auto stays off, so no memory moves.
memsync_peers = ["b"]
gossip_peers = ["b"]
gossip_interval_secs = 3
stale_after_secs = 60
EOF
cat > "$B/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:$PB"
[node.caps]
gpu = false
[a2a_agents.a]
url = "http://127.0.0.1:$PA"
auth = { type = "bearer", token = "tok-b" }
[cluster]
memsync_peers = ["a"]
gossip_peers = ["a"]
task_peers = ["a"]
gossip_interval_secs = 3
stale_after_secs = 60
EOF
echo 'A2A_PEER_TOKENS=b:tok-b' > "$A/.env"
echo 'A2A_BEARER_TOKEN=tok-a' >> "$A/.env"
echo 'A2A_PEER_TOKENS=a:tok-a' > "$B/.env"
echo 'A2A_BEARER_TOKEN=tok-b' >> "$B/.env"

start_a() {
    A2A_PORT=$PA VELES_DESKTOP_PORT=$PA_DESK VELES_HOME="$A" HOME="$A" \
        VELES_BINARY="$A/fake-child" "$BIN" gateway run >>"$ROOT/a.log" 2>&1 &
    GA=$!
}
start_b() {
    A2A_PORT=$PB VELES_DESKTOP_PORT=$PB_DESK VELES_HOME="$B" HOME="$B" \
        VELES_BINARY="$B/fake-child" "$BIN" gateway run >>"$ROOT/b.log" 2>&1 &
    GB=$!
}
clean() { sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true; }
nodes_on() { RUST_LOG=error VELES_HOME="$1" HOME="$1" "$BIN" nodes list 2>/dev/null | clean; }
tool_a() {
    RUST_LOG=error VELES_HOME="$A" HOME="$A" VELES_BINARY="$A/fake-child" \
        "$BIN" tool --name "$1" --full --args "$2" 2>/dev/null | clean
}
# Field $1 of the node named $2 in a `nodes` tool reply on stdin.
field() { jq -r --arg k "$1" --arg n "$2" '.nodes[] | select(.name == $n) | .[$k] | tostring'; }
wait_for() { # <tries> <command...>: retry every 0.5 s
    n=$1; shift
    while [ "$n" -gt 0 ]; do
        if "$@"; then return 0; fi
        n=$((n - 1)); sleep 0.5
    done
    return 1
}

start_a; start_b
echo "== homes under $ROOT (a=$GA b=$GB)"

# ── 1. cores and IO pressure ride the wire ─────────────────────────
b_row_on_a() { nodes_on "$A" | grep "cw-node-b" || true; }
has_cores() { b_row_on_a | grep -qE 'cores=[0-9]+ io=([0-9]+%|\?)'; }
wait_for 60 has_cores || {
    echo "FAIL: a never learned b's cores/io over gossip"
    nodes_on "$A"; tail -8 "$ROOT/a.log"; tail -8 "$ROOT/b.log"; exit 1
}
echo "PASS 1: b's cores and IO pressure reach a over the real wire"
echo "  a> $(b_row_on_a)"
CORES=$(nproc 2>/dev/null || echo "?")
b_row_on_a | grep -q "cores=$CORES " \
    || echo "  note: b reports $(b_row_on_a | grep -oE 'cores=[0-9]+'), nproc says $CORES"
if [ -r /proc/pressure/io ]; then
    b_row_on_a | grep -qE 'io=[0-9]+%' \
        || { echo "FAIL: this kernel has PSI but b reported io=?"; exit 1; }
fi

# ── 2. a child a placed on b counts once, as b reports it ──────────
tool_a delegate_task '{"goal":"cw-slow-one","backend":"cw-node-b"}' >"$ROOT/slow.out" &
TA=$!
b_busy_on_a() { b_row_on_a | grep -q 'children=1'; }
wait_for 40 b_busy_on_a || {
    echo "FAIL: b's work count never reached a while its child ran"
    nodes_on "$A"; nodes_on "$B"; cat "$ROOT/slow.out"; tail -8 "$ROOT/b.log"; exit 1
}
grep -q cw-slow-one "$B/child-argv.log" \
    || { echo "FAIL: the slow child is not running on b"; exit 1; }
INV=$(tool_a nodes '{}')
RUN=$(echo "$INV" | field children_running cw-node-b)
UNREP=$(echo "$INV" | field placed_from_here_unreported cw-node-b)
COUNTED=$(echo "$INV" | field children_counted cw-node-b)
ROUTE=$(echo "$INV" | field route cw-node-b)
if [ "$RUN" != 1 ] || [ "$UNREP" != 0 ] || [ "$COUNTED" != 1 ]; then
    echo "FAIL: a's nodes tool counted b's child as running=$RUN unreported=$UNREP counted=$COUNTED"
    echo "$INV"; exit 1
fi
echo "PASS 2: b reports the child a placed (children=1); a's ranking counts it once"
echo "  a nodes tool: running=$RUN unreported_from_a=$UNREP counted=$COUNTED route=$ROUTE"

# ── 3. b restarts while the child runs ─────────────────────────────
# SIGKILL: a SIGTERM drains (the runtime waits for the child's blocking
# task), so the child would end, and its row with it, before the exit.
kill -9 "$GB"; wait "$GB" 2>/dev/null || true; GB=""
sleep 0.5
if pgrep -f "$B/fake-child" >/dev/null; then ALIVE=1; else ALIVE=0; fi
start_b
booted() { grep -q 'placement work from before this start' "$ROOT/b.log"; }
wait_for 40 booted || {
    echo "FAIL: b's boot reconciliation logged nothing about the earlier work"
    tail -15 "$ROOT/b.log"; exit 1
}
LINE=$(sed -E 's/\x1b\[[0-9;]*m//g' "$ROOT/b.log" | grep 'placement work from before this start' | tail -1)
b_own() { nodes_on "$B" | grep "cw-node-b" || true; }
if [ "$ALIVE" = 1 ]; then
    echo "$LINE" | grep -qE ': 1 still running' \
        || { echo "FAIL: the child outlived b's gateway but the reconcile did not keep it: $LINE"; exit 1; }
    own_busy() { b_own | grep -q 'children=1'; }
    wait_for 20 own_busy || { echo "FAIL: restarted b does not count its running child: $(b_own)"; exit 1; }
    echo "PASS 3a: the child outlived b's gateway; the restarted b keeps counting it"
else
    echo "$LINE" | grep -qE '[1-9][0-9]* gone' \
        || { echo "FAIL: the child died with b's gateway but the reconcile kept its row: $LINE"; exit 1; }
    echo "PASS 3a: the child died with b's gateway; the restarted b dropped its row"
fi
echo "  b log: $(echo "$LINE" | sed 's/.*cluster: //')"
pkill -f "$B/fake-child" 2>/dev/null || true
own_idle() { b_own | grep -q 'children=0'; }
wait_for 40 own_idle || { echo "FAIL: b still counts a child that is gone: $(b_own)"; exit 1; }
echo "PASS 3b: once the child is gone, b's count returns to 0 ($(b_own | grep -oE 'children=[0-9+]+'))"
kill "$TA" 2>/dev/null || true; TA=""

# ── 4. the live auth probe ─────────────────────────────────────────
b_live_on_a() { tool_a nodes '{}' | field route cw-node-b | grep -q 'auth probe passed'; }
wait_for 40 b_live_on_a || { echo "FAIL: a's probe to the restarted b never passed"; tool_a nodes '{}'; exit 1; }
cp "$A/config.toml" "$A/config.toml.keep"
sed 's/token = "tok-a"/token = "tok-rotated"/' "$A/config.toml.keep" > "$A/config.toml"
INV=$(tool_a nodes '{}')
ROUTE=$(echo "$INV" | field route cw-node-b)
ROUTABLE=$(echo "$INV" | field routable cw-node-b)
if ! echo "$ROUTE" | grep -q '401' || [ "$ROUTABLE" != false ]; then
    echo "FAIL: a wrong token did not show as a refused probe: route=$ROUTE routable=$ROUTABLE"; exit 1
fi
OUT=$(tool_a delegate_task '{"goal":"cw-any-refused","backend":"any"}')
echo "$OUT" | grep -q '"summary":"ran-on-a"' \
    || { echo "FAIL: with b refusing a's token, any did not run here: $OUT"; exit 1; }
if grep -q cw-any-refused "$B/child-argv.log" 2>/dev/null; then
    echo "FAIL: b ran work a's probe said it could not reach"; exit 1
fi
mv -f "$A/config.toml.keep" "$A/config.toml"
b_live_on_a || { echo "FAIL: the restored token's probe does not pass"; exit 1; }
echo "PASS 4: a refused token is a refused probe ($ROUTE); any ran on a; restored, it passes"

# ── 5. a change to b's work is published at once ───────────────────
# Without the early publish, b's row would say children=0 until its
# first regular tick, 60 s after this boot (the gateway skips the
# immediate first tick), however often a pulls from it.
BOOTS=$(grep -c 'registered node' "$ROOT/b.log" || true)
kill "$GB"; wait "$GB" 2>/dev/null || true; GB=""
sed 's/^gossip_interval_secs = 3$/gossip_interval_secs = 60/' "$B/config.toml" > "$B/config.toml.new"
mv -f "$B/config.toml.new" "$B/config.toml"
grep -q '^gossip_interval_secs = 60$' "$B/config.toml" || { echo "FAIL: b's interval not set"; exit 1; }
start_b
T0=$(date +%s)
rebooted() { [ "$(grep -c 'registered node' "$ROOT/b.log" || true)" -gt "$BOOTS" ]; }
wait_for 40 rebooted || { echo "FAIL: b did not come back"; tail -8 "$ROOT/b.log"; exit 1; }
wait_for 40 b_live_on_a || { echo "FAIL: a's probe to b failed after b's restart"; exit 1; }
b_row_on_a | grep -q 'children=0' || { echo "FAIL: b is not idle before 5: $(b_row_on_a)"; exit 1; }
tool_a delegate_task '{"goal":"cw-slow-two","backend":"cw-node-b"}' >"$ROOT/slow2.out" &
TA=$!
started() { grep -q cw-slow-two "$B/child-argv.log" 2>/dev/null; }
wait_for 40 started || { echo "FAIL: the second slow child never started on b"; cat "$ROOT/slow2.out"; exit 1; }
T1=$(date +%s)
wait_for 24 b_busy_on_a || {
    echo "FAIL: b's child did not reach a's registry within 12 s of starting"
    echo "  (b's regular heartbeat is 60 s after its boot; the early publish should carry it)"
    nodes_on "$A"; tail -8 "$ROOT/b.log"; exit 1
}
T2=$(date +%s)
[ $((T2 - T0)) -lt 55 ] || {
    echo "FAIL: too late to tell an early publish from b's regular tick ($((T2 - T0)) s after boot)"
    exit 1
}
echo "PASS 5a: b's child reached a $((T2 - T1)) s after it started ($((T2 - T0)) s after b's boot, interval 60 s)"
pkill -f "$B/fake-child" 2>/dev/null || true
T3=$(date +%s)
b_idle_on_a() { b_row_on_a | grep -q 'children=0'; }
wait_for 24 b_idle_on_a || {
    echo "FAIL: the child's end did not reach a's registry within 12 s: $(b_row_on_a)"
    tail -8 "$ROOT/b.log"; exit 1
}
T4=$(date +%s)
[ $((T4 - T0)) -lt 58 ] || {
    echo "FAIL: too late to tell an early publish from b's regular tick ($((T4 - T0)) s after boot)"
    exit 1
}
echo "PASS 5b: the child's end reached a $((T4 - T3)) s after it ($((T4 - T0)) s after b's boot)"
kill "$TA" 2>/dev/null || true; TA=""

echo ""
echo "PASS: compute-aware two-gateway check complete (5 properties)"
exit 0
