#!/usr/bin/env bash
# push-process-plugin.sh — install a PROCESS plugin's binary onto the VPS
# and respawn it under the running gateway, without an image rebuild or
# a gateway restart (PLAN-plugins-installer.md Phase 3 / M6).
#
#   scripts/push-process-plugin.sh atproto
#   scripts/push-process-plugin.sh --no-reload atproto     (install only)
#   SSH_TARGET=host scripts/push-process-plugin.sh atproto  (default user@vps.example.org)
#
# Why this exists (2026-10-09): the gateway image carries
# `bin/veles-plugin-<name>`, but what RUNS is the copy under
# `<data>/process-plugins/<name>/` — the host spawns that path, and a
# `veles plugins reload` respawns whatever is there now. The R5–R8 image
# pushes never touched it, so the VPS ran the 2026-10-07 atproto build
# through four core deploys: it knew neither the R5 `hosted.*` meta (a
# public /tcl reply went out with the raw URL placeholder) nor the
# generic repo services (`post.create`, `repo.delete_record`,
# `hosted.delete`), so the slopdrop module found "no public adapter"
# when the owner answered a withheld entry. A core deploy that changes a
# process plugin's protocol or services ships the plugin too: this.
#
# What it does, in order:
#   1. builds the plugin's STANDALONE static musl package with nix
#      (`.#veles-plugin-<name>`, flake.nix `staticPlugin`) and takes the
#      binary from it. NOT velesPkg's `bin/` copy: that one is built in
#      one cargo invocation with the whole workspace, where cargo unifies
#      the `irc` crate's `aws-lc-rs` into the same rustls as the plugin's
#      `ring`, and rustls then refuses to pick a provider at the first
#      connect (2026-10-09: the Jetstream task panicked at start). The
#      plugin now installs its provider explicitly, but the standalone
#      build is the artifact Phase 3 deployed and the one a bundle ships;
#   2. installs it ATOMICALLY on the VPS: scp to /tmp, `install` to
#      `<dir>/veles-plugin-<name>.new`, the old copy kept as
#      `veles-plugin-<name>.pre-<sha8>` (the rollback: `mv` it back and
#      reload), then one `mv` over the entry the manifest names. A
#      running plugin keeps its mapped inode; the manifest
#      (name/kind/entry) is written only if absent;
#   3. unless `--no-reload`, asks the gateway to `reload <name>` — the
#      host respawns the plugin from the new file (atproto resumes
#      Jetstream from its cursor) — and prints `veles plugins status`.
#
# A process plugin speaks protocol v1 over JSON-RPC, so it does not need
# the module tier's build key: any gateway of the same protocol runs it.
#
# nix sees only TRACKED files: `git add` a new file before pushing.
set -euo pipefail
cd "$(dirname "$0")/.."

SSH_TARGET="${SSH_TARGET:-user@vps.example.org}"
reload=1
if [ "${1:-}" = "--no-reload" ]; then reload=0; shift; fi
name="${1:?usage: $0 [--no-reload] <plugin-name> (e.g. atproto)}"

case "$name" in
    */*) echo "a plugin NAME, not a path: $name" >&2; exit 2 ;;
esac
[ -f "crates/veles-$name/Cargo.toml" ] || { echo "no crate veles-$name under crates/" >&2; exit 1; }

echo "▸ nix build .#veles-plugin-$name (the standalone static musl plugin)"
out=$(nix build ".#veles-plugin-$name" --no-link --print-out-paths)
bin="$out/lib/veles/plugins/$name/veles-plugin-$name"
[ -x "$bin" ] || { echo "$bin: not in the package (is packages.veles-plugin-$name a staticPlugin in flake.nix?)" >&2; exit 1; }

sha=$(sha256sum "$bin" | cut -d' ' -f1)
echo "▸ veles-plugin-$name $sha → $SSH_TARGET"

dir="/srv/pool/veles/data/process-plugins/$name"
entry="veles-plugin-$name"
scp -q "$bin" "$SSH_TARGET:/tmp/$entry-$sha"
ssh "$SSH_TARGET" "set -e
    sudo install -d -o veles -g veles -m 0755 $dir
    sudo install -o veles -g veles -m 0755 /tmp/$entry-$sha $dir/$entry.new
    rm -f /tmp/$entry-$sha
    # The data tree is 0700 veles: every test goes through sudo.
    if sudo test -f $dir/$entry; then
        old=\$(sudo sha256sum $dir/$entry | cut -c1-8)
        if [ \"\$old\" = \"$(printf %s "$sha" | cut -c1-8)\" ]; then
            echo '  the installed binary is already this build'
            sudo rm -f $dir/$entry.new
        else
            sudo cp -p $dir/$entry $dir/$entry.pre-\$old
            echo \"  old copy kept as $entry.pre-\$old\"
            sudo mv $dir/$entry.new $dir/$entry
        fi
    else
        sudo mv $dir/$entry.new $dir/$entry
    fi
    if ! sudo test -f $dir/plugin.toml; then
        sudo runuser -u veles -- sh -c \\
            'printf \"name = \\\"$name\\\"\nkind = \\\"process\\\"\nentry = \\\"$entry\\\"\n\" > $dir/plugin.toml'
        echo '  manifest written'
    fi
    printf '  installed: '; sudo sha256sum $dir/$entry | cut -c1-16
"

if [ "$reload" = 0 ]; then
    echo "  installed; not reloaded (--no-reload): the gateway spawns it at its next start or reload"
    exit 0
fi

echo "▸ asking the gateway to reload $name"
ssh "$SSH_TARGET" "set -e
    uid=\$(id -u veles)
    sudo sh -c \"cd / && runuser -u veles -- env XDG_RUNTIME_DIR=/run/user/\$uid \\
        podman exec veles-gateway veles plugins reload $name 2>/dev/null || true\"
    sleep 5
    sudo sh -c \"cd / && runuser -u veles -- env XDG_RUNTIME_DIR=/run/user/\$uid \\
        podman exec veles-gateway veles plugins status\"
"
echo "  done — the journal has the plugin's respawn and (atproto) its Jetstream cursor resume"
