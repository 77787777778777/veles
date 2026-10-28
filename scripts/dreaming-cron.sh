#!/usr/bin/env bash
# S2 dreaming — schedule night reflection.
#
# Two REAL scheduling paths (veles cron jobs run agent PROMPTS, not
# scripts, so this file is the no-agent path):
#
#   1. veles cron (agent turn, needs a gateway for the scheduler):
#        veles cron add "cron:30 3 * * *" \
#          "run veles dream and summarize what was learned"
#
#   2. system crontab (no agent, this script — silent unless there are
#      durable lessons; watchdog pattern: silent = nothing worth pinging):
#        30 3 * * * /path/to/veles-agent/scripts/dreaming-cron.sh
set -euo pipefail
# BIN: override with VELES_BINARY or resolve the installed binary.
BIN="${VELES_BINARY:-$(command -v veles)}"
OUT="$("$BIN" dream 2>/dev/null)"
# Watchdog semantics: print ONLY when there's a real lessons file entry
# (the "No new durable lessons" marker stays silent).
if ! printf '%s' "$OUT" | grep -qi "no new durable lessons"; then
  printf 'veles dreaming found durable lessons:\n%b\n' "$OUT"
fi