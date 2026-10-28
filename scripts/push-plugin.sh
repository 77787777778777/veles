#!/usr/bin/env bash
# push-plugin.sh — install an in-process MODULE onto the VPS and load it
# into the running gateway, without an image rebuild or a restart
# (PLAN-plugins-installer.md R4; D5's first payload).
#
#   scripts/push-plugin.sh irc
#   scripts/push-plugin.sh --no-reload irc       (install only)
#   SSH_TARGET=host scripts/push-plugin.sh irc   (default user@vps.example.org)
#
# What it does, in order:
#   1. builds the gateway PACKAGE with nix (`.#default`, the same velesPkg
#      the image carries) and takes the module from its lib/. A module
#      matches a gateway only when both come from the same kind of build:
#      the build key hashes the RUSTFLAGS and the toolchain, and the
#      symbols the module imports must be the ones the gateway's runtime
#      defines (crates/veles-core/src/module.rs). A `cargo build` on the
#      laptop matches neither, so it is never what gets pushed. A module
#      built from a tree whose core (veles-core/-frontends/-types/-runtime,
#      Cargo.lock) differs from the running image's is refused by the
#      gateway with "core changed" — push the image then, not the module;
#   2. installs the .so CONTENT-ADDRESSED on the VPS
#      (`<data>/modules/<name>/<sha256>.so` + a relative `current`
#      symlink), so a running gateway's mapped copy is never rewritten;
#   3. asks the gateway to `rescan` (first install, or one refused
#      before) and, when the rescan did not start it, to `reload` (already
#      supervised); then prints `veles plugins status`. Never both: a
#      reload right after the rescan's start stopped telegram before its
#      poll ran (VPS, 2026-10-10). The gateway is the authority on the
#      key: a mismatched module is listed `refused` with both keys in the
#      journal.
#
# `--no-reload` stops after step 2. That is the order for a deploy that
# changes core (a new image): install the module built with the new
# image FIRST — the running gateway keeps its mapped copy and reads
# `current` only at its next start or reload — then restart the gateway
# on the new image, which loads it. Reloading the OLD gateway onto the
# NEW module would only get it refused ("core changed") and take the
# platform down until the restart.
#
# The module's RUNPATH is not touched: the gateway has libveles_runtime.so
# and libstd loaded already, and the dynamic linker matches a module's
# NEEDED entries to them by SONAME.
#
# nix sees only TRACKED files: `git add` a new file before pushing.
set -euo pipefail
cd "$(dirname "$0")/.."

SSH_TARGET="${SSH_TARGET:-user@vps.example.org}"
reload=1
if [ "${1:-}" = "--no-reload" ]; then reload=0; shift; fi
name="${1:?usage: $0 [--no-reload] <module-name> (e.g. irc)}"

case "$name" in
    */*) echo "a module NAME, not a path: $name" >&2; exit 2 ;;
esac

# The module crate: veles-module-<name> (a module is a crate of its own —
# crates/veles-module-irc says why), else veles-<name>, else <name>.
crate=""
for cand in "veles-module-$name" "veles-$name" "$name"; do
    if [ -f "crates/$cand/Cargo.toml" ]; then crate="$cand"; break; fi
done
[ -n "$crate" ] || { echo "no crate for module $name under crates/" >&2; exit 1; }

echo "▸ nix build .#default (the gateway package the image carries)"
out=$(nix build .#default --no-link --print-out-paths)
so="$out/lib/lib${crate//-/_}.so"
[ -f "$so" ] || { echo "$so: not built (is $crate a dylib, and tracked?)" >&2; exit 1; }

_keybin=$(mktemp)
trap 'rm -f "$_keybin"' EXIT
objcopy -O binary --only-section=.veles_module_key "$so" "$_keybin" 2>/dev/null || true
mod_key=$(tr -dc '0-9a-f' <"$_keybin" | head -c 64 || true)
[ -n "$mod_key" ] || { echo "no .veles_module_key in $so (not a module?)" >&2; exit 1; }
echo "  module build key: $mod_key"

sha=$(sha256sum "$so" | cut -d' ' -f1)
echo "▸ $crate $sha → $SSH_TARGET as module '$name'"

ssh "$SSH_TARGET" "set -e
    sudo install -d -o veles -g veles -m 0700 /srv/pool/veles/data/modules/$name
"
scp -q "$so" "$SSH_TARGET:/tmp/veles-module-$name-$sha.so"
ssh "$SSH_TARGET" "set -e
    sudo install -o veles -g veles -m 0600 /tmp/veles-module-$name-$sha.so \\
        /srv/pool/veles/data/modules/$name/$sha.so
    rm -f /tmp/veles-module-$name-$sha.so
    # Content-addressed: current names the new build, RELATIVE (the only
    # spelling that survives the host /srv/pool vs container /opt/data
    # bind mount). A running gateway keeps its mapped inode.
    sudo runuser -u veles -- sh -c \\
        'ln -sfn $sha.so /srv/pool/veles/data/modules/$name/current && \\
         printf %s $sha.so > /srv/pool/veles/data/modules/$name/current.sha'
    # The manifest the host reads at discovery (name + kind + entry).
    sudo runuser -u veles -- sh -c \\
        'printf \"name = \\\"$name\\\"\nkind = \\\"module\\\"\nentry = \\\"module.so\\\"\n\" \\
            > /srv/pool/veles/data/modules/$name/plugin.toml'
"

if [ "$reload" = 0 ]; then
    echo "  installed; not reloaded (--no-reload): the gateway loads it at its next start or reload"
    exit 0
fi

echo "▸ asking the gateway to load/reload $name"
ssh "$SSH_TARGET" "set -e
    uid=\$(id -u veles)
    scanned=\$(sudo sh -c \"cd / && runuser -u veles -- env XDG_RUNTIME_DIR=/run/user/\$uid \\
        podman exec veles-gateway veles plugins rescan 2>/dev/null\" || true)
    printf '%s\\n' \"\$scanned\" | grep '^started ' || true
    if printf '%s\\n' \"\$scanned\" | grep -q -E '^started module:$name(:|\$)'; then
        echo '  the rescan started it: no reload'
    else
        sudo sh -c \"cd / && runuser -u veles -- env XDG_RUNTIME_DIR=/run/user/\$uid \\
            podman exec veles-gateway veles plugins reload $name 2>/dev/null || true\"
    fi
    sudo sh -c \"cd / && runuser -u veles -- env XDG_RUNTIME_DIR=/run/user/\$uid \\
        podman exec veles-gateway veles plugins status\"
"
echo "  done — the journal has the module's load/reload/kept-mapped notes"
