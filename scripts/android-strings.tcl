# android-strings.tcl — android/strings.tsv → res/values-*/strings.xml
#
#   tclsh scripts/android-strings.tcl <strings.tsv> <res dir>
#
# The APK's build (flake.nix, velesApk) runs this. English is
# res/values/strings.xml, written by hand; the TSV holds every other
# language, one row per string:
#
#     <locale> TAB <name> TAB <text>
#
# with the desktop's locale ids (veles-desktop/msgs/*.msg). It FAILS the
# build when a translatable English string lacks a row in any language,
# when a row names a string English does not have, or when a row's
# placeholders (%1$d) differ from English's — the same rule the desktop's
# catalogs live by: never ship English in a translated UI.

if {[llength $argv] != 2} {
    puts stderr "usage: tclsh android-strings.tcl <strings.tsv> <res dir>"
    exit 2
}
lassign $argv tsv res

# The desktop's locale ids → Android resource qualifiers. Two-letter
# languages are values-xx; three-letter ones and scripts need the
# BCP 47 form (values-b+fil). Android still calls Indonesian "in".
set qualifier {
    af af  am am  as as  bg bg  bn bn  cs cs  de de  fil b+fil  gu gu
    ha ha  hi hi  hr hr  id in  ig ig  ja ja  kn kn  mg mg  ml ml  mr mr
    ms ms  ne ne  or or  pa pa  pcm b+pcm  pl pl  ru ru  rw rw  si si
    sk sk  sn sn  so so  sr_cyrl b+sr+Cyrl  sr_latn b+sr+Latn  sw sw
    ta ta  te te  th th  tl tl  uk uk  vi vi  wo b+wo  xh xh  yo yo
    zh_hans b+zh+Hans  zh_hant b+zh+Hant  zu zu
}

proc slurp {path} {
    set f [open $path]
    fconfigure $f -encoding utf-8
    set s [read $f]
    close $f
    return $s
}

proc placeholders {text} {
    lsort [regexp -all -inline {%[0-9]+\$[a-z]} $text]
}

# Android's string resource escaping, then XML's.
proc escape {text} {
    set text [string map {\\ \\\\ ' \\' \" \\\" \n \\n} $text]
    if {[string match {[@?]*} $text]} {
        set text \\$text
    }
    string map {& &amp; < &lt; > &gt;} $text
}

set english [dict create]
foreach {- name attrs text} [regexp -all -inline {<string name="([^"]+)"([^>]*)>([^<]*)</string>} \
        [slurp [file join $res values strings.xml]]] {
    if {[string match {*translatable="false"*} $attrs]} continue
    dict set english $name $text
}
if {[dict size $english] == 0} {
    puts stderr "android-strings: no translatable strings in $res/values/strings.xml"
    exit 1
}

set rows [dict create]
set errors {}
set lineno 0
foreach line [split [slurp $tsv] \n] {
    incr lineno
    if {$line eq "" || [string index $line 0] eq "#"} continue
    set cols [split $line \t]
    if {[llength $cols] != 3} {
        lappend errors "$tsv:$lineno: expected 3 tab-separated columns"
        continue
    }
    lassign $cols loc name text
    if {![dict exists $qualifier $loc]} {
        lappend errors "$tsv:$lineno: unknown locale $loc"
    } elseif {![dict exists $english $name]} {
        lappend errors "$tsv:$lineno: $name is not an English string"
    } elseif {[dict exists $rows $loc $name]} {
        lappend errors "$tsv:$lineno: $loc $name appears twice"
    } elseif {[placeholders $text] ne [placeholders [dict get $english $name]]} {
        lappend errors "$tsv:$lineno: $loc $name has placeholders\
            [placeholders $text], English has [placeholders [dict get $english $name]]"
    } elseif {[string trim $text] eq ""} {
        lappend errors "$tsv:$lineno: $loc $name is empty"
    } else {
        dict set rows $loc $name $text
    }
}
dict for {loc q} $qualifier {
    dict for {name -} $english {
        if {![dict exists $rows $loc $name]} {
            lappend errors "$loc has no translation of $name"
        }
    }
}
if {[llength $errors]} {
    puts stderr [join $errors \n]
    exit 1
}

dict for {loc q} $qualifier {
    set dir [file join $res values-$q]
    file mkdir $dir
    set f [open [file join $dir strings.xml] w]
    fconfigure $f -encoding utf-8 -translation lf
    puts $f "<?xml version=\"1.0\" encoding=\"utf-8\"?>"
    puts $f "<!-- Generated from android/strings.tsv by scripts/android-strings.tcl. -->"
    puts $f "<resources>"
    dict for {name -} $english {
        puts $f "    <string name=\"$name\">[escape [dict get $rows $loc $name]]</string>"
    }
    puts $f "</resources>"
    close $f
}
puts "android-strings: [dict size $english] strings in [dict size $qualifier] languages"
