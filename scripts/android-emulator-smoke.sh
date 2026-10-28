# android-emulator-smoke.sh — the APK, installed and run on an emulator.
#
#   nix run .#android-emulator-smoke
#
# Boots a headless x86_64 Android 16 emulator under KVM from a
# throwaway AVD in /dev/shm, with a 16 KB page kernel (the
# google_apis_ps16k image): Google Play refuses native code that cannot
# load there, and an engine linked for 4 KB pages fails to start at all.
# Installs the x86_64 debug APK, signed with a throwaway key, and checks:
#
#   install  opening the app once takes it out of Android's "stopped"
#            state;
#   start    starting the service runs the engine as the app, in a
#            foreground service of type specialUse, and the desktop API
#            answers /api/health (through adb forward); its log is
#            plain text;
#   restart  an engine that dies is started again by the service;
#            starts while it runs (every open of the app is one) leave
#            the engine and its supervisor as they were;
#   stop     Stop ends the engine and the service;
#   boot     a reboot starts nothing: the gateway runs while the app is
#            used and stops five minutes after (idle, checked there);
#   update   `adb install -r` over a running gateway starts nothing
#            either;
#   desktop  the frontend (veles-desktop on AndroWish's Tk) runs in its
#            own process, seeds its instance registry with the gateway
#            beside it and attaches to it, and starts no second engine.
#   ripgrep  the APK's librg.so runs, searches the app's own storage, and
#            executes through the files/bin/rg symlink terminal commands
#            use (the W^X question the plan left to measurement).
#   back and notifications
#            Back sends the desktop to the background and leaves it
#            running; there, a finished turn and an agent's question
#            are notified, and a tap on the question brings it back; the
#            agent's own terminal finds rg (turns by the mock provider).
#
# Two entry points: `nix run .#android-emulator-smoke` (VELES_APK_DIR,
# the plain APK) and `nix run .#android-aab-smoke` (VELES_AAB_DIR, the
# store's bundle — the script installs the split APKs bundletool makes
# for this device, with a throwaway key, and runs every check on them).
#
# The flake (apps.android-emulator-smoke) runs this with the SDK, adb and
# a JDK pinned. The last line is RC=0 when every check passed. It uses
# its own adb server port and emulator port, so a phone or another
# emulator on this machine is left alone.

set -euo pipefail

# Either VELES_APK_DIR (the plain APK) or VELES_AAB_DIR (the store's
# bundle, made into this device's split APKs below) — never both, never
# neither.
if [ -n "${VELES_AAB_DIR:-}" ] && [ -n "${VELES_APK_DIR:-}" ]; then
    echo "VELES_AAB_DIR and VELES_APK_DIR are mutually exclusive" >&2
    exit 2
fi
if [ -z "${VELES_AAB_DIR:-}" ]; then
    : "${VELES_APK_DIR:?run this through nix run .#android-emulator-smoke}"
fi
: "${VELES_ANDROID_SDK:?run this through nix run .#android-emulator-smoke}"
: "${VELES_BUILD_TOOLS:?run this through nix run .#android-emulator-smoke}"
: "${VELES_SYSTEM_IMAGE:?run this through nix run .#android-emulator-smoke}"
: "${VELES_MOCK_PROVIDER:?run this through nix run .#android-emulator-smoke}"

pkg=chat.janitor.veles
port=5580
serial=emulator-$port
fwd=18642
run=$(mktemp -d /dev/shm/veles-emu.XXXXXX)
export ANDROID_SDK_ROOT=$VELES_ANDROID_SDK ANDROID_HOME=$VELES_ANDROID_SDK
export ANDROID_USER_HOME=$run/user ANDROID_AVD_HOME=$run/avd ANDROID_EMULATOR_HOME=$run/user
export HOME=$run/home ANDROID_ADB_SERVER_PORT=5137
# No USB: this run's server has a throwaway key, and a phone plugged in
# asked its person to allow it ("Allow USB debugging?") on every run
# (2026-10-12). Emulators are reached over local TCP, which stays on.
export ADB_USB=0
mkdir -p "$ANDROID_USER_HOME" "$ANDROID_AVD_HOME" "$HOME"
emu_pid=
mock_pid=
pass=0
fail=0

cleanup() {
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

# Every helper that reads the device answers empty rather than failing:
# under set -e a missing engine would otherwise end the run with no RC.
a() { adb -s "$serial" "$@"; }
sh_() { a shell "$@" 2>/dev/null | tr -d '\r' || true; }
engine_pid() { sh_ pidof libveles.so; }
engine_up() { [ -n "$(engine_pid)" ]; }
engine_down() { [ -z "$(engine_pid)" ]; }
# Two engines would share state.db and fight over the port: an app
# process that dies must take its engine with it.
one_engine() { [ "$(engine_pid | wc -w)" -eq 1 ]; }
healthy() {
    [ "$(curl -s -m 3 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$fwd/api/health")" = 200 ]
}
# wait_for SECONDS COMMAND...: true as soon as COMMAND is.
wait_for() {
    local deadline=$((SECONDS + $1))
    shift
    until "$@"; do
        [ $SECONDS -ge $deadline ] && return 1
        sleep 1
    done
}
check() {
    local name=$1
    shift
    if "$@"; then
        echo "PASS $name"
        pass=$((pass + 1))
    else
        echo "FAIL $name"
        fail=$((fail + 1))
    fi
}
not() { ! "$@"; }
booted() { [ "$(sh_ getprop sys.boot_completed)" = 1 ]; }
# Every step returns on failure by hand: called as `if ! boot_wait`,
# the function runs with set -e off.
boot_wait() {
    timeout 300 adb -s "$serial" wait-for-device || return 1
    wait_for 300 booted || return 1
    # Root, to start the (unexported) service the way the app's button
    # does, and to kill the engine; adbd restarts as root — and the
    # request's own connection can close with it ("adb: unable to
    # connect for root: closed", every run after a reboot on 2026-10-09),
    # so the verdict is whether the NEXT shell is root, retried, not
    # whether the request returned.
    local try
    for try in 1 2 3 4 5; do
        a root >/dev/null 2>&1 || true
        timeout 60 adb -s "$serial" wait-for-device || return 1
        [ "$(sh_ id -u 2>/dev/null | tr -d '\r')" = 0 ] && break
        [ "$try" = 5 ] && return 1
        sleep 2
    done
    wait_for 60 booted || return 1
    # A boot fills the default 256 KB buffers within seconds; a failure's
    # evidence must outlive it.
    a logcat -G 16M >/dev/null 2>&1 || true
    a forward "tcp:$fwd" tcp:8642 >/dev/null
}
# Right after `adb reboot` the old boot still answers boot_completed=1:
# wait for it to go away first.
reboot_wait() {
    a reboot || true
    timeout 120 adb -s "$serial" wait-for-disconnect || true
    boot_wait
}
start_service() { a shell am start-foreground-service -n "$pkg/.GatewayService" >/dev/null || true; }
stop_service() {
    a shell am startservice -n "$pkg/.GatewayService" -a "$pkg.action.STOP" >/dev/null || true
}
applog() { sh_ run-as "$pkg" cat files/gateway.log; }
# What Start and Stop stored: the toggle the activity shows. Nothing
# reads it at boot any more; a reboot starts no engine.
prefs() { sh_ run-as "$pkg" cat shared_prefs/gateway.xml | grep -o 'name="enabled" value="[a-z]*"'; }
stopped_flag() { sh_ dumpsys package "$pkg" | grep -o -m1 'stopped=[a-z]*'; }
# What Android did with the app, when a start it should have made did
# not happen: its stopped flag, the process and foreground-service
# events, and the app's files.
diagnose() {
    echo "--- $1: $(stopped_flag)"
    a shell run-as "$pkg" ls -la files 2>&1 | tr -d '\r' || true
    sh_ logcat -d -b all \
        | grep -E "am_proc_start.*$pkg|am_proc_died.*$pkg|am_kill.*$pkg|Background started FGS.*$pkg|am_foreground_service_(start|denied|stop).*$pkg|FATAL|Exception.*$pkg|$pkg.*(denied|not allowed|stopped)|BOOT_COMPLETED|PACKAGE_REPLACED" \
        | grep -v -E 'avc: +denied' | tail -25 | cut -c1-250
}

echo "== AVD and emulator ($VELES_SYSTEM_IMAGE)"
# Every adb server scans the local emulator ports, so one already running
# on the default port (a phone's, 5037) takes the emulator's connection
# and this run's server on 5137 sees it "offline": "the emulator did not
# boot" twice on 2026-10-12, with the phone's server up. Say so instead.
if (exec 3<>/dev/tcp/127.0.0.1/5037) 2>/dev/null; then
    echo "another adb server is running on 5037 and would take the emulator from this run:" >&2
    echo "  stop it first (adb kill-server); it starts again the next time adb is used" >&2
    echo "RC=1"
    exit 1
fi
echo no | avdmanager create avd --force -n veles-smoke -k "$VELES_SYSTEM_IMAGE" \
    -p "$ANDROID_AVD_HOME/veles-smoke.avd" >/dev/null
printf '%s\n' hw.ramSize=3072 disk.dataPartition.size=4G >> "$ANDROID_AVD_HOME/veles-smoke.avd/config.ini"
adb start-server >/dev/null
"$ANDROID_SDK_ROOT/emulator/emulator" -avd veles-smoke -port "$port" -no-window -no-audio \
    -no-boot-anim -no-snapshot -wipe-data -gpu swiftshader_indirect -accel on \
    > "$run/emulator.log" 2>&1 &
emu_pid=$!
if ! boot_wait; then
    echo "the emulator did not boot; its log:" >&2
    tail -40 "$run/emulator.log" >&2
    echo "RC=1"
    exit 1
fi
echo "page size: $(sh_ getconf PAGE_SIZE), Android $(sh_ getprop ro.build.version.release)"

echo "== install"
keytool -genkeypair -keystore "$run/smoke.p12" -storetype PKCS12 -storepass smoke-test -keypass smoke-test \
    -alias smoke -keyalg RSA -keysize 2048 -validity 1 -dname CN=smoke >/dev/null
if [ -n "${VELES_AAB_DIR:-}" ]; then
    # From the store's bundle to what Play installs on THIS device:
    # bundletool's split APKs, chosen for the device's own spec, signed
    # with the throwaway key. Not the universal APK, which bundletool
    # builds differently: it kept extractNativeLibs=true while the
    # device's splits had it false, and a store install's engine (run
    # from the extracted library directory) never started, with this
    # smoke green (measured, 2026-10-01). --aapt2 must point at the
    # SDK's: bundletool's OWN embedded aapt2 cannot exec on NixOS (it
    # extracts to /tmp and dies 127).
    echo "aab: the device's split APKs from $VELES_AAB_DIR/veles-debug-unsigned.aab"
    bundletool build-apks --bundle="$VELES_AAB_DIR/veles-debug-unsigned.aab" \
        --output="$run/veles.apks" \
        --ks="$run/smoke.p12" --ks-pass=pass:smoke-test --ks-key-alias=smoke \
        --aapt2="$VELES_BUILD_TOOLS/aapt2" --overwrite >/dev/null
    bundletool get-device-spec --adb="$(command -v adb)" --device-id="$serial" \
        --output="$run/device.json" --overwrite >/dev/null
    bundletool extract-apks --apks="$run/veles.apks" --device-spec="$run/device.json" \
        --output-dir="$run/splits" >/dev/null
    install_app() { a install-multiple "$@" "$run"/splits/*.apk >/dev/null; }
    app_size() { du -ch "$run"/splits/*.apk | tail -1 | cut -f1; }
else
    "$VELES_BUILD_TOOLS/apksigner" sign --ks "$run/smoke.p12" --ks-pass pass:smoke-test \
        --ks-key-alias smoke --out "$run/veles.apk" "$VELES_APK_DIR/veles-debug-unsigned.apk"
    install_app() { a install "$@" "$run/veles.apk" >/dev/null; }
    app_size() { du -h "$run/veles.apk" | cut -f1; }
fi
install_app
a shell pm grant "$pkg" android.permission.POST_NOTIFICATIONS
echo "APK $(app_size), installed $(stopped_flag)"
# A package nobody has opened is "stopped": Android delivers it no
# broadcasts, BOOT_COMPLETED and MY_PACKAGE_REPLACED included. Opening
# it ends that, as a person does to press Start, and as
# `nix run .#android-install` does. Android writes the change to disk
# about ten seconds later, and a reboot before then forgets it: the
# package boots stopped again, and every check after the reboot fails
# for a reason no phone has (emulator, 2026-09-30).
a shell am start -W -n "$pkg/.MainActivity" >/dev/null
restrictions() {
    sh_ abx2xml /data/system/users/0/package-restrictions.xml - | grep -m1 "<pkg name=\"$pkg\""
}
unstopped() {
    local disk
    disk=$(restrictions)
    [ "$(stopped_flag)" = stopped=false ] && [ -n "$disk" ] && ! grep -q 'stopped="true"' <<<"$disk"
}
wait_for 30 unstopped || true
check install-opened-is-not-stopped unstopped

echo "== start"
start_service
wait_for 60 engine_up || true
wait_for 60 healthy || true
check start-engine-runs engine_up
check start-health healthy
services=$(sh_ dumpsys activity services "$pkg")
check start-foreground grep -q 'isForeground=true' <<<"$services"
echo "dumpsys: $(grep -m1 -o 'isForeground=.*types=[0-9A-Fa-fx]*' <<<"$services" || true)"
# ServiceRecord.dump: "isForeground=true foregroundId=1 types=0x40000000".
check start-special-use grep -q -E 'types=(0x)?0*40000000' <<<"$services"
check start-one-engine one_engine
app_uid=$(sh_ stat -c %u "/data/data/$pkg")
engine_uid=$(sh_ stat -c %u "/proc/$(engine_pid)")
same_uid() { [ -n "$app_uid" ] && [ "$app_uid" = "$engine_uid" ]; }
check start-runs-as-the-app same_uid
check start-log-reaches-the-app grep -q 'desktop API listening' <<<"$(applog)"
# The activity shows the log as text; color codes would show as junk.
check start-log-is-plain-text not grep -q $'\033' <<<"$(applog)"
echo "engine RSS: $(sh_ ps -o RSS -p "$(engine_pid)" | tail -1) KiB"

echo "== restart"
old=$(engine_pid)
[ -n "$old" ] && a shell kill -9 "$old"
new_engine() { local p; p=$(engine_pid); [ -n "$p" ] && [ "$p" != "$old" ]; }
wait_for 60 new_engine || true
wait_for 60 healthy || true
check restart-engine-back new_engine
check restart-health healthy
check restart-logged grep -q 'restarting in' <<<"$(applog)"
check restart-one-engine one_engine

echo "== start while running starts no second engine"
# Every open of the app is a start. Three, because the supervisor of
# 2026-10-04 took each start as an interrupt: the first it waited out,
# the second made it leave the engine unsupervised and the service go.
old=$(engine_pid)
start_service
start_service
start_service
sleep 5
same_engine() { [ -n "$old" ] && [ "$(engine_pid)" = "$old" ]; }
service_up() { sh_ dumpsys activity services "$pkg" | grep -q ServiceRecord; }
check start-running-one-engine one_engine
check start-running-same-engine same_engine
check start-running-service-stays service_up
check start-running-supervised not grep -q 'supervisor done' <<<"$(applog)"

echo "== stop"
stop_service
wait_for 30 engine_down || true
check stop-engine-gone engine_down
not_running() { ! sh_ dumpsys activity services "$pkg" | grep -q ServiceRecord; }
wait_for 15 not_running || true
check stop-service-gone not_running

echo "== boot (a reboot starts nothing)"
start_service
wait_for 60 engine_up || true
reboot_wait || { echo "FAIL the emulator did not come back from a reboot"; echo "RC=1"; exit 1; }
sleep 20
check boot-no-engine engine_down
# Opening the app again is how it comes back.
a shell am start -W -n "$pkg/.MainActivity" >/dev/null
start_service
wait_for 60 engine_up || true
wait_for 60 healthy || true
check boot-reopened-starts engine_up

echo "== update (an update starts nothing)"
install_app -r
sleep 15
check update-no-engine engine_down
a shell am start -W -n "$pkg/.MainActivity" >/dev/null
start_service
wait_for 60 engine_up || true
wait_for 60 healthy || true
check update-reopened-starts engine_up

echo "== desktop"
# Its first run seeds the registry with the gateway in the same app and
# attaches; attached, it keeps a connection open to the desktop API
# (an event stream). netstat -p: "... 127.0.0.1:8642 ESTABLISHED <pid>/<name>".
desk_pid() { sh_ pidof "$pkg:desktop"; }
desk_up() { [ -n "$(desk_pid)" ]; }
registry() { sh_ run-as "$pkg" cat files/.config/veles-desktop/instances.json; }
seeded() {
    local r
    r=$(registry)
    grep -q '"phone": {"url": "http://127.0.0.1:8642"}' <<<"$r" && grep -q '"last": "phone"' <<<"$r"
}
attached() {
    local p
    p=$(desk_pid)
    [ -n "$p" ] && sh_ netstat -tnp | grep -q -E "127\.0\.0\.1:8642 +ESTABLISHED +$p/"
}
no_crash() { ! sh_ logcat -d -b crash | grep -q "$pkg"; }
a logcat -c
a shell am start -W -n "$pkg/.DesktopActivity" >/dev/null
wait_for 60 seeded || true
wait_for 60 attached || true
check desktop-runs desk_up
check desktop-seeds-the-registry seeded
check desktop-attached attached
check desktop-one-engine one_engine
check desktop-no-crash no_crash
echo "desktop RSS: $(sh_ ps -o RSS -p "$(desk_pid)" | tail -1) KiB"

echo "== device"
# The desktop offers the phone's capabilities to the agent's `device` tool
# (veles_core::devices, PLAN-sessions-and-phone.md P1): its catalog reaches
# the gateway, and a call runs on AndroWish's borg and answers with what
# the phone says about itself, or posts a real notification.
dtoken=$(sh_ run-as "$pkg" cat files/home/.env | sed -n 's/^DESKTOP_BEARER_TOKEN=//p' | tr -d "\"'")
dapi() { curl -s -m 30 -H "Authorization: Bearer $dtoken" "$@" || true; }
dcall() {
    dapi -X POST -H 'Content-Type: application/json' -d "$1" "http://127.0.0.1:$fwd/api/device/call"
}
phone_listed() { dapi "http://127.0.0.1:$fwd/api/device/list" | grep -q '"name":"phone"'; }
wait_for 60 phone_listed || true
check device-phone-listed phone_listed
catalog=$(dapi "http://127.0.0.1:$fwd/api/device/list")
check device-phone-offers-speak grep -q '"name":"speak"' <<<"$catalog"
check device-phone-offers-location grep -q '"name":"location"' <<<"$catalog"
info=$(dcall '{"device":"phone","action":"info","args":{}}')
echo "device info: $(head -c 300 <<<"$info")"
check device-info-answers grep -q '"ok":true' <<<"$info"
check device-info-is-the-phones grep -q -i 'sdk' <<<"$info"
# The network field reads "none" without ACCESS_NETWORK_STATE, whatever
# the phone is on (the real phone on LTE, 2026-10-11).
info_knows_the_network() { grep -q 'network ' <<<"$info" && ! grep -q 'network none' <<<"$info"; }
check device-info-knows-the-network info_knows_the_network
note=$(dcall '{"device":"phone","action":"notify","args":{"title":"veles device check","text":"from the bridge"}}')
check device-notify-answers grep -q '"ok":true' <<<"$note"
device_notified() { sh_ dumpsys notification --noredact | grep -q 'veles device check'; }
wait_for 10 device_notified || true
check device-notification-posted device_notified
if ! device_notified; then
    echo "--- why no notification: logcat (AndroWish, NotificationService) and the app's channels"
    sh_ logcat -d | grep -i -E 'notif|AndroWish|borg|Exception|FLAG_(IM)?MUTABLE' | grep -i -E "$pkg|AndroWish|channel|borg|Exception|MUTABLE" | tail -25 || true
    sh_ dumpsys notification --noredact | grep -i -E "$pkg|borg" | head -15 || true
fi

echo "== phone page"
# DesktopActivity writes what only Java can see (notification and battery
# settings, the location grant, free space) on every resume, for the
# desktop's Phone page (veles-desktop/lib/phone.tcl). The desktop has
# resumed by now.
pstatus=$(sh_ run-as "$pkg" cat files/phone-status)
echo "phone-status: $(tr '\n' ' ' <<<"$pstatus")"
phone_status_written() {
    grep -q '^notifications=[01]$' <<<"$pstatus" \
        && grep -q '^background=[01]$' <<<"$pstatus" \
        && grep -q '^location=\(precise\|approximate\|off\)$' <<<"$pstatus" \
        && grep -q '^free=[0-9][0-9]*$' <<<"$pstatus"
}
check phone-status-written phone_status_written

echo "== ripgrep"
# Read the rg the engine was TOLD about out of the running process's own
# environment (Engine.java's VELES_RG) rather than reconstructing the
# /data/app path, which is random per install.
rg_bin=$(tr '\0' '\n' < <(sh_ cat "/proc/$(engine_pid)/environ") | sed -n 's/^VELES_RG=//p')
echo "VELES_RG: $rg_bin"
check rg-named-in-the-engine-env grep -q '/lib/x86_64/librg.so$' <<<"$rg_bin"
# Every predicate below captures the device's answer into a variable and
# tests THAT: a `grep -q` at the end of a `check` line would pipe the
# checker's own PASS/FAIL into grep, and grep -q closing the pipe early
# is a SIGPIPE, which pipefail turns into a failed command and set -e
# into an aborted run (measured: the script died here, RC=141).
rg_version() { local v; v=$(sh_ "$rg_bin" --version); grep -q '^ripgrep ' <<<"$v"; }
check rg-runs rg_version
# A real search, through the app's own storage: the file is the app's,
# so this also proves the SELinux context lets it read there. The
# device-side command is ONE single-quoted argument to sh -c — `adb
# shell` joins its argv with spaces and the device's shell re-parses
# that line, so an unquoted `;` there splits the payload and sh -c gets
# only its first word (which wrote an empty probe file and found
# nothing). $rg_bin expands HERE, inside the local double quotes.
rg_finds() {
    local out
    # -H: rg omits the filename when searching ONE file, and the
    # assertion wants it named (measured on the emulator, 2026-10-01:
    # without -H the line is just `1:hello-neon`).
    out=$(sh_ run-as "$pkg" sh -c "'echo hello-neon > files/rg-probe.txt; $rg_bin -nH --no-heading hello-neon files/rg-probe.txt'")
    grep -q 'rg-probe.txt:1:hello-neon' <<<"$out"
}
check rg-searches-app-storage rg_finds
# Terminal commands get `rg` on PATH through files/bin/rg, a symlink
# into the native library directory (Android extracts executables only
# from there). Whether an exec THROUGH that link survives the app-data
# W^X rule is what the plan left to measure here, not to assume.
rg_link() { case $(sh_ run-as "$pkg" readlink files/bin/rg) in *librg.so) return 0 ;; *) return 1 ;; esac; }
check rg-symlink-on-path rg_link
rg_via_path() {
    local out
    out=$(sh_ run-as "$pkg" sh -c "'cd files && ./bin/rg --version'")
    grep -q '^ripgrep ' <<<"$out"
}
check rg-executes-through-the-symlink rg_via_path

echo "== back and notifications"
# Back sends the desktop to the background and leaves it running; while
# it is there, a finished turn and an agent's question arrive as
# notifications, and a tap on the question brings the desktop back on it
# (USER 2026-09-30, of what the phone should notify: "Asks and finished
# turns"). Turns need a model: the mock provider on this machine, through
# adb reverse. The first turn has the AGENT's terminal run `rg`: the
# engine runs a command as the app itself, in the app's SELinux domain,
# which `run-as` above is not, so this is the check that the agent's own
# commands find ripgrep on PATH.
mock_port=18999
python3 "$VELES_MOCK_PROVIDER" "$mock_port" "$run/mock.log" >/dev/null 2>&1 &
mock_pid=$!
a reverse "tcp:$mock_port" "tcp:$mock_port" >/dev/null
cat > "$run/config.toml" <<EOF
[model]
provider = "mock"
default = "mock-model"

[[custom_providers]]
name = "mock"
base_url = "http://127.0.0.1:$mock_port/v1"
api_mode = "chat_completions"
api_key = "none"
models = ["mock-model"]

[approvals]
mode = "manual"
EOF
a shell run-as "$pkg" sh -c "'cat > files/home/config.toml'" < "$run/config.toml"
old=$(engine_pid)
[ -n "$old" ] && a shell kill -9 "$old"
wait_for 60 new_engine || true
wait_for 60 healthy || true
# The app's notifier (in its main process) listens to the new engine:
# a turn that ends while it is reconnecting is not sent again.
notifier_up() {
    local p
    p=$(sh_ pidof "$pkg")
    [ -n "$p" ] && sh_ netstat -tnp | grep -q -E "127\.0\.0\.1:8642 +ESTABLISHED +$p/"
}
wait_for 20 notifier_up || true
check notifier-reconnects-after-a-restart notifier_up
token=$(sh_ run-as "$pkg" cat files/home/.env | sed -n 's/^DESKTOP_BEARER_TOKEN=//p' | tr -d "\"'")
api() { curl -s -m 10 -H "Authorization: Bearer $token" "http://127.0.0.1:$fwd$1" || true; }
api_post() {
    curl -s -m 10 -X POST -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
        -d "$2" "http://127.0.0.1:$fwd$1" || true
}
new_session() {
    api_post /api/sessions/new "{\"title\":\"$1\"}" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin).get("session_id",""))' || true
}
# A turn's stream stays open, in the background, for as long as it runs.
chat() {
    curl -s -N -m 300 -X POST -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
        -d "{\"session_id\":\"$1\",\"text\":\"$2\"}" "http://127.0.0.1:$fwd/api/chat" >/dev/null 2>&1 &
}
# One grep: a second one closing the pipe early would SIGPIPE the first,
# and pipefail would make a match a failure.
in_front() { sh_ dumpsys activity activities | grep -q "topResumedActivity=.*$pkg/.DesktopActivity"; }
# A NotificationRecord's key: 0|<package>|<id>|<tag>|<uid>. $2 is the id,
# any if not given.
notified() { sh_ dumpsys notification --noredact | grep -q -E "key=0\|$pkg\|${2:--?[0-9]+}\|$1\|"; }
# Notifier's id for a session's turn: the session id's String.hashCode().
java_hash() {
    python3 -c 'import sys
h = 0
for c in sys.argv[1]: h = (31 * h + ord(c)) & 0xFFFFFFFF
print(h - (1 << 32) if h >= 1 << 31 else h)' "$1"
}
# Configured now, so the desktop opens without the setup wizard.
p=$(desk_pid)
[ -n "$p" ] && a shell kill "$p"
sleep 2
a shell am start -W -n "$pkg/.DesktopActivity" >/dev/null
wait_for 30 attached || true
wait_for 30 in_front || true
desk_before=$(desk_pid)
# Up to three presses: Android's own back closes the soft keyboard first
# (on the emulator AndroWish opens it at start for the PS/2 keyboard the
# x86 machine always has, hw.keyboard=no or not).
for _ in 1 2 3; do
    in_front || break
    a shell input keyevent KEYCODE_BACK
    sleep 2
done
check back-backgrounds-the-desktop not in_front
sleep 3
same_desktop() { [ -n "$desk_before" ] && [ "$(desk_pid)" = "$desk_before" ]; }
check back-keeps-the-desktop-running same_desktop

rg_session=$(new_session smoke-rg)
chat "$rg_session" "[mock:rg] which ripgrep is this?"
ran_rg() { api "/api/sessions/$rg_session/history" | grep -q 'ripgrep [0-9]'; }
wait_for 60 ran_rg || true
check rg-on-the-agent-s-own-path ran_rg
turn_noted() { notified turn "$(java_hash "$rg_session")"; }
wait_for 30 turn_noted || true
check finished-turn-notified-in-the-background turn_noted

ask_session=$(new_session smoke-ask)
chat "$ask_session" "[mock:approval] clean the build"
ask_noted() { notified ask; }
wait_for 60 ask_noted || true
check ask-notified-in-the-background ask_noted
# A person's tap: open the shade, find the notification by its title
# (the session's), tap its middle.
a shell cmd statusbar expand-notifications >/dev/null 2>&1 || true
sleep 2
a shell uiautomator dump /sdcard/ui.xml >/dev/null 2>&1 || true
x1='' y1='' x2='' y2=''
read -r x1 y1 x2 y2 < <(sh_ cat /sdcard/ui.xml \
    | grep -o 'text="smoke-ask"[^>]*bounds="\[[0-9]*,[0-9]*\]\[[0-9]*,[0-9]*\]"' \
    | sed -E 's/.*bounds="\[([0-9]+),([0-9]+)\]\[([0-9]+),([0-9]+)\]".*/\1 \2 \3 \4/' | head -1) || true
[ -n "$y2" ] && a shell input tap $(((x1 + x2) / 2)) $(((y1 + y2) / 2))
wait_for 20 in_front || true
check ask-tap-opens-the-desktop in_front
cleared() { ! notified ask; }
# Android 16 bundles an app's notifications into a group of its own
# (g:Aggregate_*), and the tap lands on the bundle: that opens the app
# with no notification's intent and takes down only the bundle, so the
# ask's own notification stays until it is answered (measured 2026-10-12:
# posted 5 s before the tap, still there after, the same ask, and the
# window was opened with no session). A tap on the ask itself sends its
# session, and the window then takes that session's notifications down
# (DesktopActivity.clearSessionNotifications): the same intent here.
a shell am start -n "$pkg/.DesktopActivity" --es session "$ask_session" >/dev/null 2>&1 || true
wait_for 10 cleared || true
check ask-opened-session-clears-it cleared
if notified ask; then
    echo "  what the window took down when it opened:"
    sh_ logcat -d -s veles-desktop | tail -4 | sed 's/^/    /'
    sh_ cmd notification list | grep "$pkg" | sed 's/^/    /'
fi
# Answered, so the turn ends.
ask_id=$(api /api/asks | python3 -c 'import json,sys; a=json.load(sys.stdin).get("asks",[]); print(a[0]["id"] if a else "")' || true)
[ -n "$ask_id" ] && api_post "/api/approvals/$ask_id/decide" '{"choice":"deny"}' >/dev/null

echo "== apps: a safe interpreter with Tk (PLAN-sessions-and-phone.md A4.0)"
# Apps the agent writes run in a safe interpreter with Tk (USER decision
# D6). Whether that works on AndroWish's sdl2tk is this check: three ways
# to give a child interpreter Tk, each asked to build a button, run its
# command, and keep exec/open/socket out of reach. The probe goes in the
# desktop's patches file, sourced at its start, and writes what it found.
cat > "$run/spike.tcl" <<'TCL'
after 4000 {
    set out [file join [file dirname $::vhotpatch::patch_file] spike.txt]
    set r {}
    proc ::spike_try {r label script} {
        upvar 1 $r rr
        if {[catch {uplevel #0 $script} res]} {
            lappend rr "$label: ERROR $res"
        } else {
            lappend rr "$label: $res"
        }
    }
    spike_try r embed {
        toplevel .spk1 -container 1
        set s [::safe::interpCreate spk1]
        ::safe::loadTk $s -use [winfo id .spk1]
        $s eval {set ::n 0; button .b -text x -command {incr ::n}; pack .b}
        update
        $s eval {.b invoke; .b invoke}
        list ok n=[$s eval {set ::n}] exec=[catch {$s eval {exec ls}}] open=[catch {$s eval {open /etc/hosts}}] socket=[catch {$s eval {socket localhost 1}}]
    }
    spike_try r own {
        set s [::safe::interpCreate spk2]
        ::safe::loadTk $s
        $s eval {set ::n 0; button .b -text y -command {incr ::n}; pack .b}
        update
        $s eval {.b invoke}
        list ok n=[$s eval {set ::n}] exec=[catch {$s eval {exec ls}}] geom=[$s eval {wm geometry .}]
    }
    spike_try r child {
        interp create spk3
        load {} Tk spk3
        spk3 eval {set ::n 0; button .b -text z -command {incr ::n}; pack .b}
        update
        spk3 eval {.b invoke}
        list ok n=[spk3 eval {set ::n}]
    }
    spike_try r limit {
        set s [::safe::interpCreate spk4]
        interp limit $s time -seconds [expr {[clock seconds] + 1}]
        list ok caught=[catch {$s eval {while 1 {}}}]
    }
    set f [open $out w]
    puts $f [join $r \n]
    close $f
}
TCL
a shell run-as "$pkg" sh -c "'mkdir -p files/home/desktop && cat > files/home/desktop/patches.tcl'" < "$run/spike.tcl"
p=$(desk_pid)
[ -n "$p" ] && a shell kill "$p"
sleep 2
a shell am start -W -n "$pkg/.DesktopActivity" >/dev/null
spike_done() { sh_ run-as "$pkg" cat files/home/desktop/spike.txt | grep -q '^limit:'; }
wait_for 60 spike_done || true
spike=$(sh_ run-as "$pkg" cat files/home/desktop/spike.txt)
while IFS= read -r l; do echo "  spike $l"; done <<<"$spike"
# Embedded in a container is the way lib/apps.tcl runs an app. (A safe
# interpreter with a toplevel of its own works too, but has no `wm` on
# AndroWish — measured 2026-10-12 — so apps must not call it.)
check apps-safe-interp-gets-tk grep -q '^embed: ok n=2' <<<"$spike"
check apps-safe-interp-hides-exec grep -q '^embed: ok n=2 exec=1 open=1 socket=1' <<<"$spike"
check apps-runaway-loop-is-stopped grep -q '^limit: ok caught=1' <<<"$spike"
a shell run-as "$pkg" rm -f files/home/desktop/patches.tcl files/home/desktop/spike.txt

echo "== apps: one runs, its picture comes back, a pinned icon opens it"
# What the agent's `app` tool does (veles_core::apps): the app's files,
# then the window's app.run and app.screenshot device actions. The
# picture is DesktopActivity's PixelCopy, asked for by a SNAP broadcast
# (on the real phone Android delivered that broadcast to nobody until it
# named the package, 2026-10-12).
a shell run-as "$pkg" sh -c "'mkdir -p files/home/apps/smoke && cat > files/home/apps/smoke/app.toml'" <<'TOML'
title = "Smoke"
description = ""
entry = "main.tcl"
capabilities = []
TOML
a shell run-as "$pkg" sh -c "'cat > files/home/apps/smoke/main.tcl'" <<'TCL'
set n 0
label .count -textvariable n -font {Helvetica 40 bold} -bg #20a050 -fg white -width 6 -height 3
button .plus -text + -command {incr n}
pack .count -fill both -expand 1
pack .plus
veles::log "smoke up"
TCL
apps_offered() { api /api/device/list | grep -q 'app.screenshot'; }
wait_for 60 apps_offered || true
check apps-window-offers-app-actions apps_offered
dev() {
    curl -s -m 20 -X POST -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
        -d "{\"action\":\"$1\",\"args\":{\"name\":\"smoke\"}}" "http://127.0.0.1:$fwd/api/device/call" || true
}
ran=$(dev app.run)
echo "  app.run: $ran"
check apps-run-through-the-device-bridge grep -q '"ok":true' <<<"$ran"
sleep 2
# The picture: a PNG of the app's area with its green label in it.
cat > "$run/png.py" <<'PNGPY'
import base64, json, struct, sys, zlib
r = json.load(sys.stdin)
if not r.get("ok"):
    print("  app.screenshot:", r.get("error"))
    sys.exit(1)
png = base64.b64decode(r["result"])
if png[:8] != b"\x89PNG\r\n\x1a\n":
    print("  app.screenshot: not a PNG")
    sys.exit(1)
w, h, depth, ctype = struct.unpack(">IIBB", png[16:26])
pos, idat = 8, b""
while pos < len(png):
    (n,) = struct.unpack(">I", png[pos:pos + 4])
    if png[pos + 4:pos + 8] == b"IDAT":
        idat += png[pos + 8:pos + 8 + n]
    pos += 12 + n
raw = zlib.decompress(idat)
bpp = {2: 3, 6: 4}[ctype]
stride = w * bpp
prev = bytearray(stride)
green = 0
for y in range(h):
    f = raw[y * (stride + 1)]
    line = bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
    for i in range(stride):
        a = line[i - bpp] if i >= bpp else 0
        b = prev[i]
        c = prev[i - bpp] if i >= bpp else 0
        if f == 1:
            line[i] = (line[i] + a) & 255
        elif f == 2:
            line[i] = (line[i] + b) & 255
        elif f == 3:
            line[i] = (line[i] + (a + b) // 2) & 255
        elif f == 4:
            p = a + b - c
            pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
            line[i] = (line[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
    for x in range(0, stride, bpp):
        if abs(line[x] - 0x20) < 12 and abs(line[x + 1] - 0xa0) < 12 and abs(line[x + 2] - 0x50) < 12:
            green += 1
    prev = line
print(f"  app.screenshot: {w}x{h}, {green} of its pixels are the label's green")
sys.exit(0 if w > 50 and h > 50 and green > 1000 else 1)
PNGPY
shot=$(dev app.screenshot)
check apps-screenshot-shows-the-app python3 "$run/png.py" <<<"$shot"
logs=$(dev app.logs)
check apps-logs-reach-the-agent grep -q 'smoke up' <<<"$logs"
# Pinning, as the launcher's "Pin to home screen" does it (::vapps::pin,
# from the patches file at the window's start), then Android's own
# question, answered by its button as the person would.
printf '%s\n' 'after 3000 {::vapps::pin smoke Smoke}' \
    | a shell run-as "$pkg" sh -c "'cat > files/home/desktop/patches.tcl'"
p=$(desk_pid)
[ -n "$p" ] && a shell kill "$p"
sleep 2
a shell am start -W -n "$pkg/.DesktopActivity" >/dev/null
# tap_text REGEX: tap the middle of the node whose text or description
# is REGEX.
tap_text() {
    local b
    a shell uiautomator dump /sdcard/ui.xml >/dev/null 2>&1 || true
    b=$(sh_ cat /sdcard/ui.xml | python3 -c '
import re, sys
rx = re.compile(sys.argv[1])
for m in re.finditer(r"<node [^>]*>", sys.stdin.read()):
    n = m.group(0)
    t = re.search(r"text=\"([^\"]*)\"", n)
    d = re.search(r"content-desc=\"([^\"]*)\"", n)
    if (t and rx.fullmatch(t.group(1))) or (d and rx.fullmatch(d.group(1))):
        x1, y1, x2, y2 = map(int, re.search(r"bounds=\"\[(\d+),(\d+)\]\[(\d+),(\d+)\]\"", n).groups())
        print((x1 + x2) // 2, (y1 + y2) // 2)
        break
' "$1") || true
    [ -n "$b" ] || return 1
    # shellcheck disable=SC2086
    a shell input tap $b
}
pin_asked() {
    a shell uiautomator dump /sdcard/ui.xml >/dev/null 2>&1 || true
    sh_ cat /sdcard/ui.xml | grep -qi 'home screen'
}
wait_for 30 pin_asked || true
a shell run-as "$pkg" rm -f files/home/desktop/patches.tcl
check apps-pin-asks-android pin_asked
tap_text '(?i)add( to home screen| automatically)?' || true
pinned() { sh_ dumpsys shortcut | grep -A3 "id=app-smoke" | grep -q 'Pin'; }
wait_for 15 pinned || true
check apps-pinned-shortcut-exists pinned
sh_ dumpsys shortcut | grep -m1 -A2 "id=app-smoke" | sed 's/^/  /'
# The icon on the home screen, tapped: the window opens and runs it.
a shell input keyevent KEYCODE_HOME
sleep 2
tap_text 'Smoke' || true
ran_again() { dev app.logs | grep -q 'smoke up'; }
wait_for 40 ran_again || true
check apps-pinned-icon-opens-the-app ran_again

echo "== reacting to the phone: triggers on notifications, the cron alarm (PLAN-sessions-and-phone.md P4)"
# What the person does once on a real phone, done here by adb: veles may
# read notifications (Settings → Notification access) and run in the
# background (the Phone page's Allow). Then a trigger on the shell's own
# notifications (`cmd notification post` posts as com.android.shell).
a shell cmd notification allow_listener "$pkg/$pkg.NotificationListener" >/dev/null 2>&1 || true
a shell dumpsys deviceidle whitelist "+$pkg" >/dev/null 2>&1 || true
# "Alarms & reminders", so the cron alarm below comes on time; without
# it Android picks the moment within its own window (Wake.java).
a shell appops set "$pkg" SCHEDULE_EXACT_ALARM allow >/dev/null 2>&1 || true
api_post /api/triggers/new '{"name":"smoke-note","event":"NOTIFICATION","channel":"com.android.shell","match_kind":"contains","pattern":"smoke-dinner","action":"prompt","arg":"Say in one line what came in."}' >/dev/null
api_post /api/triggers/new '{"name":"smoke-power","event":"POWER","channel":"connected","action":"prompt","arg":"Say that the phone is charging."}' >/dev/null
# Another app's notification, as the shell's user: posted as root (this
# image's adbd) it is put down to a package called "root" and dropped
# ("Cannot fix notification: NameNotFoundException: root", 2026-10-12).
post_note() {
    a shell "su 2000 cmd notification post -t '$1' '$2' '$3'" >/dev/null 2>&1 || true
}
listening() {
    sh_ dumpsys notification | sed -n '/Live notification listeners/,/Snoozed notification listeners/p' \
        | grep -q "$pkg/$pkg.NotificationListener"
}
wait_for 20 listening || true
check notes-listener-bound listening
# A trigger's turns land in its own session; this counts the answers
# there (0 when the session does not exist yet).
trigger_answers() {
    local sid
    sid=$(api /api/sessions | python3 -c 'import json,sys
t = "Trigger: " + sys.argv[1]
d = json.load(sys.stdin)
rows = d.get("sessions", d) if isinstance(d, dict) else d
print(next((r.get("id", r.get("session_id", "")) for r in rows if r.get("title") == t), ""))' "$1" 2>/dev/null) || true
    [ -n "$sid" ] || { echo 0; return; }
    api "/api/sessions/$sid/history" | python3 -c 'import json,sys
m = json.load(sys.stdin).get("messages", [])
print(sum(1 for x in m if x.get("role") == "assistant" and (x.get("content") or "").strip()))' 2>/dev/null || echo 0
}
answered() { [ "$(trigger_answers "$1")" -ge "$2" ]; }

# 1. The engine runs, the desktop is in the background.
a shell input keyevent KEYCODE_HOME
sleep 2
post_note Mom smoke-note-1 "Are you coming to smoke-dinner at 7?"
wait_for 60 answered smoke-note 1 || true
check notes-trigger-runs-in-the-background answered smoke-note 1
note_sid=$(api /api/sessions | python3 -c 'import json,sys
d = json.load(sys.stdin)
rows = d.get("sessions", d) if isinstance(d, dict) else d
print(next((r.get("id", r.get("session_id", "")) for r in rows if r.get("title") == "Trigger: smoke-note"), ""))' 2>/dev/null) || true
quoted() { api "/api/sessions/$note_sid/history" | grep -q '> title: Mom'; }
check notes-the-turn-quotes-the-notification quoted
done_posted() { notified turn "$(java_hash "$note_sid")"; }
wait_for 20 done_posted || true
check notes-the-answer-is-notified done_posted
post_note Podcasts smoke-note-2 "A new episode is out"
sleep 5
check notes-no-match-no-turn answered smoke-note 1
not_twice() { ! answered smoke-note 2; }
check notes-one-turn-per-notification not_twice

# 2. The service's own device actions: they answer with the window
# closed.
svc() { api_post /api/device/call "{\"device\":\"phone\",\"action\":\"$1\",\"args\":{}}"; }
listed_svc() { api /api/device/list | grep -q '"client_id":"phone-service"'; }
check notes-service-is-a-device listed_svc
battery=$(svc battery)
echo "  battery: $battery"
check notes-battery-answers grep -q '"level"' <<<"$battery"
recent=$(svc notifications)
check notes-recent-notifications-read grep -q 'smoke-dinner' <<<"$recent"

# 3. Plugged in: the POWER trigger.
a shell dumpsys battery unplug >/dev/null 2>&1 || true
sleep 2
a shell dumpsys battery set ac 1 >/dev/null 2>&1 || true
wait_for 60 answered smoke-power 1 || true
check power-trigger-runs answered smoke-power 1
a shell dumpsys battery reset >/dev/null 2>&1 || true

# 4. The engine stops when idle, and the cron alarm starts it again.
# Twenty seconds unused, from now on (files/idle-secs, read at each
# start); the engine is restarted by killing it (the service's crash
# path), and the desktop stays in the background.
api_post /api/triggers/smoke-power/disable '{}' >/dev/null
at=$(date -u -d '+150 sec' +%Y-%m-%dT%H:%M:%SZ)
api_post /api/cron/new "{\"name\":\"smoke-alarm\",\"prompt\":\"Say the alarm worked.\",\"schedule\":\"$at\"}" >/dev/null
printf '20\n' | a shell run-as "$pkg" sh -c "'cat > files/idle-secs'"
old=$(engine_pid)
[ -n "$old" ] && a shell run-as "$pkg" kill "$old"
wait_for 30 new_engine || true
wait_for 90 engine_down || true
check idle-engine-stops engine_down
wake=$(sh_ run-as "$pkg" cat files/home/wake.json)
echo "  wake.json: $(tr -d '\n ' <<<"$wake")"
check idle-wake-plan-names-the-trigger grep -q 'com.android.shell' <<<"$wake"
check idle-wake-plan-has-the-cron-job grep -q '"at": [0-9]' <<<"$wake"
alarm_set() { sh_ dumpsys alarm | grep -q "chat.janitor.veles.action.WAKE"; }
check idle-cron-alarm-armed alarm_set
wait_for 240 engine_up || true
check alarm-starts-the-engine engine_up
cron_ran() { api /api/cron | python3 -c 'import json,sys
d = json.load(sys.stdin)
jobs = d.get("jobs", d) if isinstance(d, dict) else d
print("yes" if any(j.get("name") == "smoke-alarm" and (j.get("run_count") or 0) > 0 for j in jobs) else "")' 2>/dev/null | grep -q yes; }
wait_for 90 cron_ran || true
check alarm-the-cron-job-runs cron_ran

# 5. Stopped again: a notification its trigger listens to starts it, and
# the trigger runs.
wait_for 120 engine_down || true
post_note Mom smoke-note-3 "smoke-dinner is ready"
wait_for 60 engine_up || true
check notes-a-notification-starts-the-engine engine_up
wait_for 90 answered smoke-note 2 || true
check notes-trigger-runs-after-the-start answered smoke-note 2

# 6. A notification no trigger listens to starts nothing.
api_post /api/triggers/smoke-note/disable '{}' >/dev/null
wait_for 120 engine_down || true
wake=$(sh_ run-as "$pkg" cat files/home/wake.json)
check idle-wake-plan-empty not grep -q 'com.android.shell' <<<"$wake"
post_note Mom smoke-note-4 "smoke-dinner again"
sleep 20
check notes-unwatched-app-starts-nothing engine_down
a shell run-as "$pkg" rm -f files/idle-secs

echo "== the agent's hands on the phone: busybox, the accessibility driver (PLAN-sessions-and-phone.md P5)"
# The P4 checks leave the engine stopped for idleness: start it again,
# as opening the app does.
start_service
wait_for 60 engine_up || true
wait_for 60 healthy || true
nat=$(dirname "$(sh_ run-as "$pkg" readlink files/bin/rg)")
if ! sh_ ls "$nat" | grep -q libbusybox.so; then
    # busybox and git ship for arm64-v8a only (flake.nix): static musl
    # dies under x86_64 Android's filter for app processes. This x86_64
    # build has neither, and so no git, busybox or connect proxy to test.
    echo "  busybox and git: arm64 only, absent from this $(sh_ getprop ro.product.cpu.abi) build"
    absent() { [ -z "$(sh_ run-as "$pkg" ls files/bin/git)" ] && ! sh_ ls "$nat" | grep -q libgit.so; }
    check busybox-and-git-absent-on-x86-64 absent
else
    # busybox's applets beside rg, linked at the engine's start (Engine.java):
    # what toybox lacks, awk first.
    bb_linked() { sh_ run-as "$pkg" ls -l files/bin/awk | grep -q libbusybox.so; }
    wait_for 20 bb_linked || true
    check busybox-applets-linked bb_linked
    aw=$(sh_ run-as "$pkg" files/bin/awk "'BEGIN{print 6*7}'")
    check busybox-awk-runs [ "$aw" = 42 ]
    # run-as runs outside the app's seccomp filter; the engine (and the
    # agent's terminal) inside it. So the same, from the engine's own Tcl.
    # shellcheck disable=SC2016 # $r is Tcl's, not the shell's
    in_engine=$(api_post /api/desktop/eval "$(python3 -c 'import json,sys
code = "catch {exec %s/libbusybox.so awk {BEGIN{print 6*7}}} r o; list $r [dict get $o -errorcode]" % sys.argv[1]
print(json.dumps({"code": code, "target": "gw"}))' "$nat")")
    echo "  busybox in the engine: $in_engine" | cut -c1-200
    check busybox-runs-in-the-engine grep -q '"result":"42 NONE"' <<<"$in_engine"
    sh_ run-as "$pkg" cat files/gateway.log | { grep 'busybox --list' || true; } | tail -2 | sed 's/^/  /'
    # git (libgit.so, its HTTPS helper in GIT_EXEC_PATH): a repository made
    # and committed to from the engine's own Tcl, under the app's filter. An
    # HTTPS ls-remote is printed, not checked: the network is not this
    # test's to depend on.
    gv=$(sh_ run-as "$pkg" files/bin/git --version)
    check git-runs grep -q '^git version' <<<"$gv"
    # shellcheck disable=SC2016 # Tcl's variables, not the shell's
    git_in_engine=$(api_post /api/desktop/eval "$(python3 -c 'import json
code = ("set d [file join $::env(HOME) cache git-smoke]; file delete -force $d; file mkdir $d; cd $d; "
        "catch {exec git init -q; exec git -c user.email=s@x -c user.name=s commit -q --allow-empty -m smoke-one; "
        "exec git log --oneline} r o; list $r [dict get $o -errorcode]")
print(json.dumps({"code": code, "target": "gw"}))')")
    echo "  git in the engine: $git_in_engine" | cut -c1-200
    check git-commits-in-the-engine grep -q 'smoke-one' <<<"$git_in_engine"
    # shellcheck disable=SC2016
    https=$(api_post /api/desktop/eval "$(python3 -c 'import json
print(json.dumps({"code": "catch {exec git ls-remote https://github.com/git/git refs/heads/master} r o; list [string range $r 0 50] [dict get $o -errorcode]", "target": "gw"}))')")
    echo "  git over HTTPS (not checked): $https" | cut -c1-200
    sh_ logcat -d | { grep -E 'type=1326|SECCOMP|seccomp' || true; } | tail -3 | sed 's/^/  seccomp: /' | cut -c1-200
    if ! bb_linked; then
        echo "  files/bin, and busybox in the native library directory:"
        sh_ run-as "$pkg" ls -la files/bin | head -8 | sed 's/^/    /'
        nat=$(sh_ run-as "$pkg" readlink files/bin/rg)
        sh_ ls -la "$(dirname "$nat")" | grep -i busybox | sed 's/^/    /'
        sh_ run-as "$pkg" "$(dirname "$nat")/libbusybox.so" --list 2>&1 | head -3 | sed 's/^/    /'
    fi
fi
if [ -n "${VELES_AAB_DIR:-}" ]; then
    # The store's bundle leaves the driver out (USER decision D4).
    no_driver() { ! sh_ dumpsys package "$pkg" | grep -q 'AccessibilityDriver'; }
    check driver-absent-from-the-store-bundle no_driver
else
    # What the person does in Android's accessibility settings.
    a shell settings put secure enabled_accessibility_services "$pkg/$pkg.AccessibilityDriver"
    a shell settings put secure accessibility_enabled 1
    driver_up() { sh_ logcat -d -s veles-driver | grep -q 'listening on 127.0.0.1:9876'; }
    wait_for 30 driver_up || true
    check driver-listens driver_up
    # The phone's own CLI in the engine's environment (the real phone's
    # files/vh.sh): `computer_use` as the agent calls it, with the
    # gateway's token.
    a shell run-as "$pkg" sh -c "'cat > files/vh.sh'" <<'VH'
#!/system/bin/sh
p=$(pidof libveles.so) || { echo "the engine is not running" >&2; exit 1; }
eval "$(tr '\0' '\n' < /proc/$p/environ \
    | grep -E '^(TCL_LIBRARY|VELES_[A-Z_]+|SSL_CERT_DIR|LD_LIBRARY_PATH|TZ|LANG|PATH|TMPDIR)=' \
    | sed "s/'/'\\\\''/g; s/=/='/; s/\$/'/; s/^/export /")"
cd "$VELES_HOME" || exit 1
HOME=$VELES_HOME RUST_LOG=error NO_COLOR=1 exec "$(dirname "$VELES_TCLSH")/libveles.so" "$@"
VH
    # Its errors too (the engine down, the tool refused): a capture that
    # says nothing is no use to read.
    cu() { a shell "run-as $pkg sh files/vh.sh tool --name computer_use --args '$1'" 2>&1 | tr -d '\r'; }
    # The number of the first element whose line names $2, in capture $1.
    element_named() { sed -n "s/^\[\([0-9]*\)\] .*\"[^\"]*$2.*/\1/p" <<<"$1" | head -1; }
    # Dark theme, as a person would ask for it: Display, its row (below
    # the fold on a small screen: scroll, as the agent would, and look
    # again), its page, its switch; then Airplane mode, which Android
    # protects (below).
    a shell am start -a android.settings.DISPLAY_SETTINGS >/dev/null 2>&1 || true
    sleep 3
    cap=$(cu '{"action":"capture","mode":"som"}')
    head -12 <<<"$cap" | sed 's/^/  /' | cut -c1-150
    check driver-capture-lists-elements grep -q 'interactable elements found' <<<"$cap"
    shot=$(sed -n 's/^screenshot saved: //p' <<<"$cap" | head -1)
    saved() { [ -n "$shot" ] && sh_ run-as "$pkg" head -c 8 "$shot" | grep -q PNG; }
    check driver-capture-saves-a-screenshot saved
    n=$(element_named "$cap" 'Dark theme')
    for try in 1 2 3 4; do
        [ -n "$n" ] && break
        echo "  scroll $try: $(cu '{"action":"scroll","direction":"down"}' | tail -1)"
        sleep 1
        cap=$(cu '{"action":"capture","mode":"ax"}')
        echo "    now ends: $(grep '^\[' <<<"$cap" | tail -2 | tr '\n' ' ' | cut -c1-150)"
        n=$(element_named "$cap" 'Dark theme')
    done
    echo "  Dark theme is element ${n:-none}"
    [ -n "$n" ] && cu "{\"action\":\"click\",\"element\":$n}" | sed 's/^/  /'
    sleep 2
    cap=$(cu '{"action":"capture","mode":"ax"}')
    n=$(element_named "$cap" 'Use Dark theme')
    night() { sh_ cmd uimode night; }
    before=$(night)
    echo "  Use Dark theme is element ${n:-none}; $before"
    [ -n "$n" ] && cu "{\"action\":\"click\",\"element\":$n}" | sed 's/^/  /'
    flipped() { [ -n "$(night)" ] && [ "$(night)" != "$before" ]; }
    wait_for 10 flipped || true
    check driver-toggles-a-setting flipped
    driving() { notified null 4; }
    check driver-says-the-agent-is-driving driving
    # What the agent reads back: the switch, now on. A capture after the
    # change, not a copy of the one before (Android's node cache: a
    # capture after a swipe answered the screen before it until fresh()).
    # A new theme redraws the page (a configuration change), and a capture
    # in the middle of that has nothing yet: look again, as the agent
    # would (2026-10-13, the one run that captured too early).
    sees_on() {
        cap=$(cu '{"action":"capture","mode":"ax"}')
        grep -q '"Use Dark theme" (on)' <<<"$cap"
    }
    wait_for 15 sees_on || true
    { grep -m1 'Use Dark theme' <<<"$cap" || sed -n 2p <<<"$cap"; } | sed 's/^/  /' | cut -c1-150
    check driver-sees-the-switch-on grep -q '"Use Dark theme" (on)' <<<"$cap"
    # As it was.
    n=$(element_named "$cap" 'Use Dark theme')
    [ -n "$n" ] && cu "{\"action\":\"click\",\"element\":$n}" >/dev/null
    back() { [ "$(night)" = "$before" ]; }
    wait_for 10 back || true
    check driver-toggles-it-back back
    # Airplane mode is a control Android protects. Not an accessibility
    # tool, the driver was not shown it, and once it was switched the whole
    # page hid from it (2026-10-12); declared one (USER 2026-10-13), it is
    # shown it and its taps reach it. Matched by the row's own label: with
    # it on, the Internet row says "Airplane mode is on" too.
    a shell am force-stop com.android.settings
    a shell am start -W -a android.settings.AIRPLANE_MODE_SETTINGS >/dev/null 2>&1 || true
    sleep 3
    cap=$(cu '{"action":"capture","mode":"ax"}')
    { grep -m1 '"Airplane mode"' <<<"$cap" || true; } | sed 's/^/  /'
    n=$(element_named "$cap" 'Airplane mode"')
    check driver-is-shown-airplane-mode [ -n "$n" ]
    air() { sh_ settings get global airplane_mode_on; }
    air_before=$(air)
    [ -n "$n" ] && cu "{\"action\":\"click\",\"element\":$n}" >/dev/null
    air_flipped() { [ -n "$(air)" ] && [ "$(air)" != "$air_before" ]; }
    wait_for 10 air_flipped || true
    check driver-switches-airplane-mode air_flipped
    cap=$(cu '{"action":"capture","mode":"ax"}')
    { grep -m1 '"Airplane mode"' <<<"$cap" || true; } | sed 's/^/  /'
    n=$(element_named "$cap" 'Airplane mode"')
    [ -n "$n" ] && cu "{\"action\":\"click\",\"element\":$n}" >/dev/null
    air_back() { [ "$(air)" = "$air_before" ]; }
    wait_for 10 air_back || true
    check driver-switches-it-back-again air_back
    a shell settings put global airplane_mode_on 0 >/dev/null 2>&1 || true
    a shell input keyevent KEYCODE_HOME
    # Only veles with its token: anything else on the phone is refused.
    a forward tcp:19876 tcp:9876 >/dev/null
    code=$(curl -s -m 10 -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
        -d '{"action":"capture"}' http://127.0.0.1:19876/action || true)
    wrong=$(curl -s -m 10 -o /dev/null -w '%{http_code}' -X POST -H 'Authorization: Bearer nope' \
        -d '{"action":"capture"}' http://127.0.0.1:19876/action || true)
    a forward --remove tcp:19876 >/dev/null 2>&1 || true
    check driver-refuses-without-the-token [ "$code" = 401 ]
    check driver-refuses-a-wrong-token [ "$wrong" = 401 ]
    # The notification's Stop turns the driver off; the person turns it
    # on again in Android's settings.
    a shell am broadcast -a "$pkg.action.STOP_DRIVING" -p "$pkg" >/dev/null 2>&1 || true
    stopped() { ! sh_ settings get secure enabled_accessibility_services | grep -q AccessibilityDriver; }
    wait_for 15 stopped || true
    check driver-stop-turns-it-off stopped
    off=$(cu '{"action":"capture"}')
    check driver-off-says-so grep -q "driver is off" <<<"$off"
fi

echo "== $pass passed, $fail failed"
if [ "$fail" -ne 0 ]; then
    echo "--- the app's gateway.log, last 40 lines"
    applog | tail -40
    echo "--- logcat, the app's lines"
    sh_ logcat -d -b main,system,crash | grep -E 'chat\.janitor\.veles|GatewayService|ForegroundService' | tail -30
    echo "RC=1"
    exit 1
fi
echo "RC=0"

