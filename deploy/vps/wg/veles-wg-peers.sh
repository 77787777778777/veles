#!/bin/sh
# deploy/vps/wg/veles-wg-peers.sh → /usr/local/sbin/veles-wg-peers
#
# The hub's half of pool pairing (PLAN-sessions-and-phone.md P2d, USER
# decision D10): `veles pool invite` on the arbiter mints a phone's
# WireGuard keys and address and queues the hub's side of that peer in
# its data dir; this applies it. The gateway runs rootless and cannot
# touch wg0; this runs as root, from veles-wg-peers.path, whenever the
# queue changes.
#
#   <queue>/<name>.conf    add:    "[Peer]", "PublicKey = <key>",
#                                  "AllowedIPs = <a.b.c.d>/32"
#   <queue>/<name>.remove  remove: the peer's public key
#
# Root reading a directory another user writes: everything is checked
# before it is used. Only regular files (a planted symlink is skipped,
# never followed), only a 44-character base64 key, only a /32 inside the
# subnet that no peer already holds (a gateway cannot take over the
# laptop's address), and the config file is rewritten from what was
# parsed, never from the queued text. Done files move to <queue>/done.
#
#   veles-wg-peers <interface> <subnet a.b.c.0/24> <queue dir>
#
# VELES_WG_DRYRUN=1 prints the wg commands and the config instead of
# running them (scripts/test-wg-peers.sh).
set -eu

iface=${1:?interface}
subnet=${2:?subnet}
queue=${3:?queue dir}
conf=${VELES_WG_CONF:-/etc/wireguard/$iface.conf}
dry=${VELES_WG_DRYRUN:-0}

net=${subnet%/*}
bits=${subnet#*/}
[ "$bits" = 24 ] || { echo "veles-wg-peers: only /24 subnets are handled ($subnet)" >&2; exit 2; }
prefix=${net%.*}

run() {
    if [ "$dry" = 1 ]; then echo "would run: $*"; else "$@"; fi
}

# Addresses already held by a peer of the interface (or written in its
# config), so a queued peer cannot claim one.
held() {
    if [ "$dry" = 1 ]; then
        cat "${VELES_WG_HELD:-/dev/null}"
    else
        wg show "$iface" allowed-ips 2>/dev/null | tr '\t ' '\n\n' | grep -E '^[0-9.]+/32$' | sed 's|/32$||' || true
    fi
    grep -E '^[[:space:]]*(Address|AllowedIPs)[[:space:]]*=' "$conf" 2>/dev/null \
        | sed 's/.*=//; s/,/\n/g' | tr -d ' ' | sed 's|/.*||' || true
}

valid_key() {
    printf '%s' "$1" | grep -Eq '^[A-Za-z0-9+/]{43}=$'
}

# `done/` is written by root inside a directory the gateway owns: it has
# to be a plain directory, never something the gateway pointed elsewhere
# (a symlink there would make root drop gateway-chosen files wherever it
# points). Refuse the whole run rather than guess.
if [ -L "$queue/done" ] || { [ -e "$queue/done" ] && [ ! -d "$queue/done" ]; }; then
    echo "veles-wg-peers: $queue/done is not a plain directory; nothing applied" >&2
    exit 2
fi
mkdir -p "$queue/done"

# The keys this helper itself added: each sits under a "# veles pool:"
# line in the config. Only those may be removed through the queue — the
# hub's own peers (the laptop, the phone) are root's to manage, and a
# removal of one of them would free its address for a queued newcomer.
pool_key() {
    awk -v k="$1" '
        /^# veles pool: / { mine = 1; next }
        /^\[Peer\]/ { inpeer = mine; mine = 0; next }
        /^\[/ { inpeer = 0 }
        inpeer && /^PublicKey[[:space:]]*=/ { v = $0; sub(/^PublicKey[[:space:]]*=[[:space:]]*/, "", v); if (v == k) found = 1 }
        END { exit found ? 0 : 1 }
    ' "$conf" 2>/dev/null
}

for f in $(find "$queue" -maxdepth 1 -type f -name '*.conf' | sort); do
    name=$(basename "$f" .conf)
    printf '%s' "$name" | grep -Eq '^[a-z0-9-]{1,32}$' || { echo "skip $f: bad name" >&2; continue; }
    key=$(sed -n 's/^PublicKey[[:space:]]*=[[:space:]]*//p' "$f" | head -1 | tr -d '\r')
    ip=$(sed -n 's/^AllowedIPs[[:space:]]*=[[:space:]]*//p' "$f" | head -1 | tr -d '\r ')
    addr=${ip%/32}
    if ! valid_key "$key"; then echo "skip $name: not a WireGuard public key" >&2; continue; fi
    if [ "$ip" != "$addr/32" ] || [ "${addr%.*}" != "$prefix" ]; then
        echo "skip $name: $ip is not a /32 in $subnet" >&2; continue
    fi
    last=${addr##*.}
    case "$last" in ''|*[!0-9]*) echo "skip $name: bad address $addr" >&2; continue ;; esac
    if [ "$last" -lt 1 ] || [ "$last" -gt 254 ]; then echo "skip $name: bad address $addr" >&2; continue; fi
    if held | grep -qx "$addr"; then
        echo "skip $name: $addr is already held by a peer" >&2; continue
    fi
    run wg set "$iface" peer "$key" allowed-ips "$addr/32"
    stanza=$(printf '\n# veles pool: %s\n[Peer]\nPublicKey = %s\nAllowedIPs = %s/32\n' "$name" "$key" "$addr")
    if [ "$dry" = 1 ]; then
        echo "would append to $conf:$stanza"
    else
        printf '%s\n' "$stanza" >> "$conf"
    fi
    mv "$f" "$queue/done/"
    echo "added $name: $addr"
done

for f in $(find "$queue" -maxdepth 1 -type f -name '*.remove' | sort); do
    name=$(basename "$f" .remove)
    key=$(head -c 64 "$f" | tr -d '\r\n ')
    valid_key "$key" || { echo "skip $f: not a key" >&2; continue; }
    if [ "$dry" != 1 ] && ! pool_key "$key"; then
        echo "skip $name: $key is not a peer this helper added" >&2
        mv "$f" "$queue/done/"
        continue
    fi
    run wg set "$iface" peer "$key" remove
    if [ "$dry" = 1 ]; then
        echo "would drop the [Peer] with $key from $conf"
    elif [ -f "$conf" ]; then
        # Drop that one [Peer] block (and the comment line above it).
        awk -v k="$key" '
            /^# veles pool: / { held_comment = $0; next }
            /^\[Peer\]/ { if (block != "" && !drop) printf "%s", block; block = (held_comment != "" ? held_comment "\n" : "") $0 "\n"; drop = 0; held_comment = ""; inpeer = 1; next }
            inpeer && /^PublicKey[[:space:]]*=/ { v = $0; sub(/^PublicKey[[:space:]]*=[[:space:]]*/, "", v); if (v == k) drop = 1 }
            inpeer { block = block $0 "\n"; next }
            { if (held_comment != "") { print held_comment; held_comment = "" } print }
            END { if (block != "" && !drop) printf "%s", block }
        ' "$conf" > "$conf.veles-tmp" && chmod 600 "$conf.veles-tmp" && mv "$conf.veles-tmp" "$conf"
    fi
    mv "$f" "$queue/done/"
    echo "removed $name"
done
