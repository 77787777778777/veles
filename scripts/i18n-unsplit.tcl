# scripts/i18n-unsplit.tcl — write a catalog so Tcl 8.6 can read it back.
#
# Tcl 8.6 reads a channel in 4 KiB buffers and, built with TCL_UTF_MAX=3,
# takes the first three bytes of a split 4-byte UTF-8 character (U+10000
# and up: 📎, 🔒 …) for a whole one and mis-decodes it. Measured on
# 8.6.16: only a character whose lead byte sits at 4093 mod 4096 is hit
# (a 2+2 or 1+3 split decodes correctly). msgcat `source`s catalogs, so
# the line carrying such a character loaded under a garbled key and that
# string fell back to English — ru's 📎 hint, 2026-10-04, after two keys
# added above it moved it onto the boundary.
#
# The catalog writers (i18n-tsv2msg.sh, i18n-sync.sh) write every line
# through ::unsplit::puts, which puts a two-byte comment line ("#") first
# wherever a line would land a character there. `--check` (i18n-sync.sh)
# and uitest.tcl find any file that has one anyway.

namespace eval ::unsplit {
    variable at 0
}

# Does any 4-byte character in `bytes` (UTF-8, starting at file offset
# `base`) have its lead byte at 4093 mod 4096?
proc ::unsplit::bad {bytes base} {
    foreach pair [regexp -all -indices -inline {[\xf0-\xf4]} $bytes] {
        if {([lindex $pair 0] + $base) % 4096 == 4093} { return 1 }
    }
    return 0
}

# `puts $fh $line` for a channel opened `-encoding utf-8` and written
# from offset 0 through this proc only.
proc ::unsplit::puts {fh line} {
    variable at
    set bytes [encoding convertto utf-8 "$line\n"]
    while {[bad $bytes $at]} {
        ::puts -nonewline $fh "#\n"
        incr at 2
    }
    ::puts -nonewline $fh "$line\n"
    incr at [string length $bytes]
}

# The byte offsets in `path` where a 4-byte character starts at
# 4093 mod 4096 — empty for a file Tcl 8.6 reads back intact.
proc ::unsplit::offenders {path} {
    set fh [open $path rb]
    set bytes [read $fh]
    close $fh
    set out {}
    foreach pair [regexp -all -indices -inline {[\xf0-\xf4]} $bytes] {
        set i [lindex $pair 0]
        if {$i % 4096 == 4093} { lappend out $i }
    }
    return $out
}
