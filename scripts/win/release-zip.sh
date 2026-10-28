#!/usr/bin/env bash
# scripts/win/release-zip.sh — build the portable Windows release zip on
# the Windows build host from THIS tree and fetch it back.
#
#   scripts/win/release-zip.sh <host> [out-dir]
#
# The same command the release workflow's Windows lane runs
# (scripts/install/windows.ps1 -PortableZip), for a release cut or proven
# by hand when no GitHub Windows run is available. The host keeps its own
# source copy (C:\veles\relsrc, with its own target\) so the Windows gate's
# C:\veles\src is never touched; Tcl/Tk and sqlite are the gate's
# C:\veles\tcl and C:\veles\sqlite (built by windows.ps1 on first use).
set -euo pipefail
HOST=${1:-${VELES_WIN_HOST:?usage: release-zip.sh <ssh host> [out dir] (or set VELES_WIN_HOST)}}
OUT=${2:-$PWD}
HERE=$(cd "$(dirname "$0")/../.." && pwd)
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=30 "$HOST")
SCP=(scp -q -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4)
"${SSH[@]}" "cmd /c echo up" >/dev/null 2>&1 || { echo "release-zip: $HOST does not answer" >&2; exit 1; }
rev=$(git -C "$HERE" rev-parse --short=7 HEAD)
if [[ -n $(git -C "$HERE" status --porcelain --untracked-files=no) ]]; then rev=$rev-dirty; fi
tgz=$(mktemp -t veles-relzip-XXXXXX.tgz)
trap 'rm -f "$tgz"' EXIT
(cd "$HERE" && git ls-files -z --cached --others --exclude-standard | tar --null -T - -czf "$tgz")
"${SCP[@]}" "$tgz" "$HOST:C:/veles/relsrc.tgz"
# No `if exist ... & ...`: cmd makes everything after an `if` its body.
"${SSH[@]}" 'cmd /c "rmdir /s /q C:\veles\relsrc\veles-desktop 2>nul & rmdir /s /q C:\veles\relsrc\skills 2>nul & mkdir C:\veles\relsrc 2>nul & tar -xzf C:\veles\relsrc.tgz -C C:\veles\relsrc && echo unpacked"' | grep -q unpacked
"${SSH[@]}" "pwsh -NoProfile -ExecutionPolicy Bypass -File C:\\veles\\relsrc\\scripts\\install\\windows.ps1 -SkipDeps -Prefix C:\\veles -Revision $rev -PortableZip C:\\veles\\relout\\veles-windows.zip" | tr -d '\r'
mkdir -p "$OUT"
"${SCP[@]}" "$HOST:C:/veles/relout/veles-windows.zip" "$OUT/veles-windows.zip.part"
version=$(sed -n 's/^version = "\([0-9.]*\)"/\1/p' "$HERE/Cargo.toml" | head -1)
mv -f "$OUT/veles-windows.zip.part" "$OUT/veles-$version-$rev-windows-x86_64.zip"
echo "release-zip: $OUT/veles-$version-$rev-windows-x86_64.zip"
