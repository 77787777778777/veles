#!/usr/bin/env bash
# deploy/laptop/setup.sh — the laptop's local SearXNG, as the owner's own
# user (no root): the quadlet (user/searxng.container), its settings
# (searxng/settings.yml), the idle watch (user/searxng-idle.{service,timer},
# lib/searxng-idle-stop.sh). What veles web_search's first rung needs on
# this node and `veles recover` never brings back (it is outside veles).
#
# Idempotent: an existing secret_key is kept, the image is pulled only
# when missing, the units are (re)installed and reloaded, the timer
# enabled, the service started and checked for JSON on 127.0.0.1:8888.
#
#   bash deploy/laptop/setup.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cfg=${XDG_CONFIG_HOME:-$HOME/.config}
cache=${XDG_CACHE_HOME:-$HOME/.cache}
sx="$cfg/searxng"

secret=$(sed -n 's/^  secret_key: "\([0-9a-f]*\)"$/\1/p' "$sx/settings.yml" 2>/dev/null || true)
[ -n "$secret" ] || secret=$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')
tmp=$(mktemp)
sed "s/@SECRET@/$secret/" "$here/searxng/settings.yml" >"$tmp"
# The container runs as its own user (uid 977); in podman's user
# namespace that user owns the settings dir, as it does on the VPS.
podman unshare sh -c "install -d -m 0755 -o 977 -g 977 '$sx' && install -m 0644 -o 977 -g 977 '$tmp' '$sx/settings.yml'"
rm -f "$tmp"

install -d "$cfg/containers/systemd" "$cfg/systemd/user" "$HOME/.local/lib/veles" "$cache/veles"
install -m 0644 "$here/user/searxng.container" "$cfg/containers/systemd/"
install -m 0644 "$here/user/searxng-idle.service" "$here/user/searxng-idle.timer" "$cfg/systemd/user/"
install -m 0755 "$here/lib/searxng-idle-stop.sh" "$HOME/.local/lib/veles/"

img=$(sed -n 's/^Image=//p' "$here/user/searxng.container")
podman image exists "$img" || podman pull -q "$img" >/dev/null
systemctl --user daemon-reload
systemctl --user enable --now searxng-idle.timer
systemctl --user start searxng.service
for _ in $(seq 1 30); do
    if curl -fsS 'http://127.0.0.1:8888/search?q=veles&format=json' -o /dev/null 2>/dev/null; then
        echo "searxng: serving JSON on 127.0.0.1:8888; the idle watch is on (searxng-idle.timer)"
        exit 0
    fi
    sleep 1
done
echo "searxng: not serving after 30 s; see: systemctl --user status searxng.service" >&2
exit 1
