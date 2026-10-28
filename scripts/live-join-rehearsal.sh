#!/usr/bin/env bash
# Live two-gateway desktop join: real gateway, Telegram module, Tcl/Tk
# client and persistent stores. Only the model and Bot API are local stubs.
# No production home, public chat, real account or live gateway is used.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$PWD
BIN="${VELES_BIN:-$(nix build .#default --no-link --print-out-paths)/bin/veles}"
[ -x "$BIN" ] || { echo "no veles binary at $BIN" >&2; exit 1; }
for c in curl jq sqlite3 python3 tclsh Xvfb; do command -v "$c" >/dev/null || { echo "need $c" >&2; exit 1; }; done
WISH="${WISH:-$(command -v wish)}"
[ -n "$WISH" ] || { echo "need wish" >&2; exit 1; }
MODULE="$(dirname "$BIN")/../lib/libveles_module_telegram.so"
if [ ! -f "$MODULE" ]; then MODULE="$REPO/target/debug/deps/libveles_module_telegram.so"; fi
[ -f "$MODULE" ] || { echo "no same-build Telegram module" >&2; exit 1; }
ROOT=$(mktemp -d /dev/shm/veles-live-join.XXXXXX)
O_A2A=19311 O_DESK=19312 T_A2A=19321 T_DESK=19322 TG=19323 MOCK=19324 XP=93
O_ID=11111111-0000-4000-8000-0000000000d1
T_ID=22222222-0000-4000-8000-0000000000d2
PIDS="" TPIDS="" GPID="" TPID="" DESKPID=""
cleanup() {
    rc=$?
    trap - EXIT INT TERM
    [ -z "${DESKPID:-}" ] || kill "$DESKPID" >/dev/null 2>&1 || true
    for p in $TPIDS $GPID $PIDS; do kill "$p" >/dev/null 2>&1 || true; done
    for p in $TPIDS $GPID $PIDS; do wait "$p" >/dev/null 2>&1 || true; done
    if [ "$rc" -ne 0 ]; then
        echo "live-join logs retained in $ROOT" >&2
        for f in origin.log target.log desktop-join.log desktop-restart.log mock.log telegram.log; do
            [ ! -f "$ROOT/$f" ] || { echo "--- $f"; tail -35 "$ROOT/$f"; }
        done
    fi
    exit "$rc"
}
trap cleanup EXIT INT TERM
for port in $O_A2A $O_DESK $T_A2A $T_DESK $TG $MOCK; do
    ! (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null || { echo "test port $port is occupied" >&2; exit 1; }
done
[ ! -e "/tmp/.X11-unix/X$XP" ] || { echo "Xvfb display :$XP is occupied" >&2; exit 1; }
export VELES_MODULE_PATH="$ROOT/modules"
mkdir -p "$ROOT/origin" "$ROOT/target" "$ROOT/home" "$ROOT/modules/telegram"
ln -s "$MODULE" "$ROOT/modules/telegram/module.so"
printf 'name = "telegram"\nkind = "module"\nentry = "module.so"\n' > "$ROOT/modules/telegram/plugin.toml"
spawn() { "$@" >>"$ROOT/spawn.log" 2>&1 & PIDS="$PIDS $!"; }
wait_url() {
    local url=$1 auth=${2:-}
    for _ in $(seq 1 180); do
        if curl -sf -m 2 ${auth:+-H "$auth"} "$url" >/dev/null 2>&1; then return 0; fi
        sleep 0.25
    done
    echo "timed out waiting for $url" >&2
    return 1
}
wait_file() {
    local f=$1
    for _ in $(seq 1 300); do [ -f "$f" ] && return 0; sleep 0.2; done
    echo "timed out waiting for $f" >&2
    return 1
}
config() {
    local name=$1 id=$2 port=$3 peer=$4 peer_port=$5 peer_token=$6
    printf 'id = "%s"\nname = "%s"\n' "$id" "$name" > "$ROOT/$name/node.toml"
    cat > "$ROOT/$name/config.toml" <<EOF
[model]
provider = "mock"
default = "mock-model"
[[custom_providers]]
name = "mock"
base_url = "http://127.0.0.1:$MOCK/v1"
api_mode = "chat_completions"
api_key = "none"
models = ["mock-model"]
[platforms.a2a]
enabled = true
[node]
name = "$name"
endpoint = "http://127.0.0.1:$port"
[a2a_agents.$peer]
url = "http://127.0.0.1:$peer_port"
auth = { type = "bearer", token = "$peer_token" }
[cluster]
arbiter = "$T_ID"
gossip_peers = ["$peer"]
memsync_peers = ["$peer"]
storesync_peers = ["$peer"]
task_peers = ["$peer"]
storesync_auto = true
memsync_auto = true
gossip_interval_secs = 2
[acl]
default_role = "user"
EOF
}
config origin "$O_ID" "$O_A2A" vps "$T_A2A" tok-o
config target "$T_ID" "$T_A2A" origin "$O_A2A" tok-v
cat >> "$ROOT/target/config.toml" <<EOF
[platforms.telegram]
enabled = true
[platforms.telegram.extra]
require_mention = false
EOF
printf 'DESKTOP_BEARER_TOKEN=desk-o\nA2A_BEARER_TOKEN=tok-o\nA2A_PEER_TOKENS=vps:tok-v\n' > "$ROOT/origin/.env"
printf 'DESKTOP_BEARER_TOKEN=desk-v\nA2A_BEARER_TOKEN=tok-v\nA2A_PEER_TOKENS=origin:tok-o\nTELEGRAM_BOT_TOKEN=fixture\nTELEGRAM_ALLOWED_USERS=4242,515151\n' > "$ROOT/target/.env"
chmod 600 "$ROOT/origin/.env" "$ROOT/target/.env"
start_gateway() {
    local name=$1 a2a=$2 desk=$3 extra=${4:-}
    env $extra VELES_HOME="$ROOT/$name" HOME="$ROOT/home" VELES_MODULE_PATH="$ROOT/modules" \
        A2A_HOST=127.0.0.1 A2A_PORT="$a2a" VELES_DESKTOP_PORT="$desk" \
        "$BIN" gateway run >>"$ROOT/$name.log" 2>&1 &
    TPID=$!
    TPIDS="$TPIDS $TPID"
}
start_gateway origin "$O_A2A" "$O_DESK"
GPID=$TPID
start_gateway target "$T_A2A" "$T_DESK" TELOXIDE_API_URL="http://127.0.0.1:$TG/"
spawn python3 "$REPO/scripts/devharness/mock_provider.py" "$MOCK" "$ROOT/mock.log"
spawn tclsh "$REPO/scripts/devharness/tg_join_stub.tcl" "$TG" 4242 515151 "$ROOT/telegram.log"
spawn Xvfb ":$XP" -screen 0 1280x900x24 -nolisten tcp
wait_url "http://127.0.0.1:$MOCK/v1/models"
wait_url "http://127.0.0.1:$TG/__sent"
wait_url "http://127.0.0.1:$O_DESK/api/health"
wait_url "http://127.0.0.1:$T_DESK/api/health"
wait_url "http://127.0.0.1:$T_A2A/.well-known/agent-card.json"
for _ in $(seq 1 100); do grep -q 'telegram adapter online' "$ROOT/target.log" 2>/dev/null && break; sleep 0.2; done
grep -q 'telegram adapter online' "$ROOT/target.log" || { echo "Telegram module did not start" >&2; exit 1; }
post() { curl -sf -m 20 -H "$1" -H 'Content-Type: application/json' -d "$3" "http://127.0.0.1:$2$4"; }
post '' "$TG" '{}' /__message >/dev/null
SID=""
for _ in $(seq 1 150); do
    SID=$(sqlite3 "$ROOT/target/state.db" "SELECT session_id FROM messages WHERE content='hello from the synthetic participant' ORDER BY msg_id DESC LIMIT 1" 2>/dev/null || true)
    [ -n "$SID" ] && break
    sleep 0.2
done
[ -n "$SID" ] || { echo "the synthetic participant has no chat-backed session" >&2; exit 1; }
printf '%s' "$SID" > "$ROOT/participant-id"
echo "PASS fixture: synthetic Telegram participant owns a live target session"
# The desktop registry is separate from both gateway homes. It carries
# only throwaway credentials and lives under the rehearsal's XDG root.
mkdir -p "$ROOT/home/.config/veles-desktop"
cat > "$ROOT/home/.config/veles-desktop/instances.json" <<EOF
{"laptop":{"url":"http://127.0.0.1:$O_DESK","token":"desk-o"},
 "vps":{"url":"http://127.0.0.1:$T_DESK","token":"desk-v"},
 "__last_attached__":{"last":"laptop"}}
EOF
chmod 600 "$ROOT/home/.config/veles-desktop/instances.json"
start_desktop() {
    local phase=$1
    local app="$ROOT/app-$phase"
    mkdir -p "$app"
    ln -s "$REPO/veles-desktop/lib" "$app/lib"
    ln -s "$REPO/veles-desktop/msgs" "$app/msgs"
    ln -s "$REPO/veles-desktop/scripts" "$app/scripts"
    cp "$REPO/veles-desktop/main.tcl" "$app/main.tcl"
    printf '\nset argv [list "%s" "%s"]\nsource "%s"\n' \
        "$phase" "$SID" "$REPO/scripts/devharness/desktop_join_live.tcl" >> "$app/main.tcl"
    local selected=()
    if [ "$phase" = join ]; then selected=(VELES_INSTANCE=laptop); fi
    : > "$ROOT/desktop-$phase.log"
    env HOME="$ROOT/home" XDG_CONFIG_HOME="$ROOT/home/.config" XDG_DATA_HOME="$ROOT/home/.local/share" \
        XDG_CACHE_HOME="$ROOT/home/.cache" DISPLAY=":$XP" XVFB_RUNNING=1 \
        VELES_JOIN_LIVE_ROOT="$ROOT" \
        "${selected[@]}" VELES_DESKTOP_DIR="$app" LANG=C.UTF-8 LC_ALL=C.UTF-8 \
        "$WISH" "$app/main.tcl" \
        >>"$ROOT/desktop-$phase.log" 2>&1 &
    DESKPID=$!
}
start_desktop join
wait_file "$ROOT/desktop-join.ok"
echo "PASS desktop: cross-instance join and first joined message"
JID=$(cat "$ROOT/join-id")
[ "$(sqlite3 "$ROOT/target/state.db" "SELECT COUNT(*) FROM meta WHERE key='join:gateway:v1:desktop:join:$JID'")" = 1 ]
# Assert execution from the durable join row and the exact owner text in
# the mock provider's request log. Queue rows are consumed normally at
# turn end, so requiring one to persist would race the real lifecycle.
join_mail_count() {
    sqlite3 "$ROOT/target/state.db" "SELECT COUNT(*) FROM session_mail WHERE to_session='$SID' AND kind='join' AND state='answered' AND json_extract(from_principal,'\$.role')='owner'"
}
join_text_seen() {
    grep -Fq -- "user      content='$1'" "$ROOT/mock.log"
}
for _ in $(seq 1 200); do
    [ "$(join_mail_count)" -ge 1 ] && join_text_seen 'owner line before restart' && break
    sleep 0.1
done
[ "$(join_mail_count)" -ge 1 ] && join_text_seen 'owner line before restart' || {
    echo "the first joined line did not execute with owner rights" >&2
    sqlite3 "$ROOT/target/state.db" "SELECT id,kind,state FROM session_mail WHERE to_session='$SID'" >&2
    exit 1
}
initial_mail_count=$(join_mail_count)
echo "PASS target: the joined line executed and is durable answered kind=join mail"
# Stop ONLY this fixture's target, by the PID recorded when it was
# spawned here. The origin, the desktop and production stay untouched.
kill "$TPID"
wait "$TPID" || true
start_gateway target "$T_A2A" "$T_DESK" TELOXIDE_API_URL="http://127.0.0.1:$TG/"
wait_url "http://127.0.0.1:$T_DESK/api/health" 'Authorization: Bearer desk-v'
for _ in $(seq 1 150); do
    STATUS=$(curl -sf -m 2 -H 'Authorization: Bearer desk-v' "http://127.0.0.1:$T_DESK/api/join/status?join_id=$JID&session_id=$SID" || true)
    jq -e --arg j "$JID" --arg s "$SID" '.join.join_id == $j and .join.target_session == $s' <<<"$STATUS" >/dev/null 2>&1 && break
    sleep 0.2
done
jq -e --arg j "$JID" --arg s "$SID" '.join.join_id == $j and .join.target_session == $s' <<<"$STATUS" >/dev/null
echo "PASS target restart: the same binding revalidates against the live ACL"
# A new desktop process restores its saved view, then uses the restored
# target binding for a second joined line. It waits for may-leave, which
# the shell grants only after checking that second durable mail row.
: > "$ROOT/desktop-restart.log"
start_desktop restart
for _ in $(seq 1 240); do
    [ "$(join_mail_count)" -gt "$initial_mail_count" ] && join_text_seen 'owner line after both restarts' && break
    sleep 0.25
done
[ "$(join_mail_count)" -gt "$initial_mail_count" ] && join_text_seen 'owner line after both restarts' || { echo "the post-restart joined line did not execute" >&2; exit 1; }
echo "PASS desktop restart: a fresh client restored and used the same join"
: > "$ROOT/may-leave"
wait_file "$ROOT/desktop-restart.ok"
[ "$(sqlite3 "$ROOT/target/state.db" "SELECT COUNT(*) FROM meta WHERE key='join:gateway:v1:desktop:join:$JID'")" = 0 ]
echo "PASS leave: the target removed its durable binding and the client returned to origin"
echo "LIVE_JOIN_RC=0"
