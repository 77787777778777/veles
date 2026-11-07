#!/usr/bin/env bash
# scripts/win/browser-bridge-e2e.sh — the browser tab bridge end to end on
# the Windows build host, from Linux (BX-9).
#
#   scripts/win/browser-bridge-e2e.sh <host> [firefox|edge|chrome ...]
#   BX_SKIP_BUILD=1 scripts/win/browser-bridge-e2e.sh <host>   # reuse the last build
#   BX_SKIP_BUILD=1 BX_VELES='C:\path\bin\veles.exe' scripts/win/browser-bridge-e2e.sh <host>
#       # another veles.exe on the host, e.g. one from an unpacked release zip
#
# Builds THIS tree there first (scripts/win/check.sh <host> build: the tree
# is copied to C:\veles\src and built incrementally), then runs
# scripts/win/browser-bridge-e2e.ps1 there over ssh: headless browsers, a
# scratch VELES_HOME, and the install under a test native-host name that the
# run uninstalls and checks is gone from HKCU. BX_IDLE_SECS passes through;
# BX_VELES (a plain path: letters, digits, space, `_`, `.`, `-`) becomes the
# .ps1's -Veles.
# The ssh session is non-interactive, so nothing appears on the host's
# desktop; the installed veles there is not touched.
set -euo pipefail
HOST=${1:-${VELES_WIN_HOST:-}}
[ -n "$HOST" ] || { echo "usage: scripts/win/browser-bridge-e2e.sh <ssh host> [browser ...] (or set VELES_WIN_HOST)" >&2; exit 2; }
shift || true
HERE=$(cd "$(dirname "$0")/../.." && pwd)
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=30 "$HOST")
"${SSH[@]}" "cmd /c echo up" >/dev/null 2>&1 || { echo "browser bridge e2e: $HOST does not answer — skipped"; exit 0; }
if [ "${BX_SKIP_BUILD:-0}" != 1 ]; then
    out=$(bash "$HERE/scripts/win/check.sh" "$HOST" build 2>&1) || { printf '%s\n' "$out" | tail -40; echo "browser bridge e2e: the Windows build failed" >&2; exit 1; }
    printf '%s\n' "$out" | grep -E '^(RC=|windows check: rc=)' || true
fi
# Browser names are fixed words; anything else is refused before it
# reaches a remote command line.
for b in "$@"; do
    case "$b" in firefox|edge|chrome) ;; *) echo "unknown browser $b" >&2; exit 2 ;; esac
done
idle=${BX_IDLE_SECS:-35}
case "$idle" in *[!0-9]*|'') echo "BX_IDLE_SECS must be a number" >&2; exit 2 ;; esac
veles_arg=
if [ -n "${BX_VELES:-}" ]; then
    plain='^[A-Za-z]:\\[A-Za-z0-9_. \\-]+\.exe$'
    [[ $BX_VELES =~ $plain ]] || { echo "BX_VELES must be a plain C:\\...\\veles.exe path" >&2; exit 2; }
    veles_arg="-Veles \"$BX_VELES\""
fi
set +e
"${SSH[@]}" "cmd /c \"set BX_IDLE_SECS=$idle&& pwsh -NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\\veles\\src\\scripts\\win\\browser-bridge-e2e.ps1 $veles_arg $*\"" 2>/dev/null | tr -d '\r'
rc=${PIPESTATUS[0]}
exit "$rc"
