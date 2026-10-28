#!/usr/bin/env bash
# g-three-nodes.sh — the three-node pool, rehearsed on this machine:
# the VPS as the hub (arbiter, Telegram, cron), the laptop, the phone.
#
#   nix run .#pool-three-rehearsal          (~5 minutes, one lease TTL)
#   bash scripts/g-three-nodes.sh           (this tree's target/debug/veles)
#
# Three scratch gateways under /dev/shm, ports 199xx. Nothing touches the
# live config, :8642 or :9900, or the live gateway's sandbox boxes.
#
# What is REAL: three gateway processes (this build), the A2A wire with
# bearer auth, gossip, the arbiter's leases, re-homing (`[tools.home]` as
# an order, `[cluster] sandbox_home`), storesync and memsync on their own,
# the owner's ask relay across nodes, the Telegram adapter (teloxide), a
# `veles signer run` with its own key, podman user envs — the laptop's
# through the local podman, the vps's through a podman socket
# (CONTAINER_HOST), as on the VPS. What is STUBBED: the model
# (mock_provider.py), Telegram's servers (tg_bot_stub.tcl for the
# gateway's bot, tg_stub.tcl for the signer's), and the phone (a gateway
# whose podman does not exist, which is the phone's whole difference
# here).
#
# Checks:
#   0   gossip converges: every node lists the other two
#   0b  the laptop and the phone, restarted once, hold their node leases
#       at the vps (a fresh node boots with an empty registry)
#   1   the vps holds the singletons: telegram and cron; its Telegram
#       adapter runs against the stub, nobody else's does
#   2a  a phone execute_code{sandbox} runs in a box on the laptop
#   2b  a phone background shell in its sandbox is refused by name
#   2c  the vps's own sandbox runs through its podman socket (a volume,
#       no host path)
#   3a  a vps web_extract runs on the laptop ("web_*" = [laptop, vps])
#   3b  with the laptop down it runs on the vps, saying the laptop was
#       passed over
#   4a  memory written on the vps reaches the laptop and the phone
#   4b  memory both sides changed is held as a conflict, the local
#       content kept
#   5   a laptop desktop approval is relayed to the vps's Telegram,
#       answered there by the owner, and the turn goes on
#   6   a phone call that needs a ticket is signed by the headless
#       signer, reached over HTTP, on the owner's yes
#   7   sessions replicate all ways (each node holds the other two's)
#   8a  the phone stops: the laptop takes its session over once the
#       phone's node lease at the vps runs out
#   8b  the phone returns and yields
#   9a  a config edit on the laptop reaches the vps and the phone
#   9b  an [acl] edit on the laptop is NOT applied on the vps…
#   9c  …it waits in its inbox for the owner
#
# The last line is RC=0 when every check passed.
set -u
# Under writeShellApplication (nix run .#pool-three-rehearsal) errexit and
# pipefail are on as well: every wait that may time out ends in `|| true`,
# so its check reports the failure instead of the shell ending the run.
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${VELES_BIN:-$REPO/target/debug/veles}"
MOCK="${VELES_MOCK_PROVIDER:-$REPO/scripts/devharness/mock_provider.py}"
TG_BOT_STUB="${VELES_TG_BOT_STUB:-$REPO/scripts/devharness/tg_bot_stub.tcl}"
TG_STUB="${VELES_TG_STUB:-$REPO/scripts/devharness/tg_stub.tcl}"
IMAGE="${VELES_SANDBOX_IMAGE:-veles-sandbox:latest}"
ROOT=$(mktemp -d /dev/shm/g3-XXXXXX)
V="$ROOT/vps"; L="$ROOT/laptop"; P="$ROOT/phone"; S="$ROOT/signer"
mkdir -p "$V" "$L" "$P" "$S"

V_ID="11111111-0000-4000-8000-0000000000a1"
L_ID="22222222-0000-4000-8000-0000000000a2"
P_ID="33333333-0000-4000-8000-0000000000a3"
V_A2A=19911; V_DESK=19912
L_A2A=19921; L_DESK=19922
P_A2A=19931; P_DESK=19932
MOCK_PORT=19940; TG_PORT=19941; SIGNER_PORT=19942; SIGNER_TG=19943
OWNER=4242
SOCK="$ROOT/podman.sock"
# A gateway's podman reads its storage and config from the XDG dirs, and
# HOME is the scratch home here: point podman at the real ones, where the
# sandbox image is (veles itself ignores them under VELES_HOME).
PODMAN_XDG=(XDG_DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}" XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}")

pass=0; fail=0
GV=""; GL=""; GP=""; MP=""; TP=""; SP=""; STP=""; PSVC=""
before_boxes=""; before_vols=""; snapshot=""

cleanup() {
    for p in $GV $GL $GP $MP $TP $SP $STP $PSVC; do kill "$p" 2>/dev/null || true; done
    sleep 0.5
    # On a failure the logs outlive the scratch homes: /dev/shm/g3-logs.
    if [ "$fail" != 0 ] || [ "${setup_failed:-}" = 1 ]; then
        rm -rf /dev/shm/g3-logs && mkdir -p /dev/shm/g3-logs
        cp "$ROOT"/*.log /dev/shm/g3-logs/ 2>/dev/null || true
        echo "logs kept in /dev/shm/g3-logs"
    fi
    # Only what this run made: boxes and volumes that were not there
    # before — and nothing at all without a snapshot to compare with.
    [ -n "$snapshot" ] || { rm -rf "$ROOT"; return; }
    for b in $(podman ps -a --format '{{.Names}}' 2>/dev/null | grep '^veles-user-env-' | sort); do
        # A here-string, not a pipe: under pipefail a matching grep's early
        # exit fails the pipe — and this would remove a box that was here
        # before the run (the live gateway's).
        grep -qx -- "$b" <<<"$before_boxes" || podman rm -f "$b" >/dev/null 2>&1 || true
    done
    for v in $(podman volume ls -q 2>/dev/null | grep '^veles-env-' | sort); do
        grep -qx -- "$v" <<<"$before_vols" || podman volume rm -f "$v" >/dev/null 2>&1 || true
    done
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

diag() {
    for n in vps laptop phone signer; do
        echo "--- $n.log"; grep -v ' DEBUG ' "$ROOT/$n.log" 2>/dev/null | tail -20 || true
    done
    echo "--- tg (gateway bot)"; tail -8 "$ROOT/tg.log" 2>/dev/null || true
}
failfast() { echo "FAIL (setup): $*"; setup_failed=1; diag; echo "RC=1"; exit 1; }
check() { # name cmd... — one property
    local name=$1; shift
    if "$@" >/dev/null 2>&1; then echo "PASS $name"; pass=$((pass + 1));
    else echo "FAIL $name"; fail=$((fail + 1)); fi
}
need() { command -v "$1" >/dev/null 2>&1 || failfast "needs $1 on PATH"; }
for c in curl jq sqlite3 python3 tclsh podman; do need "$c"; done
[ -x "$BIN" ] || failfast "no veles binary at $BIN (cargo build, or VELES_BIN)"
# Telegram is a module, not a gateway-builtin adapter. Give discovery a
# scratch manifest and the dylib from THIS build, never another build's
# runtime. The Nix package has lib/ beside bin/; cargo keeps it in deps.
TELEGRAM_MODULE="${VELES_TELEGRAM_MODULE:-$(dirname "$BIN")/../lib/libveles_module_telegram.so}"
[ -f "$TELEGRAM_MODULE" ] \
    || TELEGRAM_MODULE="$REPO/target/debug/deps/libveles_module_telegram.so"
[ -f "$TELEGRAM_MODULE" ] || failfast "no Telegram module beside $BIN or in target/debug/deps"
mkdir -p "$ROOT/modules/telegram"
ln -s "$TELEGRAM_MODULE" "$ROOT/modules/telegram/module.so"
printf 'name = "telegram"\nkind = "module"\nentry = "module.so"\n' \
    > "$ROOT/modules/telegram/plugin.toml"
podman image exists "$IMAGE" || failfast "the sandbox image $IMAGE is not here (scripts/build-sandbox-image.sh)"
# What is here before the run (the live gateway's boxes): never touched.
before_boxes=$(podman ps -a --format '{{.Names}}') || failfast "podman ps failed"
before_vols=$(podman volume ls -q) || failfast "podman volume ls failed"
snapshot=1

cli() { # home args… — the node's CLI, colour and log lines stripped
    local h=$1; shift
    RUST_LOG=error NO_COLOR=1 VELES_HOME="$h" HOME="$h" "$BIN" "$@" 2>&1 \
        | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20[0-9][0-9]-' || true
}
cli_p() { VELES_PODMAN_BIN=/nonexistent/podman cli "$P" "$@"; }
cli_v() { CONTAINER_HOST="unix://$SOCK" cli "$V" "$@"; }
api_post() { # port path json
    curl -s -m 60 -X POST -H 'Content-Type: application/json' -d "$3" "http://127.0.0.1:$1$2"
}
api_get() { curl -s -m 10 "http://127.0.0.1:$1$2"; }
wait_for() { # seconds cmd… — poll every half second
    local n=$(($1 * 2)); shift
    for _ in $(seq 1 "$n"); do "$@" >/dev/null 2>&1 && return 0; sleep 0.5; done
    return 1
}

# ── the stand-ins ─────────────────────────────────────────────────────
python3 "$MOCK" "$MOCK_PORT" "$ROOT/mock.log" >/dev/null 2>&1 &
MP=$!
tclsh "$TG_BOT_STUB" "$TG_PORT" "$OWNER" press-first "$ROOT/tg.log" &
TP=$!
podman system service --time=0 "unix://$SOCK" >"$ROOT/podman-svc.log" 2>&1 &
PSVC=$!
wait_for 15 curl -s -m 1 "http://127.0.0.1:$MOCK_PORT/v1/models" || failfast "the mock provider never came up"
wait_for 15 test -S "$SOCK" || failfast "the podman socket never appeared"

# The headless signer's key, in its own directory (the VPS's signer).
KEYOUT=$(printf 'g3-passphrase\ng3-passphrase\n' | VELES_HOME="$S/home" HOME="$S" "$BIN" ticket keygen vps-signer 2>&1)
PUB=$(printf '%s' "$KEYOUT" | grep -o 'ed25519:[A-Za-z0-9_-]*' | head -1 || true)
[ -n "$PUB" ] || failfast "keygen printed no public key: $KEYOUT"
KEYDIR="$S/veles-signer"
cat > "$KEYDIR/signer.toml" <<EOF
key = "vps-signer"
listen = "127.0.0.1:$SIGNER_PORT"
wait_secs = 30
[telegram]
owner_id = $OWNER
api_url = "http://127.0.0.1:$SIGNER_TG"
EOF
printf 'SIGNER_GATEWAY_TOKENS=vps:gw-v,laptop:gw-l,phone:gw-p\nSIGNER_TELEGRAM_TOKEN=STUBTOKEN\n' > "$KEYDIR/signer.env"
chmod 600 "$KEYDIR/signer.env"
printf 'g3-passphrase\n' > "$S/pass"
tclsh "$TG_STUB" "$SIGNER_TG" "$OWNER" approve "$ROOT/signer-tg.log" &
STP=$!
SIGNER_PASSPHRASE_FILE="$S/pass" HOME="$S" "$BIN" signer run --dir "$KEYDIR" > "$ROOT/signer.log" 2>&1 &
SP=$!
wait_for 15 curl -sf -o /dev/null "http://127.0.0.1:$SIGNER_PORT/v1/health" || failfast "the signer never came up"

# ── the three configs ─────────────────────────────────────────────────
common() { # name endpoint-port
    cat <<EOF
[model]
provider = "mock"
default  = "mock-model"

[[custom_providers]]
name = "mock"
base_url = "http://127.0.0.1:$MOCK_PORT/v1"
api_mode = "chat_completions"
api_key = "none"
models = ["mock-model"]

[platforms.a2a]
enabled = true

[sandbox]
backend = "podman"
image = "$IMAGE"

[tools.home]
"web_*" = ["laptop", "vps"]
"vision_*" = "laptop"

[platforms.telegram]
enabled = true

[platforms.telegram.home_channel]
platform = "telegram"
chat_id = "$OWNER"

[notify]
ask_relay = true
ask_relay_delay_secs = 1
ask_relay_kinds = ["approval"]

[approvals]
mode = "manual"

[node]
name = "$1"
endpoint = "http://127.0.0.1:$2"

[cluster.ticket_signers.vps-signer]
key = "$PUB"
EOF
}
cluster() { # peer1 peer2 extra…
    cat <<EOF

[cluster]
arbiter = "$V_ID"
gossip_peers = ["$1", "$2"]
memsync_peers = ["$1", "$2"]
task_peers = ["$1", "$2"]
storesync_peers = ["$1", "$2"]
storesync_auto = true
memsync_auto = true
failover = true
gossip_interval_secs = 2
stale_after_secs = 60
signers = ["http://127.0.0.1:$SIGNER_PORT"]
EOF
    shift 2
    for x in "$@"; do printf '%s\n' "$x"; done
}
agent() { # name url token
    printf '\n[a2a_agents.%s]\nurl = "%s"\nauth = { type = "bearer", token = "%s" }\n' "$1" "$2" "$3"
}

printf 'id = "%s"\nname = "vps"\n' "$V_ID" > "$V/node.toml"
printf 'id = "%s"\nname = "laptop"\n' "$L_ID" > "$L/node.toml"
printf 'id = "%s"\nname = "phone"\n' "$P_ID" > "$P/node.toml"
# Each node presents ITS token to the others; each knows the others by
# theirs (A2A_PEER_TOKENS), and the names match the [a2a_agents] names.
{ common vps $V_A2A; cluster laptop phone
  agent laptop "http://127.0.0.1:$L_A2A" tok-v; agent phone "http://127.0.0.1:$P_A2A" tok-v; } > "$V/config.toml"
{ common laptop $L_A2A; cluster vps phone
  agent vps "http://127.0.0.1:$V_A2A" tok-l; agent phone "http://127.0.0.1:$P_A2A" tok-l; } > "$L/config.toml"
{ common phone $P_A2A; cluster vps laptop 'sandbox_home = "laptop"'
  agent vps "http://127.0.0.1:$V_A2A" tok-p; agent laptop "http://127.0.0.1:$L_A2A" tok-p; } > "$P/config.toml"
printf 'A2A_PEER_TOKENS=laptop:tok-l,phone:tok-p\nA2A_BEARER_TOKEN=tok-v\nVELES_SIGNER_TOKEN=gw-v\nTELEGRAM_BOT_TOKEN=STUBTOKEN\nTELEGRAM_ALLOWED_USERS=%s\n' "$OWNER" > "$V/.env"
printf 'A2A_PEER_TOKENS=vps:tok-v,phone:tok-p\nA2A_BEARER_TOKEN=tok-l\nVELES_SIGNER_TOKEN=gw-l\n' > "$L/.env"
printf 'A2A_PEER_TOKENS=vps:tok-v,laptop:tok-l\nA2A_BEARER_TOKEN=tok-p\nVELES_SIGNER_TOKEN=gw-p\n' > "$P/.env"
chmod 600 "$V/.env" "$L/.env" "$P/.env"

# The Telegram module is a same-build runtime dependency, never an
# ambient module from the operator's installed data dir.
export VELES_MODULE_PATH="$ROOT/modules"
# VELES_DESKTOP_OPEN=1: no desktop token minted into .env at boot (d1).
start_vps() {
    CONTAINER_HOST="unix://$SOCK" TELOXIDE_API_URL="http://127.0.0.1:$TG_PORT/" \
    A2A_HOST=127.0.0.1 A2A_PORT=$V_A2A VELES_DESKTOP_PORT=$V_DESK VELES_DESKTOP_OPEN=1 \
        VELES_HOME="$V" HOME="$V" "$BIN" gateway run >> "$ROOT/vps.log" 2>&1 &
    GV=$!
}
start_laptop() {
    env "${PODMAN_XDG[@]}" A2A_HOST=127.0.0.1 A2A_PORT=$L_A2A VELES_DESKTOP_PORT=$L_DESK VELES_DESKTOP_OPEN=1 \
        VELES_HOME="$L" HOME="$L" "$BIN" gateway run >> "$ROOT/laptop.log" 2>&1 &
    GL=$!
}
start_phone() {
    VELES_PODMAN_BIN=/nonexistent/podman \
    A2A_HOST=127.0.0.1 A2A_PORT=$P_A2A VELES_DESKTOP_PORT=$P_DESK VELES_DESKTOP_OPEN=1 \
        VELES_HOME="$P" HOME="$P" "$BIN" gateway run >> "$ROOT/phone.log" 2>&1 &
    GP=$!
}
up() { # pid a2a-port name
    for _ in $(seq 1 120); do
        kill -0 "$1" 2>/dev/null || failfast "the $3 gateway died"
        curl -sf -o /dev/null "http://127.0.0.1:$2/.well-known/agent-card.json" && return 0
        sleep 0.25
    done
    failfast "the $3 gateway never came up"
}
stop() { # pid
    kill "$1" 2>/dev/null || true
    wait "$1" 2>/dev/null || true
}

echo "== homes under $ROOT"
start_vps; up "$GV" $V_A2A vps
start_laptop; up "$GL" $L_A2A laptop
start_phone; up "$GP" $P_A2A phone

# Matches are made on captured output, never `cmd | grep -q`: under
# pipefail an early grep exit is the writer's SIGPIPE, a failed pipeline.
contains() { [[ $1 == *"$2"* ]]; }
has_line() { grep -qx -- "$2" <<<"$1"; }
lists() { local o; o=$(cli "$1" nodes list); contains "$o" "$2" && contains "$o" "$3"; }
ok=""
for _ in $(seq 1 120); do
    sleep 0.5
    if lists "$V" laptop phone && lists "$L" vps phone && lists "$P" vps laptop; then ok=1; break; fi
done
[ -n "$ok" ] || failfast "gossip never converged"
check 0-gossip-converged true

# The laptop and the phone booted into an empty registry: their fences
# refused (no arbiter row yet). A restart now arms them.
stop "$GL"; start_laptop; up "$GL" $L_A2A laptop
stop "$GP"; start_phone; up "$GP" $P_A2A phone
lease_holder() { sqlite3 "$V/state.db" "SELECT holder FROM singleton_leases WHERE key='$1'" 2>/dev/null; }
node_leases() { [ "$(lease_holder "node:$L_ID")-$(lease_holder "node:$P_ID")" = "$L_ID-$P_ID" ]; }
wait_for 75 node_leases || true
check 0b-laptop-and-phone-hold-their-node-leases-at-the-vps node_leases

# ── 1. the singletons are the vps's ───────────────────────────────────
polled() { contains "$(curl -s "http://127.0.0.1:$TG_PORT/__calls")" getme; }
wait_for 30 polled || true
check 1a-vps-holds-telegram test "$(lease_holder singleton:telegram)" = "$V_ID"
check 1b-vps-holds-cron test "$(lease_holder singleton:cron)" = "$V_ID"
check 1c-the-vps-telegram-adapter-runs grep -q 'telegram adapter online' "$ROOT/vps.log"
no_tg_elsewhere() { ! grep -q 'telegram adapter online' "$ROOT/laptop.log" "$ROOT/phone.log"; }
check 1d-no-other-node-runs-telegram no_tg_elsewhere

# ── 2. sandboxes ──────────────────────────────────────────────────────
OUT=$(cli_p tool --name execute_code --args '{"code":"puts [expr {6*7}]","sandbox":true}')
echo "  phone execute_code: $(printf '%s' "$OUT" | head -3 | tr '\n' ' ')"
ran_on_laptop_42() { contains "$OUT" 'execute_code ran on node laptop' && has_line "$OUT" 42; }
check 2a-phone-sandbox-ran-on-the-laptop ran_on_laptop_42
OUT=$(cli_p tool --name terminal --args '{"command":"sleep 1","background":true,"host":false}')
check 2b-phone-background-sandbox-shell-refused contains "$OUT" foreground
OUT=$(cli_v tool --name execute_code --args '{"code":"puts [expr {6*7}]","sandbox":true}')
echo "  vps execute_code: $(printf '%s' "$OUT" | head -3 | tr '\n' ' ')"
vps_box() { has_line "$OUT" 42 && contains "$(podman volume ls -q)" veles-env-; }
check 2c-vps-sandbox-through-its-podman-socket vps_box

# ── 3. web_* is an order: the laptop, else the vps ───────────────────
OUT=$(cli "$V" tool --name web_extract --args '{"url":"http://127.0.0.1:1/x"}')
check 3a-vps-web-extract-ran-on-the-laptop contains "$OUT" 'web_extract ran on node laptop'
stop "$GL"; GL=""
OUT=$(cli "$V" tool --name web_extract --args '{"url":"http://127.0.0.1:1/x"}')
echo "  with the laptop down:"; printf '%s\n' "$OUT" | head -6 | sed 's/^/    /' || true
fell_back() { contains "$OUT" 'passed over' && contains "$OUT" 'web_extract ran here'; }
check 3b-laptop-down-it-ran-on-the-vps fell_back
start_laptop; up "$GL" $L_A2A laptop

# ── 4. memory replicates on its own ──────────────────────────────────
printf 'from the vps\n' | cli "$V" memory write reference/g3.md >/dev/null
has() { contains "$(cli "$1" memory cat "$2")" "$3"; }
ok=""
for _ in $(seq 1 120); do
    sleep 0.5
    if has "$L" reference/g3.md 'from the vps' && has "$P" reference/g3.md 'from the vps'; then ok=1; break; fi
done
check 4a-vps-memory-reached-laptop-and-phone test -n "$ok"
stop "$GP"; GP=""
printf 'vps side\n' | cli "$V" memory write reference/g3c.md >/dev/null
wait_for 60 has "$L" reference/g3c.md 'vps side' || true
printf 'phone side\n' | cli "$P" memory write reference/g3c.md >/dev/null
start_phone; up "$GP" $P_A2A phone
held() { contains "$(cli "$1" memory inbox)" from; }
ok=""
for _ in $(seq 1 120); do
    sleep 0.5
    if held "$V" || held "$P"; then ok=1; break; fi
done
conflict_held() { [ -n "$ok" ] && has "$V" reference/g3c.md 'vps side'; }
check 4b-a-both-sides-change-is-held-as-a-conflict conflict_held

# ── 5. the owner's ask relay crosses to the vps's Telegram ───────────
SID=$(api_post $L_DESK /api/sessions/new '{"title":"g3-relay"}' | jq -r '.session_id // empty')
[ -n "$SID" ] || failfast "no laptop session"
api_post $L_DESK /api/chat "{\"session_id\":\"$SID\",\"text\":\"[mock:approval] tidy up\"}" >/dev/null
ok=""
for _ in $(seq 1 120); do
    sleep 0.5
    n=$(sqlite3 "$L/state.db" "SELECT COUNT(*) FROM messages WHERE session_id='$SID' AND role='tool'" 2>/dev/null || echo 0)
    [ "${n:-0}" -ge 1 ] && { ok=1; break; }
done
relayed() { contains "$(curl -s "http://127.0.0.1:$TG_PORT/__sent")" 'from node laptop'; }
none_open() { [ "$(curl -s "http://127.0.0.1:$L_DESK/api/asks" | jq 'if type == "array" then length else (.asks // []) | length end')" = 0 ]; }
check 5a-the-relay-reached-the-vps-telegram relayed
check 5b-the-laptop-turn-went-on-after-the-owner-answered test -n "$ok"
check 5c-no-question-left-open-on-the-laptop none_open

# ── 6. a ticket from the headless signer, over HTTP ───────────────────
OUT=$(cli_p tool --name vision_analyze --args '{"image_url":"https://example.invalid/g3.png","question":"what is this"}')
signed() { contains "$OUT" 'vision_analyze ran on node laptop' && grep -q "signed, approved by telegram:$OWNER" "$ROOT/signer.log"; }
check 6-signer-ticket-ran-the-call-on-the-laptop signed

# ── 7. sessions replicate all ways ───────────────────────────────────
VS=$(api_post $V_DESK /api/sessions/new '{"title":"g3-vps"}' | jq -r '.session_id // empty')
PS=$(api_post $P_DESK /api/sessions/new '{"title":"g3-phone"}' | jq -r '.session_id // empty')
api_post $V_DESK /api/chat "{\"session_id\":\"$VS\",\"text\":\"hello from the vps\"}" >/dev/null
api_post $P_DESK /api/chat "{\"session_id\":\"$PS\",\"text\":\"hello from the phone\"}" >/dev/null
rep() { # home owner-id session
    [ "$(sqlite3 "$1/replicas/$2.db" "SELECT COUNT(*) FROM messages WHERE session_id='$3'" 2>/dev/null || echo 0)" -ge 2 ]
}
ok=""
for _ in $(seq 1 120); do
    sleep 0.5
    if rep "$L" $V_ID "$VS" && rep "$P" $V_ID "$VS" && rep "$V" $P_ID "$PS" && rep "$L" $P_ID "$PS" \
        && rep "$V" $L_ID "$SID" && rep "$P" $L_ID "$SID"; then ok=1; break; fi
done
check 7-sessions-replicate-all-ways test -n "$ok"

# ── 8. failover with the vps as arbiter ──────────────────────────────
stop "$GP"; GP=""
ok=""
for _ in $(seq 1 40); do
    sleep 5
    OUT=$(cli "$L" session takeover "$PS")
    contains "$OUT" "took $PS over" && { ok=1; break; }
done
check 8a-laptop-took-the-phone-session-over test -n "$ok"
start_phone; up "$GP" $P_A2A phone
wait_for 90 grep -q "failover: $PS: yielded" "$ROOT/phone.log" || true
check 8b-the-phone-yielded grep -q "failover: $PS: yielded" "$ROOT/phone.log"

# ── 9. config replicates on its own ──────────────────────────────────
# `[cluster] configsync_auto` is on by default: an edit to one node's
# file is scanned, pushed and applied on the others within a few ticks.
# A change to a security section is pushed too, and waits in the inbox
# for the owner instead of applying.
printf '\n[agent]\nreasoning_effort = "low"\n' >> "$L/config.toml"
both_low() { grep -q 'reasoning_effort = "low"' "$V/config.toml" && grep -q 'reasoning_effort = "low"' "$P/config.toml"; }
wait_for 60 both_low || true
check 9a-a-laptop-config-edit-reached-vps-and-phone both_low
printf '\n[acl]\ndefault_role = "user"\n' >> "$L/config.toml"
acl_held() { contains "$(cli "$V" configsync apply --inbox --dry-run)" acl.default_role; }
wait_for 60 acl_held || true
check 9b-a-laptop-acl-edit-waits-on-the-vps test -z "$(grep default_role "$V/config.toml")"
check 9c-and-is-listed-for-the-owner acl_held

echo ""
echo "== $pass passed, $fail failed"
if [ "$fail" = 0 ]; then
    echo "RC=0"
else
    diag
    echo "RC=1"
    exit 1
fi
