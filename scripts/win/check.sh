#!/usr/bin/env bash
# scripts/win/check.sh — the Windows gate, run from Linux against the
# Windows build host (PLAN-windows.md §7).
#
#   scripts/win/check.sh <host> [step]      step: check | clippy | build | test | all
#   VELES_WIN_FRESH=1 scripts/win/check.sh <host>  # wipe the remote tree first
#
# Snapshots the WORKING TREE (tracked + untracked-not-ignored files) with
# tar, copies it to <host>:C:\veles\src, runs scripts/win/check.bat
# there over ssh, fetches C:\veles\check.log and prints each step's rc
# line and the errors. The remote target/ dir is kept between runs, so
# a second run is incremental.
#
# The host is ephemeral by design (a workstation that is "not on very
# often"): a host that does not answer is reported, not a failure of
# the tree. precommit.sh runs this only when VELES_WIN_HOST is set.
#
# Long test runs go through a scheduled task on the host (OpenSSH kills
# the process tree when the session ends); the check/clippy/build steps
# are short enough for a foreground ssh with keepalives.
set -u
HOST="${1:-${VELES_WIN_HOST:-}}"
[ -n "$HOST" ] || { echo "usage: scripts/win/check.sh <ssh host> [step]  (or set VELES_WIN_HOST)" >&2; exit 2; }
STEP="${2:-check}"
HERE="$(cd "$(dirname "$0")/../.." && pwd)"
SSH="ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=30 $HOST"
# scp's SFTP mode has no transfer idle timeout. A reachable Windows
# host with a stalled SFTP server used to leave an scp (and the whole
# precommit gate) waiting forever after the remote tests had finished.
SCP="scp -q -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=2"
export SCP

if ! $SSH "cmd /c echo up" >/dev/null 2>&1; then
    echo "windows check: $HOST does not answer — skipped"
    exit 0
fi

tgz="$(mktemp -t veles-win-XXXXXX.tgz)"
trap 'rm -f "$tgz"' EXIT
( cd "$HERE" && git ls-files -z --cached --others --exclude-standard \
    | tar --null -T - -czf "$tgz" ) || { echo "windows check: snapshot failed"; exit 1; }
echo "windows check: $(du -h "$tgz" | cut -f1) → $HOST (step: $STEP)"

$SCP "$tgz" "$HOST:C:/veles/src.tgz" || { echo "windows check: copy failed"; exit 1; }
$SCP "$HERE/scripts/win/check.bat" "$HOST:C:/veles/check.bat" || exit 1
if [ "${VELES_WIN_FRESH:-0}" = "1" ]; then
    $SSH 'cmd /c "if exist C:\veles\src rmdir /s /q C:\veles\src"'
fi
# No `if … & …` here: cmd makes everything after an `if` its body.
$SSH 'cmd /c "mkdir C:\veles\src 2>nul & tar -xzf C:\veles\src.tgz -C C:\veles\src && echo unpacked"' \
    | grep -q unpacked || { echo "windows check: unpack failed"; exit 1; }

# The bat writes C:\veles\check.log; ssh's own stdout carries only the
# step banners. Its exit status is the worst step's rc (`all` runs
# clippy, build and test and a red clippy is not hidden by green tests).
# Quoted as one cmd argument: unquoted, Windows sshd's own wrapper left
# a stray `"` on the step word and the batch file choked on it.
$SSH "cmd /c \"C:\\veles\\check.bat $STEP\"" 2>/dev/null | tr -d '\r'
rc=${PIPESTATUS[0]}

log="$(mktemp -t veles-win-log-XXXXXX)"
$SCP "$HOST:C:/veles/check.log" "$log" 2>/dev/null
echo "── windows check: errors ──"
tr -d '\r' < "$log" | grep -E "^(error|warning: unused|warning: .*never used)" | sort | uniq -c | sort -rn | head -40
tr -d '\r' < "$log" | grep -E "^error" -A 3 | grep -E "^\s+--> " | sed 's/^\s*--> //' | sort | uniq -c | sort -rn | head -40
echo "── windows check: steps ──"
tr -d '\r' < "$log" | grep -E "^(STEP|RC)="
rm -f "$log"
echo "windows check: rc=$rc"
exit "$rc"
