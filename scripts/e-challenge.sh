#!/bin/sh
# e-challenge.sh — a bot challenge put in front of a person and solved by
# them (PLAN-pool-round.md Part E), end to end:
#
#   a scripted model → a real gateway → the real agent-browser → a site
#   behind a "verify you are human" gate → the relay's question (parked,
#   with its screenshot) → the relay PAGE a chat user would open → a click
#   on the picture → the gate opens → the model is handed the real page.
#
#   sh scripts/e-challenge.sh        (~30 s; needs agent-browser)
#
# Scratch home under /dev/shm; its own agent-browser session, so the
# owner's browser is never touched; nothing on :8642 or :9900.
#
# Properties:
#   1. The gate is recognised on the node running the browser, and the
#      question reaches the session: a `challenge` ask with the page as a
#      PNG and a link to the relay page.
#   2. The relay page is served for that ask (no bearer token), and a
#      click on it — `click 200 140`, the gate's button — is the answer.
#   3. The gate opens, and what the model is handed is the page past it,
#      said to have been solved by the user.
#   4. The page is gone once the relay ends.
set -e
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO/target/debug/veles"
ROOT=$(mktemp -d /dev/shm/e-challenge-XXXXXX)
H="$ROOT/home"
mkdir -p "$H"
export AGENT_BROWSER_SESSION="veles-e2e-$$"

cleanup() {
    for p in ${GW:-} ${MP:-} ${SP:-}; do kill "$p" 2>/dev/null || true; done
    agent-browser close >/dev/null 2>&1 || true
    sleep 0.3
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM
fail() {
    echo "FAIL: $*"
    echo "--- gateway"; tail -20 "$ROOT/gw.log" 2>/dev/null || true
    echo "--- mock"; tail -5 "$ROOT/mock.log" 2>/dev/null | cut -c1-300 || true
    exit 1
}
command -v agent-browser >/dev/null || { echo "SKIP: no agent-browser on PATH"; exit 0; }

cat > "$H/config.toml" <<'EOF'
[model]
provider = "mock"
default  = "mock-model"

[[custom_providers]]
name = "mock"
base_url = "http://127.0.0.1:19915/v1"
api_mode = "chat_completions"
api_key = "none"
models = ["mock-model"]

[tools]
tier = "full"
EOF

tclsh "$REPO/scripts/devharness/challenge_site.tcl" 19913 &
SP=$!
tclsh "$REPO/scripts/devharness/challenge_mock.tcl" 19915 "$ROOT/mock.log" "http://127.0.0.1:19913/" &
MP=$!
VELES_HOME="$H" HOME="$H" VELES_DESKTOP_PORT=19912 VELES_DESKTOP_OPEN=1 \
    "$BIN" gateway run > "$ROOT/gw.log" 2>&1 &
GW=$!
API=http://127.0.0.1:19912
for i in $(seq 1 60); do
    sleep 0.25
    kill -0 "$GW" 2>/dev/null || fail "the gateway died"
    curl -sf -o /dev/null "$API/api/health" && break
done
curl -sf -o /dev/null "$API/api/health" || fail "the gateway never came up"

SID=$(curl -s -X POST "$API/api/sessions/new" -H 'Content-Type: application/json' \
    -d '{"title":"e2e challenge"}' | jq -r '.session_id // empty')
[ -n "$SID" ] || fail "no session was created"
CHAT=$(curl -s -w ' %{http_code}' -X POST "$API/api/chat" -H 'Content-Type: application/json' \
    -d "{\"session_id\":\"$SID\",\"text\":\"open the shop and read it\"}")
case "$CHAT" in *' 202') ;; *) fail "the turn was not started: $CHAT" ;; esac

# ── 1. the question reaches the session ─────────────────────────────
ASK=""
for i in $(seq 1 120); do
    ASK=$(curl -s "$API/api/asks" | jq -c '[.asks[] | select(.kind == "challenge")][0] // empty')
    [ -n "$ASK" ] && break
    sleep 0.5
done
[ -n "$ASK" ] || fail "no challenge ask appeared: $(curl -s "$API/api/asks" | cut -c1-300)"
printf '%s' "$ASK" | jq -r .image | base64 -d 2>/dev/null | head -c 4 | grep -q 'PNG' \
    || fail "the ask does not carry the page as a PNG"
printf '%s' "$ASK" | jq -r .prompt | grep -q '127.0.0.1:19913' \
    || fail "the question does not name the site: $(printf '%s' "$ASK" | jq -r .prompt)"
LINK=$(printf '%s' "$ASK" | jq -r .detail | grep -o "$API/challenge/[0-9a-f]*" | head -1)
[ -n "$LINK" ] || fail "no relay link in the question: $(printf '%s' "$ASK" | jq -r .detail)"
echo "PASS 1: the gate was recognised and the question reached the session, with the page and a link"

# ── 2. the relay page, and a click on it ────────────────────────────
curl -sf "$LINK" | grep -q 'id="shot"' || fail "the relay page is not served"
curl -sf "$LINK/state" | jq -e '.round == 1' > /dev/null || fail "the page's state is wrong"
curl -sf "$LINK/shot" | head -c 4 | grep -q PNG || fail "the page's picture is not the PNG"
curl -sf -X POST "$LINK/answer" -H 'Content-Type: application/json' \
    -d '{"text":"click 200 140"}' | jq -e '.ok' > /dev/null || fail "the click was not taken"
echo "PASS 2: the relay page served the question, and a click on it answered"

# ── 3. the model is handed the page past the gate ───────────────────
ok=""
for i in $(seq 1 80); do
    if grep -q 'the secret content' "$ROOT/mock.log"; then ok=1; break; fi
    sleep 0.5
done
[ -n "$ok" ] || fail "the model never saw the page past the gate"
grep -q 'challenge was solved by the user' "$ROOT/mock.log" \
    || fail "the tool result does not say the user solved it"
echo "PASS 3: the gate opened, and the model was handed the real page"

# ── 4. the page ends with its relay ─────────────────────────────────
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$LINK/state")
[ "$CODE" = 404 ] || fail "the relay page outlived its relay ($CODE)"
echo "PASS 4: the relay page is gone once the relay ends"

echo "PASS: a bot challenge was put to a person and solved by them (4 properties)"
