#!/usr/bin/env bash
# scripts/i18n-live.sh — drive the REAL desktop in a locale and read the
# strings off the widgets it built.
#
# WHY THIS EXISTS. Every other check in this tree is STATIC: it asks
# whether a source string resolves to a catalog key. That finds gaps, but
# it cannot tell whether a user SEES the translation — and the
# 2026-09-25 review found three shipped bugs that no static scan could
# see (a picker that never persisted, a chip reading "de ⇄ de", two
# transcript lines that ignored the swap). They were found by running the
# app.
#
# The 2026-09-26 round's five USER reports were all "wrapped but not a
# key", which is exactly the class a static scan can see. This is the
# other half: proof that the wiring renders.
#
# How it works: main.tcl is the shipping entry point, booted on the demo
# fixture (VELES_DESKTOP_DEMO, so no gateway and no model are needed)
# under a throwaway X server. A probe sourced just before it walks the
# live widget tree once the scenario has settled, and for every widget
# whose -text is a catalog key it compares what the widget shows with
# what the ACTIVE locale says that key should be.
#
# READING THE OUTPUT. `PROBE-GAPS` counts widgets that still show the
# English. NOT every gap is a bug: a locale's word for "Stop" may BE
# "Stop" (Afrikaans says Stop, Status, Unstage), and the probe cannot
# tell that from a missing translation. A gap is worth checking when the
# English is a phrase the locale plainly translates — or when the same
# widget is translated in one key and not another.
#
# Usage: scripts/i18n-live.sh <locale>
#   Run it inside the dev shell: direnv exec . bash scripts/i18n-live.sh de
set -uo pipefail
LOC="${1:?usage: i18n-live.sh <locale>}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"

DISP=""
for n in $(seq 200 220); do
    [ ! -e "/tmp/.X${n}-lock" ] && { DISP=":$n" ; break ; }
done
[ -n "$DISP" ] || { echo "no free X display between :200 and :220" >&2 ; exit 1 ; }

# A scratch XDG root: a probe must never read or write the real config,
# state, pins or layout. Same rule scripts/desktop_demo.sh follows.
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/veles-live.XXXXXX")"
export XDG_CONFIG_HOME="$SCRATCH/config" XDG_DATA_HOME="$SCRATCH/data" XDG_CACHE_HOME="$SCRATCH/cache"
mkdir -p "$XDG_CONFIG_HOME/veles" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
printf '[tui]\nlocale = "%s"\n' "$LOC" > "$XDG_CONFIG_HOME/veles/config.toml"

Xvfb "$DISP" -screen 0 1600x1000x24 >/dev/null 2>&1 &
XPID=$!
trap 'kill $XPID 2>/dev/null; rm -rf "$SCRATCH"' EXIT
sleep 2

cat > "$SCRATCH/probe.tcl" <<'TCL'
namespace eval ::i18nprobe {}

# Walk the tree the way the swap sweep does: every widget with a -text,
# compared against the ACTIVE locale.
proc ::i18nprobe::walk {w gapsv sv tv} {
    upvar 1 $gapsv gaps
    upvar 1 $sv checked
    upvar 1 $tv translated
    if {![winfo exists $w]} { return }
    set t ""
    if {![catch { set t [$w cget -text] }] && $t ne ""} {
        incr checked
        set want [::vi18n::mc $t]
        if {$want ne $t} {
            incr translated
        } elseif {[::vi18n::key_for $t] ne ""} {
            # The text IS a catalog key and the locale has no different
            # value for it: either the translation is the same word (a
            # real possibility in a Germanic or Romance locale) or the
            # row is English. Reported either way; a human decides.
            lappend gaps [list $w $t]
        }
    }
    foreach c [winfo children $w] { ::i18nprobe::walk $c gaps checked translated }
}

proc ::i18nprobe::dump {} {
    puts "PROBE-LOCALE: [::vi18n::current]"
    puts "PROBE-EN-INDEX: [::vi18n::build_revmap en]"
    set gaps {}
    set checked 0
    set translated 0
    ::i18nprobe::walk . gaps checked translated
    puts "PROBE-CHECKED: $checked"
    puts "PROBE-TRANSLATED: $translated"
    puts "PROBE-GAPS: [llength $gaps]"
    foreach g [lrange $gaps 0 40] { puts "  GAP [lindex $g 0]: <[lindex $g 1]>" }
    exit 0
}
# The demo scenario needs a few seconds to lay out panes and transcript.
after 5000 ::i18nprobe::dump
TCL

# main.tcl builds at source time, so the probe is sourced first and its
# `after` fires once the window exists.
cat > "$SCRATCH/wrap.tcl" <<TCL
source "$SCRATCH/probe.tcl"
source [file join $REPO veles-desktop main.tcl]
TCL

export VELES_DEMO_AUTO=1 VELES_PRINT_WINDOW_ID=1 VELES_DESKTOP_DEMO=1
cd "$REPO"
DISPLAY="$DISP" wish "$SCRATCH/wrap.tcl" > "$SCRATCH/out.log" 2>&1
grep -E 'PROBE-|GAP ' "$SCRATCH/out.log" | head -60
echo "BGERRORs: $(grep -c BGERROR "$SCRATCH/out.log")"
if grep -q 'PROBE-GAPS: [1-9]' "$SCRATCH/out.log"; then
    echo "(a gap is a widget showing English; a locale whose word IS the"
    echo " English word reports one too — check the string before fixing)"
fi
exit 0