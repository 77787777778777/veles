#!/usr/bin/env bash
# i18n-part10-run.sh — translate the 88 setup-wizard strings into all 46
# locales (PLAN-setup-wizard-tk.md, work item 4).
#
# WHY THESE 88. The Tk first-run wizard (veles-desktop/lib/setup.tcl) and
# its three desktop-API endpoints added ~88 new desktop strings: screen
# titles ("Setup — welcome", …), field labels, help blurbs, the ACL
# curated-mode warning, the finish report, and the buttons. They ride the
# DESKTOP catalog pipeline (msgs/*.msg), not the engine's (locales/*.yml).
# i18n-sync.sh put them in en.msg as identity rows; every other locale is
# now short them, so the completeness contract (i18ntest.tcl) is red until
# this round fills the gap.
#
# These are all English-as-key (no symbolic ids), so keys-part-10-english.tsv
# is `key<TAB>key` and the translator's job is to translate the TEXT.
#
# ROUTING (USER steer 2026-09-26): z.ai FIRST, then ali personal, then ali
# team — in that order of preference. z.ai is free extra quota (coding plan,
# expires 2026-10-01) so it takes the bulk; personal/team are overflow.
#
# NOT GETTING KICKED (the other half of the steer): total in-flight is hard-
# capped at MAX, and the tier is chosen by how many are ALREADY running, so
# the load SPREADS across all three providers (≈ZAI_SLOTS on z.ai, then
# personal, then team) instead of hammering one provider's requests-per-
# minute. Launches are staggered by LAUNCH_GAP so the first MAX don't hit a
# provider in the same instant. Each child is ONE short request (88 strings)
# that runs tens of seconds, so sustained RPM per provider stays low.
#
# Usage:  bash scripts/i18n-part10-run.sh
# Then:   for each locale, bash scripts/i18n-build.sh <loc>   (writes the .msg)
# And:    bash scripts/i18n-catalog-check.tcl   +   i18n-sync.sh --check
set -uo pipefail
WORK=/home/user/i18n-work
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$WORK" || exit 1

# Concurrency per tier. z.ai preferred (free, own 5h rolling quota), ali
# personal next (also runs nothing else right now — the brain is on team),
# ali team as durable overflow. Kept modest on purpose: the cap is about
# not tripping any one provider's RPM/concurrency limit, not about speed.
ZAI_SLOTS="${ZAI_SLOTS:-4}"
PLAN_SLOTS="${PLAN_SLOTS:-3}"
TEAM_SLOTS="${TEAM_SLOTS:-2}"
MAX=$((ZAI_SLOTS + PLAN_SLOTS + TEAM_SLOTS))
LAUNCH_GAP="${LAUNCH_GAP:-1}"        # seconds between launches (anti-burst)
# Per-tier model + effort. z.ai has no deepseek, so its tier uses glm; the
# short strings + mechanical verifier make a mid model + medium effort
# plenty, and it conserves z.ai's rolling quota. ali tiers keep the proven
# part-07 recipe (deepseek-v4-flash-0731 at high).
ZAI_MODEL="${ZAI_MODEL:-glm-4.5-air}"
ALI_MODEL="${ALI_MODEL:-deepseek-v4-flash-0731}"
PART=10
NKEYS=$(grep -c . "$WORK/keys-part-$PART.txt" 2>/dev/null || echo 0)

# A human-readable language name per locale id, so a child is told
# "German" and not "de".
lang_name() {
    case "$1" in
        af) echo Afrikaans ;; am) echo Amharic ;; as) echo Assamese ;;
        bg) echo Bulgarian ;; bn) echo Bengali ;; cs) echo Czech ;;
        de) echo German ;; fil) echo Filipino ;; gu) echo Gujarati ;;
        ha) echo Hausa ;; hi) echo Hindi ;; hr) echo Croatian ;;
        id) echo Indonesian ;; ig) echo Igbo ;; ja) echo Japanese ;;
        kn) echo Kannada ;; mg) echo Malagasy ;; ml) echo Malayalam ;;
        mr) echo Marathi ;; ms) echo Malay ;; ne) echo Nepali ;;
        or) echo Odia ;; pa) echo Punjabi ;; pcm) echo "Nigerian Pidgin" ;;
        pl) echo Polish ;; ru) echo Russian ;; rw) echo Kinyarwanda ;;
        si) echo Sinhala ;; sk) echo Slovak ;; sn) echo Shona ;;
        so) echo Somali ;; sr_cyrl) echo "Serbian written in Cyrillic" ;;
        sr_latn) echo "Serbian written in Latin script" ;; sw) echo Swahili ;;
        ta) echo Tamil ;; te) echo Telugu ;; th) echo Thai ;;
        tl) echo Tagalog ;; uk) echo Ukrainian ;; vi) echo Vietnamese ;;
        wo) echo Wolof ;; xh) echo Xhosa ;; yo) echo Yoruba ;;
        zh_hans) echo "Chinese (Simplified)" ;;
        zh_hant) echo "Chinese (Traditional)" ;;
        zu) echo Zulu ;; *) echo "$1" ;;
    esac
}

# The locale ids to translate: every msgs/*.msg except en. Derived from
# the tree, not hand-listed, so a new locale joins automatically.
locales() {
    for f in "$ROOT"/veles-desktop/msgs/*.msg; do
        b=$(basename "$f" .msg); [ "$b" = "en" ] && continue; echo "$b"
    done | sort
}

write_task() {
    local loc="$1" lang
    lang=$(lang_name "$loc")
    cat > "$WORK/task-$loc-$PART.txt" <<EOF
You are translating $NKEYS short UI strings into $lang for "veles", a
personal AI agent (terminal TUI + Tk desktop + Telegram + IRC).

They are the first-run SETUP WIZARD: a Tk window that walks a new operator
through picking an LLM provider, a default model, optional Telegram/IRC,
and an access-control posture, then previews and writes config.toml + .env.
So the strings are window titles, step headings, field labels, short help
blurbs, warnings and buttons.

STEP 1. Read $WORK/keys-part-$PART-english.tsv. Each row is:
    key<TAB>English text
Here the key IS the English text (English-as-key catalog), so both columns
are identical. Copy the key column EXACTLY as written into column 1 of your
output — it is the lookup key, not prose to translate.

STEP 2. Write $WORK/$loc-part-$PART.tsv with one row per key:
    key<TAB>$lang translation
Translate the English TEXT into column 2. Never alter column 1.

RULES (i18n-verify.sh checks these mechanically — run it before reporting):
- All $NKEYS keys appear exactly once. A missing key silently falls back to
  English, which is the bug this round exists to fix.
- Keep every format specifier EXACTLY: %s, %d, %1\$s, %2\$s, %1\$d, %2\$d, %%.
  Reordering numbered ones (%2\$s before %1\$s) is fine and often correct;
  DROPPING one is a runtime crash in a widget's -text. A key with two %s
  needs two %s in the translation.
- Keep these verbatim, untranslated, because a user types them or the code
  reads them: config.toml, .env, config.toml.bak, key_env, [acl], [model],
  [[custom_providers]], [platforms.telegram], IRC_ALLOWED_USERS,
  TELEGRAM_ALLOWED_USERS, TELEGRAM_BOT_TOKEN, IRC_CHANNELS, @BotFather,
  chat_completions, responses, api_mode, veles, Telegram, IRC, /model,
  veles gateway restart, telegram:123456, irc:nick!user@host, setup_done,
  alibaba/token-plan model ids. A command name, config key, env var or file
  path that gets translated stops working.
- Use ONE literal TAB between key and translation. No other tab anywhere (a
  second tab corrupts the row). One row per line, no header, no surrounding
  quotes, no trailing spaces.
- Preserve leading/trailing spaces if a key has them (layout, not prose).
- Do not translate emoji or the structural marks · — ⇄ ↑ ↓ ✕ ▢.
- Register: informal and direct, matching the rest of the $lang veles UI.
  Keep buttons SHORT (Back, Next, Skip, Close, Summary, Cancel) — these sit
  in a narrow nav bar. Sentence case for labels, matching the English.
- "Step %1\$d of %2\$d" is a progress line: translate the word "Step", keep
  both numbered placeholders in order.

Some strings are full sentences of help text (the ACL curated-mode warning,
the IRC channels note). Translate those as real prose a native operator
would read — accurate, not word-for-word — but still keep the config/env
tokens above verbatim inside them.

Write the file with the write_file tool, then report the row count and the
exact output of:
  bash $ROOT/scripts/i18n-verify.sh $WORK/$loc-part-$PART.tsv --part $PART
EOF
}

echo "part-$PART: $NKEYS keys, locales: $(locales | wc -l)"
# PART10_TASKS_ONLY: write the task files and stop (test/dry-run mode —
# lets a caller inspect the prompt without launching 46 children).
if [ "${PART10_TASKS_ONLY:-0}" = "1" ]; then
    for loc in $(locales); do
        [ -s "$WORK/$loc-part-$PART.tsv" ] && continue
        write_task "$loc"
    done
    echo "task files (dry run): $(ls "$WORK"/task-*-$PART.txt 2>/dev/null | wc -l)"
    exit 0
fi
for loc in $(locales); do
    # Skip a locale already done (idempotent re-run).
    [ -s "$WORK/$loc-part-$PART.tsv" ] && continue
    write_task "$loc"
done
echo "task files: $(ls "$WORK"/task-*-$PART.txt 2>/dev/null | wc -l)"

# ── dispatch: z.ai → ali personal → ali team, bounded + staggered ────
#
# Per-tier PID lists, pruned with `kill -0`. The tier for each launch is
# the FIRST with a free slot in preference order (zai, plan, team); if all
# three are full we wait for any child to finish and re-decide. That both
# honours the routing preference AND keeps each provider under its slot
# cap, so no single provider's RPM/concurrency limit is tripped.
declare -a ZAI_PIDS=() PLAN_PIDS=() TEAM_PIDS=()

# Drop finished PIDs from a named array. Usage: reap ARRAYNAME
reap() {
    local -n arr="$1"
    local live=()
    local pid
    for pid in "${arr[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then live+=("$pid"); fi
    done
    arr=("${live[@]}")
}

reap_all() { reap ZAI_PIDS; reap PLAN_PIDS; reap TEAM_PIDS; }

# Launch one locale on a named tier. The tier decides provider + model.
#
# PROVIDER NAMES ARE BARE (`zai-coding-plan`, not `custom:zai-coding-plan`):
# `Agent::apply_overrides` prepends `custom:` itself, so `-p custom:X`
# resolves to `custom:custom:X` and fails with "unknown provider" — the
# override silently drops and the child runs on the config default
# (ali-token-plan-team), which for a z.ai/glm model is a 404
# "Model not exist". The part-07 script hid this because its `-p` was only
# ever the team name and its model existed on the default route.
#
# LAUNCH_CMD is an override for testing the dispatch logic without
# spending real children: when set, it is eval'd with $loc and $route
# exported, so the REAL pick_tier/reap/dispatch code runs against a stub.
launch() {
    local loc="$1" route="$2"
    if [ -n "${LAUNCH_CMD:-}" ]; then
        loc="$loc" route="$route" bash -c "$LAUNCH_CMD"
        return
    fi
    # Tier → (bare provider name, model). Preference order (zai first,
    # then personal, then team) is enforced by pick_tier, not here.
    local prov model
    case "$route" in
        zai)  prov="zai-coding-plan";        model="$ZAI_MODEL" ;;
        plan) prov="ali-token-plan-personal"; model="$ALI_MODEL" ;;
        team) prov="ali-token-plan-team";     model="$ALI_MODEL" ;;
        *)    prov="ali-token-plan-team";     model="$ALI_MODEL" ;;
    esac
    (cd /home/user && timeout 2400 veles chat -q "$(cat "$WORK/task-$loc-$PART.txt")" \
        -p "$prov" -m "$model" -r medium -Q > "$WORK/log-$loc-$PART.txt" 2>&1)
}

# Pick the first tier with a free slot, in preference order. "" when all
# three are full (caller waits and retries).
pick_tier() {
    reap_all
    if [ "${#ZAI_PIDS[@]}"  -lt "$ZAI_SLOTS" ];  then echo zai;  return; fi
    if [ "${#PLAN_PIDS[@]}" -lt "$PLAN_SLOTS" ]; then echo plan; return; fi
    if [ "${#TEAM_PIDS[@]}" -lt "$TEAM_SLOTS" ]; then echo team; return; fi
    echo ""
}

dispatch() {
    local loc
    for loc in $(locales); do
        [ -s "$WORK/$loc-part-$PART.tsv" ] && continue   # idempotent re-run
        local tier
        # Wait until some tier has room. `wait -n` returns as soon as ANY
        # background child finishes; reap then frees its slot.
        while :; do
            tier=$(pick_tier)
            [ -n "$tier" ] && break
            wait -n 2>/dev/null || sleep 1
        done
        launch "$loc" "$tier" &
        local pid=$!
        case "$tier" in
            zai)  ZAI_PIDS+=("$pid") ;;
            plan) PLAN_PIDS+=("$pid") ;;
            team) TEAM_PIDS+=("$pid") ;;
        esac
        # Stagger launches so a provider isn't hit by several in the same
        # instant — the burst is what trips an RPM limit, not the total.
        sleep "$LAUNCH_GAP"
    done
    wait
}

dispatch

# ── retry pass: anything a rate-limit or timeout left empty ──────────
#
# A kicked child writes no (or a partial) TSV. Re-run just those, slower
# (wider gap, and they spread across tiers again because the first pass's
# children are done). Bounded to RETRY_PASSES so a genuinely broken locale
# cannot loop forever.
RETRY_PASSES="${RETRY_PASSES:-2}"
pass=0
while [ "$pass" -lt "$RETRY_PASSES" ]; do
    missing=0
    for loc in $(locales); do
        # A file with fewer rows than the key count is partial (a child cut
        # off mid-write) — treat it as missing so it is redone.
        rows=$(grep -c . "$WORK/$loc-part-$PART.tsv" 2>/dev/null || echo 0)
        [ "$rows" -ge "$NKEYS" ] && continue
        missing=$((missing + 1))
    done
    [ "$missing" -eq 0 ] && break
    pass=$((pass + 1))
    echo "retry pass $pass: $missing locale(s) short or empty — redoing"
    LAUNCH_GAP=$((LAUNCH_GAP + 1))
    # Re-dispatch only handles locales whose file is short/absent (the
    # guard inside dispatch skips complete ones), so a bare call redoes
    # exactly the missing set.
    dispatch
done

ok=0
for loc in $(locales); do
    rows=$(grep -c . "$WORK/$loc-part-$PART.tsv" 2>/dev/null || echo 0)
    [ "$rows" -ge "$NKEYS" ] && ok=$((ok + 1))
done
echo
echo "part-$PART files written: $ok / $(locales | wc -l)"
echo "next: for each locale, bash $ROOT/scripts/i18n-build.sh <loc>"
echo "      then bash $ROOT/scripts/i18n-catalog-check.tcl $ROOT"
