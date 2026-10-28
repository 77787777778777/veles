#!/usr/bin/env bash
# scripts/release/install-smoke.sh — the real Linux archive through the real
# installer on clean distributions. ON DEMAND (needs podman and network for
# the base images and their package managers); never in precommit/flake.
#
#   scripts/release/install-smoke.sh /path/veles-X-REV-linux-x86_64.tar.zst [debian fedora arch]
#
# The archive is published as a fixture release (SHA256SUMS, a GitHub-shaped
# loopback server: scripts/install/fixture-release.sh), plus a second
# release X+0.0.1 that is the same tree with only BUNDLE-INFO's version
# bumped, so the upgrade path runs the real `veles install bundle` too.
# In each clean container (debian:trixie, fedora:latest, archlinux:latest;
# only test prerequisites are added up front: curl, as a curl|sh user has it,
# and Xvfb/fonts for the desktop leg):
#   cat install.sh | sh -s -- --version vX      install; zstd etc. come from
#                                                the installer's own package step
#   veles --version ; veles doctor ; no dangling link inside the launcher's
#   nix-user-chroot ; veles chat -q against scripts/install/stub-provider.sh ;
#   veles gui under Xvfb maps a window and the gateway answers /api/health
#   config written, then install.sh --version vX+1  -> current switches,
#   previous kept, config byte-identical, veles --version still runs
# Logs: $VELES_SMOKE_RESULTS (default ~/.cache/veles-release-eng/install-smoke).
set -euo pipefail
cd "$(dirname "$0")/../.."
repo_root=$PWD
archive=$(realpath "${1:?usage: install-smoke.sh ARCHIVE.tar.zst [debian fedora arch]}")
shift
targets=("$@"); ((${#targets[@]})) || targets=(debian fedora arch)
declare -A images=([debian]=docker.io/library/debian:trixie [fedora]=registry.fedoraproject.org/fedora:latest [arch]=docker.io/library/archlinux:latest)
# Test prerequisites only: curl (a curl|sh user has it), Xvfb + fonts for the
# desktop leg, procps for pkill. zstd and the rest come from install.sh.
declare -A addcurl=([debian]='apt-get update -qq && apt-get install -y -qq --no-install-recommends curl ca-certificates xvfb xauth fontconfig fonts-dejavu-core procps >/dev/null' [fedora]='dnf install -y -q curl xorg-x11-server-Xvfb xauth fontconfig dejavu-sans-fonts procps-ng findutils >/dev/null' [arch]='pacman -Sy --noconfirm --needed curl xorg-server-xvfb xorg-xauth fontconfig ttf-dejavu procps-ng findutils >/dev/null')
for tool in podman socat curl tar zstd sha256sum jq; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 2; }; done
results=${VELES_SMOKE_RESULTS:-$HOME/.cache/veles-release-eng/install-smoke}
mkdir -p "$results"
name=$(basename "$archive")
[[ $name =~ ^veles-([0-9]+)\.([0-9]+)\.([0-9]+)-([0-9a-z]+)-linux-x86_64\.tar\.zst$ ]] || { echo "unexpected archive name $name" >&2; exit 2; }
v1=${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}
v2=${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.$((BASH_REMATCH[3] + 1))
rev=${BASH_REMATCH[4]}
work=$(mktemp -d "${TMPDIR:-/tmp}/veles-install-smoke.XXXXXX")
server="" stub="" containers=()
cleanup() {
    for p in $server $stub; do kill "$p" 2>/dev/null || true; done
    for c in "${containers[@]}"; do podman rm -f "$c" >/dev/null 2>&1 || true; done
    chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"
}
trap cleanup EXIT
mkdir -p "$work/srv/download/v$v1" "$work/srv/download/v$v2" "$work/tree"
ln "$archive" "$work/srv/download/v$v1/$name" 2>/dev/null || cp "$archive" "$work/srv/download/v$v1/$name"
(cd "$work/srv/download/v$v1" && sha256sum "$name" > SHA256SUMS)
echo "▸ making v$v2 (same tree, version bumped) for the upgrade leg"
tar --zstd -xf "$archive" -C "$work/tree"
chmod u+w "$work/tree" "$work/tree/BUNDLE-INFO.json"
jq -c --arg v "$v2" '.version = $v' "$work/tree/BUNDLE-INFO.json" > "$work/info" && cat "$work/info" > "$work/tree/BUNDLE-INFO.json"
name2=veles-$v2-$rev-linux-x86_64.tar.zst
tar --sort=name --owner=0 --group=0 --numeric-owner -cf - -C "$work/tree" . | zstd -q -3 -T4 -o "$work/srv/download/v$v2/$name2"
chmod -R u+w "$work/tree"; rm -rf "$work/tree"
(cd "$work/srv/download/v$v2" && sha256sum "$name2" > SHA256SUMS)
echo "v$v2" > "$work/srv/LATEST"
port=$((20000 + RANDOM % 20000))
bash scripts/install/fixture-release.sh serve "$work/srv" "$port" 2>"$results/server.log" &
server=$!
for _ in $(seq 50); do curl -fsS -o /dev/null "http://127.0.0.1:$port/releases/tag/v0" 2>/dev/null && break; sleep 0.1; done
stub_port=$((port + 1))
bash scripts/install/stub-provider.sh serve "$stub_port" "$results/stub-provider.log" 2>>"$results/server.log" &
stub=$!
cat > "$work/guest.sh" <<GUEST
set -eu
export VELES_RELEASE_BASE_URL=http://127.0.0.1:$port/releases HOME=/root
. /etc/os-release; echo "guest: \$PRETTY_NAME"
test ! -e /nix || { echo "this guest has /nix; it cannot prove the bundle is self-contained" >&2; exit 1; }
cat /install/install.sh | sh -s -- --version v$v1
export PATH=/root/.local/bin:\$PATH
veles --version
veles doctor && echo DOCTOR_RC=0 || echo DOCTOR_RC=\$?
# The closure's absolute /nix/store links dangle on this host by design and
# resolve inside the launcher's nix-user-chroot (which mounts <release>/nix
# at /nix). Count both views: outside is informational, inside must be 0.
rel=\$(readlink -f /root/.local/share/veles/install/current)
chroot=\$(ls -d "\$rel"/nix/store/*-nix-user-chroot-*/bin/nix-user-chroot)
echo "DANGLING_OUTSIDE=\$(find "\$rel" -xtype l | wc -l) of \$(find "\$rel" -type l | wc -l) links"
find "\$rel" -xtype l -exec sh -c 'printf "  dangling outside: %s -> %s\\n" "\$1" "\$(readlink "\$1")"' _ {} \; | head -5
inside=\$("\$chroot" "\$rel/nix" /bin/sh -c "find '\$rel' -xtype l | wc -l")
echo "DANGLING_INSIDE_CHROOT=\$inside"
test "\$inside" = 0
# A real turn against a canned provider.
mkdir -p /root/.config/veles
cat > /root/.config/veles/config.toml <<CFG
[model]
provider = "stub"
default = "stub-model"

[[custom_providers]]
name = "stub"
base_url = "http://127.0.0.1:$stub_port/v1"
api_mode = "chat_completions"
api_key = "none"
models = ["stub-model"]
CFG
before=\$(sha256sum /root/.config/veles/config.toml)
timeout 120 veles chat -q 'reply with anything' </dev/null > /tmp/chat.out 2>&1 || { cat /tmp/chat.out; exit 1; }
grep -q STUB_PROVIDER_REPLY /tmp/chat.out || { cat /tmp/chat.out; exit 1; }
echo CHAT_PASS
# The desktop starts: gateway up, a real Tk window mapped.
Xvfb :91 -screen 0 1280x900x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
for i in \$(seq 80); do [ -S /tmp/.X11-unix/X91 ] && break; sleep 0.1; done
DISPLAY=:91 VELES_PRINT_WINDOW_ID=1 VELES_DESKTOP_PORT=19371 timeout -s TERM 60 veles gui > /tmp/gui.log 2>&1 &
gp=\$!
for i in \$(seq 300); do grep -q WINDOW_ID= /tmp/gui.log && break; kill -0 \$gp 2>/dev/null || break; sleep 0.1; done
grep -q WINDOW_ID= /tmp/gui.log || { cat /tmp/gui.log; exit 1; }
curl -fsS http://127.0.0.1:19371/api/health; echo
echo GUI_PASS
kill \$gp 2>/dev/null || true
pkill -f 'veles.*gateway run' 2>/dev/null || kill \$(cat /root/.local/share/veles/gateway.pid 2>/dev/null) 2>/dev/null || true
sleep 1
# Upgrade to the latest release over the existing install.
cat /install/install.sh | sh -s --
test "\$(readlink /root/.local/share/veles/install/current)" = /root/.local/share/veles/install/releases/$v2-$rev
test -d /root/.local/share/veles/install/releases/$v1-$rev
test "\$(sha256sum /root/.config/veles/config.toml)" = "\$before"
veles --version
timeout 120 veles chat -q 'again' </dev/null 2>&1 | grep -q STUB_PROVIDER_REPLY
echo INSTALL_SMOKE_PASS
GUEST
for t in "${targets[@]}"; do
    c=veles-install-smoke-$t-$$
    containers+=("$c")
    echo "▸ $t: ${images[$t]}"
    if ! timeout 1800 podman run --rm --name "$c" --network=host --pull=newer \
        --security-opt seccomp=unconfined --cap-add SYS_ADMIN --device /dev/fuse \
        -v "$repo_root/scripts/install:/install:ro" -v "$work/guest.sh:/guest.sh:ro" \
        "${images[$t]}" sh -c "${addcurl[$t]} && sh /guest.sh" >"$results/$t.log" 2>&1; then
        echo "FAILED $t:" >&2; tail -40 "$results/$t.log" >&2; exit 1
    fi
    grep -q INSTALL_SMOKE_PASS "$results/$t.log"
    echo "PASS $t ($(grep -m1 '^guest:' "$results/$t.log" | cut -d' ' -f2-); doctor: $(grep -m1 DOCTOR_RC "$results/$t.log"))"
done
echo "ALL_INSTALL_SMOKES_PASS (logs: $results)"
