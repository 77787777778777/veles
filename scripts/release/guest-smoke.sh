#!/usr/bin/env bash
# Runs as the unprivileged user inside the network-free distro guest.
# Only the two artifact files, this script, a scratch results directory,
# and the explicitly controlled Podman socket are mounted.
set -euo pipefail
: "${HOME:?}" "${VELES_HOME:?}" "${CONTAINER_HOST:?}" "${VELES_SMOKE_RESULTS_DIR:?}" "${VELES_SMOKE_DISTRO:?}"
result=$VELES_SMOKE_RESULTS_DIR
mkdir -p "$HOME/tmp" "$VELES_HOME" "$HOME/bundle" "$result"
gp=""
mp=""
xp=""
active_home=""
kill_numeric() {
    case ${1:-} in ''|*[!0-9]*) ;; *) kill -TERM "$1" 2>/dev/null || true ;; esac
}
stop_gui() {
    kill_numeric "$gp"
    if [[ -n $gp ]]; then wait "$gp" 2>/dev/null || true; fi
    gp=""
    if [[ -n $active_home && -r $active_home/gateway.pid ]]; then
        read -r gw < "$active_home/gateway.pid" || true
        kill_numeric "$gw"
    fi
    if [[ -n $active_home && -r $active_home/gui-gateway.log ]]; then
        cp "$active_home/gui-gateway.log" "$result/$VELES_SMOKE_DISTRO.gateway.log"
    fi
    active_home=""
}
stop_mount() {
    kill_numeric "$mp"
    if [[ -n $mp ]]; then wait "$mp" 2>/dev/null || true; fi
    mp=""
}
cleanup_guest() {
    stop_gui
    stop_mount
    kill_numeric "$xp"
    if [[ -n $xp ]]; then wait "$xp" 2>/dev/null || true; fi
    xp=""
}
trap cleanup_guest EXIT
# Do not print environment values: a leaked name is enough to fail safely.
if env | cut -d= -f1 | grep -Ei '(api.*key|token|secret|password|credential)' > "$result/$VELES_SMOKE_DISTRO.credential-names"; then
    echo "provider credentials leaked into the guest" >&2
    cat "$result/$VELES_SMOKE_DISTRO.credential-names" >&2
    exit 1
fi
[ "$(ls /sys/class/net)" = lo ] || { echo "final guest has a non-loopback interface" >&2; exit 1; }
tar --zstd -xf /artifacts/bundle.tar.zst -C "$HOME/bundle"
[ ! -e /nix ] || { echo "host Nix leaked into the guest" >&2; exit 1; }
for tool in cargo rustc tclsh; do
    if command -v "$tool" >/dev/null; then echo "build/development tool $tool leaked into the guest" >&2; exit 1; fi
done
if find "$HOME/bundle" -type f \( -name cargo -o -name rustc \) -print -quit | grep -q .; then
    echo "compiler in the portable closure" >&2; exit 1
fi
"$HOME/bundle/bin/veles" --version
write_config() {
    cat > "$1/config.toml" <<'CFG'
[sandbox]
backend = "podman"
image = "localhost/veles-sandbox:r10-smoke"
network = false
user_env_network = "none"
[plugins]
disabled = ["atproto"]
[platforms.a2a]
enabled = false
CFG
}
run_doctor() {
    local label=$1 home=$2
    shift 2
    mkdir -p "$home"
    write_config "$home"
    VELES_HOME="$home" "$@" doctor --sandbox 2>&1 | tee "$result/$VELES_SMOKE_DISTRO.$label.doctor.log"
    grep -F "result: OK (42)" "$result/$VELES_SMOKE_DISTRO.$label.doctor.log"
    grep -F "user envs: OK" "$result/$VELES_SMOKE_DISTRO.$label.doctor.log"
}
check_language_hosts() {
    local log=$1
    grep -F "Racket CS 9.3 booted" "$log"
    grep -F "ECL 26.5.5 booted" "$log"
    grep -F "module lang-js: running" "$log"
}
launch_gui() {
    local label=$1 home=$2 port=$3
    shift 3
    mkdir -p "$home"
    write_config "$home"
    DISPLAY=:91 VELES_HOME="$home" VELES_PRINT_WINDOW_ID=1 VELES_DESKTOP_PORT="$port" \
        timeout --signal=TERM --kill-after=5s 75 "$@" gui >"$result/$VELES_SMOKE_DISTRO.$label.gui.log" 2>&1 &
    gp=$!
    active_home=$home
    for _ in $(seq 1 250); do
        grep -q "WINDOW_ID=" "$result/$VELES_SMOKE_DISTRO.$label.gui.log" && break
        kill -0 "$gp" 2>/dev/null || break
        sleep 0.1
    done
    cat "$result/$VELES_SMOKE_DISTRO.$label.gui.log"
    grep -q "WINDOW_ID=" "$result/$VELES_SMOKE_DISTRO.$label.gui.log"
    check_language_hosts "$home/gui-gateway.log"
    cp "$home/gui-gateway.log" "$result/$VELES_SMOKE_DISTRO.$label.gateway.log"
    stop_gui
    echo "${label}_GUI_AND_LANGUAGE_HOSTS_PASS"
}
Xvfb :91 -screen 0 1280x900x24 -nolisten tcp >"$result/$VELES_SMOKE_DISTRO.xvfb.log" 2>&1 &
xp=$!
for _ in $(seq 1 80); do
    [ -S /tmp/.X11-unix/X91 ] && break
    kill -0 "$xp"
    sleep 0.1
done
[ -S /tmp/.X11-unix/X91 ] || { cat "$result/$VELES_SMOKE_DISTRO.xvfb.log"; exit 1; }
run_doctor archive "$VELES_HOME" "$HOME/bundle/bin/veles"
launch_gui archive "$VELES_HOME" 19371 "$HOME/bundle/bin/veles"
apphome=$HOME/appimage-home
mkdir -p "$apphome"
# --appimage-mount must produce a real FUSE mount. A runtime fallback to
# extraction is not accepted as proof of direct AppImage execution.
timeout --signal=TERM --kill-after=5s 240 /artifacts/veles.AppImage --appimage-mount \
    >"$result/$VELES_SMOKE_DISTRO.appimage-mount.log" 2>&1 &
mp=$!
mount=""
for _ in $(seq 1 200); do
    mount=$(sed -n 's#^\([^[:space:]]*/\.mount_[^[:space:]]*\)$#\1#p' "$result/$VELES_SMOKE_DISTRO.appimage-mount.log" | tail -1)
    [[ -n $mount && -x $mount/AppRun ]] && break
    kill -0 "$mp" 2>/dev/null || break
    sleep 0.1
done
[[ -n $mount && -x $mount/AppRun ]] || { cat "$result/$VELES_SMOKE_DISTRO.appimage-mount.log"; exit 1; }
fuse=0
while read -r _device point type _rest; do
    if [[ $point == "$mount" && $type == *fuse* ]]; then fuse=1; fi
done < /proc/self/mounts
[[ $fuse == 1 ]] || { echo "AppImage mount is not FUSE: $mount" >&2; exit 1; }
printf '%s\n' "$mount" > "$result/$VELES_SMOKE_DISTRO.appimage-fuse-path"
"$mount/AppRun" --version
run_doctor appimage "$apphome" "$mount/AppRun"
stop_mount
launch_gui appimage "$apphome" 19372 /artifacts/veles.AppImage
mkdir "$HOME/appimage"
cd "$HOME/appimage"
/artifacts/veles.AppImage --appimage-extract >/dev/null
# Absolute Nix symlinks resolve only inside the private mount namespace.
# Compare symlink TEXT, not unrelated paths outside that namespace.
diff -qr --no-dereference "$HOME/bundle" "$HOME/appimage/squashfs-root"
"$HOME/appimage/squashfs-root/AppRun" --version
echo "APPIMAGE_FUSE_AND_EXTRACTION_MATCH_PASS"
echo "PORTABLE_SMOKE_PASS"
