#!/usr/bin/env bash
# i18n-part07-run.sh — translate the 7 symbolic keys the extractor missed.
#
# WHY THESE 7. They render /tasks, /subagents, /shells and /monitors.
# Until 2026-09-21 the extractor's Rust walk followed `lookup(` call sites
# only, so keys that arrive through a match arm
# (`("tasks_header", "tasks_none")` in tasks.rs) were never in keys.txt —
# no translator was ever asked for them, and all four commands rendered
# English in all 46 translated locales while the catalogs reported
# complete.
#
# The extractor is fixed (it now also reads en.yml's symbolic keys), so
# these are part of the key set and en.msg carries them — which is what
# made i18ntest's completeness contract go red: en.msg at 976 keys, every
# locale .msg still at 969. This fills the gap.
#
# Routing: personal plan, up to PLAN_SLOTS children in flight. These are
# 7 short strings each, so the whole round is small; overflow to team
# only if PLAN_SLOTS is lowered.
set -uo pipefail
WORK=/home/user/i18n-work
ROOT=/home/user/veles-agent
cd "$WORK" || exit 1

PLAN_SLOTS="${PLAN_SLOTS:-6}"
TEAM_SLOTS="${TEAM_SLOTS:-2}"
MODEL="${MODEL:-deepseek-v4-flash-0731}"

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

write_tasks() {
    # Iterate LOCALES, not existing task files: the first run has no task
    # files yet, so a loop over them produces nothing (the chicken-and-egg
    # this replaced).
    for b in $(ls "$WORK"/?-part-00.tsv "$WORK"/*-part-00.tsv 2>/dev/null \
                   | sed 's|.*/||; s|-part-00.tsv||' | sort -u); do
        [ -n "$b" ] || continue
        [ -s "$WORK/$b-part-07.tsv" ] && continue
        lang=$(lang_name "$b")
        cat > "$WORK/task-$b-07.txt" <<EOF
You are translating SEVEN short UI strings into $lang for "veles", a
personal AI agent (terminal TUI + Tk desktop + Telegram + IRC).

They are the headings and empty-state lines of the agent's work listings —
what you see when you ask it what is currently running.

STEP 1. Read $WORK/keys-part-07-english.tsv. Each row is:
    key<TAB>English text
The key is an identifier, NOT prose. Copy the key EXACTLY as written.

STEP 2. Write $WORK/$b-part-07.tsv with one row per key:
    key<TAB>$lang translation
Translate the English TEXT, never the identifier.

What each English line means (all seven):
  tasks_header       "running tasks:"  — a heading above a short list
  subagents_header   "running subagents:"
  subagents_none     "(no running subagents — /btw <prompt> starts one, or the model uses delegate_task)"
  shells_header      "background shells:"
  shells_none        "(no background shells — /background <cmd> starts one)"
  monitors_header    "monitors:"
  monitors_none      "(no monitors — the model starts these with the monitor tool)"

RULES (a verifier checks the first three):
- All 7 keys appear exactly once. A missing key silently falls back to
  English, which is the bug this round exists to fix.
- Keep these spellings EXACTLY, untranslated, because a user types them or
  the model calls them: /btw, /background, delegate_task, tool.
- Use ONE literal TAB between key and translation, no other tab, one row
  per line, no header, no quotes, no trailing spaces.
- Headings stay SHORT — they sit above a narrow list. The ":" stays.
- Empty-state lines keep their parenthesised, em-dash structure.
- Register: informal and direct, matching the rest of the $lang UI. If you
  translated other parts of this catalog, reuse your terminology for task,
  shell, monitor and subagent so the four listings read consistently.

Write the file with the write_file tool, then report the 7 rows and the
exact output of:
  bash $ROOT/scripts/i18n-verify.sh $WORK/$b-part-07.tsv --part 07
EOF
    done
}

write_tasks
echo "task files: $(ls "$WORK"/task-*-07.txt 2>/dev/null | wc -l)"
echo "parts already done: $(ls "$WORK"/*-part-07.tsv 2>/dev/null | wc -l)"

# Bounded parallelism: never more than PLAN_SLOTS on the personal plan and
# TEAM_SLOTS on team at once. The invocation mirrors the main driver's
# exactly (`i18n-run.sh:144`): the DEFAULT provider is the personal plan,
# so only the team overflow passes `-p`, and children run from
# /home/user so a relative path in a task file resolves as it did there.
MAX=$((PLAN_SLOTS + TEAM_SLOTS))
launch() {
    local loc="$1" route="$2"
    local extra=()
    [ "$route" = "team" ] && extra=(-p custom:ali-token-plan-team)
    (cd /home/user && timeout 2400 veles chat -q "$(cat "$WORK/task-$loc-07.txt")" \
        "${extra[@]}" -m "$MODEL" -r high -Q > "$WORK/log-$loc-07.txt" 2>&1)
}

for loc in $(ls "$WORK"/task-*-07.txt 2>/dev/null | sed 's|.*/task-||; s|-07.txt||' | sort); do
    [ -s "$WORK/$loc-part-07.tsv" ] && continue
    # Personal until its slots are busy, then team.
    if [ "$(jobs -rp | wc -l)" -lt "$PLAN_SLOTS" ]; then
        route="plan"
    else
        route="team"
    fi
    while [ "$(jobs -rp | wc -l)" -ge "$MAX" ]; do wait -n; done
    launch "$loc" "$route" &
done
wait

ok=0; bad=0
for f in "$WORK"/*-part-07.tsv; do
    [ -e "$f" ] || continue
    ok=$((ok + 1))
done
echo
echo "part-07 files written: $ok / 46"
echo "run: for each, bash $ROOT/scripts/i18n-build.sh <loc> && i18n-verify.sh"