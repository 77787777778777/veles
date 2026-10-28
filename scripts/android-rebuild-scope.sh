#!/usr/bin/env bash
# scripts/android-rebuild-scope.sh — a desktop edit must not rebuild the
# Android engine.
#
# USER 2026-10-01: "the tcl rebuild thing sounds nasty, we need to do that
# one before I start deploying because we're not doing a 20-minute round
# trip each iteration". The Android engine was built from the whole
# source tree, desktop included, so every edit to a desktop .tcl file
# cross-built it again before the APK could be repacked.
#
# This copies the tracked tree (the working copy, as `nix flake check`
# sees it), evaluates the APKs' and bundles' derivations, edits a desktop
# .tcl file, android/desktop.tcl, an engine catalog and the README, and
# evaluates them again. Only the payload, the APK and the bundle may
# change; any other derivation that does (the engine, ripgrep, Tk, the
# compiled Java) is named and the script fails. It also fails when the
# engine `include_str!`s a file engineSrc leaves out. Nothing is built.
#
# Usage:  bash scripts/android-rebuild-scope.sh     (precommit runs it)
set -euo pipefail
cd "$(dirname "$0")/.."

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tree=$work/tree
mkdir -p "$tree"
# Tracked files from the working copy; a tracked file deleted but not
# yet staged is skipped, as a flake skips it.
git ls-files -z | tar --null --ignore-failed-read -T - -cf - 2>/dev/null | tar -xf - -C "$tree"

# The other direction: engineSrc is opt-in, so a file the engine
# `include_str!`s from outside it fails only the APK's build, which no
# gate runs. Every literal include under crates/ must resolve inside it.
src=$(nix eval --raw "path:$tree#packages.x86_64-linux.veles-android-x86_64.src")
missing=""
while IFS=: read -r file lit; do
    rel=$(realpath -m --relative-to="$tree" "$tree/$(dirname "$file")/$lit")
    [ -e "$src/$rel" ] || missing="$missing $file:$lit"
done < <(cd "$tree" && grep -roE --include='*.rs' 'include_(str|bytes)!\("[^"]+"\)' crates \
             | sed -E 's/^([^:]+):include_(str|bytes)!\("([^"]+)"\)$/\1:\3/')
if [ -n "$missing" ]; then
    echo "the engine includes files flake.nix engineSrc does not carry:" >&2
    printf '  %s\n' $missing >&2
    exit 1
fi

# The same fetcher on both sides (path:), so nothing but the edit differs.
closure() {
    local drvs
    drvs=$(nix eval --raw "path:$tree#packages.x86_64-linux" --apply \
        'p: toString (map (d: d.drvPath) [ p.veles-apk p.veles-apk-x86_64 p.veles-aab p.veles-aab-x86_64 ])')
    # shellcheck disable=SC2086
    nix-store -qR $drvs | grep '\.drv$' | sort
}

closure >"$work/before"
printf '\n# android-rebuild-scope\n' >>"$tree/veles-desktop/lib/cmdtree.tcl"
printf '\n# android-rebuild-scope\n' >>"$tree/android/desktop.tcl"
printf '\n# android-rebuild-scope\n' >>"$tree/crates/veles-core/locales/de.yml"
printf '\nandroid-rebuild-scope\n' >>"$tree/android/README.md"
closure >"$work/after"

# A derivation whose inputs changed has a new .drv; name each one.
changed=$(comm -13 "$work/before" "$work/after" \
    | sed -E 's|^/nix/store/[a-z0-9]{32}-||; s|\.drv$||' | sort -u)
allowed='^veles-(android-payload|apk|aab)-[0-9.]+$'
unexpected=$(printf '%s\n' "$changed" | grep -vE "$allowed" | grep . || true)

if [ -z "$changed" ]; then
    echo "the edit changed no derivation: the desktop is not reaching the APK" >&2
    exit 1
fi
if [ -n "$unexpected" ]; then
    echo "a desktop, catalog or README edit rebuilds more than the APK's assembly:" >&2
    printf '  %s\n' $unexpected >&2
    exit 1
fi
echo "a desktop edit rebuilds only:" $changed
