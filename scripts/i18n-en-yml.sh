#!/usr/bin/env bash
# i18n-en-yml.sh — check the engine's identity catalog (en.yml).
#
# args: [--emit]
#
# WHY THIS EXISTS, SEPARATELY FROM i18n-tsv2yml.sh
#
# `i18n-tsv2yml.sh` builds a TRANSLATED catalog and deliberately DROPS
# any row whose value equals its key ("an English-as-key row whose value
# IS the key is not a translation"). That rule is right for de.yml and
# wrong for en.yml: feeding it `<key>\t<key>` rows writes an EMPTY file.
#
# WHAT en.yml IS
#
# The identity catalog holds the SYMBOLIC keys — identifiers like
# `agents_none` or `subagents_header` whose key is not English prose. A
# missing entry for one of those renders the IDENTIFIER in the UI,
# because fallback-to-key has nothing better to offer. For the
# running-work family (tasks.rs) this file is the only copy of their
# English text, so it is hand-maintained prose, not a generated artifact.
#
# English-as-key rows need no entry: the fallback already IS the English
# text, and duplicating ~950 of them would bury the symbolic half.
#
# THE CHECK, AND WHY IT IS SHAPED LIKE THIS
#
# A symbolic key reaches `lookup()` through a VARIABLE
# (`lookup(none, &[])`, where `none` came from a match arm), so no
# source scan can enumerate them reliably — a grep for bare snake_case
# strings in the tree catches tool names (`web_search`) and misses the
# real ones. So the invariant checked here is internal to the file:
# every entry is well-formed, and the file's SHAPE matches what
# `tsv2yml`/`i18n.rs` can read. The cross-surface key-set invariants are
# the Rust test `catalog_keys_and_code_do_not_drift`, which sees the
# whole tree; this script covers the file's own integrity.
#
# Usage:
#   bash scripts/i18n-en-yml.sh            # check, exit 1 on a problem
#   bash scripts/i18n-en-yml.sh --emit     # print the key list
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/crates/veles-core/locales/en.yml"
EMIT=0
[ "${1:-}" = "--emit" ] && EMIT=1

if [ ! -s "$OUT" ]; then
    echo "en.yml is missing or empty" >&2
    exit 1
fi

rc=0

# 1. Header. Every catalog carries the format marker `_version: 1` (flat
#    keys — the format rust-i18n defined, which i18n.rs reads); a file
#    without it is not one the generators wrote.
if ! head -20 "$OUT" | grep -q '^_version: 1$'; then
    echo "  en.yml has no _version: 1 header" >&2
    rc=1
fi

# 2. Every line is one of: comment, blank, `_version`, or
#    `snake_case_key: "text"`. A malformed line is a key that will never
#    resolve, which is the silent-failure case this file cannot afford.
BAD="$(grep -nvE '^(#.*|_version: 1|[a-z][a-z0-9]*(_[a-z0-9]+)*: ".*")$' "$OUT" || true)"
if [ -n "$BAD" ]; then
    echo "$BAD" | head -5 | sed 's/^/  malformed line: /' >&2
    rc=1
fi

# 3. The file must actually carry the symbolic set, not be a stub.
N="$(grep -cE '^[a-z][a-z0-9]*(_[a-z0-9]+)*: "' "$OUT" || true)"
if [ "$N" -lt 20 ]; then
    echo "  en.yml carries only $N entries — expected the symbolic set (~32)" >&2
    rc=1
fi

if [ "$EMIT" = "1" ]; then
    echo "en.yml symbolic keys ($N):"
    sed -n 's/^\([a-z][a-z0-9_]*\): ".*/\1/p' "$OUT" | LC_ALL=C sort | tr '\n' ' '
    echo
fi

if [ "$rc" = "0" ]; then
    echo "en.yml: $N symbolic entries, shape OK"
fi
exit $rc