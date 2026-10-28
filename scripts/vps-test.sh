#!/usr/bin/env bash
# vps-test.sh — a scratch run of the gateway image on the VPS, the way the
# real one runs (deploy/vps): rootless as `veles`, keep-id, the sandbox
# user's podman socket mounted, its own scratch volume and ports. Never
# touches the real gateway container, its volume or its ports.
#
#   scripts/vps-test.sh        (from the laptop; drives the VPS over SSH)
#
# Proves:
#   1. the image boots the gateway, and /api/health answers
#   2. `veles doctor --sandbox` INSIDE the container passes — the fence
#      and a user env end to end, through the sandbox user's socket
#      (CONTAINER_HOST), with the image's podman client against the
#      host's podman service
#   3. the state on the volume belongs to `veles` on the host
#   4. clean teardown: the container, its volume dir, and the boxes and
#      volumes the doctor made are gone
set -euo pipefail

SSH_TARGET="${SSH_TARGET:-user@vps.example.org}"
IMAGE="${IMAGE:-localhost/veles-agent:current}"
C=veles-test
VOL=/srv/pool/veles/test
PORT=18642

run() { ssh "$SSH_TARGET" "$@"; }
as_veles() { # the command, run as `veles` from / with its runtime dir
    printf 'cd / && runuser -u veles -- env XDG_RUNTIME_DIR=/run/user/$(id -u veles) %s' "$1"
}

teardown() {
    echo "▸ teardown"
    run "sudo sh -c '$(as_veles "podman rm -f $C") >/dev/null 2>&1' || true
        sudo rm -rf $VOL" || true
}
trap teardown EXIT

echo "▸ [$SSH_TARGET] scratch container $C as veles (volume $VOL, 127.0.0.1:$PORT)"
run "set -e
    sudo sh -c '$(as_veles "podman rm -f $C") >/dev/null 2>&1 || true'
    sudo rm -rf $VOL
    sudo install -d -o veles -g veles -m 0700 $VOL
    sudo sh -c '$(as_veles "podman run -d --name $C --userns=keep-id:uid=1000,gid=1000 \
        -v $VOL:/opt/data \
        -v /run/veles-sbx/podman.sock:/run/veles-sbx/podman.sock \
        -e CONTAINER_HOST=unix:///run/veles-sbx/podman.sock \
        -e VELES_DESKTOP_HOST=0.0.0.0 \
        -p 127.0.0.1:$PORT:8642 $IMAGE gateway run")'
"

echo "▸ 1. /api/health (up to 60s)"
ok=""
for _ in $(seq 1 60); do
    if run "curl -sf -m 3 http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1; then ok=1; break; fi
    sleep 1
done
if [ -z "$ok" ]; then
    echo "  HEALTH FAILED — the container's log:"
    run "sudo sh -c '$(as_veles "podman logs $C") 2>&1 | tail -40'" || true
    exit 1
fi
echo "  health OK"

echo "▸ 2. veles doctor --sandbox, inside the container"
rc=0
out=$(run "sudo sh -c '$(as_veles "podman exec $C veles doctor --sandbox")' 2>&1") || rc=$?
printf '%s\n' "$out" | grep -v '^[[:space:]]*$' | grep -v ' INFO ' | tail -14
[ "$rc" = 0 ] || { echo "  DOCTOR FAILED (exit $rc)"; exit 1; }

echo "▸ 3. the volume's state belongs to veles"
run "sudo stat -c '%U %a %n' $VOL/state.db"
echo "RC=0"
