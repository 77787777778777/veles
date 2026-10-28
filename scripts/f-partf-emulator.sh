# f-partf-emulator.sh — PLAN-pool-round.md Part F, rehearsed on the
# emulator: the two-node pool with one node ON THE PHONE.
#
#   nix run .#android-pool-rehearsal        (~10 minutes, one lease TTL)
#
# The emulator is "the phone" (the debug APK's own gateway, node
# `phone`); a scratch gateway on this laptop (home in /dev/shm, ports
# 198xx) is "the laptop" (node `laptop`), and the laptop also holds the
# arbiter — the two-device path, which is what a phone+laptop fleet
# without the VPS runs. The mock provider stands in for the model, so
# turns are reproducible and free.
#
# Connections: two adb tunnels, one per direction, standing in for the
# WireGuard pair a real deployment runs. Every listener stays bound to
# loopback (resolve_bind_host refuses to widen without a token, and a
# widened one is not what this rehearses); 10.0.2.2 would reach the
# laptop's external interface, not its loopback, so it is not used.
#   laptop → phone A2A      adb forward tcp:19910 tcp:9900
#   laptop → phone desktop  adb forward tcp:18642 tcp:8642
#   phone  → laptop A2A     adb reverse tcp:19901 tcp:19901
#   phone  → mock provider  adb reverse tcp:18999 tcp:18999
#
# Checks (the plan's Part F list):
#   0. gossip converges across the boundary, and the phone — which boots
#      into an empty registry, where its fence cannot arm — restarts
#      holding its node lease at the arbiter (the c2/b3a pattern);
#   1. a phone session's web_extract runs ON THE LAPTOP ([tools.home]);
#   2. replicas land on BOTH sides (storesync auto, both directions);
#   3. stopping the phone's gateway → the laptop takes its session over
#      once the node lease expires; restarting the phone heals it (the
#      phone yields; a fork appears only if it wrote while apart);
#   4. a ticketed call works ACROSS the boundary: the phone refuses an
#      elevated call with a request, the laptop signs it with a key the
#      phone trusts, the call runs on the phone, and the ticket is
#      single-use.
#
# Nothing here touches the live config, :8642 or :9900 on this machine.
# The last line is RC=0 when every check passed. On failure both
# gateway logs and the phone's are dumped.

set -euo pipefail

: "${VELES_APK_DIR:?run this through nix run .#android-pool-rehearsal}"
: "${VELES_ANDROID_SDK:?run this through nix run .#android-pool-rehearsal}"
: "${VELES_BUILD_TOOLS:?run this through nix run .#android-pool-rehearsal}"
: "${VELES_SYSTEM_IMAGE:?run this through nix run .#android-pool-rehearsal}"
: "${VELES_BIN:?run this through nix run .#android-pool-rehearsal}"
: "${VELES_MOCK_PROVIDER:?run this through nix run .#android-pool-rehearsal}"

pkg=chat.janitor.veles
port=5580
serial=emulator-$port
fwd_desk=18642
fwd_a2a=19910
LAP_A2A=19901
LAP_DESK=19902
MOCK_PORT=18999

LAP_ID="11111111-0000-4000-8000-0000000000f6"
PHONE_ID="22222222-0000-4000-8000-0000000000f6"

run=$(mktemp -d /dev/shm/veles-partf.XXXXXX)
L="$run/laptop"
mkdir -p "$run/user" "$run/avd" "$run/home" "$L"
export ANDROID_SDK_ROOT=$VELES_ANDROID_SDK ANDROID_HOME=$VELES_ANDROID_SDK
export ANDROID_USER_HOME=$run/user ANDROID_AVD_HOME=$run/avd ANDROID_EMULATOR_HOME=$run/user
export HOME=$run/home ANDROID_ADB_SERVER_PORT=5137
emu_pid=
mock_pid=
lap_pid=
pass=0
fail=0

cleanup() {
    [ -n "$lap_pid" ] && kill "$lap_pid" 2>/dev/null || true
    [ -n "$mock_pid" ] && kill "$mock_pid" 2>/dev/null || true
    adb -s "$serial" emu kill >/dev/null 2>&1 || true
    if [ -n "$emu_pid" ]; then
        for _ in $(seq 30); do kill -0 "$emu_pid" 2>/dev/null || break; sleep 1; done
        kill -9 "$emu_pid" 2>/dev/null || true
    fi
    adb kill-server >/dev/null 2>&1 || true
    rm -rf "$run"
}
trap cleanup EXIT

a() { adb -s "$serial" "$@"; }
sh_() { a shell "$@" 2>/dev/null | tr -d '\r' || true; }
engine_pid() { sh_ pidof libveles.so; }
check() { # $1 name, $2... command
    local name=$1; shift
    if "$@" >/dev/null 2>&1; then
        echo "PASS $name"; pass=$((pass + 1))
    else
        echo "FAIL $name"; fail=$((fail + 1))
    fi
}
diag() {
    echo "--- laptop gateway log (tail 40):" >&2
    tail -40 "$run/laptop.log" >&2 || true
    echo "--- phone gateway log (tail 40):" >&2
    sh_ run-as "$pkg" cat files/gateway.log | tail -40 >&2 || true
    echo "--- mock log (tail 10):" >&2
    tail -10 "$run/mock.log" >&2 || true
}
failfast() { echo "FAIL $*"; diag; echo "RC=1"; exit 1; }

start_service() { a shell am start-foreground-service -n "$pkg/.GatewayService" >/dev/null || true; }
stop_service() { a shell am startservice -n "$pkg/.GatewayService" -a "$pkg.action.STOP" >/dev/null || true; }
# stderr included: a CLI call that prints nothing is a failure whose
# reason is ON stderr, and hiding it left the diagnosis blind
# (measured, 2026-10-01: `phome tool` came back empty and the only
# thing that explained it was the gateway log two sections away).
phome() { a shell run-as "$pkg" sh files/pf.sh "$@" 2>&1 | tr -d '\r' || true; }
# Wrap a string in LITERAL single quotes for the DEVICE shell. `adb
# shell` joins its argv with spaces and the device's shell re-parses
# that line, so a JSON argument's own double quotes are stripped and
# the CLI gets `{url:http://…}` — "--args is not valid JSON: key must
# be a string" (measured, 2026-10-01). The same wrapping the smoke
# test's ripgrep checks use. The argument must contain no single quote.
qdev() { printf "'%s'" "$1"; }

echo "== emulator"
echo no | avdmanager create avd --force -n veles-partf -k "$VELES_SYSTEM_IMAGE" \
    -p "$ANDROID_AVD_HOME/veles-partf.avd" >/dev/null
printf '%s\n' hw.ramSize=3072 disk.dataPartition.size=4G >> "$ANDROID_AVD_HOME/veles-partf.avd/config.ini"
adb start-server >/dev/null
"$ANDROID_SDK_ROOT/emulator/emulator" -avd veles-partf -port "$port" -no-window -no-audio \
    -no-boot-anim -no-snapshot -wipe-data -gpu swiftshader_indirect -accel on \
    > "$run/emulator.log" 2>&1 &
emu_pid=$!
timeout 300 adb -s "$serial" wait-for-device
booted=""
for _ in $(seq 1 300); do [ "$(sh_ getprop sys.boot_completed)" = 1 ] && { booted=1; break; }; sleep 1; done
[ -n "$booted" ] || { tail -30 "$run/emulator.log"; failfast "the emulator did not boot"; }
a root >/dev/null 2>&1 || true
timeout 60 adb -s "$serial" wait-for-device
for _ in $(seq 1 60); do [ "$(sh_ getprop sys.boot_completed)" = 1 ] && break; sleep 1; done
a forward tcp:$fwd_desk tcp:8642 >/dev/null
a forward tcp:$fwd_a2a tcp:9900 >/dev/null
# The phone → laptop direction. Everything on the laptop binds
# loopback-only (resolve_bind_host stays 127.0.0.1 without a widened
# host), and 10.0.2.2 would reach the laptop's *external* interface,
# not its loopback. adb reverse tunnels the phone's loopback to the
# laptop's — a paired tunnel, exactly like the WireGuard pair this
# rehearses, and it keeps every listener loopback-bound.
a reverse tcp:$LAP_A2A tcp:$LAP_A2A >/dev/null
a reverse tcp:$MOCK_PORT tcp:$MOCK_PORT >/dev/null

echo "== install the phone's app"
keytool -genkeypair -keystore "$run/pf.p12" -storetype PKCS12 -storepass pf-test -keypass pf-test \
    -alias pf -keyalg RSA -keysize 2048 -validity 1 -dname CN=pf >/dev/null
"$VELES_BUILD_TOOLS/apksigner" sign --ks "$run/pf.p12" --ks-pass pass:pf-test \
    --ks-key-alias pf --out "$run/veles.apk" "$VELES_APK_DIR/veles-debug-unsigned.apk"
a install "$run/veles.apk" >/dev/null
a shell pm grant "$pkg" android.permission.POST_NOTIFICATIONS >/dev/null 2>&1 || true
a shell am start -W -n "$pkg/.MainActivity" >/dev/null

# Boot the engine once with NO config: its environ names the native
# library directory, which the pushed helper script needs (the path is
# random per install).
start_service
for _ in $(seq 1 90); do [ -n "$(engine_pid)" ] && break; sleep 1; done
[ -n "$(engine_pid)" ] || failfast "the phone's engine never started"
NATIVE=$(tr '\0' '\n' < <(a exec-out cat "/proc/$(engine_pid)/environ") | sed -n 's|^VELES_TCLSH=||p' | xargs -r dirname)
case "$NATIVE" in */lib/*) ;; *) failfast "no native dir in the engine env: '$NATIVE'";; esac
echo "phone native dir: $NATIVE"
stop_service
for _ in $(seq 1 30); do [ -z "$(engine_pid)" ] && break; sleep 1; done

echo "== the laptop's signer key (before the phone's config is written)"
KEYOUT=$(printf 'pf-passphrase\npf-passphrase\n' | VELES_HOME="$L" HOME="$L" "$VELES_BIN" ticket keygen laptop 2>&1)
PUB=$(printf '%s' "$KEYOUT" | grep -o 'ed25519:[A-Za-z0-9_-]*' | head -1 || true)
[ -n "$PUB" ] || failfast "keygen printed no public key: $KEYOUT"

echo "== push the phone's config pair"
# A fresh home: the first boot minted a node.toml and a state.db under
# that id; a stale self-row would gossip a SECOND node named "phone".
# rm -rf is safe — the app creates its home at boot (Step 5.2).
sh_ run-as "$pkg" rm -rf files/home files/pf.sh
sh_ run-as "$pkg" mkdir -p files/home
# The phone's node identity, so ids are known ahead of the first boot.
printf 'id = "%s"\nname = "phone"\n' "$PHONE_ID" | a shell run-as "$pkg" sh -c "'cat > files/home/node.toml'"
cat > "$run/phone-config.toml" <<EOF
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

[node]
endpoint = "http://127.0.0.1:9900"

[a2a_agents.laptop]
url = "http://127.0.0.1:$LAP_A2A"
auth = { type = "bearer", token = "tok-p" }

[cluster]
arbiter = "11111111"
gossip_peers = ["laptop"]
memsync_peers = ["laptop"]
task_peers = ["laptop"]
storesync_peers = ["laptop"]
storesync_auto = true
failover = true
gossip_interval_secs = 2
stale_after_secs = 60

[tools.home]
"web_*" = "laptop"
"browser_*" = "laptop"

[cluster.ticket_signers.laptop]
key = "$PUB"
EOF
a shell run-as "$pkg" sh -c "'cat > files/home/config.toml'" < "$run/phone-config.toml"
# DESKTOP_BEARER_TOKEN is seeded so the desktop API does not mint one
# into this file at boot and rewrite the A2A lines out from under it
# (the d1 measurement). VELES_CHALLENGE_URL is what a relay link names;
# on a real phone it is the WireGuard address.
cat > "$run/phone-env" <<EOF
DESKTOP_BEARER_TOKEN=tok-desk-p
A2A_BEARER_TOKEN=tok-p
A2A_PEER_TOKENS=laptop:tok-l
VELES_CHALLENGE_URL=http://127.0.0.1:$fwd_desk
EOF
a shell run-as "$pkg" sh -c "'cat > files/home/.env'" < "$run/phone-env"
# The phone-side CLI helper: run-as cannot carry env or quotes through
# adb's arg joining, so the quoting lives in a file on the device.
sed "s|@NATIVE@|$NATIVE|" > "$run/pf.sh" <<'EOS'
#!/system/bin/sh
cd files/home || exit 1
H=$(pwd)
N=@NATIVE@
HOME=$H VELES_HOME=$H LD_LIBRARY_PATH=$N RUST_LOG=error NO_COLOR=1 \
    $N/libveles.so "$@"
EOS
a shell run-as "$pkg" sh -c "'cat > files/pf.sh'" < "$run/pf.sh"

echo "== the laptop's gateway (mock provider, arbiter)"
python3 "$VELES_MOCK_PROVIDER" "$MOCK_PORT" "$run/mock.log" >/dev/null 2>&1 &
mock_pid=$!
for _ in $(seq 1 50); do curl -s -m 1 "http://127.0.0.1:$MOCK_PORT/v1/models" >/dev/null 2>&1 && break; sleep 0.2; done
printf 'id = "%s"\nname = "laptop"\n' "$LAP_ID" > "$L/node.toml"
cat > "$L/config.toml" <<EOF
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

[node]
# What the PHONE dials for the arbiter: its registry endpoint, through
# the adb reverse tunnel. (On the real deployment this is the laptop's
# WireGuard address, and the phone's [a2a_agents.laptop] url is the
# same string — bearer_for_peer matches the arbiter's bearer by URL,
# not by name.)
endpoint = "http://127.0.0.1:$LAP_A2A"

[a2a_agents.phone]
url = "http://127.0.0.1:$fwd_a2a"
auth = { type = "bearer", token = "tok-l" }

[cluster]
arbiter = "11111111"
gossip_peers = ["phone"]
memsync_peers = ["phone"]
task_peers = ["phone"]
storesync_peers = ["phone"]
storesync_auto = true
failover = true
gossip_interval_secs = 2
stale_after_secs = 60
EOF
printf 'A2A_BEARER_TOKEN=tok-l\nA2A_PEER_TOKENS=phone:tok-p\n' > "$L/.env"
chmod 600 "$L/.env"
A2A_HOST=127.0.0.1 A2A_PORT=$LAP_A2A VELES_DESKTOP_PORT=$LAP_DESK VELES_DESKTOP_OPEN=1 \
    VELES_HOME="$L" HOME="$L" "$VELES_BIN" gateway run > "$run/laptop.log" 2>&1 &
lap_pid=$!
for _ in $(seq 1 60); do
    curl -sf -o /dev/null "http://127.0.0.1:$LAP_A2A/.well-known/agent-card.json" && break
    kill -0 "$lap_pid" 2>/dev/null || failfast "the laptop gateway died"
    sleep 0.5
done
cli_l() { RUST_LOG=error VELES_HOME="$L" HOME="$L" "$VELES_BIN" "$@" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' | grep -v '^20' || true; }
sql_l() { sqlite3 "$L/state.db" "$@"; }
phone_api() { curl -s -m 5 -H "Authorization: Bearer tok-desk-p" "http://127.0.0.1:$fwd_desk$1"; }
phone_api_post() { curl -s -m 60 -X POST -H "Authorization: Bearer tok-desk-p" -H 'Content-Type: application/json' -d "$2" "http://127.0.0.1:$fwd_desk$1"; }
lap_api_post() { curl -s -m 60 -X POST -H 'Content-Type: application/json' -d "$2" "http://127.0.0.1:$LAP_DESK$1"; }

echo "== the phone's gateway, with its config"
start_service
for _ in $(seq 1 90); do [ -n "$(engine_pid)" ] && break; sleep 1; done
[ -n "$(engine_pid)" ] || failfast "the phone's engine never came back"
for _ in $(seq 1 60); do
    [ "$(phone_api /api/health | grep -c '"ok"')" = 1 ] && break; sleep 1
done
phone_api /api/health | grep -q '"ok"' || failfast "the phone's desktop API never answered"

echo "== gossip converges (both registries name both nodes)"
ok=""
for _ in $(seq 1 120); do
    sleep 0.5
    cli_l nodes list | grep -q phone && phome nodes list | grep -q laptop && { ok=1; break; }
done
[ -n "$ok" ] || failfast "gossip never converged (laptop sees: $(cli_l nodes list | head -3); phone sees: $(phome nodes list | head -3))"
echo "PASS 0: two nodes up across the emulator boundary, gossip converged"

# The phone booted into an EMPTY registry, so its singleton fence
# refused ("arbiter matches no node") and failover stayed off — the
# exact thing c2 hits on a fresh node. A restart now that gossip has
# landed the arbiter's row arms the fence and takes the node lease,
# which every takeover check below needs. (The laptop IS the arbiter,
# so it armed on its first boot.)
echo "== 0b. the phone restarts knowing the arbiter"
stop_service
for _ in $(seq 1 60); do [ -z "$(engine_pid)" ] && break; sleep 1; done
[ -z "$(engine_pid)" ] || failfast "the phone's engine did not stop for its arbiter restart"
sh_ run-as "$pkg" rm -f files/gateway.log
start_service
for _ in $(seq 1 90); do [ -n "$(engine_pid)" ] && break; sleep 1; done
[ -n "$(engine_pid)" ] || failfast "the phone's engine never came back from its arbiter restart"
# The lease arrives either on the boot-time acquire (the CLI's "holding
# the node lease" line) or on the 30s RETRY after a first refusal — and
# the retry path logs only the arbiter's own "lease node:X held by X",
# never the CLI line (measured, 2026-10-01: the retry granted the lease
# at :19 and the 60s window watching for the CLI line expired anyway).
# Match either, over a window that spans two retry cycles.
LEASE=""
for _ in $(seq 1 90); do
    sleep 1
    LEASE=$(sh_ run-as "$pkg" cat files/gateway.log \
        | grep -m1 -E "holding the node lease node:$PHONE_ID|lease node:$PHONE_ID held by $PHONE_ID" || true)
    [ -n "$LEASE" ] && break
done
[ -n "$LEASE" ] || failfast "the phone never held its node lease at the arbiter: $(sh_ run-as "$pkg" cat files/gateway.log | grep -i 'fence\|lease' | head -5)"
check 0b-phone-holds-its-node-lease-at-the-arbiter grep -q "$PHONE_ID" <<<"$LEASE"

echo "== 2a. a phone session replicates to the laptop"
PSESS=$(phone_api_post /api/sessions/new '{"title":"pf-phone-session"}' | jq -r '.session_id // empty')
[ -n "$PSESS" ] || failfast "no phone session id"
phone_api_post /api/chat "{\"session_id\":\"$PSESS\",\"text\":\"hello from the phone\"}" >/dev/null
ok=""
for _ in $(seq 1 120); do
    sleep 0.5
    n=$(sqlite3 "$L/replicas/$PHONE_ID.db" "SELECT COUNT(*) FROM messages WHERE session_id='$PSESS'" 2>/dev/null || echo 0)
    [ "$n" -ge 2 ] && { ok=1; break; }
done
[ -n "$ok" ] || failfast "the phone's session never reached the laptop's replica (n=$n)"
check 2a-phone-session-replicates-to-the-laptop test "$n" -ge 2

echo "== 2b. a laptop session replicates to the phone"
LSESS=$(lap_api_post /api/sessions/new '{"title":"pf-laptop-session"}' | jq -r '.session_id // empty')
[ -n "$LSESS" ] || failfast "no laptop session id"
lap_api_post /api/chat "{\"session_id\":\"$LSESS\",\"text\":\"hello from the laptop\"}" >/dev/null
ok=""
for _ in $(seq 1 120); do
    sleep 0.5
    # The phone's replica, pulled (with its WAL, which sqlite3 then
    # reads) — the device has no sqlite3 of its own.
    a exec-out run-as "$pkg" cat "files/home/replicas/$LAP_ID.db" > "$run/pf-rep.db" 2>/dev/null || true
    a exec-out run-as "$pkg" cat "files/home/replicas/$LAP_ID.db-wal" > "$run/pf-rep.db-wal" 2>/dev/null || true
    n=$(sqlite3 "$run/pf-rep.db" "SELECT COUNT(*) FROM messages WHERE session_id='$LSESS'" 2>/dev/null || echo 0)
    [ "$n" -ge 2 ] && { ok=1; break; }
done
[ -n "$ok" ] || failfast "the laptop's session never reached the phone's replica (n=$n)"
check 2b-laptop-session-replicates-to-the-phone test "$n" -ge 2

echo "== 1. the phone session's web_extract runs on the laptop"
OUT=$(phome tool --name web_extract --args "$(qdev '{"url":"http://127.0.0.1:1/x"}')")
echo "$OUT" | grep -q '\[web_extract ran on node laptop\]' || failfast "web_extract was not re-homed to the laptop: $OUT"
grep '"event":"remote_tool"' "$L/a2a_audit.jsonl" 2>/dev/null | tail -1 | grep -q '"tool":"web_extract"' \
    || failfast "the laptop logged no remote_tool run of web_extract"
check 1-web-extract-ran-on-the-laptop grep -q 'ran on node laptop' <<<"$OUT"

echo "== 4. a ticketed call across the boundary (laptop signs, phone runs)"
OWNER='{"id":"local:owner","subject":"local:owner","role":"owner","caps":["all"],"rule_context_limit":null,"mask":null,"via":null}'
SKILL_ARGS='{"action":"create","name":"pf-e2e-skill","content":"---\nname: pf-e2e-skill\ndescription: x\n---\n\nbody\n"}'
json_escape() {
    printf '%s' "$1" | awk 'BEGIN { ORS="" }
        { gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); if (NR > 1) print "\\n"; print }'
}
send_ask() { # $1 bearer token, $2 ask JSON → the reply JSON (d1's exact wire shape)
    local ask="$2" sha b64 text
    sha=$(printf '%s' "$ask" | sha256sum | cut -d' ' -f1)
    b64=$(printf '%s' "$ask" | base64 | tr -d '\n')
    text="[veles remote tool]
one tool call over the cluster link
\`\`\`b64
sha256:$sha
$b64"
    curl -s -X POST "http://127.0.0.1:$fwd_a2a/" -H 'Content-Type: application/json' \
        -H "Authorization: Bearer $1" \
        -d "{\"jsonrpc\":\"2.0\",\"id\":\"pf\",\"method\":\"message/send\",\"params\":{\"message\":{\"role\":\"ROLE_USER\",\"parts\":[{\"kind\":\"text\",\"text\":\"$(json_escape "$text")\"}],\"messageId\":\"pf-$$-$RANDOM\",\"contextId\":\"pf-ctx-$$\"}}}"
}
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
    printf '{"status":"error","error":%s}' "$(printf '%s' "$1" | jq -c '.error // {raw: .}' 2>/dev/null || echo '"unparsable reply"')"
}
ask_json() { # $1 tool $2 args $3 principal $4 ticket
    local p="" t=""
    [ -n "$3" ] && p=",\"principal\":$(printf '%s' "$3" | jq -Rs .)"
    [ -n "$4" ] && t=",\"ticket\":$(printf '%s' "$4" | jq -Rs .)"
    printf '{"from_node":"%s","session":"pf-e2e","tool":"%s","args":%s%s%s}' \
        "$LAP_ID" "$1" "$2" "$p" "$t"
}
ASK=$(ask_json skill_manage "$SKILL_ARGS" "$OWNER" "")
R=$(reply_json "$(send_ask tok-l "$ASK")")
echo "$R" | grep -q '"status":"needs_ticket"' || failfast "the phone ran an elevated call unasked: $R"
REQ=$(printf '%s' "$R" | jq -r '.request')
case "$REQ" in veles-ticket-request:1.*) ;; *) failfast "no request token in the refusal: $R";; esac
TICKET=$(printf 'pf-passphrase\n' | VELES_HOME="$L" HOME="$L" "$VELES_BIN" ticket sign "$REQ" --yes --key laptop 2>/dev/null \
    | grep -o 'veles-ticket:1\.[A-Za-z0-9_.-]*' | head -1 || true)
[ -n "$TICKET" ] || failfast "signing the phone's request failed"
ASK=$(ask_json skill_manage "$SKILL_ARGS" "$OWNER" "$TICKET")
R=$(reply_json "$(send_ask tok-l "$ASK")")
echo "$R" | grep -q '"status":"ok"' || failfast "the ticketed call did not run on the phone: $R"
sh_ run-as "$pkg" cat files/home/skills/pf-e2e-skill/SKILL.md | grep -q 'pf-e2e-skill' \
    || failfast "the skill file is not on the phone — the call did not really run there"
check 4a-ticketed-call-ran-on-the-phone grep -q '"status":"ok"' <<<"$R"
R=$(reply_json "$(send_ask tok-l "$ASK")")
echo "$R" | grep -q 'already used' || failfast "a replayed ticket ran again: $R"
check 4b-one-call-per-ticket grep -q 'already used' <<<"$R"

echo "== 3. stop the phone: the laptop takes its session over after the lease"
stop_service
for _ in $(seq 1 60); do [ -z "$(engine_pid)" ] && break; sleep 1; done
[ -z "$(engine_pid)" ] || failfast "the phone's engine did not stop"
ok=""
for _ in $(seq 1 36); do
    sleep 5
    OUT=$(cli_l session takeover "$PSESS")
    echo "$OUT" | grep -q "took $PSESS over" && { ok=1; break; }
    echo "$OUT" | grep -q "is alive" || failfast "unexpected takeover answer: $OUT"
done
[ -n "$ok" ] || failfast "the laptop never took the phone's session over: $OUT"
P=$(sql_l "SELECT system_prompt FROM session_prompt WHERE session_id='$PSESS'")
[ -n "$P" ] || failfast "the frozen prompt did not come along with the takeover"
N=$(sql_l "SELECT COUNT(*) FROM messages WHERE session_id='$PSESS' AND role='user'")
[ "$N" -ge 1 ] || failfast "the takeover carried no history ($N user messages)"
check 3a-laptop-took-over-after-the-lease grep -q "took $PSESS over" <<<"$OUT"

echo "== 3b. the phone returns and yields (one writer)"
start_service
for _ in $(seq 1 90); do [ -n "$(engine_pid)" ] && break; sleep 1; done
[ -n "$(engine_pid)" ] || failfast "the phone's engine never came back"
ok=""
for _ in $(seq 1 120); do
    sleep 1
    sh_ run-as "$pkg" cat files/gateway.log | grep -q "failover: $PSESS: yielded" && { ok=1; break; }
done
[ -n "$ok" ] || failfast "the phone never yielded the taken-over session"
OUT_P=$(phome session owner "$PSESS")
echo "$OUT_P" | grep -q "RETIRED" || failfast "the phone still claims the session: $OUT_P"
OUT_L=$(cli_l session owner "$PSESS")
echo "$OUT_L" | grep -q "$LAP_ID" || failfast "the laptop does not own the session: $OUT_L"
check 3b-phone-yielded-and-the-laptop-owns-it grep -q "$LAP_ID" <<<"$OUT_L"

echo ""
echo "== $pass passed, $fail failed"
if [ "$fail" = 0 ]; then
    echo "RC=0"
else
    diag
    echo "RC=1"
    exit 1
fi
