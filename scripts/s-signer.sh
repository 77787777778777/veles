#!/bin/sh
# s-signer.sh — the headless ticket signer (PLAN-pool-round.md Part S)
# end to end: two real gateways, a real `veles signer run`, and a stub
# Telegram Bot API where api.telegram.org would be.
#
#   sh scripts/s-signer.sh        (~1 minute)
#
# Scratch homes under /dev/shm; nothing touches the live config, :8642
# or :9900.
#
# The shape is the two-node pool: A (the phone) re-homes vision_analyze
# to B (the laptop); B runs it only with a ticket (vision is above the
# User level a peer gets unasked); A asks the headless signer, whose
# key B trusts. The owner answers on the signer's own bot. (Where the
# signer runs does not matter: any trusted key signs, ticket.rs.)
#
# What is REAL: both gateways, the A2A wire, gossip, B's decide/verify/
# spend, A's dispatch re-homing the call and asking for a ticket, the
# signer process (its key, its HTTP endpoint, its Telegram client). What
# is STUBBED: Telegram's servers (scripts/devharness/tg_stub.tcl).
#
# Properties:
#   1. THE OWNER'S YES SIGNS — the stub's owner presses Sign (after a
#      stranger pressed it first, which must sign nothing); A's call runs
#      on B with the ticket: A's answer is B's wrapper, and B's audit log
#      records the run as ticketed, signed by the signer's key. (B has no
#      vision model, so the tool's own answer is an error — which is
#      still proof it RAN there, at the level the ticket raised it to.)
#   2. THE OWNER'S NO DECLINES — with the stub in decline mode, the call
#      is refused on A and B runs nothing.
#   3. AN UNKNOWN GATEWAY IS REFUSED — a wrong bearer token gets 401.
set -e
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO/target/debug/veles"
ROOT=$(mktemp -d /dev/shm/s-signer-XXXXXX)
A="$ROOT/a"; B="$ROOT/b"; S="$ROOT/signer"
mkdir -p "$A" "$B" "$S"

cleanup() {
    for p in ${GA:-} ${GB:-} ${SP:-} ${TP:-}; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM
fail() {
    echo "FAIL: $*"
    echo "--- a"; tail -15 "$ROOT/a.log" 2>/dev/null || true
    echo "--- b"; tail -15 "$ROOT/b.log" 2>/dev/null || true
    echo "--- signer"; tail -15 "$ROOT/signer.log" 2>/dev/null || true
    echo "--- tg"; tail -15 "$ROOT/tg.log" 2>/dev/null || true
    exit 1
}

A_ID="aaaaaaaa-0000-4000-8000-0000000005a0"
B_ID="bbbbbbbb-0000-4000-8000-0000000005b0"
printf 'id = "%s"\nname = "s-node-a"\n' "$A_ID" > "$A/node.toml"
printf 'id = "%s"\nname = "s-node-b"\n' "$B_ID" > "$B/node.toml"

# The signer's key: in its own directory, never in a gateway home.
# `ticket keygen` writes into the key dir beside the config dir; --dir
# for the signer points straight at it.
KEYOUT="$(printf 's5-passphrase\ns5-passphrase\n' \
    | VELES_HOME="$S/home" HOME="$S" "$BIN" ticket keygen laptop 2>&1)" \
    || fail "keygen failed: $KEYOUT"
PUB=$(printf '%s' "$KEYOUT" | grep -o 'ed25519:[A-Za-z0-9_-]*' | head -1)
[ -n "$PUB" ] || fail "keygen printed no public key: $KEYOUT"
KEYDIR="$S/veles-signer"
[ -f "$KEYDIR/laptop.key" ] || fail "no key file in $KEYDIR: $(ls -R "$S")"

cat > "$A/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19899"
[a2a_agents.b]
url = "http://127.0.0.1:19901"
auth = { type = "bearer", token = "tok-a" }
[cluster]
gossip_peers = ["b"]
memsync_peers = ["b"]
task_peers = ["b"]
gossip_interval_secs = 2
stale_after_secs = 60
signers = ["http://127.0.0.1:19905"]
[tools.home]
"vision_*" = "s-node-b"
EOF
cat > "$B/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19901"
[a2a_agents.a]
url = "http://127.0.0.1:19899"
auth = { type = "bearer", token = "tok-b" }
[cluster]
gossip_peers = ["a"]
memsync_peers = ["a"]
task_peers = ["a"]
gossip_interval_secs = 2
stale_after_secs = 60
[cluster.ticket_signers.laptop]
key = "$PUB"
EOF
printf 'A2A_PEER_TOKENS=b:tok-b\nA2A_BEARER_TOKEN=tok-a\nVELES_SIGNER_TOKEN=gw-a-token\n' > "$A/.env"
printf 'A2A_PEER_TOKENS=a:tok-a\nA2A_BEARER_TOKEN=tok-b\n' > "$B/.env"

OWNER=4242
start_signer() { # $1 tg mode
    cat > "$KEYDIR/signer.toml" <<EOF
key = "laptop"
listen = "127.0.0.1:19905"
wait_secs = 30
[telegram]
owner_id = $OWNER
api_url = "http://127.0.0.1:19907"
EOF
    printf 'SIGNER_GATEWAY_TOKENS=phone:gw-a-token\nSIGNER_TELEGRAM_TOKEN=STUBTOKEN\n' > "$KEYDIR/signer.env"
    chmod 600 "$KEYDIR/signer.env"
    tclsh "$REPO/scripts/devharness/tg_stub.tcl" 19907 "$OWNER" "$1" "$ROOT/tg.log" &
    TP=$!
    printf 's5-passphrase\n' > "$ROOT/pass"
    SIGNER_PASSPHRASE_FILE="$ROOT/pass" HOME="$S" "$BIN" signer run --dir "$KEYDIR" \
        > "$ROOT/signer.log" 2>&1 &
    SP=$!
    for i in $(seq 1 40); do
        sleep 0.25
        kill -0 "$SP" 2>/dev/null || fail "the signer died"
        curl -sf -o /dev/null http://127.0.0.1:19905/v1/health && return 0
    done
    fail "the signer never came up"
}
stop_signer() {
    kill "$SP" "$TP" 2>/dev/null || true
    wait "$SP" "$TP" 2>/dev/null || true
    SP=""; TP=""
}
run_b() {
    A2A_PORT=19901 VELES_DESKTOP_PORT=19903 VELES_DESKTOP_OPEN=1 VELES_HOME="$B" HOME="$B" \
        "$BIN" gateway run > "$ROOT/b.log" 2>&1 &
    GB=$!
}
run_a() {
    A2A_PORT=19899 VELES_DESKTOP_PORT=19902 VELES_DESKTOP_OPEN=1 VELES_HOME="$A" HOME="$A" \
        "$BIN" gateway run > "$ROOT/a.log" 2>&1 &
    GA=$!
}
wait_health() { # $1 pid $2 name $3 a2a port
    for i in $(seq 1 60); do
        sleep 0.25
        kill -0 "$1" 2>/dev/null || fail "gateway $2 died"
        curl -sf -o /dev/null "http://127.0.0.1:$3/.well-known/agent-card.json" \
            2>/dev/null && return 0
    done
    fail "gateway $2 never came up"
}
cli_a() {
    RUST_LOG=error VELES_HOME="$A" HOME="$A" "$BIN" "$@" 2>/dev/null \
        | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true
}
cli_b() {
    RUST_LOG=error VELES_HOME="$B" HOME="$B" "$BIN" "$@" 2>/dev/null \
        | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true
}
vision_args() { # $1 a marker for this call
    printf '{"image_url":"https://example.invalid/%s.png","question":"what is this"}' "$1"
}
ticketed_runs() { # B's audit lines for a ticketed vision_analyze run
    grep '"event":"remote_tool"' "$B/a2a_audit.jsonl" 2>/dev/null \
        | grep '"tool":"vision_analyze"' | grep -c 'signed by laptop' || true
}

echo "== homes under $ROOT"
run_b; wait_health "$GB" b 19901
run_a; wait_health "$GA" a 19899
ok=""
for i in $(seq 1 60); do
    sleep 0.5
    if cli_a nodes list | grep -q s-node-b && cli_b nodes list | grep -q s-node-a; then
        ok=1; break
    fi
done
[ -n "$ok" ] || fail "gossip never converged"
echo "PASS 0: two gateways up, gossip converged"

# ── 1. the owner's yes signs ────────────────────────────────────────
start_signer approve
OUT="$(cli_a tool --name vision_analyze --args "$(vision_args s5-yes)")"
echo "$OUT" | grep -q '\[vision_analyze ran on node s-node-b\]' \
    || fail "the ticketed call did not run on B: $OUT"
[ "$(ticketed_runs)" = 1 ] \
    || fail "B's audit has no ticketed run signed by laptop: $(tail -3 "$B/a2a_audit.jsonl" 2>/dev/null)"
grep -q 'sendMessage .*"chat_id":\s*'"$OWNER" "$ROOT/tg.log" \
    || fail "the owner was not asked on the signer's bot: $(cat "$ROOT/tg.log")"
grep -q 'sendMessage .*vision_analyze' "$ROOT/tg.log" \
    || fail "the question does not show the call"
grep -q 'answerCallbackQuery .*not yours to answer' "$ROOT/tg.log" \
    || fail "the stranger's press was not refused: $(cat "$ROOT/tg.log")"
grep -q 'editMessageText .*signed' "$ROOT/tg.log" \
    || fail "the question was not marked signed"
grep -q 'signed, approved by telegram:'"$OWNER" "$ROOT/signer.log" \
    || fail "the signer did not log the owner's approval"
echo "PASS 1: the owner's Sign (not a stranger's) ran A's call on B"
stop_signer

# ── 2. the owner's no declines ──────────────────────────────────────
: > "$ROOT/tg.log"
start_signer decline
OUT="$(cli_a tool --name vision_analyze --args "$(vision_args s5-no)")"
[ "$(ticketed_runs)" = 1 ] || fail "a declined call ran on B: $OUT"
echo "$OUT" | grep -q 'ran on node s-node-b\]' && fail "a declined call ran on B: $OUT"
echo "$OUT" | grep -qi 'ticket' || fail "the refusal does not say a ticket was needed: $OUT"
grep -q 'declined by telegram:'"$OWNER" "$ROOT/signer.log" \
    || fail "the signer did not log the decline"
echo "PASS 2: the owner's Decline refused the call; B ran nothing"
stop_signer

# ── 3. an unknown gateway ───────────────────────────────────────────
start_signer approve
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:19905/v1/sign \
    -H 'Authorization: Bearer wrong' -H 'Content-Type: application/json' \
    -d '{"request":"veles-ticket-request:1.e30"}')
[ "$CODE" = 401 ] || fail "a wrong token got $CODE"
echo "PASS 3: a gateway with the wrong token is refused"
stop_signer

echo "PASS: the headless signer verified end to end (3 properties)"
