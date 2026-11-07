#!/usr/bin/env bash
# scripts/i18n-translate-keys.sh — translate NEW desktop keys into every
# locale with veles children, on a pool of cheap models.
#
#   bash scripts/i18n-translate-keys.sh <keys-file> <topic> <context-file> [locale …]
#
# For each locale (default: every catalog but en) a `veles chat -q` child
# reads the keys, the context (what the strings are and where they show:
# machine translation misreads a short noun phrase as a verb unless told)
# and the locale's existing catalog (for its established terms), and
# writes ~/i18n-work/<date>/<loc>-<topic>.tsv: one `key<TAB>translation`
# row per key. Each file must pass two gates before it counts:
#   scripts/i18n-verify.sh   — every key once, placeholders kept, no empty
#   scripts/i18n-script-check.tcl — written in the locale's own script
#                               (a batch has come back in the wrong one)
# Then, by hand: i18n-tsv2msg.sh per locale, i18n-sync.sh, the suites.
#
# THE MODEL (USER 2026-11-04: "use deepseek flash 4.1 1017 for
# translations not k3.1-pro"). Every lane runs the ali personal plan's
# deepseek-v4.1-flash-1017 by default. The pool used to mix in
# qwen3.8-flash and kimi-for-coding-highspeed, so a round reported as
# "DeepSeek" really came from three models (the 2026-11-04 grant keys:
# 13 deepseek, 12 qwen, 21 kimi locale jobs), and one round was even
# rerouted to k3.1-pro by hand. Other lanes are an explicit choice:
#   I18N_POOL="ali-token-plan-personal deepseek-v4.1-flash-1017;kimi-coding-plan kimi-for-coding-highspeed"
# (semicolon-separated "<provider> <model>" lanes). I18N_LANES sets how
# many children run at once (default 2: three already brushed the
# plan's per-minute token allocation in the docs round). Locales are
# dealt round-robin to the lanes; a failed attempt moves to the next
# lane, after a quadratic backoff with jitter when it was a 429 (the
# per-minute allocation, not the monthly quota). Each OK line names the
# model that produced it; report THOSE, not the default.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEFAULT_LANE="ali-token-plan-personal deepseek-v4.1-flash-1017"
POOL=()
if [ -n "${I18N_POOL:-}" ]; then
    IFS=';' read -r -a _lanes <<<"$I18N_POOL"
    for _lane in "${_lanes[@]}"; do
        _lane="$(echo "$_lane" | xargs)"
        [ -n "$_lane" ] && POOL+=("$_lane")
    done
else
    for ((_i = 0; _i < ${I18N_LANES:-2}; _i++)); do POOL+=("$DEFAULT_LANE"); done
fi
if [ "${#POOL[@]}" -eq 0 ]; then
    echo "i18n-translate-keys: empty model pool (I18N_POOL/I18N_LANES)" >&2
    exit 2
fi
# I18N_POOL_ONLY=1 prints the lanes and exits, before any argument or
# work directory is touched (i18ntest.tcl pins the default this way).
if [ "${I18N_POOL_ONLY:-0}" = 1 ]; then
    printf '%s\n' "${POOL[@]}"
    exit 0
fi
KEYS="${1:?usage: i18n-translate-keys.sh <keys-file> <topic> <context-file> [locale …]}"
TOPIC="${2:?topic}"
CONTEXT_FILE="${3:?context file}"
shift 3
KEYS="$(realpath "$KEYS")"
CONTEXT="$(cat "$CONTEXT_FILE")"
OUT="${I18N_WORK:-$HOME/i18n-work}/$(date +%F)"
LOGS="$OUT/logs"
mkdir -p "$OUT" "$LOGS"

MAXTRY="${MAXTRY:-4}"
GAP="${GAP:-4}"

lang_name() {
  case "$1" in
    af) echo Afrikaans;; am) echo Amharic;; as) echo Assamese;; bg) echo Bulgarian;;
    bn) echo Bengali;; cs) echo Czech;; de) echo German;; fil) echo Filipino;;
    gu) echo Gujarati;; ha) echo Hausa;; hi) echo Hindi;; hr) echo Croatian;;
    id) echo Indonesian;; ig) echo Igbo;; ja) echo Japanese;; kn) echo Kannada;;
    mg) echo Malagasy;; ml) echo Malayalam;; mr) echo Marathi;; ms) echo Malay;;
    ne) echo Nepali;; or) echo Odia;; pa) echo "Punjabi (Gurmukhi script)";; pcm) echo "Nigerian Pidgin";;
    pl) echo Polish;; ru) echo Russian;; rw) echo Kinyarwanda;; si) echo Sinhala;;
    sk) echo Slovak;; sn) echo Shona;; so) echo Somali;;
    sr_cyrl) echo "Serbian (Cyrillic script)";; sr_latn) echo "Serbian (Latin script)";;
    sw) echo Swahili;; ta) echo Tamil;; te) echo Telugu;; th) echo Thai;;
    tl) echo Tagalog;; uk) echo Ukrainian;; vi) echo Vietnamese;; wo) echo Wolof;;
    xh) echo Xhosa;; yo) echo Yoruba;; zh_hans) echo "Chinese (Simplified)";;
    zh_hant) echo "Chinese (Traditional)";; zu) echo Zulu;; *) echo "$1";;
  esac
}

task_for() {
  local loc="$1" dest="$2" lang
  lang="$(lang_name "$loc")"
  cat <<EOF
You are translating user-interface strings of the veles desktop app (a Tk app for a personal AI agent) into $lang (locale code $loc).

THE KEYS: $KEYS — one English string per line. Read it first (read_file).

WHAT THEY ARE (read this before translating; a short label translated without it comes out wrong):
$CONTEXT

THE EXISTING CATALOG for $lang: $ROOT/veles-desktop/msgs/$loc.msg (rows look like \`::msgcat::mcset $loc {English} {translation}\`). Read its first ~150 lines and reuse its established words for recurring veles terms (agent, approval, plan, note, answer, choice, …) so the new strings match the app.

Read each requested input at most once. Do not use search_files, session_search, terminal or delegate_task, and do not inspect any other files. Translate from those two inputs, then write the complete file in one call. Do not wait for another agent or ask for confirmation.

WRITE the file $dest with write_file: exactly one line per key, in the keys file's order, each line \`<the English key, byte for byte><TAB><the $lang translation>\`. Nothing else in the file: no header, no quotes, no markdown, no blank lines.

RULES: keep every %s / %d / %1\$s placeholder exactly as in the key; translate naturally, as a native UI label in $lang, in its own script; keep a trailing colon when $lang puts one after a field label; never leave the English.

Then reply with one line: DONE $loc
EOF
}

gates_pass() {
  local loc="$1" f="$2"
  [ -s "$f" ] || return 1
  bash "$ROOT/scripts/i18n-verify.sh" "$f" "$KEYS" >/dev/null 2>&1 || return 1
  tclsh "$ROOT/scripts/i18n-script-check.tcl" "$loc" "$f" >/dev/null 2>&1 || return 1
}

translate_one() {
  local loc="$1" lane="$2"
  local dest="$OUT/$loc-$TOPIC.tsv" try=0 log idx provider model backoff
  if gates_pass "$loc" "$dest"; then echo "  skip $loc (present, gates ok)"; return 0; fi
  while [ "$try" -lt "$MAXTRY" ]; do
    idx=$(( (lane + try) % ${#POOL[@]} ))
    read -r provider model <<<"${POOL[$idx]}"
    try=$((try + 1))
    log="$LOGS/$loc-$TOPIC.$try.log"
    command rm -f "$dest"
    timeout --signal=TERM --kill-after=10s "${TRANSLATE_TIMEOUT:-600}" \
        veles chat -q "$(task_for "$loc" "$dest")" -p "$provider" -m "$model" -r low -Q \
        </dev/null >"$log" 2>&1
    if gates_pass "$loc" "$dest"; then
        echo "  OK $loc ($model, try $try)"
        return 0
    fi
    if command grep -qE 'insufficient_quota|rate limited|\b429\b|Throttling' "$log"; then
        backoff=$(( 45 * try * try + RANDOM % 30 ))
        echo "  429 $loc on $model — backing off ${backoff}s, then the next model"
        sleep "$backoff"
    else
        echo "  FAIL $loc on $model (try $try): $(bash "$ROOT/scripts/i18n-verify.sh" "$dest" "$KEYS" 2>&1 | tail -1; tclsh "$ROOT/scripts/i18n-script-check.tcl" "$loc" "$dest" 2>&1 | tail -1)"
    fi
  done
  echo "  GAVE UP $loc"
  return 1
}

if [ "$#" -gt 0 ]; then
    LOCALES=("$@")
else
    LOCALES=()
    for f in "$ROOT"/veles-desktop/msgs/*.msg; do
        loc="$(basename "$f" .msg)"
        [ "$loc" = en ] || LOCALES+=("$loc")
    done
fi

echo "translating $(wc -l <"$KEYS") key(s) into ${#LOCALES[@]} locale(s) → $OUT"
LANES=${#POOL[@]}
for ((lane = 0; lane < LANES; lane++)); do
    (
        i=0
        for loc in "${LOCALES[@]}"; do
            if (( i % LANES == lane )); then
                translate_one "$loc" "$lane"
                sleep "$GAP"
            fi
            i=$((i + 1))
        done
    ) &
    sleep 2
done
# Bash's bare wait only propagates the last lane's status. A failed
# earlier lane must not be reported as a successful batch.
lane_fail=0
for _ in "${POOL[@]}"; do
    wait -n || lane_fail=1
done
fail=$lane_fail
for loc in "${LOCALES[@]}"; do
    gates_pass "$loc" "$OUT/$loc-$TOPIC.tsv" || { echo "MISSING $loc"; fail=1; }
done
echo "done: ${#LOCALES[@]} locale(s), $(if [ "$fail" = 0 ]; then echo all passed; else echo some missing; fi) — $OUT"
exit "$fail"
