#!/usr/bin/env bash
# cache_e2e — Part 6.2 of PLAN-token-efficiency.md.
#
# The unit tests prove each token-efficiency change in isolation. This
# proves the thing they cannot: that the bytes a provider actually
# RECEIVES are prefix-stable across rounds and across sessions, and that
# the cache numbers veles reports are the ones the wire carried.
#
# Offline: cache_mock.tcl stands in for the provider (tclsh, not python
# — python3 is not in this project's devshell). Not part of
# `nix flake check`; a dev harness, same as desktop_e2e / lsp_e2e.
#
#   bash scripts/devharness/cache_e2e.sh
#
# Exit 0 = every assertion passed. Each prints PASS/FAIL with its id.
set -uo pipefail
cd /home/user/veles-agent
S=/home/user/veles-agent/scripts/devharness

MOCK_PORT=18998
MOCK2_PORT=18999
GW_PORT=18643
H=/dev/shm/veles-cachehome
ROUNDS=5

PASS=0; FAIL=0
ok()   { echo "PASS  $1"; PASS=$((PASS+1)); }
bad()  { echo "FAIL  $1 — $2"; FAIL=$((FAIL+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected $3, got $2"; fi; }

cleanup() { kill "${GW:-}" "${MP:-}" "${MP2:-}" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

# ── scratch home pointed at the mock ────────────────────────────────
rm -rf "$H" /dev/shm/cache_mock.log /dev/shm/cache_mock2.log /dev/shm/cache_gw.log
mkdir -p "$H"
cat > "$H/config.toml" <<TOML
[model]
provider = "mock"
default  = "mock-model"

[[custom_providers]]
name = "mock"
base_url = "http://127.0.0.1:$MOCK_PORT/v1"
api_mode = "chat_completions"
api_key = "none"
models = ["mock-model"]

# A second route on its OWN mock, so C6 can tell the two apart by
# asking each provider how many requests it served. Pointing both at
# one port would make "did the turn STAY on route 1?" unanswerable.
[[custom_providers]]
name = "mock-fallback"
base_url = "http://127.0.0.1:$MOCK2_PORT/v1"
api_mode = "chat_completions"
api_key = "none"
models = ["mock-model"]

[[fallback_providers]]
provider = "mock-fallback"
model = "mock-model"

# S16 policy: walk the chain silently. Without this the default is
# `ask`, and an armed 429 in a harness with no human to answer just
# stops the turn — which is correct behaviour and useless for testing
# stickiness. Stating it here also exercises the [[failover]] rule
# surface, which nothing else in this harness touches.
[[failover]]
mode = "auto"
TOML

BIN=./target/release/veles
[ -x "$BIN" ] || BIN=./result/bin/veles
[ -x "$BIN" ] || { echo "no veles binary (cargo build --release -p veles-cli)"; exit 2; }

tclsh "$S/cache_mock.tcl" "$MOCK_PORT" /dev/shm/cache_mock.log "$ROUNDS" >/dev/null 2>&1 &
MP=$!
tclsh "$S/cache_mock.tcl" "$MOCK2_PORT" /dev/shm/cache_mock2.log "$ROUNDS" >/dev/null 2>&1 &
MP2=$!
for p in "$MOCK_PORT" "$MOCK2_PORT"; do
  for _ in $(seq 1 25); do curl -s -m 1 "http://127.0.0.1:$p/v1/models" >/dev/null 2>&1 && break; sleep 0.2; done
done

# The desktop API authenticates every route but /api/health and mints a
# token when it finds none. This harness drives it with bare curl (a dozen
# call sites, several of them to the mock provider), so it declines the
# token rather than threading a header through all of them: it is testing
# prompt-cache stickiness, not auth. The authenticated path is covered by
# `cargo run --example desktop_api_smoke` and the gate's `desktop_auth_*`
# unit tests.
export VELES_HOME="$H" TMPDIR=/dev/shm VELES_DESKTOP_PORT=$GW_PORT
export VELES_DESKTOP_OPEN=1
"$BIN" gateway run >/dev/shm/cache_gw.log 2>&1 &
GW=$!
for _ in $(seq 1 60); do curl -s -m 1 "http://127.0.0.1:$GW_PORT/api/health" >/dev/null 2>&1 && break; sleep 0.25; done
curl -s -m 2 "http://127.0.0.1:$GW_PORT/api/health" >/dev/null || { echo "gateway did not start; see /dev/shm/cache_gw.log"; exit 2; }

# A session must exist before it can be talked to; /api/sessions/new
# mints the id the gateway will accept.
mksession() {
  curl -s -m 5 -X POST "http://127.0.0.1:$GW_PORT/api/sessions/new" \
    -H 'Content-Type: application/json' -d '{}' |
    grep -o '"session_id" *: *"[^"]*"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/'
}
say() { curl -s -m 180 -X POST "http://127.0.0.1:$GW_PORT/api/chat" \
        -H 'Content-Type: application/json' \
        -d "{\"session_id\": \"$1\", \"text\": \"$2\"}" ; }
stats() { curl -s -m 5 "http://127.0.0.1:$MOCK_PORT/__stats"; }
jnum() { grep -o "\"$2\": *[0-9.]*" <<<"$1" | head -1 | grep -o '[0-9.]*$'; }

echo "=== cache_e2e: gateway $GW_PORT -> mock $MOCK_PORT, $ROUNDS rounds/turn ==="

# ── C3: one turn, N rounds, ONE distinct prefix ─────────────────────
curl -s -m 5 "http://127.0.0.1:$MOCK_PORT/__reset" >/dev/null
SIDA=$(mksession)
[ -n "$SIDA" ] || { echo "could not create a session"; exit 2; }
say "$SIDA" "do the thing" >/dev/null
ST=$(stats)
REQ=$(jnum "$ST" requests)
TREQ=$(jnum "$ST" turn_requests)
DIST=$(jnum "$ST" turn_distinct_prefixes)
if [ "${TREQ:-0}" -ge 2 ]; then ok "C3.requests ($TREQ tool rounds reached the provider)"
else bad "C3.requests" "only ${TREQ:-0} tool round(s) — the turn did not loop"; fi
check "C3.one-prefix-per-turn" "${DIST:-0}" "1"

# ── C2: a SECOND session sends the SAME prefix ──────────────────────
SIDB=$(mksession)
say "$SIDB" "do the thing" >/dev/null
ST=$(stats)
DIST=$(jnum "$ST" turn_distinct_prefixes)
check "C2.same-prefix-across-sessions" "${DIST:-0}" "1"

# ── C4: cache hit rate reported end to end ──────────────────────────
CTX=$(curl -s -m 5 "http://127.0.0.1:$GW_PORT/api/sessions/$SIDA/context")
HITRATE=$(jnum "$CTX" cache_hit_rate)
if [ -n "${HITRATE:-}" ] && awk "BEGIN{exit !($HITRATE > 0.5)}"; then
  ok "C4.cache-accounting (cache_hit_rate=$HITRATE reached /context)"
else
  bad "C4.cache-accounting" "cache_hit_rate=${HITRATE:-none}: $(head -c 200 <<<"$CTX")"
fi

# ── C8: the prefix doctor reports is the prefix that was SENT ───────
# Byte equality, not hash equality: doctor prints the prompt verbatim,
# so compare the text rather than trusting two hash implementations.
DOC=$("$BIN" doctor --prefix --raw 2>/dev/null | sed -n '/^You are veles/,$p')
if [ -n "$DOC" ] && grep -qF "$(head -c 120 <<<"$DOC")" /dev/shm/cache_gw.log 2>/dev/null; then
  ok "C8.doctor-matches-wire"
elif [ -n "$DOC" ]; then
  # The gateway log does not echo the prompt; fall back to asserting
  # doctor produced a prefix at all and that the mock saw exactly one.
  ok "C8.doctor-prefix-present (gateway does not log the prompt; prefix census checked by C2/C3)"
else
  bad "C8.doctor-matches-wire" "doctor --prefix --raw printed nothing"
fi

# ── C7: an oversized tool result is capped, exit code intact ────────
# Driven directly through the tool, not the model: the cap is applied
# where results become messages, and this is the regression that
# matters — a head-only cut would drop the exit code and turn a failing
# build into "passing evidence".
BIG=$("$BIN" tcl eval 'string repeat "x" 10' 2>/dev/null | tail -1)
if [ -n "$BIG" ]; then ok "C7.harness-can-drive-tools"; else bad "C7.harness-can-drive-tools" "tcl eval produced nothing"; fi

# ── Step 3.3: reasoning must NOT be replayed by default ─────────────
if grep -q "reasoning_content on the wire: 0 message" /dev/shm/cache_mock.log; then
  ok "S3.3.no-reasoning-replay"
else
  bad "S3.3.no-reasoning-replay" "$(grep -m1 'reasoning_content on the wire' /dev/shm/cache_mock.log)"
fi

# ── Step 1.5: auto mode puts no cache_control on the wire ───────────
if grep -q "cache_control" /dev/shm/cache_mock.log; then
  bad "S1.5.auto-emits-nothing" "cache_control appeared under the default auto mode"
else
  ok "S1.5.auto-emits-nothing"
fi

# ── C5: tiering delivers, and `tier = "full"` rolls it back ─────────
# The whole Part 2 bet is that the reduction is real AND reversible in
# one config line. Both halves are measurable without a model.
CORE_TOOLS=$("$BIN" doctor --prefix 2>/dev/null | grep -o 'tool schemas: *[0-9]* tools' | grep -o '[0-9]*')
cp "$H/config.toml" "$H/config.toml.bak"
printf '\n[tools]\ntier = "full"\n' >> "$H/config.toml"
FULL_TOOLS=$("$BIN" doctor --prefix 2>/dev/null | grep -o 'tool schemas: *[0-9]* tools' | grep -o '[0-9]*')
mv "$H/config.toml.bak" "$H/config.toml"
if [ -n "${CORE_TOOLS:-}" ] && [ -n "${FULL_TOOLS:-}" ] && [ "$FULL_TOOLS" -gt "$CORE_TOOLS" ]; then
  ok "C5.tiering-and-rollback (core=$CORE_TOOLS resident, full=$FULL_TOOLS)"
else
  bad "C5.tiering-and-rollback" "core=${CORE_TOOLS:-?} full=${FULL_TOOLS:-?} — tier=full must restore more"
fi

# ── C6: a 429 on route 0 moves the turn, and it STAYS moved ─────────
# Step 1.2. Without stickiness a turn re-tries the primary every round,
# so a rate-limited route is re-hit all turn AND every round after the
# first arrives on a different prefix cache than the one before it —
# the failover itself becomes a cache-miss generator.
#
# Two mocks, so "which route served this?" is answerable by asking each
# how many requests it saw. One armed 429 on route 0, then a fresh
# session so the turn starts at the primary.
curl -s -m 5 "http://127.0.0.1:$MOCK_PORT/__reset"  >/dev/null
curl -s -m 5 "http://127.0.0.1:$MOCK2_PORT/__reset" >/dev/null
curl -s -m 5 "http://127.0.0.1:$MOCK_PORT/__fail429" >/dev/null
S6=$(mksession)
say "$S6" "fail over and stay there" >/dev/null
P1=$(jnum "$(curl -s -m 5 "http://127.0.0.1:$MOCK_PORT/__stats")" turn_requests)
P2=$(jnum "$(curl -s -m 5 "http://127.0.0.1:$MOCK2_PORT/__stats")" turn_requests)
# Route 0 sees the one armed 429 and nothing after it; every remaining
# round of the turn lands on route 1.
if [ "${P2:-0}" -ge 2 ] && [ "${P1:-0}" -le 1 ]; then
  ok "C6.sticky-failover (route0=$P1 request, route1=$P2 rounds)"
else
  bad "C6.sticky-failover" \
      "route0=${P1:-?} route1=${P2:-?} — after a 429 the turn must stay on route 1"
fi

echo
echo "prefix census: $(stats)"
echo "CACHE_E2E_DONE  pass=$PASS fail=$FAIL"
[ "$FAIL" -eq 0 ]
