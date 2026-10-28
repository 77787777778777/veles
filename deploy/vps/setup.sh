#!/bin/sh
# deploy/vps/setup.sh — the host side of veles on the VPS (vps.example.org).
# Idempotent: run it again after changing any file in deploy/vps/.
#
#   scp -r deploy/vps user@vps.example.org:veles-deploy
#   ssh user@vps.example.org sudo sh veles-deploy/setup.sh [--with-units]
#
# What it sets up (deploy/README.md says why):
#   - /srv/pool, a bind mount of /var/lib/gitea/veles-pool on /dev/sdb
#     (the root disk has no room for container storage)
#   - three loginless system users with homes there, a subuid range each
#     and linger: veles (the gateway), veles-sbx (every sandbox),
#     veles-signer (the ticket signer)
#   - /run/veles-sbx and veles-sbx's podman API socket, which the
#     gateway's container drives sandboxes through
#   - a Tor client (instance `velessbx`): the sandboxes' only way out
#   - a second Tor client (instance `velesweb`) and SearXNG over Tor
#     (searxng-tor, a veles-sbx container): how other people's fetches
#     and searches leave (PLAN-challenge-relay.md D5)
#   - the nft table `inet veles`, loaded at boot by veles-nft.service,
#     and a drop-in that keeps the veles users' managers (so every
#     container) down without it
#   - veles-identd on :113: a verified ident for the owner's IRC client
#     only (identd/veles-identd.py says why)
#   - slopdrop as a system service of its own loginless user, behind
#     the same Tor-only rule as a sandbox, with cgroup limits
#     (slopdrop/slopdrop.service says why; slopdrop/slopdrop.sh moves
#     the owner's old slopdrop over once, with `migrate`)
#   - /var/www/veles-media, the IRC adapter's published media (nginx
#     serves it as veles-media.7778777.online; the gateway's quadlet
#     mounts it at /opt/media-public)
#   - with --with-units: the gateway's and the signer's quadlets
#     (installed, not started — their images come from push-image.sh)
#   - with --with-wg-peers: veles-wg-peers (wg/), which adds the
#     WireGuard peers `veles pool invite` queues on the arbiter (pool
#     pairing, PLAN-sessions-and-phone.md P2d) to wg0
#   - with --take-media: /var/www/veles-media becomes veles'. Run at the
#     switch, once the laptop's last rsync to it has landed and its
#     veles-media-sync units are stopped (deploy/README.md)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
disk=/var/lib/gitea/veles-pool
pool=/srv/pool
media=/var/www/veles-media
# The owner's Unix user: veles-identd answers for it.
owner=wrath
with_units=0
take_media=0
with_wg_peers=0
for a in "$@"; do
    case "$a" in
        --with-units) with_units=1 ;;
        --take-media) take_media=1 ;;
        --with-wg-peers) with_wg_peers=1 ;;
        *) echo "usage: setup.sh [--with-units] [--take-media] [--with-wg-peers]" >&2; exit 2 ;;
    esac
done

[ "$(id -u)" = 0 ] || { echo "setup.sh: run as root (sudo)" >&2; exit 1; }
mountpoint -q /var/lib/gitea || {
    echo "setup.sh: /var/lib/gitea (/dev/sdb) is not mounted" >&2
    exit 1
}

# Run a command as USER with its systemd user manager reachable. Linger
# starts that manager; its bus appears a moment later.
as_user() {
    u=$1
    shift
    uid=$(id -u "$u")
    i=0
    while [ ! -S "/run/user/$uid/bus" ] && [ "$i" -lt 100 ]; do
        sleep 0.1
        i=$((i + 1))
    done
    # From /: podman refuses to start in a directory its user cannot read.
    cd /
    runuser -u "$u" -- env XDG_RUNTIME_DIR="/run/user/$uid" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" "$@"
}

# The first id past every range already in /etc/subuid and /etc/subgid.
next_range() {
    awk -F: 'BEGIN { m = 100000 } { e = $2 + $3; if (e > m) m = e } END { print m }' \
        /etc/subuid /etc/subgid
}

# ── storage ───────────────────────────────────────────────────────────
install -d -m 0755 "$disk" "$pool"
if ! grep -qF "$disk $pool " /etc/fstab; then
    echo "$disk $pool none bind,x-systemd.requires-mounts-for=/var/lib/gitea 0 0" >>/etc/fstab
    systemctl daemon-reload
fi
mountpoint -q "$pool" || mount "$pool"

# ── users ─────────────────────────────────────────────────────────────
for u in veles veles-sbx veles-signer; do
    if ! id -u "$u" >/dev/null 2>&1; then
        useradd --system --user-group --create-home --home-dir "$pool/$u" \
            --shell /usr/sbin/nologin "$u"
    fi
    chmod 0700 "$pool/$u"
    if ! grep -q "^$u:" /etc/subuid; then
        s=$(next_range)
        usermod --add-subuids "$s-$((s + 65535))" --add-subgids "$s-$((s + 65535))" "$u"
    fi
    loginctl enable-linger "$u"
done
# slopdrop's: a system service, so no linger and no subuids. Made here,
# before the firewall, because veles.nft names it.
if ! id -u slopdrop >/dev/null 2>&1; then
    useradd --system --user-group --home-dir "$pool/slopdrop" \
        --shell /usr/sbin/nologin slopdrop
fi
install -d -m 0700 -o veles -g veles "$pool/veles/data"
install -d -m 0700 -o veles-signer -g veles-signer "$pool/veles-signer/signer"

# ── published media ───────────────────────────────────────────────────
# The gateway writes it (as veles: keep-id), nginx reads it (0755/0644).
# Until the switch the laptop's rsync owns it, so an existing dir is
# left alone without --take-media.
[ -d "$media" ] || install -d -m 0755 -o veles -g veles "$media"
if [ "$take_media" = 1 ]; then
    chown -R veles:veles "$media"
    chmod 0755 "$media"
fi

# ── the sandbox user's podman socket ──────────────────────────────────
install -m 0644 "$here/tmpfiles-veles.conf" /etc/tmpfiles.d/veles.conf
systemd-tmpfiles --create /etc/tmpfiles.d/veles.conf
units="$pool/veles-sbx/.config/systemd/user"
install -d -o veles-sbx -g veles-sbx "$pool/veles-sbx/.config" "$pool/veles-sbx/.config/systemd" "$units"
install -m 0644 -o veles-sbx -g veles-sbx \
    "$here/user/veles-sbx/veles-sbx-podman.socket" \
    "$here/user/veles-sbx/veles-sbx-podman.service" "$units/"
as_user veles-sbx systemctl --user daemon-reload
as_user veles-sbx systemctl --user enable --now veles-sbx-podman.socket

# ── Tor, the sandboxes' way out ───────────────────────────────────────
# veles.nft sends a sandbox's IPv4 TCP here and refuses the rest, so
# nothing of theirs leaves from this host's address (tor/torrc says
# why). Its own instance keeps the package's torrc untouched; the name
# is alphanumeric because the package's generator skips any other. The
# default instance is masked first, so the install never starts it.
systemctl mask tor@default.service >/dev/null 2>&1
if [ ! -x /usr/sbin/tor-instance-create ]; then
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends tor
fi
[ -d /etc/tor/instances/velessbx ] || tor-instance-create velessbx
install -m 0644 "$here/tor/torrc" /etc/tor/instances/velessbx/torrc
systemctl daemon-reload
systemctl enable tor@velessbx.service
systemctl reload-or-restart tor@velessbx.service

# ── Tor for other people's fetches ────────────────────────────────────
# The gateway sends a fetch for anyone but the owner through this
# instance's SOCKS port (tor/torrc-web says why it is a second one).
[ -d /etc/tor/instances/velesweb ] || tor-instance-create velesweb
install -m 0644 "$here/tor/torrc-web" /etc/tor/instances/velesweb/torrc
install -d -m 0755 /etc/systemd/system/tor@velesweb.service.d
install -m 0644 "$here/tor/tor-web.conf" /etc/systemd/system/tor@velesweb.service.d/veles.conf
systemctl daemon-reload
systemctl enable tor@velesweb.service
systemctl reload-or-restart tor@velesweb.service

# ── the firewall table ────────────────────────────────────────────────
install -d -m 0755 /etc/veles
install -m 0644 "$here/veles.nft" /etc/veles/veles.nft
nft -c -f /etc/veles/veles.nft
install -m 0644 "$here/veles-nft.service" /etc/systemd/system/veles-nft.service
systemctl daemon-reload
# reenable: its [Install] moved from multi-user.target to sysinit.target.
systemctl reenable veles-nft.service
# A reload swaps the table in one transaction; a restart would leave the
# users unconfined between its stop and its start.
systemctl reload-or-restart veles-nft.service
for u in veles veles-sbx veles-signer; do
    d="/etc/systemd/system/user@$(id -u "$u").service.d"
    install -d -m 0755 "$d"
    install -m 0644 "$here/veles-user-manager.conf" "$d/veles.conf"
done
systemctl daemon-reload

# ── SearXNG over Tor, for other people's searches ─────────────────────
# A veles-sbx container, so it leaves through Tor like a sandbox
# (user/veles-sbx/searxng-tor.container). Its settings come from the
# template with the secret it already has, or a fresh one; the image is
# pulled (over Tor) when missing. After the firewall: its port is the
# gateway's only through it.
sx="$pool/veles-sbx/searxng-tor"
install -d -m 0755 -o veles-sbx -g veles-sbx "$sx"
secret=$(sed -n 's/^  secret_key: "\([0-9a-f]*\)"$/\1/p' "$sx/settings.yml" 2>/dev/null || true)
[ -n "$secret" ] || secret=$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')
sed "s/@SECRET@/$secret/" "$here/searxng-tor/settings.yml" >"$sx/settings.yml.new"
chmod 0644 "$sx/settings.yml.new"
chown veles-sbx:veles-sbx "$sx/settings.yml.new"
mv -f "$sx/settings.yml.new" "$sx/settings.yml"
q="$pool/veles-sbx/.config/containers/systemd"
install -d -o veles-sbx -g veles-sbx "$pool/veles-sbx/.config/containers" "$q"
install -m 0644 -o veles-sbx -g veles-sbx "$here/user/veles-sbx/searxng-tor.container" "$q/"
img=$(sed -n 's/^Image=//p' "$here/user/veles-sbx/searxng-tor.container")
as_user veles-sbx podman image exists "$img" || as_user veles-sbx podman pull -q "$img" >/dev/null
as_user veles-sbx systemctl --user daemon-reload
as_user veles-sbx systemctl --user restart searxng-tor.service

# ── ident for the owner's IRC client ──────────────────────────────────
# Sandboxes reach IRC from the owner's address; a verified ident is what
# tells them apart (identd/veles-identd.py). The script checks itself
# before it goes live.
python3 -I "$here/identd/veles-identd.py" --self-test
install -d -m 0755 /usr/local/lib/veles-identd
install -m 0755 "$here/identd/veles-identd.py" /usr/local/lib/veles-identd/veles-identd.py
for u in veles-identd.socket veles-identd@.service veles-identd.slice; do
    install -m 0644 "$here/identd/$u" "/etc/systemd/system/$u"
done
systemctl daemon-reload
systemctl enable veles-identd.socket
systemctl restart veles-identd.socket

# ── slopdrop, the channels' Tcl interpreter ───────────────────────────
# Its user, directory, unit and deploy key. The first start is
# `slopdrop/slopdrop.sh migrate`, once (it stops the owner's old user
# unit); a new build after that is `slopdrop.sh update`.
sh "$here/slopdrop/slopdrop.sh" setup

# ── the containers' units ─────────────────────────────────────────────
if [ "$with_units" = 1 ]; then
    for pair in veles:veles-gateway veles-signer:veles-signer; do
        u=${pair%%:*}
        n=${pair#*:}
        q="$pool/$u/.config/containers/systemd"
        install -d -o "$u" -g "$u" "$pool/$u/.config" "$pool/$u/.config/containers" "$q"
        install -m 0644 -o "$u" -g "$u" "$here/user/$u/$n.container" "$q/"
        as_user "$u" systemctl --user daemon-reload
    done
fi

# Pool pairing's WireGuard peers (opt-in): the queue the gateway writes,
# the root helper that applies it, and the path unit that runs it.
if [ "$with_wg_peers" = 1 ]; then
    install -m 0755 "$here/wg/veles-wg-peers.sh" /usr/local/sbin/veles-wg-peers
    install -m 0644 "$here/wg/veles-wg-peers.path" "$here/wg/veles-wg-peers.service" /etc/systemd/system/
    install -d -o veles -g veles -m 0700 "$pool/veles/data/pool"
    install -d -o veles -g veles -m 0700 "$pool/veles/data/pool/wg-pending"
    systemctl daemon-reload
    systemctl enable --now veles-wg-peers.path
fi

echo "setup.sh: done (users: veles veles-sbx veles-signer slopdrop; pool: $pool)"
