# scripts/i18n-script-check.tcl — is each translation in a TSV written in
# its locale's own script?
#
#   tclsh scripts/i18n-script-check.tcl <locale> <tsv>
#
# Machine translations come back in the wrong script and nothing else
# notices: 37 pa rows in Shahmukhi (pa is Gurmukhi), Chinese inside a rw
# row, a Cyrillic а in sr_latn (2026-09-27). A row FAILS when its
# translation has a letter from a script the locale does not use, or —
# for a non-Latin locale — none from the one it does. Digits,
# punctuation, spaces and the Latin of a kept name (veles, Tcl, IRC) are
# allowed everywhere. Exit 0 when every row passes, 1 with the rows.

set loc [lindex $argv 0]
set tsv [lindex $argv 1]

# Scripts as codepoint ranges.
array set R {
    latin      {{0x41 0x5A} {0x61 0x7A} {0xC0 0x24F} {0x1E00 0x1EFF}}
    cyrillic   {{0x400 0x52F}}
    greek      {{0x370 0x3FF}}
    arabic     {{0x600 0x6FF} {0x750 0x77F}}
    devanagari {{0x900 0x97F}}
    bengali    {{0x980 0x9FF}}
    gurmukhi   {{0xA00 0xA7F}}
    gujarati   {{0xA80 0xAFF}}
    oriya      {{0xB00 0xB7F}}
    tamil      {{0xB80 0xBFF}}
    telugu     {{0xC00 0xC7F}}
    kannada    {{0xC80 0xCFF}}
    malayalam  {{0xD00 0xD7F}}
    sinhala    {{0xD80 0xDFF}}
    thai       {{0xE00 0xE7F}}
    ethiopic   {{0x1200 0x139F}}
    kana       {{0x3040 0x30FF}}
    han        {{0x3400 0x4DBF} {0x4E00 0x9FFF} {0xF900 0xFAFF}}
    hangul     {{0xAC00 0xD7AF}}
}

# What each locale writes in: its own script(s); Latin is always allowed
# alongside (kept names), but a non-Latin locale must use its own too.
set own {latin}
switch -- $loc {
    ru - uk - bg - sr_cyrl { set own {cyrillic} }
    hi - mr - ne           { set own {devanagari} }
    bn - as                { set own {bengali} }
    pa                     { set own {gurmukhi} }
    gu                     { set own {gujarati} }
    or                     { set own {oriya} }
    ta                     { set own {tamil} }
    te                     { set own {telugu} }
    kn                     { set own {kannada} }
    ml                     { set own {malayalam} }
    si                     { set own {sinhala} }
    th                     { set own {thai} }
    am                     { set own {ethiopic} }
    ja                     { set own {kana han} }
    zh_hans - zh_hant      { set own {han} }
}

proc script_of {cp} {
    # Danda is sentence punctuation shared by several Indic scripts.
    # Unicode places it in the Devanagari block, but a block is not a
    # script. Do not reject Bengali, Assamese or Odia prose for using it;
    # actual foreign letters still fail below.
    if {$cp == 0x964 || $cp == 0x965} { return "" }
    foreach name [array names ::R] {
        foreach r $::R($name) {
            if {$cp >= [lindex $r 0] && $cp <= [lindex $r 1]} { return $name }
        }
    }
    return ""
}

set fh [open $tsv r]
fconfigure $fh -encoding utf-8
set bad 0
set n 0
while {[gets $fh line] >= 0} {
    if {[string trim $line] eq ""} { continue }
    incr n
    set parts [split $line "\t"]
    set tr [lindex $parts 1]
    set seen {}
    foreach ch [split $tr ""] {
        set s [script_of [scan $ch %c]]
        if {$s ne "" && $s ni $seen} { lappend seen $s }
    }
    set foreign {}
    foreach s $seen {
        if {$s ne "latin" && $s ni $own} { lappend foreign $s }
    }
    set has_own [expr {$own eq {latin}}]
    foreach s $own { if {$s in $seen} { set has_own 1 } }
    if {[llength $foreign] || !$has_own} {
        puts "$loc: [lindex $parts 0] → $tr  (scripts: $seen; expected: $own)"
        incr bad
    }
}
close $fh
if {$n == 0} { puts "$loc: empty"; exit 1 }
exit [expr {$bad > 0}]
