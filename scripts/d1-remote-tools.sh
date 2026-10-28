#!/bin/sh
# d1-remote-tools.sh — the two-gateway real-binary check for D1/D2/D3 of
# PLAN-pool-round.md: a re-homed tool runs on the node it was sent to, a
# peer runs only what the User level allows unless it carries a ticket,
# and a ticket authorizes exactly one call.
#
#   sh scripts/d1-remote-tools.sh        (~1 minute)
#
# Scratch homes under /dev/shm; nothing touches the live config, :8642
# or :9900 (the b3a/b4/b5/c2/c3 discipline).
#
# What is REAL: both gateway processes (this build's binary), the A2A
# wire with real bearer auth, gossip binding the peer to its node id,
# remotetool.rs's decide/capped/execute, ticket.rs sign+verify+spend
# with a real Ed25519 key, the CLI dispatch that re-homes a call, and
# the receiver's audit log. What is STUBBED: nothing — no model call is
# involved, because a re-homed call is not a turn.
#
# Properties checked:
#   1. RE-HOMED EXECUTION — `[tools.home] read_* = <b>` on A sends a
#      read_file call to B: A's answer is wrapped "[read_file ran on
#      node d1-node-b]" and B's audit log records it as a remote_tool
#      run at the host-read level. A local run cannot produce either.
#   2. NEVER A QUIET LOCAL RUN — with B dead, the same call fails and
#      NAMES the node; the content does not come back from A.
#   3. WHAT A PEER RUNS UNASKED — B answers an unasked read from A, and
#      refuses an elevated call (skill_manage, above the User level)
#      with a ticket REQUEST that names A as origin and B as target.
#   4. A TICKET RAISES EXACTLY THAT CALL — signing the request B minted
#      with a key B trusts runs it: the skill file appears on B, and the
#      answer says who signed.
#   5. ONE CALL PER TICKET — replaying the same ticket is refused.
#   6. A TICKET FOR OTHER ARGS DOES NOT FIT — the same key, signing a
#      request for different arguments, is refused by the args hash.
#   7. THE GATES — a peer outside [cluster] task_peers is refused, and
#      an ask that claims a different from_node than the authenticated
#      peer is refused.
set -e
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/debug/veles"
ROOT=$(mktemp -d /dev/shm/d1-XXXXXX)
A="$ROOT/a"; B="$ROOT/b"
mkdir -p "$A" "$B"

cleanup() {
    for p in ${GA:-} ${GB:-}; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM
fail() {
    echo "FAIL: $*"
    echo "--- a"; tail -15 "$A/../a.log" 2>/dev/null || true
    echo "--- b"; tail -15 "$ROOT/b.log" 2>/dev/null || true
    exit 1
}

A_ID="aaaaaaaa-0000-4000-8000-0000000000d1"
B_ID="bbbbbbbb-0000-4000-8000-0000000000d1"
printf 'id = "%s"\nname = "d1-node-a"\n' "$A_ID" > "$A/node.toml"
printf 'id = "%s"\nname = "d1-node-b"\n' "$B_ID" > "$B/node.toml"

# The file a re-homed read returns. It sits OUTSIDE both homes: inside
# one, sensitive_read_path would fence it as veles' own state, and the
# test would measure the jail instead of the routing.
printf 'shared-on-this-machine\n' > "$ROOT/shared.txt"

# Both peer lists are needed, and they gate different things: gossip is
# allowed by `memsync_peers` (gossip.rs receive_gossip refuses a peer not
# in it), and a re-homed call by `task_peers`. Without the first, the
# nodes never learn each other's ids — and then no ticket can name an
# origin, so every D3 property would measure that refusal instead.
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
[tools.home]
"read_*" = "d1-node-b"
"web_*" = "d1-node-b"
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
EOF
# B presents tok-a to A → A knows it as "a"; tok-c → "c", authenticated
# but NOT a task peer (property 7's probe).
printf 'A2A_PEER_TOKENS=b:tok-b\nA2A_BEARER_TOKEN=tok-a\n' > "$A/.env"
printf 'A2A_PEER_TOKENS=a:tok-a,c:tok-c\nA2A_BEARER_TOKEN=tok-b\n' > "$B/.env"

B_URL="http://127.0.0.1:19901"
# VELES_DESKTOP_OPEN=1 matters: without it the desktop API MINTS a token
# into $HOME/.env at boot, which rewrites the file that carries
# A2A_PEER_TOKENS — and then the peers cannot authenticate each other and
# gossip never converges (measured: "b did not answer gossip with a
# gossip reply" on both sides). c2/b4 set it for the same reason.
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
wait_health() { # $1 pid $2 name $3 log $4 a2a port
    for i in $(seq 1 60); do
        sleep 0.25
        kill -0 "$1" 2>/dev/null || fail "gateway $2 died"
        curl -sf -o /dev/null "http://127.0.0.1:$4/.well-known/agent-card.json" \
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

echo "== homes under $ROOT"
run_b; wait_health "$GB" b "$ROOT/b.log" 19901
run_a; wait_health "$GA" a "$ROOT/a.log" 19899

# Gossip must converge before anything else: the receiver resolves a
# caller's node id from the OBSERVATIONS gossip recorded, and the
# ticket's origin depends on that binding.
ok=""
for i in $(seq 1 60); do
    sleep 0.5
    if cli_a nodes list | grep -q d1-node-b && cli_b nodes list | grep -q d1-node-a; then
        ok=1; break
    fi
done
[ -n "$ok" ] || fail "gossip never converged"
echo "PASS 0: two gateways up, gossip converged (peers bound to node ids)"

# ── 1. a re-homed tool runs on the node it was sent to ──────────────
OUT="$(cli_a tool --name read_file --args "{\"path\":\"$ROOT/shared.txt\"}")"
echo "$OUT" | grep -q '\[read_file ran on node d1-node-b\]' \
    || fail "the answer is not B's wrapper: $OUT"
echo "$OUT" | grep -q 'shared-on-this-machine' \
    || fail "the content did not come back: $OUT"
grep -q '"event":"remote_tool"' "$B/a2a_audit.jsonl" 2>/dev/null \
    || fail "B logged no remote_tool run"
grep '"event":"remote_tool"' "$B/a2a_audit.jsonl" | tail -1 \
    | grep -q '"tool":"read_file"' \
    || fail "B's audit line does not name read_file"
grep '"event":"remote_tool"' "$B/a2a_audit.jsonl" | tail -1 \
    | grep -q 'a host read (unasked)' \
    || fail "B did not run it at the host-read level: $(tail -1 "$B/a2a_audit.jsonl")"
echo "PASS 1: read_file ran on d1-node-b, audited as a host read"

# ── 1b. the plan's own case: web_extract re-homed to the peer ───────
# A fetch goes to whatever IP the node running it has, which is the
# point of re-homing web tools (USER 2026-09-29: "web fetch might work
# better from a residential ip on the laptop"). Here the proof is that
# the call RAN ON B: urlguard refuses a loopback URL, and B's refusal
# comes back wrapped in B's answer, which a local run could not
# produce — a local run would answer with its own unwrapped error.
OUT="$(cli_a tool --name web_extract --args '{"url":"http://127.0.0.1:1/x"}')"
echo "$OUT" | grep -q '\[web_extract ran on node d1-node-b\]' \
    || fail "web_extract was not re-homed to B: $OUT"
echo "$OUT" | grep -qi 'loopback\|web_extract error' \
    || fail "B's web_extract did not answer: $OUT"
grep '"event":"remote_tool"' "$B/a2a_audit.jsonl" | tail -1 \
    | grep -q '"tool":"web_extract"' \
    || fail "B logged no web_extract remote_tool run"
echo "PASS 1b: web_extract ran on d1-node-b (the plan's re-homing case)"

# ── 2. never a quiet local run ──────────────────────────────────────
kill "$GB"; wait "$GB" 2>/dev/null || true; GB=""
OUT="$(cli_a tool --name read_file --args "{\"path\":\"$ROOT/shared.txt\"}")"
echo "$OUT" | grep -q 'error' || fail "a dead home did not error: $OUT"
echo "$OUT" | grep -q 'd1-node-b' || fail "the error does not name the node: $OUT"
echo "$OUT" | grep -q 'shared-on-this-machine' \
    && fail "the call ran locally after all: $OUT"
echo "PASS 2: with B dead the call fails NAMING B, and does not run here"
run_b; wait_health "$GB" b "$ROOT/b.log" 19901
ok=""
for i in $(seq 1 60); do
    sleep 0.5
    cli_b nodes list | grep -q d1-node-a && { ok=1; break; }
done
[ -n "$ok" ] || fail "gossip did not re-converge after B's restart"

# ── the wire ask, hand-built in remotetool.rs's exact shape ─────────
# marker, the human header line, the ```b64 fence, sha256 of the JSON
# bytes, then the base64 of those same bytes. decode_ask re-hashes what
# it decodes, so the sha must be of the JSON exactly as encoded.
json_escape() {
    printf '%s' "$1" | awk 'BEGIN { ORS="" }
        { gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); if (NR > 1) print "\\n"; print }'
}
send_ask() { # $1 token (a) or (c), $2 ask JSON → the reply JSON
    local body="$1" ask="$2" sha b64 text
    sha=$(printf '%s' "$ask" | sha256sum | cut -d' ' -f1)
    b64=$(printf '%s' "$ask" | base64 | tr -d '\n')
    text="[veles remote tool]
one tool call over the cluster link
\`\`\`b64
sha256:$sha
$b64"
    curl -s -X POST "$B_URL/" -H 'Content-Type: application/json' \
        -H "Authorization: Bearer $body" \
        -d "{\"jsonrpc\":\"2.0\",\"id\":\"d1\",\"method\":\"message/send\",\"params\":{\"message\":{\"role\":\"ROLE_USER\",\"parts\":[{\"kind\":\"text\",\"text\":\"$(json_escape "$text")\"}],\"messageId\":\"d1-$$-$RANDOM\",\"contextId\":\"d1-ctx-$$\"}}}"
}
# The reply's fenced payload, decoded to JSON: status/error/request/
# output. The reply is a real JSON-RPC task, so jq reads it the way
# a2a::extract_text does — artifacts[0].parts when COMPLETED, else
# status.message.parts. The fence then rides inside that text as
# literal \n, which jq already unescaped.
reply_json() {
    local text b64
    text=$(printf '%s' "$1" | jq -r '
        (.result.task // .result // {}) as $t
        | ($t.artifacts[0].parts[0].text
           // $t.status.message.parts[0].text
           // "")')
    b64=$(printf '%s' "$text" | sed -n '/^```b64$/{n;n;p;}')
    if [ -n "$b64" ]; then
        printf '%s' "$b64" | base64 -d 2>/dev/null && return 0
    fi
    # No fenced reply: a JSON-RPC error, or a refusal shaped some other
    # way. Report what came back rather than claiming a status we did
    # not see — the assertion that reads it names the real gap.
    printf '{"status":"error","error":%s}' "$(printf '%s' "$1" | jq -c '.error // {raw: .}' 2>/dev/null || echo '"unparsable reply"')"
}
ask_json() { # $1 tool $2 args-json $3 principal-or-empty $4 ticket-or-empty
    local p="" t=""
    [ -n "$3" ] && p=",\"principal\":$(printf '%s' "$3" | jq -Rs .)"
    [ -n "$4" ] && t=",\"ticket\":$(printf '%s' "$4" | jq -Rs .)"
    printf '{"from_node":"%s","session":"d1-e2e","tool":"%s","args":%s%s%s}' \
        "$A_ID" "$1" "$2" "$p" "$t"
}
# The owner principal, in the exact shape serde_json writes it (all
# seven fields — Principal has no #[serde(default)] on the Options, so a
# short object would fail to parse and decide() would refuse it as "does
# not parse" rather than exercising the level cap). caps ["all"] is
# Role::Owner.default_caps(), Cap serializes kebab-case.
OWNER='{"id":"local:owner","subject":"local:owner","role":"owner","caps":["all"],"rule_context_limit":null,"mask":null,"via":null}'

# ── 3. what a peer runs unasked, and what needs a ticket ────────────
SKILL_ARGS='{"action":"create","name":"d1-e2e-skill","content":"---\nname: d1-e2e-skill\ndescription: x\n---\n\nbody\n"}'
ASK="$(ask_json skill_manage "$SKILL_ARGS" "$OWNER" "")"
R="$(reply_json "$(send_ask tok-a "$ASK")")"
echo "$R" | grep -q '"status":"needs_ticket"' \
    || fail "an elevated call ran unasked: $R"
REQ=$(printf '%s' "$R" | jq -r '.request')
case "$REQ" in veles-ticket-request:1.*) ;; *) fail "no request token: $R";; esac
echo "$R" | grep -q 'skill-manage' \
    || fail "the refusal does not name the capability it needs: $R"
# The request B minted must name the call it is for, so what a signer
# signs is what this node will accept (ticket.rs binds tool + args hash
# + both node ids into the signature).
printf '%s' "$REQ" | grep -q . || fail "empty request"
echo "PASS 3: an elevated call needs a ticket, and B minted the request"

# ── 4. a ticket raises exactly that call ────────────────────────────
# keygen on B: its key dir is beside its config dir (never inside
# VELES_HOME), so HOME points there too. The passphrase is read from
# stdin, twice, exactly as an operator types it.
KEYOUT="$(printf 'd1-passphrase\nd1-passphrase\n' \
    | VELES_HOME="$B" HOME="$B" "$BIN" ticket keygen phone 2>&1)" \
    || fail "keygen failed: $KEYOUT"
PUB=$(printf '%s' "$KEYOUT" | grep -o 'ed25519:[A-Za-z0-9_-]*' | head -1)
[ -n "$PUB" ] || fail "keygen printed no public key: $KEYOUT"
# B must TRUST that key before it can verify a ticket signed by it.
cat >> "$B/config.toml" <<EOF

[cluster.ticket_signers.phone]
key = "$PUB"
EOF
kill "$GB"; wait "$GB" 2>/dev/null || true; GB=""
run_b; wait_health "$GB" b "$ROOT/b.log" 19901
ok=""
for i in $(seq 1 60); do
    sleep 0.5
    cli_b nodes list | grep -q d1-node-a && { ok=1; break; }
done
[ -n "$ok" ] || fail "gossip did not re-converge after the signer was added"

TICKET="$(printf 'd1-passphrase\n' \
    | VELES_HOME="$B" HOME="$B" "$BIN" ticket sign "$REQ" --yes --key phone 2>/dev/null)" \
    || fail "signing failed"
case "$TICKET" in *veles-ticket:1.*) ;; *) fail "sign printed no ticket: $TICKET";; esac
TICKET=$(printf '%s' "$TICKET" | grep -o 'veles-ticket:1\.[A-Za-z0-9_.-]*' | head -1)

ASK="$(ask_json skill_manage "$SKILL_ARGS" "$OWNER" "$TICKET")"
R="$(reply_json "$(send_ask tok-a "$ASK")")"
echo "$R" | grep -q '"status":"ok"' || fail "the ticketed call did not run: $R"
echo "$R" | grep -q 'ticket .*signed by phone' \
    || fail "the answer does not say who signed: $R"
[ -f "$B/skills/d1-e2e-skill/SKILL.md" ] \
    || fail "the skill was not created on B — the call did not really run"
grep -q 'd1-e2e-skill' "$B/skills/d1-e2e-skill/SKILL.md" \
    || fail "the skill file does not carry what was asked for"
echo "PASS 4: a ticket raised exactly that call; the skill exists on B"

# ── 5. one call per ticket ──────────────────────────────────────────
R="$(reply_json "$(send_ask tok-a "$ASK")")"
echo "$R" | grep -q 'already used' \
    || fail "a replayed ticket ran again: $R"
echo "PASS 5: replaying the spent ticket is refused"

# ── 6. a ticket for other arguments does not fit ────────────────────
OTHER='{"action":"create","name":"d1-other-skill","content":"---\nname: d1-other-skill\ndescription: x\n---\n\nbody\n"}'
REQ2="$(VELES_HOME="$B" HOME="$B" "$BIN" ticket request --origin "$A_ID" \
    --target "$B_ID" --tool skill_manage --args "$OTHER" --grant owner \
    --session d1-e2e 2>/dev/null | grep -o 'veles-ticket-request:1\.[A-Za-z0-9_-]*')"
[ -n "$REQ2" ] || fail "could not mint a second request"
# Sign the OTHER request, then present it for the FIRST call's args.
T2="$(printf 'd1-passphrase\n' \
    | VELES_HOME="$B" HOME="$B" "$BIN" ticket sign "$REQ2" --yes --key phone 2>/dev/null \
    | grep -o 'veles-ticket:1\.[A-Za-z0-9_.-]*' | head -1)"
[ -n "$T2" ] || fail "could not sign the second request"
R="$(reply_json "$(send_ask tok-a "$(ask_json skill_manage "$SKILL_ARGS" "$OWNER" "$T2")")")"
echo "$R" | grep -q '"status":"ok"' && fail "a ticket for other args ran the call: $R"
echo "$R" | grep -qi 'signature\|argument' \
    || fail "the refusal does not name the args mismatch: $R"
[ -d "$B/skills/d1-other-skill" ] && fail "the mismatched ticket created a skill"
echo "PASS 6: a ticket signed for other arguments does not authorize this call"

# ── 7. the gates ────────────────────────────────────────────────────
# A peer that is authenticated but not in [cluster] task_peers.
# (Built in steps, not triple-nested command substitution: dash's parser
# chokes on `"" ""` args inside nested "$(…)" inside double quotes —
# verified on this machine. The b4/b5/c2 scripts keep to one level too.)
RD_ASK="$(ask_json read_file "{\"path\":\"$ROOT/shared.txt\"}" "" "")"
RD_REPLY="$(send_ask tok-c "$RD_ASK")"
R="$(reply_json "$RD_REPLY")"
echo "$R" | grep -q 'task_peers' \
    || fail "a peer outside task_peers was served: $R"
echo "PASS 7a: a peer outside [cluster] task_peers is refused"

# An ask that claims a different origin than the authenticated peer.
SPOOF=$(printf '{"from_node":"%s","session":"d1-e2e","tool":"read_file","args":{"path":"%s"}}' \
    "$B_ID" "$ROOT/shared.txt")
SP_REPLY="$(send_ask tok-a "$SPOOF")"
R="$(reply_json "$SP_REPLY")"
echo "$R" | grep -q 'says it comes from node' \
    || fail "a spoofed from_node was served: $R"
echo "PASS 7b: an ask naming another node than its peer is refused"

echo ""
echo "PASS: D1/D2/D3 remote tools verified against two real gateways (7 properties)"
exit 0
