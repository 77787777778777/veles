#!/usr/bin/env bash
# LSP end to end: a real gateway + the real tcl-lsp binary + the real Tk
# desktop under Xvfb, driving hover, go-to-definition, tab switching and
# the Shift-hold tooltip. A DEVELOPMENT harness like desktop_e2e.sh — it
# needs a localhost socket and a real X server and is NOT part of
# `nix flake check` (the gate is scripts/tcl_tests.sh + cargo test).
#
# Why it exists: the Tk suites stub ::vlsp (devtest) or the event stream
# (lspstarttest), so none of them drives a real handshake — and the LSP
# bugs that hurt most (a killed session after a tab switch, a stub the
# index never sees, a root switch that races itself) lived exactly there.
#
#   bash scripts/devharness/lsp_e2e.sh
#
# Prints a pass/fail line per check, then `LSP_E2E_DONE` and `BGERRORS: 0`.
set -uo pipefail
cd "$(dirname "$0")/../.."
REPO="$(pwd)"
S="$REPO/scripts/devharness"
H=/dev/shm/veles-lsp-e2e
PORT=18646
GWLOG=/dev/shm/lsp_e2e_gw.log

command -v tcl-lsp >/dev/null || { echo "FAIL: tcl-lsp not on PATH"; exit 1; }
[ -x "$REPO/target/debug/veles" ] || cargo build -p veles-cli || exit 1

# ── scratch home: a project (its own git root) + a plugins tree ──
rm -rf "$H"
mkdir -p "$H/plugins" "$H/proj"
cp -r "$REPO/plugins/reasoning-summary" "$H/plugins/"
git init -q "$H/proj"
cat > "$H/proj/lib.tcl" <<'TCL'
namespace eval ::demo {}
# Double a number — the canonical example proc, so hover has prose to show.
proc ::demo::twice {n} { return [expr {$n * 2}] }
TCL
cat > "$H/proj/app.tcl" <<'TCL'
source [file join [file dirname [info script]] lib.tcl]
# A call site for go-to-definition to land on.
puts [::demo::twice 21]
TCL

# ── gateway (outside the dev shell — the binary needs nothing from it) ──
# The desktop API authenticates every route but /api/health and mints a
# token into $H/.env when it finds none. Seed one instead of scraping the
# minted file: deterministic, and no race with the gateway writing it.
export VELES_TOKEN=devharness-lsp-token
printf 'DESKTOP_BEARER_TOKEN=%s\n' "$VELES_TOKEN" > "$H/.env"
chmod 600 "$H/.env"
rm -f "$GWLOG"
VELES_HOME="$H" VELES_DESKTOP_PORT="$PORT" TMPDIR=/dev/shm \
  "$REPO/target/debug/veles" gateway run >"$GWLOG" 2>&1 &
GW=$!
trap 'kill $GW 2>/dev/null' EXIT
for _ in $(seq 1 80); do
  curl -s -m1 "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 && break
  sleep 0.25
done
if ! curl -s -m2 "http://127.0.0.1:$PORT/api/health" | grep -q '"status":"ok"'; then
  echo "FAIL: gateway did not come up"; tail -20 "$GWLOG"; exit 1
fi
echo "gateway up on :$PORT  (VELES_HOME=$H)"

# ── the desktop, inside the dev shell (Xvfb + wish must share it) ──
if command -v direnv >/dev/null && [ -e "$REPO/.envrc" ]; then
  direnv exec "$REPO" bash "$S/lsp_e2e_inner.sh" "$PORT" "$H"
else
  bash "$S/lsp_e2e_inner.sh" "$PORT" "$H"
fi
rc=$?

# The driver forces 9 project-root changes. Each legitimately respawns
# the server once (a new root means a new index). More than ~one per
# switch means a start is thrashing the session — the re-entrant _stop
# bug, or a probe that replaces when it should reuse.
spawns=$(grep -c 'tcl-lsp .* starting' "$GWLOG" 2>/dev/null || echo 0)
echo "=== tcl-lsp spawns in the gateway log: $spawns (expect ~9, one per root switch) ==="
if [ "$spawns" -gt 13 ]; then
  echo "FAIL: $spawns spawns for 9 root switches — the session is thrashing"
  rc=1
fi
exit $rc
