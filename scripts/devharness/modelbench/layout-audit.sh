#!/usr/bin/env bash
# modelbench layout audit — the guard that asks the question mutation
# testing cannot.
#
# Mutation testing asks whether WRONG answers are punished; it cannot
# ask whether RIGHT ones are, because every mutant inherits the
# reference's layout. Grading compiles hidden.rs against the candidate,
# which hardcodes a shape: crate name, module paths, every public item
# name the suite `use`s. A candidate that reorganises internals while
# honouring the documented API — legal, and what a strong model tends
# to do — must still score full marks, or a zero score says something
# about THIS HARNESS instead of about the model. Three runs already
# cost that confusion (see COVERAGE.md, "a near-zero benchmark score is
# an accusation against the harness first").
#
# This audit rebuilds the reference implementation in every shape the
# task statement permits and asserts each still scores full marks:
#
#   control   the reference as written (must pass, or the audit is
#             grading nothing)
#   submod    items re-exported from a submodule, not defined in lib.rs
#   facade    an explicit `pub use` list instead of a glob re-export
#   split     one module per public item (the section markers in the
#             reference are load-bearing for this variant)
#   generic   generic signatures where the reference is concrete, the
#             way a strong model "improves" an API while keeping every
#             call site compiling
#
# Env: MB_AUDIT_VISIBLE / MB_AUDIT_HIDDEN override the expected tallies
# (the suites grow one day; one knob each). MB_AUDIT_HIDDEN also exists
# so the audit can be pointed at a DELIBERATELY BROKEN suite to prove
# it goes red — an audit that has never failed proves nothing.
set -uo pipefail
cd "$(dirname "$0")/../../.."

FIXTURE="${1:-jsonkit}"
SEED="scripts/devharness/modelbench/fixtures/$FIXTURE"
REF="scripts/devharness/modelbench/fixtures/$FIXTURE-hidden/reference/lib.rs"
HIDDEN="${MB_AUDIT_HIDDEN:-scripts/devharness/modelbench/fixtures/$FIXTURE-hidden/hidden.rs}"
WANT_VISIBLE="${MB_AUDIT_VISIBLE:-45}"
WANT_HIDDEN="${MB_AUDIT_HIDDEN_COUNT:-36}"

[ -d "$SEED" ] || { echo "layout-audit: no such fixture: $FIXTURE"; exit 2; }
[ -f "$REF" ] || { echo "layout-audit: $FIXTURE has no reference implementation ($REF)"; exit 2; }
[ -f "$HIDDEN" ] || { echo "layout-audit: no hidden suite at $HIDDEN"; exit 2; }

WORK="/dev/shm/mb-layout-audit-$FIXTURE"
rm -rf "$WORK"
mkdir -p "$WORK"

# One `N passed` line per test binary — take the MAX, never the first
# (run.sh learned that one: `head -1` picks the empty lib target and
# scores a correct implementation as zero).
tally() {
  ( cd "$1" && cargo test --offline ${2:+--test "$2"} 2>&1 ) \
    | grep -oE "[0-9]+ passed" | grep -oE "^[0-9]+" | sort -n | tail -1
}

fail=0

# grade VARIANT_NAME SRC_DIR — copy the seed crate, drop in the
# variant's src/ and the hidden suite, assert both tallies.
grade() {
  local name="$1" src="$2"
  local dir="$WORK/$name"
  rm -rf "$dir"
  mkdir -p "$dir/src" "$dir/tests"
  cp "$SEED/Cargo.toml" "$dir/"
  cp "$SEED/tests/visible.rs" "$dir/tests/"
  cp -r "$src/." "$dir/src/"
  cp "$HIDDEN" "$dir/tests/hidden.rs"
  local vis hid
  vis=$(tally "$dir" visible)
  hid=$(tally "$dir" hidden)
  rm -rf "$dir/target/debug/deps/hidden-"* "$dir/tests/hidden.rs"
  if [ "${vis:-0}" -eq "$WANT_VISIBLE" ] && [ "${hid:-0}" -eq "$WANT_HIDDEN" ]; then
    printf 'layout-audit: %-8s %s/%s visible · %s/%s hidden — PASS\n' \
      "$name" "${vis:-0}" "$WANT_VISIBLE" "${hid:-0}" "$WANT_HIDDEN"
  else
    printf 'layout-audit: %-8s %s/%s visible · %s/%s hidden — FAIL\n' \
      "$name" "${vis:-0}" "$WANT_VISIBLE" "${hid:-0}" "$WANT_HIDDEN"
    fail=1
  fi
}

# ── control ──────────────────────────────────────────────────────────
mkdir -p "$WORK/src-control"
cp "$REF" "$WORK/src-control/lib.rs"
grade control "$WORK/src-control"

# ── submod: re-exported from a submodule, not defined in lib.rs ─────
mkdir -p "$WORK/src-submod"
cp "$REF" "$WORK/src-submod/core.rs"
printf '// A candidate is free to keep lib.rs a re-export shell.\nmod core;\npub use core::*;\n' \
  > "$WORK/src-submod/lib.rs"
grade submod "$WORK/src-submod"

# ── facade: an explicit pub-use list instead of a glob ───────────────
mkdir -p "$WORK/src-facade"
cp "$REF" "$WORK/src-facade/core.rs"
printf '%s\n' \
  '// The documented API, named item by item.' \
  'mod core;' \
  'pub use core::{Json, JsonError, merge_patch, parse, patch, pointer, serialize};' \
  > "$WORK/src-facade/lib.rs"
grade facade "$WORK/src-facade"

# ── split: one module per public item ────────────────────────────────
# The reference carries `// ── item: NAME ──` markers exactly so this
# cut is deterministic. Everything before the first marker (the import
# preamble) goes into every module; each marker starts a module named
# after its item. If the markers ever drift, the control stays green
# and this variant goes red — the right failure mode.
mkdir -p "$WORK/src-split"
# Every function module imports the two public types through the crate
# root — exactly what a model doing a mod-per-function split would
# write. The type modules DEFINE them, so they get neither the import
# nor the shared `err` helper (it names JsonError, which is out of
# scope inside item_Json.rs). err is captured apart from the preamble
# for exactly that reason.
awk -v out="$WORK/src-split" '
  {
    line = $0
    if (line ~ /^fn err\(/) in_err = 1
    if (in_err) {
      errblock = errblock line "\n"
      if (line == "}") in_err = 0
      next
    }
    if (line ~ /^\/\/ ── item: /) {
      name = $4
      file = out "/item_" name ".rs"
      names[++n] = name
      if (name == "Json" || name == "JsonError") {
        print preamble > file
      } else {
        print (preamble errblock) > file
        print "use crate::{Json, JsonError};" >> file
      }
      printing = 1
      next
    }
    if (printing) { print line >> file; next }
    preamble = preamble line "\n"
  }
  END {
    lib = out "/lib.rs"
    for (i = 1; i <= n; i++) {
      printf "mod item_%s;\n", names[i] >> lib
      printf "pub use item_%s::*;\n", names[i] >> lib
    }
  }
' "$REF"
grade split "$WORK/src-split"

# ── generic: signatures widened, call sites untouched ────────────────
mkdir -p "$WORK/src-generic"
cp "$REF" "$WORK/src-generic/lib.rs"
# parse(&str) becomes parse(impl AsRef<str>) delegating to parse_str.
sed -i 's|^pub fn parse(src: &str) -> Result<Json, JsonError> {$|pub fn parse<S: AsRef<str>>(src: S) -> Result<Json, JsonError> {\n    parse_str(src.as_ref())\n}\n\nfn parse_str(src: \&str) -> Result<Json, JsonError> {|' \
  "$WORK/src-generic/lib.rs"
# pointer(&Json) becomes pointer(&impl Borrow<Json>) delegating inward.
sed -i "s|^pub fn pointer<'a>(v: &'a Json, ptr: &str) -> Option<&'a Json> {\$|pub fn pointer<'a, T: std::borrow::Borrow<Json>>(v: \&'a T, ptr: \&str) -> Option<\&'a Json> {\n    pointer_json(v.borrow(), ptr)\n}\n\nfn pointer_json<'a>(v: \&'a Json, ptr: \&str) -> Option<\&'a Json> {|" \
  "$WORK/src-generic/lib.rs"
grade generic "$WORK/src-generic"

rm -rf "$WORK"
if [ "$fail" -ne 0 ]; then
  echo "layout-audit: RED — a legal layout failed to grade. A zero hidden"
  echo "layout-audit: score from this harness would indict the harness, not"
  echo "layout-audit: the model. Fix the suite or the reference first."
  exit 1
fi
echo "layout-audit: green — every legal layout scores full marks."
