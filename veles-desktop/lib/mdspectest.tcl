# veles-desktop/lib/mdspectest.tcl — run lib/mdspec.tcl against the real
# renderer.
#
# The spec file is the source of truth; this only executes it. Two
# verdicts, deliberately distinct:
#
#   supported  a construct we implement. A mismatch is a FAILURE.
#   gap        a construct we do NOT implement, pinned at today's
#              behaviour. A mismatch is a DRIFT — reported and counted
#              separately, because it usually means someone improved
#              the renderer and the spec entry should be promoted to
#              `case`, not that anything broke.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]
source [file join $_dir safeexec.tcl]
source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
namespace eval ::vstatus { proc set_msg {m} {} }
source [file join $_dir markdown.tcl]
source [file join $_dir mdspec.tcl]

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

text .t -width 100 -height 40
pack .t
update

set failed 0
set passed 0
set drift  0
set gaps   0

proc esc {s} { return [string map {"\n" "\\n" "\t" "\\t"} $s] }

# All text covered by $tag, concatenated in document order.
proc tagtext {w tag} {
    set out ""
    set r {}
    catch { set r [$w tag ranges $tag] }
    foreach {a b} $r { append out [$w get $a $b] }
    return $out
}

proc run_case {kind name ref input want_text want_spans} {
    .t configure -state normal
    .t delete 1.0 end
    ::vmd::forget_links .t
    if {[catch {::vmd::render .t $input} err]} {
        puts "  ✗ $name  [$ref]\n  render error: $err"
        incr ::failed
        return
    }
    update

    set got_text [.t get 1.0 end-1c]
    set problems {}
    if {$got_text ne $want_text} {
        lappend problems "  text  want '[esc $want_text]'\n        got  '[esc $got_text]'"
    }
    foreach {tag want} $want_spans {
        set got [tagtext .t $tag]
        if {$got ne $want} {
            lappend problems "  $tag  want '[esc $want]'  got '[esc $got]'"
        }
    }

    if {![llength $problems]} {
        if {$kind eq "gap"} {
            incr ::gaps
        } else {
            incr ::passed
        }
        return
    }
    if {$kind eq "gap"} {
        incr ::drift
        puts "  ~ DRIFT  $name  \[$ref\]"
    } else {
        incr ::failed
        puts "  ✗ $name  \[$ref\]"
    }
    foreach p $problems { puts $p }
}

puts "markdown spec — [llength $::mdspec::cases] cases from lib/mdspec.tcl"
foreach c $::mdspec::cases {
    lassign $c kind name ref input want_text want_spans
    run_case $kind $name $ref $input $want_text $want_spans
}

puts ""
# The tally line's FORMAT is a contract: flake.nix greps
#   ^<suite>: <n> passed, 0 failed
# to decide whether the suite is clean. Extra words in it break the
# gate while every assertion passes — which is exactly how this suite
# first went red in CI and green locally. Detail goes on its own line.
puts "mdspectest: $passed passed, $failed failed"
puts "  ($gaps documented gaps, $drift drifted)"
# A drifted gap is not a build break: it means the renderer got better
# and the spec entry should be promoted. Say so loudly, exit green.
if {$drift} {
    puts "NOTE: $drift gap(s) no longer match — if that is an improvement,"
    puts "      move the entry from `gap` to `case` in lib/mdspec.tcl."
}
exit [expr {$failed > 0 ? 1 : 0}]
