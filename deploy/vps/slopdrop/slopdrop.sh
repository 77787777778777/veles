#!/bin/sh
# deploy/vps/slopdrop/slopdrop.sh — slopdrop as a service of its own user
# (slopdrop.service says why).
#
#   sudo sh slopdrop.sh setup     the user, its directory, the unit and
#                                 its deploy key (idempotent)
#   sudo sh slopdrop.sh migrate   once: the state and the config from the
#                                 owner's ~/slopdrop, then the first start
#   sudo sh slopdrop.sh update    a new build: binary and tcl/, restart
#   sudo sh slopdrop.sh stock-proxy
#                                 the stock commands' way out that is not
#                                 Tor (stock-proxy.service), and
#                                 `[tcl] stock_proxy` (idempotent)
#
# The build stays the owner's, in ~/slopdrop:
#   git pull --ff-only && cargo build --release --features frontend-web
# (a build without frontend-web restart-loops with no API), then `update`.
#
# What lives where, in /srv/pool/slopdrop:
#   bin/, tcl/      root's, readable by slopdrop: it cannot rewrite them
#   config.toml     root's, readable by slopdrop (it holds the bridge's
#                   tokens; slopdrop cannot change its own settings)
#   state/          slopdrop's: the git-backed interpreter state
#   .ssh/           slopdrop's deploy key for the state repo, and the
#                   state repo host's keys (gitea on this host)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
home=/srv/pool/slopdrop
user=slopdrop
owner=wrath
src=$(getent passwd "$owner" | cut -d: -f6)/slopdrop
proxy=socks5h://10.0.30.1:9050

[ "$(id -u)" = 0 ] || { echo "slopdrop.sh: run as root (sudo)" >&2; exit 1; }

# The owner's user manager, for their old slopdrop unit.
as_owner() {
    uid=$(id -u "$owner")
    cd /
    runuser -u "$owner" -- env XDG_RUNTIME_DIR="/run/user/$uid" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" "$@"
}

setup() {
    if ! id -u "$user" >/dev/null 2>&1; then
        useradd --system --user-group --home-dir "$home" --shell /usr/sbin/nologin "$user"
    fi
    install -d -m 0750 -o "$user" -g "$user" "$home"
    install -d -m 0750 -o root -g "$user" "$home/bin" "$home/tcl"
    install -d -m 0700 -o "$user" -g "$user" "$home/.ssh"
    if [ ! -f "$home/.ssh/id_ed25519" ]; then
        runuser -u "$user" -- ssh-keygen -q -t ed25519 -N '' \
            -C "slopdrop-state@$(hostname)" -f "$home/.ssh/id_ed25519"
    fi
    # The state repo's host is this host's sshd (gitea), so its keys are
    # read from here rather than from the network.
    kh=$(mktemp)
    for k in /etc/ssh/ssh_host_*_key.pub; do
        printf 'gitea.phosphoros.delivery %s\n' "$(cut -d' ' -f1,2 "$k")" >>"$kh"
    done
    install -m 0644 -o "$user" -g "$user" "$kh" "$home/.ssh/known_hosts"
    rm -f "$kh"
    install -m 0644 "$here/slopdrop.service" /etc/systemd/system/slopdrop.service
    systemctl daemon-reload
    echo "slopdrop.sh: the state repo's deploy key (write access) is:"
    cat "$home/.ssh/id_ed25519.pub"
}

update() {
    b="$src/target/release/slopdrop"
    [ -x "$b" ] || { echo "slopdrop.sh: no build at $b" >&2; exit 1; }
    install -m 0750 -o root -g "$user" "$b" "$home/bin/slopdrop.new"
    mv -f "$home/bin/slopdrop.new" "$home/bin/slopdrop"
    rm -rf "$home/tcl.new" "$home/tcl.old"
    cp -r "$src/tcl" "$home/tcl.new"
    chown -R root:"$user" "$home/tcl.new"
    chmod -R u=rwX,g=rX,o= "$home/tcl.new"
    mv "$home/tcl" "$home/tcl.old"
    mv "$home/tcl.new" "$home/tcl"
    rm -rf "$home/tcl.old"
    if systemctl is-enabled --quiet slopdrop.service; then
        systemctl restart slopdrop.service
        check
    fi
}

# Up means the bridge listens and nothing restarted.
check() {
    i=0
    while ! ss -ltn | grep -q '10\.0\.30\.1:8080 ' && [ "$i" -lt 30 ]; do
        sleep 1
        i=$((i + 1))
    done
    ss -ltn | grep -q '10\.0\.30\.1:8080 ' || {
        echo "slopdrop.sh: the bridge is not listening on 10.0.30.1:8080" >&2
        exit 1
    }
    n=$(systemctl show slopdrop.service -p NRestarts --value)
    [ "$n" = 0 ] || { echo "slopdrop.sh: NRestarts=$n" >&2; exit 1; }
    echo "slopdrop.sh: up, as $(systemctl show slopdrop.service -p User --value)"
}

migrate() {
    [ ! -e "$home/state" ] || { echo "slopdrop.sh: $home/state exists: migrated already" >&2; exit 1; }
    [ -d "$src/state/.git" ] || { echo "slopdrop.sh: no state at $src/state" >&2; exit 1; }
    setup
    # Two interpreters must never commit to one repo: the old one stops
    # first, and its unit is disabled so nothing starts it again.
    as_owner systemctl --user disable --now slopdrop.service || true
    cp -a "$src/state" "$home/state"
    chown -R "$user:$user" "$home/state"
    # The config, with the state repo's key and the proxy changed and
    # nothing else (checked by parsing both).
    python3 -I - "$src/config.toml" "$home/config.toml.new" "$home/.ssh/id_ed25519" "$proxy" <<'EOF'
import sys, tomllib
src, dst, key, proxy = sys.argv[1:]
text = open(src, encoding="utf-8").read()
before = tomllib.loads(text)
out, in_tcl, added = [], False, False
for line in text.splitlines(keepends=True):
    s = line.strip()
    if s.startswith("["):
        if in_tcl and not added:
            out.append(f'http_proxy = "{proxy}"\n')
            added = True
        in_tcl = s == "[tcl]"
    if in_tcl and s.split("=")[0].strip() in ("ssh_key", "http_proxy"):
        if s.startswith("ssh_key"):
            out.append(f'ssh_key = "{key}"\n')
        continue
    out.append(line)
if in_tcl and not added:
    out.append(f'http_proxy = "{proxy}"\n')
new = "".join(out)
after = tomllib.loads(new)
want = dict(before)
want["tcl"] = dict(before["tcl"], ssh_key=key, http_proxy=proxy)
if after != want:
    sys.exit("slopdrop.sh: the new config is not the old one plus the key and the proxy")
open(dst, "w", encoding="utf-8").write(new)
EOF
    chown root:"$user" "$home/config.toml.new"
    chmod 0640 "$home/config.toml.new"
    mv "$home/config.toml.new" "$home/config.toml"
    # The old state stays beside the old tree, renamed, for a rollback.
    mv "$src/state" "$src/state.migrated-$(date +%Y%m%d)"
    systemctl enable slopdrop.service
    update
}

# Yahoo refuses Tor exits, so the stock commands leave through a proxy
# of their own (stock-proxy.service says why and what it allows). The
# proxy's password is made once and kept; the proxy's config and
# slopdrop's `[tcl] stock_proxy` are rewritten from it every run, and
# slopdrop restarts only when its config changed.
stock_proxy() {
    conf=/etc/veles/slopdrop-stock-proxy.conf
    port=8650
    # The binary alone: the `tinyproxy` package starts the distro's own
    # proxy (port 8888, every interface) the moment it is installed.
    dpkg -s tinyproxy-bin >/dev/null 2>&1 ||
        DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends tinyproxy-bin
    install -d -m 0755 /etc/veles
    install -m 0644 "$here/stock-proxy.filter" /etc/veles/slopdrop-stock-proxy.filter
    # Ubuntu's AppArmor profile for /usr/bin/tinyproxy reads only
    # /etc/tinyproxy/tinyproxy.conf; its local file lets it read ours.
    if [ -f /etc/apparmor.d/tinyproxy ]; then
        install -m 0644 "$here/stock-proxy.apparmor" /etc/apparmor.d/local/tinyproxy
        apparmor_parser -r /etc/apparmor.d/tinyproxy
    fi
    pw=
    [ -f "$conf" ] && pw=$(sed -n 's/^BasicAuth slopdrop //p' "$conf")
    [ -n "$pw" ] || pw=$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')
    (
        umask 077
        cat >"$conf.new" <<EOF
# $conf — written by slopdrop.sh stock-proxy
# (deploy/vps/slopdrop/stock-proxy.service). Root's, 0600: the password
# is slopdrop's, in its config.toml as \`[tcl] stock_proxy\`.
Port $port
Listen 127.0.0.1
Allow 127.0.0.1
BasicAuth slopdrop $pw
ConnectPort 443
Filter "/etc/veles/slopdrop-stock-proxy.filter"
FilterType ere
FilterURLs Off
FilterDefaultDeny Yes
DisableViaHeader Yes
Timeout 30
MaxClients 16
LogLevel Notice
EOF
    )
    mv -f "$conf.new" "$conf"
    install -m 0644 "$here/stock-proxy.service" /etc/systemd/system/slopdrop-stock-proxy.service
    # slopdrop's own unit wants the proxy (and starts after it).
    install -m 0644 "$here/slopdrop.service" /etc/systemd/system/slopdrop.service
    systemctl daemon-reload
    systemctl enable slopdrop-stock-proxy.service
    systemctl restart slopdrop-stock-proxy.service
    i=0
    while ! ss -ltn | grep -q "127\.0\.0\.1:$port " && [ "$i" -lt 15 ]; do
        sleep 1
        i=$((i + 1))
    done
    ss -ltn | grep -q "127\.0\.0\.1:$port " || {
        echo "slopdrop.sh: the stock proxy is not listening on 127.0.0.1:$port" >&2
        exit 1
    }
    # The config, with `[tcl] stock_proxy` set and nothing else changed
    # (checked by parsing both). The URL carries the password, so it
    # goes in through the environment, never argv or the terminal.
    rc=0
    STOCK_PROXY_URL="http://slopdrop:$pw@127.0.0.1:$port" \
        python3 -I - "$home/config.toml" "$home/config.toml.new" <<'EOF' || rc=$?
import os, sys, tomllib
src, dst = sys.argv[1:]
url = os.environ["STOCK_PROXY_URL"]
text = open(src, encoding="utf-8").read()
before = tomllib.loads(text)
if before.get("tcl", {}).get("stock_proxy") == url:
    sys.exit(3)
line_out = f'stock_proxy = "{url}"\n'
out, in_tcl, added = [], False, False
for line in text.splitlines(keepends=True):
    s = line.strip()
    if s.startswith("["):
        if in_tcl and not added:
            out.append(line_out)
            added = True
        in_tcl = s == "[tcl]"
    if in_tcl and s.split("=")[0].strip() == "stock_proxy":
        continue
    out.append(line)
if in_tcl and not added:
    if out and not out[-1].endswith("\n"):
        out.append("\n")
    out.append(line_out)
    added = True
new = "".join(out)
after = tomllib.loads(new)
want = dict(before)
want["tcl"] = dict(before["tcl"], stock_proxy=url)
if not added or after != want:
    sys.exit("slopdrop.sh: the new config is not the old one plus [tcl] stock_proxy")
open(dst, "w", encoding="utf-8").write(new)
EOF
    case $rc in
        3) echo "slopdrop.sh: [tcl] stock_proxy already set" ;;
        0)
            cp -p "$home/config.toml" "$home/config.toml.bak-pre-stock-proxy"
            chown root:"$user" "$home/config.toml.new"
            chmod 0640 "$home/config.toml.new"
            mv "$home/config.toml.new" "$home/config.toml"
            echo "slopdrop.sh: [tcl] stock_proxy set"
            if systemctl is-enabled --quiet slopdrop.service; then
                systemctl restart slopdrop.service
                check
            fi
            ;;
        *) rm -f "$home/config.toml.new"; exit "$rc" ;;
    esac
}

case "${1:-}" in
    setup) setup ;;
    update) update ;;
    migrate) migrate ;;
    check) check ;;
    stock-proxy) stock_proxy ;;
    *) echo "usage: slopdrop.sh setup|migrate|update|check|stock-proxy" >&2; exit 2 ;;
esac
