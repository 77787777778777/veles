#!/usr/bin/env bash
# R1 check 7 (PLAN-plugins-installer.md): rebuild a module and reload it
# into a RUNNING process, then show the two refusals.
#
#   1. edit the echo module, rebuild it WITH the gateway, reload: same pid,
#      the new build answers, the old copy is unmapped;
#   2. rebuild the module ON ITS OWN: refused (another cargo build);
#   3. edit veles-types, rebuild everything: refused (core changed), and
#      the running process keeps serving the build it has.
#
# It edits two source files and restores them on exit (with a fresh
# mtime, so cargo rebuilds what the edit built). Run it from the dev
# shell, never during a gate or another build.
set -euo pipefail
cd "$(dirname "$0")/.."

ECHO=crates/veles-module-echo/src/lib.rs
TYPES=crates/veles-types/src/lib.rs
work=$(mktemp -d "${TMPDIR:-/dev/shm}/veles-r1c7.XXXXXX")
cp "$ECHO" "$work/echo.rs"
cp "$TYPES" "$work/types.rs"
restore() {
  exec 3>&- 2>/dev/null || true
  cat "$work/echo.rs" >| "$ECHO"
  cat "$work/types.rs" >| "$TYPES"
  [ -n "${harness:-}" ] && kill "$harness" 2>/dev/null || true
  rm -rf "${work:?}"
}
trap restore EXIT

build() { cargo build -q "$@" </dev/null 2>&1 | grep -E '^(error|warning: unused)' || true; }
step() { echo "== $*"; }
# Wait until the harness log has $1 lines starting with "live:".
await() {
  for _ in $(seq 1 600); do
    [ "$(grep -c '^live:' "$work/live.log" || true)" -ge "$1" ] && break
    sleep 0.1
  done
  grep '^live:' "$work/live.log" | tail -1
}

step "baseline: gateway + module in one build"
build -p veles-cli -p veles-module-echo
mkfifo "$work/in"
target/debug/veles module-spike --check live <"$work/in" >"$work/live.log" 2>&1 &
harness=$!
exec 3>"$work/in"
await 1

step "1. edit the module (echo-1 -> echo-2), rebuild with the gateway, reload"
sed -i 's/const SPIKE_VERSION: &str = "echo-1";/const SPIKE_VERSION: \&str = "echo-2";/' "$ECHO"
grep -q '"echo-2"' "$ECHO"
build -p veles-cli -p veles-module-echo
echo >&3
await 2

step "2. rebuild the module on its own, reload"
build -p veles-module-echo
echo >&3
await 3

step "3. edit veles-types, rebuild everything, reload"
build -p veles-cli -p veles-module-echo   # back to the gateway's runtime first
printf '\n// r1 check 7: a core edit\n' >>"$TYPES"
build -p veles-cli -p veles-module-echo
echo >&3
await 4

exec 3>&-
wait "$harness" || true
step "harness pid $harness, full log:"
cat "$work/live.log"
