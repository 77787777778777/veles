#!/usr/bin/env bash
# scripts/i18n-sync.sh — regenerate the desktop catalogs from the keys
# the tree actually uses.
#
# WHY. The catalogs drifted: keys appeared in code with no catalog entry
# (loud-missing fallback hid it), and entries outlived their keys. This
# regenerates en.msg as a complete identity catalog and rebuilds every
# other locale PRESERVING its hand translations, dropping only keys that
# no longer exist in code.
#
# The key set comes from scripts/i18n-keys.tcl, which reads BOTH sides:
# the desktop's ::vi18n::mc/::vi18n::label calls (balanced-brace walk)
# and the engine's i18n::lookup/i18n! calls (string literals). One key
# set, so a translator works both catalogs and an engine string that
# passes through the desktop is already translatable on arrival.
#
# The engine catalogs (crates/veles-core/locales/*.yml) use the same
# keys under a `_version: 1` header; this script reports their drift
# against the same key set but does NOT rewrite them — a YAML rewrite
# needs to preserve comments, which is the next round's problem.
#
# Usage:
#   bash scripts/i18n-sync.sh            # rewrite en.msg + locale .msgs
#   bash scripts/i18n-sync.sh --check    # report drift, write nothing
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Catalog writes go through ::unsplit::puts, and --check names a split
# 4-byte character (scripts/i18n-unsplit.tcl).
export I18N_UNSPLIT="$ROOT/scripts/i18n-unsplit.tcl"
MSGS="$ROOT/veles-desktop/msgs"
LOCALES="$ROOT/crates/veles-core/locales"

CHECK=0
for a in "$@"; do [ "$a" = "--check" ] && CHECK=1; done

run_tcl() {
    if command -v direnv >/dev/null 2>&1; then
        direnv exec "$ROOT" tclsh "$@" 2>/dev/null || tclsh "$@"
    else
        tclsh "$@"
    fi
}

KEYS_TCL="$ROOT/scripts/i18n-keys.tcl"
KEYS="$(mktemp)"
# `--keys-only` is LOAD-BEARING, not cosmetic: without it the extractor
# writes "<file>:<key>" rows (its report format), and the catalogs came
# out with keys like "acl.tcl:%d identities · %d users" — every
# translation in every locale then failed to match its source string
# and fell back to English, silently. The check below catches exactly
# this shape, so it is asserted rather than trusted.
run_tcl "$KEYS_TCL" "$ROOT" --keys-only > "$KEYS"
if grep -qE '\.(tcl|rs):' "$KEYS"; then
    echo "FATAL: the key extractor emitted file-prefixed rows." >&2
    echo "       Keys must be bare; catalogs keyed by filename match nothing." >&2
    rm -f "$KEYS" ; exit 2
fi

count=$(grep -c . "$KEYS" || true)
echo "keys found in the tree: $count"

if [ "$CHECK" = "1" ]; then
    # Report per-locale drift without writing. Exit 1 when any locale is
    # incomplete, so this can gate a round.
    # The catalog is read the way msgcat reads it — as Tcl lists — not
    # with sed. The sed version (`s/^::msgcat::mcset id \(.*\) .*/\1/`)
    # was greedy over a line whose key and value both contain spaces, so
    # it extracted "key + most of the value" and reported ~900 of ~970
    # keys "missing" from catalogs that were complete: a gate that is red
    # on a correct tree gets ignored, which is worse than no gate.
    CHK="$(mktemp --suffix=.tcl)"
    cat > "$CHK" <<'TCL'
lassign $argv keysfile msgs
source $::env(I18N_UNSPLIT)
set fh [open $keysfile r]; fconfigure $fh -encoding utf-8
set want [lsort -unique [lsearch -all -inline -not [split [read $fh] \n] {}]]
close $fh
set fail 0
foreach f [lsort [glob [file join $msgs *.msg]]] {
    set id [file rootname [file tail $f]]
    if {$id eq "en"} { continue }
    set have {}
    foreach at [::unsplit::offenders $f] {
        puts "  $id: a 4-byte character at byte $at starts 3 bytes before a 4 KiB boundary — \
              Tcl 8.6 mis-decodes it; rewrite the catalog with scripts/i18n-sync.sh"
        set fail 1
    }
    set fh [open $f r]; fconfigure $fh -encoding utf-8 -buffersize 1048576
    foreach line [split [read $fh] \n] {
        set line [string trim $line]
        if {![string match "::msgcat::mcset $id *" $line]} { continue }
        if {[catch { llength $line } n] || $n != 4} { continue }
        lappend have [lindex $line 2]
    }
    close $fh
    set have [lsort -unique $have]
    set miss 0; set extra 0
    foreach k $want { if {[lsearch -exact -sorted $have $k] < 0} { incr miss } }
    foreach k $have { if {[lsearch -exact -sorted $want $k] < 0} { incr extra } }
    puts [format "  %-9s %5d entries  missing %4d  extra %4d" $id [llength $have] $miss $extra]
    if {$miss > 0} { set fail 1 }
}
exit $fail
TCL
    run_tcl "$CHK" "$KEYS" "$MSGS"
    fail=$?
    rm -f "$KEYS" "$CHK"
    exit $fail
fi

# ─ en.msg: the identity catalog ────────────────────────────────────
#
# The generator reads its logic from a file (nothing needs escaping).
GEN="$(mktemp --suffix=.tcl)"
cat > "$GEN" <<'TCL'
# args: keys-file msg-path locale [preserve-from]
lassign $argv keysfile msgpath locale preserve
source $::env(I18N_UNSPLIT)
set fh [open $keysfile r]
set keys [split [read $fh] \n]
close $fh

set existing {}
if {$preserve ne "" && [file exists $preserve]} {
    # Read back as Tcl LISTS, not by regexp: the file is written with
    # `list` quoting, so a translation containing a brace survives the
    # round trip instead of being silently dropped.
    set fh [open $preserve r]
    fconfigure $fh -encoding utf-8 -buffersize 1048576
    foreach line [split [read $fh] \n] {
        set line [string trim $line]
        if {![string match "::msgcat::mcset $locale *" $line]} { continue }
        if {[catch { llength $line } n] || $n != 4} { continue }
        dict set existing [lindex $line 2] [lindex $line 3]
    }
    close $fh
}

set out [open $msgpath w]
fconfigure $out -encoding utf-8
# The header names the pipeline that made the FILE, not this script.
# A translator's locale catalog is built by i18n-tsv2msg.sh from their
# TSV; i18n-sync.sh only regenerates it (adding new identity keys,
# dropping dead ones). Stamping every file with "GENERATED by
# i18n-sync.sh" was wrong about where the translations came from — and
# `git diff` showed it as the only change across 47 catalogs.
if {$locale eq "en"} {
    ::unsplit::puts $out [format {# ::msgcat 1.0 — veles-desktop strings. GENERATED by} ]
    ::unsplit::puts $out {# scripts/i18n-sync.sh — regenerate, do not edit ad hoc.}
} else {
    ::unsplit::puts $out "# ::msgcat 1.0 — veles-desktop strings, locale $locale."
    ::unsplit::puts $out {# GENERATED by scripts/i18n-tsv2msg.sh from a translator's TSV;}
    ::unsplit::puts $out {# scripts/i18n-sync.sh drops dead keys; it never copies English in.}
}
# i18n-tsv2msg.sh writes the same five lines — keep the two in step, or
# every run of one rewrites all 46 headers the other wrote.
::unsplit::puts $out {# Keys are the English source strings; values are the translation.}
::unsplit::puts $out {# Missing keys fall back to English (the key itself).}
::unsplit::puts $out {package require msgcat}
set n 0
foreach k [lsort -unique $keys] {
    if {[string trim $k] eq ""} { continue }
    # Quote with `list`, never by hand-wrapping in braces: a key that
    # contains a brace must survive the round trip, and msgcat runs the
    # catalog as Tcl.
    if {[dict exists $existing $k]} {
        ::unsplit::puts $out [list ::msgcat::mcset $locale $k [dict get $existing $k]]
    } elseif {$locale eq "en"} {
        ::unsplit::puts $out [list ::msgcat::mcset en $k $k]
    } else {
        # A new key is left OUT of the other locales, on purpose: that
        # is what makes i18ntest's completeness check fail until the key
        # is translated. An identity row here would pass the check and
        # ship English (the 2026-09-26 l10n round found 263 keys in
        # exactly that state), and the clarify round briefly made this
        # the default. Translate through i18n-tsv2msg.sh instead.
        continue
    }
    incr n
}
close $out
puts $n
TCL

# The first line of the header needs the real generator name in it; do it
# with sed after the fact so the Tcl above stays plain ASCII.
n=$(run_tcl "$GEN" "$KEYS" "$MSGS/en.msg" en "" | tail -1)
echo "en.msg: $n keys"

# ─ every other locale: preserve translations, drop dead keys ───────
for f in "$MSGS"/*.msg; do
    id=$(basename "$f" .msg)
    [ "$id" = "en" ] && continue
    tmp="$(mktemp)"
    cp "$f" "$tmp"
    n=$(run_tcl "$GEN" "$KEYS" "$f" "$id" "$tmp" | tail -1)
    rm -f "$tmp"
    echo "$id.msg: $n keys (translations preserved)"
done

# ─ en.yml: NOT regenerated here, deliberately ─────────────────────────
#
# en.yml is the engine's identity catalog and it is still on the OLD
# symbolic-only key set (32 keys) while the locale catalogs carry the
# unified ~900-key English-as-key set. That split is real and it is the
# next commit's work, but it cannot be done as a blind regeneration:
#
#   * the obvious move — feed `<key>\t<key>` rows to tsv2yml.sh — writes
#     an EMPTY file, because tsv2yml deliberately drops every row whose
#     value equals its key ("an English-as-key row whose value IS the key
#     is not a translation"). That drop is right for a translated
#     catalog and wrong for the identity one.
#   * and en.yml's key set is half of a two-rule contract in
#     `i18n::tests::catalog_keys_and_code_do_not_drift`: locale catalogs
#     must be a subset of en.yml, AND every en.yml key must be looked up
#     from Rust source. Those two cannot BOTH hold once the catalogs
#     merge the desktop's keys in — "Cancel" is a real key that no Rust
#     line mentions. The test's second rule has to grow to cover both
#     surfaces (which is the honest reading of its intent: no dead keys),
#     and that change ships with the en.yml rewrite, not before it.
#
# Doing it here would trade a red test for an empty catalog and a
# silently widened net, so it is left as the next commit.

rm -f "$KEYS" "$GEN"
echo "catalogs synced. Run: bash scripts/i18n-sync.sh --check"