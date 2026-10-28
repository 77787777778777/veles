#!/usr/bin/env bash
# install-module-dev.sh — install a module built in THIS tree's
# target/debug into the laptop's module dir, content-addressed, for the
# dev gateway (veles-gateway-dev runs target/debug/veles).
#
#   scripts/install-module-dev.sh irc slopdrop
#
# Build the gateway AND the modules in ONE cargo invocation first
# (`cargo build --workspace --exclude veles-runtime`): a module built on
# its own links a different runtime variant and the gateway refuses it
# (crates/veles-core/src/module.rs). The module dir is what
# `modulehost::default_roots` lists second: `<data>/modules/<name>/`.
set -euo pipefail
cd "$(dirname "$0")/.."
data="${XDG_DATA_HOME:-$HOME/.local/share}/veles/modules"
for name in "$@"; do
    so="target/debug/deps/libveles_module_${name//-/_}.so"
    [ -f "$so" ] || { echo "$so: not built (cargo build --workspace --exclude veles-runtime)" >&2; exit 1; }
    sha=$(sha256sum "$so" | cut -d' ' -f1)
    dir="$data/$name"
    install -d -m 0700 "$dir"
    [ -f "$dir/$sha.so" ] || install -m 0600 "$so" "$dir/$sha.so"
    ln -sfn "$sha.so" "$dir/current"
    printf 'name = "%s"\nkind = "module"\nentry = "module.so"\n' "$name" >| "$dir/plugin.toml"
    echo "$name → $dir/$sha.so (current)"
done
echo "restart the dev gateway to load them: systemctl --user restart veles-gateway-dev (check session_runs first)"
