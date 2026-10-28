# veles-desktop/lib/uitest.tcl — assertions for the non-markdown desktop
# modules. mdtest.tcl covers markdown + theme; this covers the rest.
#
# Run:  scripts/tcl_tests.sh   (which runs both under Xvfb)
#
# Exists because the desktop Tcl had exactly ONE test file covering 2 of
# 29 modules, and nothing ran it. The invariants here are the ones that
# rotted silently: /help drifting from the commands actually handled,
# bindings.tcl being the single source of truth, and Tk option names
# that only fail at runtime.

set _dir [file dirname [file normalize [info script]]]

set failed 0
set passed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

proc slurp {path} {
    set fh [open $path r]
    set data [read $fh]
    close $fh
    return $data
}

# ── /help must document exactly what slash_command handles ───────────
#
# These drifted: 20 commands were implemented and undocumented, so the
# only way to discover /steer, /branch, /compress or /diff was reading
# the source.
puts "slash command coverage"

set chat [slurp [file join $_dir chat.tcl]]
# The proc body, from its definition to the next top-level proc.
set start [string first "proc ::vchat::slash_command" $chat]
ok "found slash_command" {$start >= 0}
set rest [string range $chat $start end]
set stop [string first "\nproc " [string range $rest 1 end]]
if {$stop < 0} { set body $rest } else { set body [string range $rest 0 $stop] }

# Documented: the union of the two lists /help actually renders — the
# ENGINE's table (`frontends/commands.rs::help_entries`, served at
# /api/commands/builtin) and `cmds.tcl`'s `desktop_local`.
#
# It used to be the 50-line literal in this proc, which is exactly the
# copy that drifted: it advertised commands that do not exist and
# omitted six that do. Scanning the two real sources instead is the
# same idiom `every_shared_command_reaches_every_surface` uses on the
# Rust side.
set documented {}
proc _note_cmd {nm} {
    if {[lsearch -exact $::documented $nm] < 0} { lappend ::documented $nm }
}
set _cmds_tcl [slurp [file join $_dir cmds.tcl]]
foreach line [split $_cmds_tcl \n] {
    if {[regexp {^\s+\{/([a-z]+)} $line -> nm]} { _note_cmd $nm }
}
set _engine [slurp [file join $_dir .. .. crates veles-frontends src commands.rs]]
# SHARED_COMMANDS moved to core's slash_names.rs in the 2026-09-28 crate
# split (aliases::is_builtin reads it); the help rows stayed in commands.rs.
append _engine \n [slurp [file join $_dir .. .. crates veles-core src slash_names.rs]]
foreach line [split $_engine \n] {
    # Only the two real tables: a help row `("/model [name]", "…")` and
    # a SHARED_COMMANDS entry `"/model",`. Matching any `"/…"` anywhere
    # would sweep in every path literal in the file.
    if {[regexp {^\s+\("/([a-z]+)} $line -> nm]} { _note_cmd $nm }
    if {[regexp {^\s+"/([a-z]+)",\s*$} $line -> nm]} { _note_cmd $nm }
}
# Handled: switch arms at the proc's indentation ( 8 spaces, "name" ).
set handled {}
foreach line [split $body \n] {
    if {[regexp {^        "([a-z]+)"} $line -> nm]} {
        if {[lsearch -exact $handled $nm] < 0} { lappend handled $nm }
    }
}

ok "help block parsed"    {[llength $documented] > 10}
ok "switch arms parsed"   {[llength $handled] > 10}

set undocumented {}
foreach h $handled {
    if {[lsearch -exact $documented $h] < 0} { lappend undocumented $h }
}
if {[llength $undocumented]} { puts "      undocumented: $undocumented" }
ok "every handled command is documented in /help" {[llength $undocumented] == 0}

# The reverse direction is no longer "there is a switch arm for it".
# The desktop implements natively only what opens a PANE (/model's
# picker, /log, /find); everything else the engine already knows is run
# by the engine, through POST /api/command in the `default` arm. That
# is the fix for exactly this drift — /help could advertise /queue and
# /context and then answer "unknown command" — so what must hold is
# that the fallthrough EXISTS.
ok "unhandled names go to the engine, not to 'unknown command'" {
    [string first {_try_shared_command} $body] >= 0
}
ok "…and that call is in the default arm, before the custom-command path" {
    [string first {_try_shared_command} $body]
        < [string first {api/commands/render} $body]
}
# The desktop-only commands are the exception: nothing else answers
# them, so each one needs a real arm here.
set _local {}
foreach line [split $_cmds_tcl \n] {
    if {[regexp {^\s+\{/([a-z]+)} $line -> nm]} { lappend _local $nm }
}
set missing_local {}
foreach l $_local {
    if {[lsearch -exact $handled $l] < 0} { lappend missing_local $l }
}
if {[llength $missing_local]} { puts "      desktop-only but unhandled: $missing_local" }
ok "every desktop-only command has a real arm" {[llength $missing_local] == 0}

# ── the desktop must not reach into hermes' skill root ───────────────
#
# /skills globbed ~/.hermes/skills directly, which is the read-only
# MIGRATION root — it missed every skill under ~/.veles and reported a
# bare category count. The engine knows both roots; ask it.
puts "\nengine-owned data"
ok "no hardcoded .hermes path in chat.tcl" \
    {![regexp {\.hermes"? +"?skills} $chat]}
ok "/skills goes through the API" {[string match "*api/skills*" $chat]}
ok "/memory goes through the API" {[string match "*api/memory*" $chat]}

# ── API failures must be reported, not swallowed ─────────────────────
#
# `catch {...}` with no error variable discards the error. In chat.tcl
# that meant /steer printed "steer queued" and /retry printed "retrying
# last prompt…" whether or not the POST succeeded, and /branch and /undo
# said nothing at all when they failed. A whole slash command doing
# nothing while claiming success is the single most confusing failure
# this app can produce.
#
# The check covered chat.tcl alone, and the rest of the app had the same
# bug in three more places: the cron overlay's Pause and Resume buttons
# swallowed their POST (clicking Pause looked identical to clicking
# nothing), and every desktop-state writer swallowed its own failure, so
# a theme or a set of pins that could not be saved came back wrong on
# the next start with no explanation. It runs over every module now.
puts "\nerror reporting"

# NOTE: a literal unbalanced brace inside a quoted string still breaks
# Tcl's brace counting for the enclosing block, so the marker is built
# with [format] rather than written inline.
# Walk the file character by character; for every `catch {` find the
# MATCHING close brace by counting depth, then look at what follows it.
# A window-of-N-lines heuristic does not work here: too small and it
# misses the closing line of a long block (false positive), too large
# and it picks up an unrelated API call further down (also a false
# positive). Brace matching is exact.
proc silent_api_catches {src} {
    set marker [format {catch %c} 123]
    set out {}
    set idx 0
    while {1} {
        set hit [string first $marker $src $idx]
        if {$hit < 0} break
        # Position of the opening brace, then scan to its match.
        set open_at [expr {$hit + [string length $marker] - 1}]
        set depth 0
        set close_at -1
        for {set k $open_at} {$k < [string length $src]} {incr k} {
            set c [string index $src $k]
            set prev [expr {$k > 0 ? [string index $src [expr {$k - 1}]] : ""}]
            if {$prev eq "\\"} { continue }
            if {$c eq "\{"} { incr depth }
            if {$c eq "\}"} {
                incr depth -1
                if {$depth == 0} { set close_at $k ; break }
            }
        }
        if {$close_at < 0} { set idx [expr {$hit + 1}] ; continue }
        set body [string range $src $open_at $close_at]
        # `$::vapi::state_error` is a VARIABLE read, not a call — the
        # error reporters themselves mention it, and counting those
        # would make the check flag its own fix.
        set calls [string map {{$::vapi::} {}} $body]
        # The 40 chars after the close carry the optional error variable.
        set tail [string range $src [expr {$close_at + 1}] [expr {$close_at + 40}]]
        if {[string match "*::vapi::*" $calls]} {
            if {![regexp {^\s*[a-zA-Z_][a-zA-Z0-9_]*} $tail]} {
                lappend out [llength [split [string range $src 0 $hit] \n]]
            }
        }
        set idx [expr {$hit + 1}]
    }
    return $out
}

set silent 0
foreach f [lsort [glob [file join $_dir *.tcl]]] {
    if {[string match "*test.tcl" $f]} continue
    set fh [open $f] ; set fsrc [read $fh] ; close $fh
    foreach line [silent_api_catches $fsrc] {
        incr silent
        puts "      silent API catch at [file tail $f]:$line"
    }
}
ok "no module swallows an API call's error" {$silent == 0}

# ── i18n drift gate: set_msg literals go through ::vi18n::mc ─────────
#
# The count of raw `set_msg "..."` literals had grown TWICE before this
# check existed (122 → ~136 → more). A status line that bypasses
# ::vi18n::mc is untranslatable, and every one added is a new debt the
# de.msg catalog cannot see. The rule: static text goes through
# [::vi18n::mc "..."], interpolated text through [format [mc ...] args].
#
# Exemptions:
#   - `set_msg ""` — clearing the status line is not user-visible text.
#   - lines already routing through mc (obviously).
#   - test files — they stub ::vi18n anyway.
puts "\ni18n drift"

# BOTH literal spellings. The first version looked only for
# `set_msg "`, so `set_msg {still untranslated}` walked straight past a
# check whose entire job is stopping the count from growing again.
set untranslated {}
foreach f [lsort [glob -nocomplain [file join $_dir *.tcl]]] {
    if {[string match "*test.tcl" $f]} { continue }
    set fh [open $f r]; set src [read $fh]; close $fh
    foreach line [split $src \n] {
        if {[regexp {^\s*#} $line]} { continue }
        # …but not the DEFINITION, whose argument list is braced too.
        if {[regexp {proc\s+\S*set_msg} $line]} { continue }
        set hit ""
        if {[string first {set_msg "} $line] >= 0} { set hit dquote }
        if {[string first "set_msg \{" $line] >= 0} { set hit brace }
        if {$hit eq ""} { continue }
        # Clearing the line is not text.
        if {[string first {set_msg ""} $line] >= 0} { continue }
        if {[string first "set_msg \{\}" $line] >= 0} { continue }
        lappend untranslated "[file tail $f]: [string trim $line]"
    }
}
foreach u $untranslated { puts "      untranslated set_msg: $u" }
ok "no raw set_msg literals outside ::vi18n::mc" {[llength $untranslated] == 0}

# ── a 4-byte character Tcl 8.6 would split ──────────────────────────
#
# Tcl 8.6 reads a file in 4 KiB buffers and mis-decodes a 4-byte UTF-8
# character (U+10000 and up) whose lead byte sits at 4093 mod 4096 —
# the first three bytes look like a whole character to it. A string
# literal carrying one is then garbled when the file is sourced, and an
# mc key no longer matches its catalog (ru.msg's 📎 hint, 2026-10-04;
# the catalog writers pad around it, scripts/i18n-unsplit.tcl). A
# hand-written source file has no writer to do that, so: a line moved,
# or a comment line added above it, and the character is off the edge.
set split4 {}
foreach f [lsort [concat [glob -nocomplain [file join $_dir *.tcl]] \
                         [glob -nocomplain [file join $_dir .. *.tcl]]]] {
    set fh [open $f rb]; set bytes [read $fh]; close $fh
    foreach pair [regexp -all -indices -inline {[\xf0-\xf4]} $bytes] {
        set at [lindex $pair 0]
        if {$at % 4096 == 4093} { lappend split4 "[file tail $f] byte $at" }
    }
}
foreach u $split4 { puts "      split 4-byte character: $u" }
ok "no source file puts a 4-byte character where Tcl 8.6 splits it" {[llength $split4] == 0}

# ── the catalog tracks the keys, in both directions ──────────────────
#
# ::vi18n::mc falls back to the literal when a key is absent, so a
# missing entry is INVISIBLE at runtime: English speakers see English
# either way and the German build silently degrades one string at a
# time. Nothing was checking, and the catalog drifted twice inside the
# session that generated it — both times caught by hand.
#
# Orphans matter too: an entry whose key no longer exists is a
# translation someone paid for that reaches nobody, and it hides the
# real coverage number.
# ONE extractor, shared with the generator.
#
# This had its own copy of the regexp scripts/i18n-sync.sh used, and
# both stopped at the first closing brace — so a key containing a brace
# was truncated identically on both sides and the check stayed green
# over a catalog that would not even parse. Sourcing the generator's
# extractor means the gate measures exactly what the generator writes.
source [file join $_dir .. .. scripts i18n-keys.tcl]
set mc_keys {}
foreach f [lsort [glob -nocomplain [file join $_dir *.tcl]]] {
    if {[string match "*test.tcl" $f]} { continue }
    set fh [open $f r]; set src [read $fh]; close $fh
    # The extractor names this per-file entry point `tcl_keys_in`, and
    # it now also reads ::vi18n::label calls (the registered-widget
    # shape) — both are keys the catalog must carry.
    foreach k [tcl_keys_in $src] { lappend mc_keys $k }
}
# AND the engine's keys. en.msg is ONE catalog feeding both surfaces
# (that is the whole English-as-key convention), and the generator fills
# it from `tcl_keys_in` + `rust_keys_in` together. Checking only the Tcl
# half reported every engine-only key — agents_header, (%{route}), the
# failover strings — as an "orphan", which is the opposite of true: they
# are keys the engine asks for and the catalog must carry.
set _rust_files {}
walk_rs [file join $_dir .. .. crates] _rust_files
foreach f $_rust_files {
    set fh [open $f r]; set src [read $fh]; close $fh
    foreach k [rust_keys_in $src] { lappend mc_keys $k }
}
# AND the symbolic keys that reach lookup() through a VARIABLE rather than
# a literal in a call. en.yml is their written list, and the GENERATOR
# reads them from there (i18n-keys.tcl's en_yml_keys) — so this check has
# to read the same source, or it reports the generator's own keys as
# orphans. That is exactly what happened when the extractor's match-arm
# blind spot was fixed: en.msg grew to 976 while this list stayed at 969,
# and the seven keys (tasks_header, subagents_*, shells_*, monitors_*)
# were flagged "an entry that no key uses" when they are in fact entries
# no CALL SITE can see.
foreach k [en_yml_keys [file join $_dir .. ..]] { lappend mc_keys $k }
# AND the engine's help-table descriptions, which reach lookup() through
# a variable too (help_entries_localized). The generator reads them with
# rust_help_descs_in; this gate reads the same, or it would report the
# generator's own keys as orphans.
foreach f $_rust_files {
    if {[file tail $f] ne "commands.rs"} { continue }
    set fh [open $f r]; set src [read $fh]; close $fh
    foreach k [rust_help_descs_in $src] { lappend mc_keys $k }
}
set mc_keys [lsort -unique $mc_keys]

# The catalog is generated with `list` quoting, so read it back AS a
# list. The regexp this replaced could not read an entry whose key
# carried a brace, and reported it as missing.
set cat_keys {}
set _en [file join $_dir .. msgs en.msg]
if {[file exists $_en]} {
    set fh [open $_en r]
    foreach line [split [read $fh] \n] {
        set line [string trim $line]
        if {![string match "::msgcat::mcset en *" $line]} { continue }
        if {[catch { llength $line } n] || $n != 4} { continue }
        lappend cat_keys [lindex $line 2]
    }
    close $fh
}
set cat_keys [lsort -unique $cat_keys]

# The catalog must be SOURCEABLE, not merely present. msgcat runs it as
# Tcl, so a badly quoted entry is not a missing translation — it is
# ::vi18n::init throwing, and main.tcl calls that unguarded at startup.
# That is what a truncated key did, and nothing here noticed.
set _cat_ok 1
if {[file exists $_en]} {
    set _probe [interp create]
    if {[catch { $_probe eval [list source -encoding utf-8 $_en] } _e]} {
        # msgcat may be absent in the probe; only a PARSE failure counts.
        if {[string match "*brace*" $_e] || [string match "*quote*" $_e]} {
            set _cat_ok 0
            puts "      en.msg does not parse: $_e"
        }
    }
    interp delete $_probe
}
ok "en.msg is sourceable Tcl" {$_cat_ok}

set cat_missing {}
foreach k $mc_keys {
    if {[lsearch -exact $cat_keys $k] < 0} { lappend cat_missing $k }
}
set cat_orphan {}
foreach k $cat_keys {
    if {[lsearch -exact $mc_keys $k] < 0} { lappend cat_orphan $k }
}
foreach k $cat_missing { puts "      key with no en.msg entry: $k" }
foreach k $cat_orphan  { puts "      en.msg entry with no key: $k" }
ok "en.msg has an entry for every ::vi18n::mc key" {[llength $cat_missing] == 0}
ok "…and no entry that no key uses" {[llength $cat_orphan] == 0}
ok "the catalog is not empty" {[llength $cat_keys] > 100}


# ── every `exec` with a substituted argument has been reviewed ────────
#
# Tcl's `exec` parses redirection operators out of the ARGUMENT LIST, so
# a word whose value begins with `>` is a redirection and a word that is
# `|` starts a pipeline — no shell required, and it applies to the
# contents of a variable exactly as it does to a literal.
#
# irc.tcl did `exec $notify_path $title $body &` where $title is a
# sender's NICK and $body their MESSAGE TEXT. A body of ">/some/path"
# truncated that file; a nick of "|" (legal, RFC 2812) executed the
# body. Unauthenticated, over a private message. See safeexec.tcl.
#
# The nine other `exec` sites in the desktop were all reviewed at the
# same time, and this check is what makes the TENTH one a build failure
# rather than a thing somebody notices later. It is an allow-list keyed
# on the command text rather than the line number, so moving code around
# does not silently re-approve it — and adding a site means writing down
# why it is safe, right here, which is the actual point.
puts "\nexec with substituted arguments"

# Each entry: a distinctive fragment of the command, and the reason it
# is not remote-controlled. Matched as a substring rather than as the
# whole line, because the line also carries whatever `catch {...} e]} {`
# scaffolding surrounds it and that is not what is being approved.
#
# `::vsafe::spawn` and `::vsafe::arg` call sites need no entry — they
# are the fix, not an exception to it.
array set exec_reviewed {
    {exec which $term}
        "the loop variable, from a literal list of terminal names"
    {exec $tclsh $driver $path}
        "interpreter, driver and buffer path are all local"
    {exec {*}$cmd 2>@1}
        "media converter argv; its IN/OUT words go through ::vsafe::arg"
    {exec $exe {*}$words << "$pass\n" 2>@1}
        "the ticket-signer child (ask.tcl _ticket_sign): $exe is this
         machine's `veles` from auto_execok, $words is a literal
         subcommand (`ticket sign`) plus the request token and the key
         name, both through ::vsafe::arg, and the passphrase travels on
         stdin — never argv, where `ps` would show it. The request
         token is remote data (a peer's ask) and is the reason `arg` is
         there: without it a token starting with `-` would become a
         flag, and one starting with `>` a truncation."
}

proc exec_sites {src} {
    set out {}
    set n 0
    foreach line [split $src \n] {
        incr n
        set t [string trim $line]
        if {[string match "#*" $t]} continue
        # `exec` as a command word, not as part of auto_execok.
        if {![regexp {(^|\[|\{|;|\s)exec\s} $t]} continue
        set at [string first "exec " $t]
        set cmd [string trim [string range $t $at end]]
        # Trim a trailing close brace / bracket so the key is stable
        # when the surrounding `catch {...}` changes shape.
        set cmd [string trimright $cmd "\}\] "]
        # Only substituting words can carry remote data.
        if {![regexp {[\$\[]} $cmd]} continue
        lappend out [list $n $cmd]
    }
    return $out
}

set unreviewed 0
foreach f [lsort [glob [file join $_dir *.tcl]]] {
    if {[string match "*test.tcl" $f]} continue
    if {[file tail $f] eq "safeexec.tcl"} continue
    set fh [open $f] ; set fsrc [read $fh] ; close $fh
    foreach site [exec_sites $fsrc] {
        lassign $site line cmd
        if {[string match "*::vsafe::arg*" $cmd]} continue
        set approved 0
        foreach key [array names exec_reviewed] {
            if {[string first $key $cmd] >= 0} { set approved 1 ; break }
        }
        if {$approved} continue
        incr unreviewed
        puts "      unreviewed exec at [file tail $f]:$line — $cmd"
    }
}
ok "every exec with a substituted argument is reviewed or wrapped" \
    {$unreviewed == 0}

# A reviewed entry that no longer matches any site is stale, and a stale
# allow-list is how an approval outlives the thing it approved.
set stale {}
foreach key [array names exec_reviewed] {
    set found 0
    foreach f [glob [file join $_dir *.tcl]] {
        if {[string match "*test.tcl" $f]} continue
        set fh [open $f] ; set fsrc [read $fh] ; close $fh
        foreach site [exec_sites $fsrc] {
            if {[string first $key [lindex $site 1]] >= 0} { set found 1 ; break }
        }
        if {$found} break
    }
    if {!$found} { lappend stale $key }
}
if {[llength $stale]} { puts "      stale allow-list entries: $stale" }
ok "no stale entry in the exec allow-list" {[llength $stale] == 0}

# ── Tk option names that only fail at runtime ────────────────────────
#
# markdown.tcl configured -rmargin1, which does not exist (only
# -rmargin, plus lmargin1/lmargin2). It threw into a catch and code
# blocks silently rendered unstyled for months.
puts "\nTk text tag options"
text .probe
# option -> a value that is valid FOR THAT OPTION (a pixel count is not
# a colour), so a failure means the option name is wrong, not the value.
foreach {opt val} {
    -rmargin 4 -lmargin1 4 -lmargin2 4 -spacing1 4 -spacing3 4
    -background "#112233" -foreground "#ddeeff"
} {
    ok "$opt is a real text tag option" \
        {![catch {.probe tag configure probe $opt $val}]}
}
foreach opt {-rmargin1 -rmargin2} {
    ok "$opt is correctly rejected by Tk (it does not exist)" \
        {[catch {.probe tag configure probe $opt 4}]}
}
set md [slurp [file join $_dir markdown.tcl]]
# Code lines only — the comment above the fix names the old option on
# purpose, and matching that would make this test permanently red.
# `--` too, or regexp reads the leading dash as a switch.
set bad_rmargin 0
foreach line [split $md \n] {
    if {[regexp {^\s*#} $line]} { continue }
    if {[regexp -- {-rmargin[12]} $line]} { incr bad_rmargin }
}
ok "markdown.tcl uses no -rmargin1/-rmargin2 in code" {$bad_rmargin == 0}

# ── overlays must actually resize ───────────────────────────────────
#
# The skills list was packed `-side left -fill y` with no `-expand`, so
# it kept a listbox's default ~20-character width however big you made
# the window, and skill names were clipped with no way to widen them.
puts "\noverlay sizing"

set ov [slurp [file join $_dir overlays.tcl]]

# Count `wm geometry $w WxH` lines and require a matching floor for each.
set geoms 0
set mins 0
foreach line [split $ov \n] {
    if {[regexp {wm geometry \$w [0-9]+x[0-9]+} $line]} { incr geoms }
    if {[regexp {wm minsize \$w [0-9]+ [0-9]+} $line]} { incr mins }
}
ok "every overlay declares a size" {$geoms > 0}
ok "every overlay declares a minimum size" {$mins == $geoms}

# No list may be packed -fill y without -expand: that is the exact
# incantation that produced the clipping.
set clipped 0
foreach line [split $ov \n] {
    if {[regexp {pack \S*\.list .*-fill y} $line] && ![string match "*-expand*" $line]} {
        incr clipped
        puts "      non-expanding list: [string trim $line]"
    }
}
ok "no list is packed -fill y without -expand" {$clipped == 0}

ok "the skills split is a draggable paned window" \
    {[string match "*ttk::panedwindow \$w.main*" $ov]}

# ── a window may only lose its native title bar if it has its own ───
#
# `wm overrideredirect 1` removes the WM decoration and with it drag,
# minimise, maximise and close. A window without its own strip would
# become unmovable and unclosable.
puts "\nwindow controls"

set tb [slurp [file join $_dir titlebar.tcl]]
# Read here: the palette/overlay sources are loaded later in this file
# for other sections, and this one runs first.
set pal [slurp [file join $_dir palette.tcl]]
set ov  [slurp [file join $_dir overlays.tcl]]
ok "there is a titlebar helper" {[string match "*proc ::vtitle::attach*" $tb]}
ok "it offers move, maximise and minimise" \
    {[string match "*proc ::vtitle::toggle_max*" $tb]
     && [string match "*proc ::vtitle::minimise*" $tb]
     && [string match "*proc ::vtitle::_drag*" $tb]}
# The guard that makes turning decorations off safe.
ok "set_native skips windows without their own strip" \
    {[string match "*winfo exists \[_child \$w vtitlebar\]*" $tb]}
# Root-window child paths are ".foo", not "..foo".
ok "child paths handle the root window" {[string match "*proc ::vtitle::_child*" $tb]}
# Withdrawn windows need a route back, since an undecorated window has
# no taskbar entry.
ok "hidden windows can be restored" {[string match "*proc ::vtitle::restore_all*" $tb]}
ok "and that route is in the palette" \
    {[string match "*vtitle::restore_all*" $pal]
     && [string match "*vtitle::toggle_native*" $pal]}
ok "overlays use the shared strip" \
    {[string match "*::vtitle::attach*" $ov]}

# ── icon-only buttons must carry an accessible name ──────────────────
#
# A button whose whole label is "✕" or "↻" tells a screen reader, and a
# user who does not recognise the glyph, nothing at all. ::va11y::name
# attaches a description AND makes the button reachable by Tab.
puts "\naccessible names"

set unlabeled {}
foreach f {sidebar.tcl findbar.tcl overlays.tcl panes.tcl dock.tcl} {
    set src [slurp [file join $_dir $f]]
    foreach line [split $src \n] {
        # ttk::button <path> -text "<single non-alphanumeric glyph>"
        if {[regexp {ttk::button\s+(\S+)\s+-text\s+"([^"]{1,2})"} $line -> path label]} {
            if {[regexp {^[A-Za-z0-9]} $label]} { continue }
            # Match the FULL widget path. An earlier version compared
            # only the last element, so a button at $w.head.x looked for
            # "x" — which occurs in nearly any file, making the whole
            # check pass vacuously. Verified by deleting a real
            # ::va11y::name line and watching this stay green.
            if {[string first "::va11y::name $path" $src] < 0} {
                lappend unlabeled "$f: $path (\"$label\")"
            }
        }
    }
}
foreach u $unlabeled { puts "      unlabeled icon button: $u" }
ok "every icon-only button has an accessible name" {[llength $unlabeled] == 0}

# ── the four core surfaces carry accessible names ────────────────────
#
# The icon-button check above covers buttons; this covers the WIDGETS a
# screen reader lands on first: the transcript, the composer, the
# palette's entry+list, and every overlay body. A nameless text widget
# is announced as "text" — indistinguishable from any other.
puts "\nsurface names"

# THE TRANSCRIPT IS NO LONGER ONE PATH. Sessions keep their own widget
# and `::vchat::_make_log` builds every one of them, so grepping for
# `.mid.chatwrap.chat.log` would ask whether the FIRST session's
# transcript is named — and answer yes forever while the twelfth is
# nameless. Read the factory's body and ask it of the widget it builds.
proc proc_body {src name} {
    set i [string first "proc $name " $src]
    if {$i < 0} { return "" }
    set rest [string range $src $i end]
    set j [string first "\nproc ::" $rest]
    if {$j < 0} { return $rest }
    return [string range $rest 0 $j]
}
set chat_log_src [proc_body $chat ::vchat::_make_log]
ok "the transcript factory exists" {$chat_log_src ne ""}

set nameless {}
set irc_src [slurp [file join $_dir irc.tcl]]
foreach {what srcname widget} {
    transcript LOG_SRC  {$w}
    composer   CHAT_SRC .mid.chatwrap.chat.input
    preview    CHAT_SRC .mid.chatwrap.chat.preview
    "IRC transcript" IRC_SRC {$R_view.body}
    "IRC composer"   IRC_SRC {$R_view.input}
} {
    set src [switch -- $srcname {
        CHAT_SRC { set _ $chat }
        LOG_SRC  { set _ $chat_log_src }
        default  { set _ $irc_src }
    }]
    if {[string first "::va11y::name $widget" $src] < 0} {
        lappend nameless "$what ([string range $widget 1 end])"
    }
}
foreach n $nameless { puts "      unnamed surface: $n" }
ok "chat and IRC transcripts, composers are named" {[llength $nameless] == 0}

# A named surface is only half of it: the composer band must also be a
# KEEP-OUT zone, or hovering the transcript parks its name on the line you
# are typing on (USER 2026-09-06). Checked in the SOURCE because the
# runtime geometry depends on which widgets a given test happened to
# build — and nothing but a registration check catches the line being
# deleted outright.
#
# The paths are matched INSIDE THE KEEP-OUT BLOCK, not anywhere in the
# file: every one of them also appears in widget-creation code, so a loose
# search passes vacuously. Verified by deleting the registration and
# watching this go red.
set a11y_src [slurp [file join $_dir a11y.tcl]]
set band_zone {input bar chips preview grip}
set _clines [split $chat \n]
set _kwhere -1
for {set i 0} {$i < [llength $_clines]} {incr i} {
    if {[string match "*::va11y::keepout*" [lindex $_clines $i]]} { set _kwhere $i; break }
}
set _window ""
if {$_kwhere >= 0} {
    set _from [expr {max(0, $_kwhere - 6)}]
    set _window [join [lrange $_clines $_from [expr {$_kwhere + 2}]] \n]
}
set missing {}
foreach w $band_zone {
    if {[string first ".mid.chatwrap.chat.$w" $_window] < 0} { lappend missing $w }
}
foreach m $missing { puts "      composer widget outside the keep-out block: $m" }
ok "the composer band is registered as a tooltip keep-out" \
    {$_kwhere >= 0 && [llength $missing] == 0}
ok "…and a11y honours keep-outs" \
    {[string match "*proc ::va11y::keepout*" $a11y_src]
     && [string match "*::va11y::_keepout_rects*" $a11y_src]
     && [string match "*::va11y::_is_zone*" $a11y_src]}

set pal_src [slurp [file join $_dir palette.tcl]]
ok "the palette names its entry and list" \
    {[string match "*va11y::name .palette.entry*" $pal_src]
     && [string match "*va11y::name .palette.list*" $pal_src]}
# Plain substrings: the tab_order call contains [list ...], and inside a
# double-quoted match pattern \[ is eaten by the quote layer, turning the
# rest into a character class that never matches.
ok "the palette pins its tab cycle" \
    {[string first {::va11y::tab_order} $pal_src] >= 0
     && [string first {.palette.entry .palette.list} $pal_src] >= 0}

# Overlay bodies: every `text $w.body` in overlays.tcl carries a name.
# $ov is read later in the window-controls section; read it here too.
set ov [slurp [file join $_dir overlays.tcl]]
set ov_bodies 0
set ov_named 0
foreach line [split $ov \n] {
    if {[regexp {^\s*text \$w\.body } $line]} { incr ov_bodies }
    if {[string first {::va11y::name $w.body} $line] >= 0} { incr ov_named }
}
ok "every overlay text body has an accessible name" {$ov_named >= $ov_bodies && $ov_bodies > 0}

# ── the main surfaces have context menus ────────────────────────────
#
# There was exactly ONE right-click menu in the whole app (the
# transcript's); every other surface did nothing at all.
puts "\ncontext menus"

set sidebar_src [slurp [file join $_dir sidebar.tcl]]
set ctx [slurp [file join $_dir ctxmenu.tcl]]
# panes.tcl is read later for the cycling checks too; read it here since
# this section runs first.
set panes_src [slurp [file join $_dir panes.tcl]]

ok "there is a shared context-menu helper" \
    {[string match "*proc ::vctx::attach*" $ctx]}
# Right-clicking a row must act on THAT row, not on the old selection.
ok "the helper selects the item under the pointer" \
    {[string match "*proc ::vctx::_select_at*" $ctx]}
# Right-click-only is unreachable without a mouse.
ok "the helper binds a keyboard equivalent" \
    {[string match "*<Key-Menu>*" $ctx] && [string match "*<Shift-F10>*" $ctx]}

foreach {what src widget} [list \
        transcript $chat_log_src {$w} \
        composer   $chat        .mid.chatwrap.chat.input \
        sessions   $sidebar_src {$::vsidebar::R.list} \
        files      $panes_src   {$w.tree} \
        review     $panes_src   {$w.files}] {
    ok "$what has a context menu" \
        {[string match "*::vctx::attach $widget*" $src]}
}

# The transcript's hand-rolled menu should be gone, not duplicated.
ok "no hand-rolled tk_popup outside the helper" \
    {![string match "*tk_popup*" $chat] && ![string match "*tk_popup*" $sidebar_src]}

# ── window cycling moves between BUFFERS, not chrome ────────────────
#
# `focus_target .panes` returned `$::vpanes::R.tabs.files` — a tab
# BUTTON — so C-x o and Shift-arrows landed on the tab strip. Emacs
# other-window moves between windows showing buffers.
puts "\nbuffer cycling"

set dock [slurp [file join $_dir dock.tcl]]
set panes_src [slurp [file join $_dir panes.tcl]]

# Isolate focus_target's body and check it names no tab widget.
set ft [string range $dock [string first "proc ::vdock::focus_target" $dock] end]
set ft [string range $ft 0 [string first "\n}" $ft]]
ok "focus_target exists" {[string length $ft] > 0}
ok "focus_target returns no tab-strip widget" {![string match "*tabs.*" $ft]}
ok "focus_target asks panes for its content widget" \
    {[string match "*vpanes::content_widget*" $ft]}

ok "panes expose a content widget" \
    {[string match "*proc ::vpanes::content_widget*" $panes_src]}
ok "dock builds a buffer ring" {[string match "*proc ::vdock::buffer_ring*" $dock]}
ok "C-x o / arrows go through the ring" \
    {[string match "*focus_buffer -1*" $dock] && [string match "*focus_buffer 1*" $dock]}

# ── everything reachable by key is reachable by palette ─────────────
#
# The Dev pane was bound to Alt-5 and listed nowhere, so unless you knew
# the chord it did not exist. The palette is the discovery surface; if a
# pane or overlay is missing from it, it is invisible.
puts "\npalette completeness"

set pal [slurp [file join $_dir palette.tcl]]
set panes_src [slurp [file join $_dir panes.tcl]]

# Every tab ::vpanes::show accepts (from its own foreach of tab names).
set pane_tabs {}
if {[regexp {foreach t \{([a-z ]+)\} \{\s*\n\s*catch \{pack forget} $panes_src -> tabs]} {
    set pane_tabs $tabs
}
ok "found the pane tab list" {[llength $pane_tabs] > 0}
set missing {}
foreach t $pane_tabs {
    if {![string match "*::vpanes::show $t*" $pal]} { lappend missing $t }
}
foreach m $missing { puts "      pane with no palette entry: $m" }
ok "every pane has a palette entry" {[llength $missing] == 0}

# Every overlay ::voverlay::open dispatches to.
set ov_names {}
set body [string range $ov [string first "proc ::voverlay::open" $ov] end]
set body [string range $body 0 [string first "\n}" $body]]
foreach line [split $body \n] {
    if {[regexp {^\s+([a-z]+)\s+\{ _} $line -> nm]} { lappend ov_names $nm }
}
ok "found the overlay list" {[llength $ov_names] > 5}
set missing {}
foreach n $ov_names {
    if {![string match "*::voverlay::open $n*" $pal]} { lappend missing $n }
}
foreach m $missing { puts "      overlay with no palette entry: $m" }
ok "every overlay has a palette entry" {[llength $missing] == 0}

# ── key hints must come from the registry, never be typed out ───────
#
# "why is it the hardest problem in computing to keep the keybinding
# hints and help text consistent when you switch keymaps?"
#
# Because they were typed by hand in three places. chat.tcl carried its
# own `normie ? "Ctrl-K" : "M-x"` ternary, dock.tcl hardcoded "Ctrl-L
# for composer", and help.tcl claimed "M-x opens the command palette in
# every mode" — false the moment normie became pure Ctrl.
#
# ::vkeymap::key reads the live registry for the ACTIVE mode. Anything
# user-visible must go through it.
puts "\nkey hints follow the keymap"

set offenders {}
foreach f {chat.tcl dock.tcl help.tcl statusbar.tcl sidebar.tcl panes.tcl
           overlays.tcl findbar.tcl palette.tcl ask.tcl dev.tcl} {
    set path [file join $_dir $f]
    if {![file exists $path]} { continue }
    set n 0
    set exempt_until -1
    foreach line [split [slurp $path] \n] {
        incr n
        # An explicit `;# keymap-literal:` marker exempts the next few
        # lines. Deliberate, reviewable, and rare — the point is that
        # writing a chord by hand has to be a decision, not a habit.
        if {[string match "*keymap-literal*" $line]} { set exempt_until [expr {$n + 6}] }
        # Comments are documentation, not UI text.
        if {[regexp {^\s*#} $line]} { continue }
        # Only lines that put text on screen.
        if {![regexp {set_msg|insert end|-text |-label } $line]} { continue }
        # A literal chord in user-visible text.
        if {[regexp {(Ctrl-[A-Za-z]|C-[a-z]\y|M-[a-z]\y|Alt-[A-Za-z])} $line -> hit]} {
            # …unless it is derived from the registry on the same line,
            # or the block carries an explicit, justified opt-out.
            if {[string match "*vkeymap::key*" $line]} { continue }
            if {[string match "*keymap-literal*" $line]} { continue }
            if {$exempt_until >= $n} { continue }
            lappend offenders "$f:$n: $hit"
        }
    }
}
foreach o $offenders { puts "      hardcoded key hint: $o" }
ok "no hardcoded key chords in user-visible text" {[llength $offenders] == 0}

# ── one style per keymap mode ───────────────────────────────────────
#
# A shared `_common` table was appended to EVERY mode carrying Alt-x,
# the C-x and M-o emacs prefixes and both Alt- and Shift-arrows, so all
# three modes behaved like a blend of all three. Worse, `_event` maps
# `M-x` and `Alt-x` to the SAME Tk event, so emacs bound <Alt-x> twice
# and the help listed "command palette" twice.
puts "\nkeymap separation"

source [file join $_dir safeexec.tcl]
source [file join $_dir bindings.tcl]
source [file join $_dir keymap.tcl]
::vbindings::init

# A scratch widget to bind against — never `.`, because these tests run
# in the same interpreter as the rest of the suite and a stray binding
# on the toplevel would outlive this block.
catch { destroy .uitest_probe }
frame .uitest_probe

foreach mode {normie emacs vi} {
    set rows [::vbindings::table $mode]
    ok "$mode has bindings" {[llength $rows] > 5}
    set seen [dict create]
    set dupes {}
    foreach row $rows {
        lassign $row keys target action help category
        set ev [::vkeymap::_event $keys]
        set k "$ev/$target"
        if {[dict exists $seen $k]} {
            lappend dupes "$keys and [dict get $seen $k] both -> $ev"
        } else {
            dict set seen $k $keys
        }
    }
    foreach d $dupes { puts "      duplicate: $d" }
    ok "$mode binds no Tk event twice" {[llength $dupes] == 0}
}

# The Dev editor's own chords are not normie's: dev.tcl binds C-x C-s
# and C-x <arrow> on the editor in every mode, so they are the editor's
# truth whatever the keymap.
set leaked {}
foreach row [::vbindings::table normie] {
    lassign $row keys target action help category
    if {$target in {dev repl}} { continue }
    if {[regexp {^(M-|C-x|Alt-x)} $keys]} { lappend leaked $keys }
}
foreach l $leaked { puts "      emacs chord in normie: $l" }
ok "normie has no emacs chords" {[llength $leaked] == 0}

set has_prefix 0
foreach row [::vbindings::table emacs] {
    lassign $row keys target action help category
    if {$keys eq "C-x" || $keys eq "M-o"} { incr has_prefix }
}
ok "emacs keeps its prefixes" {$has_prefix == 2}

foreach {mode want} {normie Ctrl-K emacs M-x vi M-x} {
    set found {}
    foreach row [::vbindings::table $mode] {
        lassign $row keys target action help category
        if {$help eq "command palette"} { lappend found $keys }
    }
    ok "$mode reaches the palette with exactly one key ($want)" \
        {[llength $found] == 1 && [lindex $found 0] eq $want}
}

# ── every row binds a KEY, never a mouse button ─────────────────────
#
# Tk resolves an unqualified single-digit detail to a BUTTON: `bind .
# <Alt-1>` is stored as <Alt-Button-1>. So Alt-1..Alt-5 — the five pane
# hotkeys printed on the tab strip ("1 Files", "2 Review" …), listed in
# the help overlay, in which-key and in the palette — were bound to
# ALT-CLICK and had never once fired from the keyboard. Being on `all`,
# they also meant Alt-clicking anywhere silently switched the pane.
#
# Alt-0 worked, which is exactly why nobody caught it: there is no
# Button-0, so Tk fell back to the key for that one row alone.
#
# This asks TK what it stored rather than checking the string we built,
# because the string looked perfectly reasonable — <Alt-1> is what the
# docs use in prose. Only the canonical form gives it away.
puts "\nevery binding is a key, not a mouse button"

set mouse_rows {}
set bind_errors {}
foreach mode {normie emacs vi} {
    foreach row [::vbindings::table $mode] {
        lassign $row keys target action help category
        set ev [::vkeymap::_event $keys]
        if {[catch { bind .uitest_probe $ev {#} } e]} {
            lappend bind_errors "$mode $keys -> $ev: $e"
            continue
        }
        set canon [lindex [bind .uitest_probe] 0]
        catch { bind .uitest_probe $ev {} }
        if {[string match "*Button*" $canon]} {
            lappend mouse_rows "$mode $keys -> $ev (Tk stored $canon)"
        }
    }
}
foreach m $mouse_rows   { puts "      MOUSE BINDING: $m" }
foreach e $bind_errors  { puts "      UNBINDABLE: $e" }
ok "no registry row binds a mouse button" {[llength $mouse_rows] == 0}
ok "every registry row is bindable at all"  {[llength $bind_errors] == 0}

# The digits specifically, since they are the ones Tk overloads and the
# ones the pane strip advertises.
set digit_rows 0
set digit_keys 0
foreach row [::vbindings::table normie] {
    lassign $row keys target action help category
    if {![regexp {\d$} $keys]} { continue }
    incr digit_rows
    if {[string match "*Key-*" [::vkeymap::_event $keys]]} { incr digit_keys }
}
ok "the pane hotkeys are digits at all" {$digit_rows >= 6}
ok "every digit row is Key-qualified"   {$digit_keys == $digit_rows}

# ── and the hotkeys actually FIRE ───────────────────────────────────
#
# The check above proves the sequence we build is a key event. This one
# drives the whole install path — ::vkeymap::use → _event → bind all —
# and generates real keypresses, because "the string is well-formed" and
# "pressing the key runs the command" are different claims and only the
# second one is what a user experiences.
#
# It also pins the behaviour change: Alt+CLICK must no longer switch
# panes, which it did from anywhere in the app for as long as the
# bindings were on `all`.
puts "\nthe pane hotkeys fire"

namespace eval ::uitest_stub {}
namespace eval ::vstate    { proc put {k v} { return 1 } }
namespace eval ::vstatus   { variable last "" ; proc set_msg {m} { variable last $m } }
# keymap.tcl now routes status lines through ::vi18n (C3). Without this
# stub the ESC-prefix tests threw inside a binding, and Tk's bgerror
# dialog BLOCKED under Xvfb — the suite hung with no output at all.
if {![llength [info commands ::vi18n::mc]]} {
    namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
    namespace eval ::vi18n {
        proc label {w key args} {
            if {![winfo exists $w]} { return 0 }
            catch { $w configure -text [::vi18n::mc $key {*}$args] }
            return 1
        }
        proc aname {w key args} {
            if {![winfo exists $w]} { return 0 }
            catch { ::va11y::name $w [::vi18n::mc $key {*}$args] }
            return 1
        }
    }
}
namespace eval ::vemacs    { proc apply_mode {n} {} }
namespace eval ::vwhichkey { proc arm {p} {} ; proc hide {} {} }
namespace eval ::vpanes {
    variable fired {}
    proc show {tab} { variable fired ; lappend fired $tab }
    proc hide {}    { variable fired ; lappend fired HIDE }
}

# ::vkeymap::use binds composer-local rows on this exact path.
foreach f {.mid .mid.chatwrap .mid.chatwrap.chat} {
    if {![winfo exists $f]} { frame $f }
}
if {![winfo exists .mid.chatwrap.chat.input]} { text .mid.chatwrap.chat.input }
pack .mid.chatwrap.chat.input
update

# ── boot's fallback must not overwrite the saved keymap ─────────────
#
# USER 2026-09-20: "something simply reset my keybindings to normie and
# I never noticed" / "the resets keep happening, presumably during
# leaky testing". boot() calls use("normie") whenever the state read
# fails, and use() used to end in _persist — so a gateway that was busy
# at launch REWROTE the preference. boot arms a one-shot exemption;
# a deliberate switch (the picker, /keymap) still persists. There is
# no ::vapi here at all, so the state read fails exactly the way a
# stalled gateway makes it fail.
puts "\nboot's normie fallback does not persist itself"
set ::uitest_stub::puts_log {}
proc ::vstate::put {k v} {
    lappend ::uitest_stub::puts_log [list $k $v]
    return 1
}
array unset ::env VELES_KEYMAP
::vkeymap::boot
ok "the boot fallback wrote nothing" {[llength $::uitest_stub::puts_log] == 0}
ok "…and the mode still applied"     {$::vkeymap::current eq "normie"}
::vkeymap::use emacs
ok "a deliberate switch persists"    {[lindex $::uitest_stub::puts_log end] eq {keymap emacs}}
# Restore the inert stub for the suites below.
proc ::vstate::put {k v} { return 1 }

set want {files review preview terminal tasks dev HIDE}
foreach mode {normie emacs vi} {
    set ::vpanes::fired {}
    ::vkeymap::use $mode
    focus -force .
    update
    foreach n {1 2 3 4 5 6 0} { event generate . <Alt-Key-$n> -when now }
    update
    if {$::vpanes::fired ne $want} {
        puts "      $mode fired {$::vpanes::fired}, wanted {$want}"
    }
    ok "$mode: Alt-1..6 switch panes and Alt-0 hides" {$::vpanes::fired eq $want}
}

::vkeymap::use normie
set ::vpanes::fired {}
event generate . <Alt-Button-1> -when now
update
if {[llength $::vpanes::fired]} { puts "      Alt+click fired: $::vpanes::fired" }
ok "Alt+click does NOT switch panes" {[llength $::vpanes::fired] == 0}

# ── ESC is Meta ─────────────────────────────────────────────────────
#
# emacs law: `ESC x` is `M-x`, which is how you reach a Meta chord on a
# keyboard where Alt is spoken for. USER (spot test, 2026-08-25):
# "Esc+\d should do the same thing as C-\d or M-\d".
#
# It does M-\d. `ESC 1` is `M-1` — pane 1 of this tab. C-1 is a
# different chord (tab 1) and stays one; collapsing them would make one
# of the two unreachable.
#
# Real events through the real bindings, like the Alt-digit check above:
# the whole point is that the sequence arrives as two keystrokes.
puts "\nESC is the Meta prefix"

::vkeymap::use emacs
focus -force .
update
set ::vpanes::fired {}
foreach n {1 2 3 4 5 6 0} {
    event generate . <Key-Escape> -when now
    event generate . <Key-$n> -when now
}
update
if {$::vpanes::fired ne $want} {
    puts "      ESC-digit fired {$::vpanes::fired}, wanted {$want}"
}
ok "emacs: ESC 1..6 switch panes and ESC 0 hides, exactly as Alt does" \
    {$::vpanes::fired eq $want}

# A digit on its OWN must still be a digit. If arming leaked, every
# keystroke in the app would be interpreted as a Meta chord.
set ::vpanes::fired {}
foreach n {1 2 3} { event generate . <Key-$n> -when now }
update
ok "a bare digit is not a chord" {[llength $::vpanes::fired] == 0}

# ESC ESC cancels, rather than leaving the prefix armed forever.
event generate . <Key-Escape> -when now
ok "ESC arms the prefix" {$::vkeymap::meta_armed == 1}
set ::vstatus::last ""
event generate . <Key-Escape> -when now
ok "ESC ESC cancels it" {$::vkeymap::meta_armed == 0}
# Quietly. Falling through to the normal lookup ALSO leaves the prefix
# disarmed — "M-Escape" is simply unbound — so disarmed alone does not
# distinguish a cancel from a miss. What differs is that a cancel is not
# an error, and does not tell you a key you never pressed is unbound.
ok "…without complaining about an unbound key" \
    {![string match "*not bound*" $::vstatus::last]}
set ::vpanes::fired {}
event generate . <Key-1> -when now
update
ok "…so the next digit is just a digit" {[llength $::vpanes::fired] == 0}

# Reaching for Shift on the way to the key must not eat the prefix.
event generate . <Key-Escape> -when now
event generate . <Key-Shift_L> -when now
ok "a modifier alone does not consume ESC" {$::vkeymap::meta_armed == 1}
set ::vpanes::fired {}
event generate . <Key-2> -when now
update
ok "…and the real key still arrives" {$::vpanes::fired eq {review}}

# In vi, Escape is normal mode. That is the whole point of vi, and it
# must not be quietly turned into a prefix.
::vkeymap::use vi
focus -force .
update
set ::vpanes::fired {}
event generate . <Key-Escape> -when now
event generate . <Key-1> -when now
update
ok "vi: ESC is NOT meta" {[llength $::vpanes::fired] == 0}
ok "…and nothing is left armed" {$::vkeymap::meta_armed == 0}

# normie has no Meta chords to reach, so it gets no ESC prefix either.
::vkeymap::use normie
focus -force .
update
set ::vpanes::fired {}
event generate . <Key-Escape> -when now
event generate . <Key-1> -when now
update
ok "normie: ESC is not a prefix" {$::vkeymap::meta_armed == 0}

# The Dev pane's rows carry an action now (the command tree runs them
# from outside the editor), and their chords are still dev.tcl's to
# bind on its widgets. A global bind of one would turn C-c C-c into
# "source the buffer" in the composer, or worse: _event cannot render
# a two-key chord and binds some unrelated single key instead.
set _global {}
foreach mode {normie emacs vi} {
    ::vkeymap::use $mode
    foreach row [::vbindings::table $mode] {
        lassign $row keys target action help category
        if {$target in {all input} || $action eq ""} { continue }
        foreach seq [bind all] {
            if {[string first [::vbindings::_norm $action] [::vbindings::_norm [bind all $seq]]] >= 0} {
                lappend _global "$mode: $keys bound as $seq"
            }
        }
    }
}
foreach g $_global { puts "      WIDGET-LOCAL ROW BOUND GLOBALLY: $g" }
ok "a widget-local row with an action is bound by no mode" {[llength $_global] == 0}

# Leave no `bind all` behind for whatever runs after this.
::vkeymap::_clear
catch { destroy .mid }
catch { destroy .uitest_probe }

# ── every key hint is the key that actually runs that action ────────
#
# The check that the old hint layer failed. It resolved an action to a
# key by fuzzy-matching HELP PROSE through a hand-maintained keyword
# map, with a second hand-maintained label→keyword map on top of it in
# palette.tcl, and 7 of the 18 palette rows that have a key advertised
# the wrong one — "Pane: Review (git)" told you to press Ctrl-P, which
# opens the composer preview.
#
# So this does not ask "did we look the hint up the same way twice". It
# goes the OTHER WAY: take the hint the palette will display, find what
# that chord is bound to in the registry, and require it to be the row's
# own action. A wrong hint fails no matter how it was derived.
puts "\nkey hints run the action they are printed next to"

source [file join $_dir palette.tcl]

# key display string -> action, straight from the registry.
proc keymap_actions {mode} {
    set map [dict create]
    foreach row [::vbindings::table $mode] {
        lassign $row keys target action help category
        if {$category eq "prefix"} {
            foreach prow [::vbindings::prefix_continuations $keys] {
                lassign $prow k a h
                dict lappend map "$keys $k" $a
            }
        } elseif {$action ne ""} {
            # A list: a widget-local chord means one thing per widget
            # (C-c C-a is code actions in the Dev editor and the app
            # target in its REPL).
            dict lappend map $keys $action
        }
    }
    return $map
}

set liars {}
set hinted 0
foreach mode {normie emacs vi} {
    set map [keymap_actions $mode]
    foreach item [::vpalette::actions] {
        lassign $item label action
        set hint [::vbindings::keys_for $action $mode]
        if {$hint eq ""} { continue }
        incr hinted
        if {![dict exists $map $hint]} {
            lappend liars "$mode: \"$label\" shows $hint, which $mode does not bind"
        } elseif {[::vbindings::_norm $action] ni [lmap a [dict get $map $hint] {::vbindings::_norm $a}]} {
            lappend liars "$mode: \"$label\" shows $hint, but $hint runs {[dict get $map $hint]}"
        }
    }
}
foreach l $liars { puts "      WRONG HINT: $l" }
ok "the palette hints something at all"        {$hinted > 30}
ok "every palette hint runs that palette row"  {[llength $liars] == 0}

# A compound action must not inherit its first command's key. "Dev: new
# buffer" runs {::vpanes::show dev ; ::vdev::buf_new}; a prefix match
# would promise Alt-5 does it, when Alt-5 only does the first half.
ok "a compound action inherits no key" \
    {[::vbindings::keys_for {::vpanes::show dev ; ::vdev::buf_new} normie] eq ""}
ok "its first command still resolves" \
    {[::vbindings::keys_for {::vpanes::show dev} normie] eq "Alt-6"}

# normie deliberately has no C-x and no M-o, so it must never be told
# about a chord it does not answer to. The old lookup walked the prefix
# tables directly and offered "M-o m" to every mode.
# The Dev editor's own C-x chords are its widget's, in every mode.
set _editor_keys {}
foreach row [::vbindings::_dev_pane] { lappend _editor_keys [lindex $row 0] }
set chords {}
foreach item [::vpalette::actions] {
    lassign $item label action
    set hint [::vbindings::keys_for $action normie]
    if {$hint in $_editor_keys} { continue }
    if {[regexp {^(C-x|M-o) } $hint]} { lappend chords "$label -> $hint" }
}
foreach c $chords { puts "      normie offered a chord it lacks: $c" }
ok "normie is offered no prefix chords" {[llength $chords] == 0}

# The rows that were wrong, pinned by name so a regression says which.
#
# The last three used to pin the GAP: "Gateway status" and "Models &
# settings" had no key in normie at all, which was the complaint
# ("a lot of stuff doesn't even seem to have a keyboard shortcut because
# it was never added"). They have flat ones now.
#
# "Models & settings" in emacs is the interesting row: it must stay
# M-o m. The flat chords fill GAPS per mode (::vbindings::_add_missing),
# because a non-prefix row wins the keys_for lookup and would otherwise
# start advertising C-S-M to someone whose fingers know M-o m.
foreach {label mode want} {
    "Pane: Review (git)"       normie Alt-2
    "Pane: Preview"            normie Alt-3
    "Pane: Tasks (todo + plan)" normie Alt-5
    "Pane: Dev (REPL + LSP)"   normie Alt-6
    "Hide right panes"         normie Alt-0
    "Composer preview"         normie Ctrl-P
    "Gateway status"           normie F3
    "Gateway status"           emacs  F3
    "Models & settings"        normie C-S-M
    "Models & settings"        emacs  {M-o m}
    "Hooks (what plugins run on)" normie C-S-H
    "Hooks (what plugins run on)" emacs  C-S-H
} {
    set action ""
    foreach item [::vpalette::actions] {
        if {[lindex $item 0] eq $label} { set action [lindex $item 1] ; break }
    }
    set got [::vbindings::keys_for $action $mode]
    if {$got ne $want} { puts "      $mode \"$label\": got {$got}, want {$want}" }
    ok "$mode: \"$label\" -> [expr {$want eq "" ? "no key" : $want}]" {$got eq $want}
}

# ── bindings.tcl is the single source of truth ───────────────────────
puts "\nbinding registry"
set keymap [slurp [file join $_dir keymap.tcl]]
set whichkey [slurp [file join $_dir whichkey.tcl]]
ok "keymap reads the registry"   {[string match "*vbindings*" $keymap]}
ok "whichkey reads the registry" {[string match "*vbindings*" $whichkey]}

# ── popup placement goes through ONE helper ─────────────────────────
#
# Two different jobs, and getting them the wrong way round is what put
# M-x off-screen: the palette is the app's command list and must appear
# in the SAME place every time, while which-key annotates the chord you
# just pressed and belongs next to it. Both clamp via lib/place.tcl.
# The arithmetic itself is tested in listtest.tcl.
puts "\npopup placement"
set palette [slurp [file join $_dir palette.tcl]]
ok "palette is centred, NOT anchored on focus" \
    {[string match "*vplace::centered*" $palette]
     && ![string match "*focus -displayof*\n*wm geometry .palette*" $palette]}
ok "whichkey is anchored near the focus" {[string match "*vplace::near*" $whichkey]}
ok "neither popup computes its own geometry" \
    {![regexp {wm geometry \S*palette} $palette]
     && ![regexp {wm geometry \$w "\+} $whichkey]}

# ── no transcript click handler may append to the transcript ────────
#
# USER: "Clicking on a guard seems to dump new text in the transcript
# window at the bottom instead of expanding like reasoning, ideally it
# would just do that." `_note_expand` ended in `emit_message system`, so
# every click appended a whole new message at the end — the one click
# target in the app that did. Reasoning and tool rounds flip `-elide` in
# place. This walks chat.tcl's `tag bind … <Button-1>` handlers (the
# things you click inside the transcript) and asserts none of their
# target procs reaches `emit_message` or `log_insert`. A handler that
# appends fails here exactly the way `_note_expand` did.
#
# (`bind … <Button-1>` on the resize grip is a plain widget bind, has no
# `tag`, and is not one of these — the `tag bind` prefix is the discriminator.)
puts "\nno click appends"

set click_targets {}
foreach line [split $chat \n] {
    # A click is bound directly or through ::vtext::click (which waits for
    # a tap's release on a phone).
    if {![regexp {(?:tag bind .*<Button-1>|::vtext::click ).*?(::vchat::[a-zA-Z0-9_]+)} $line -> pn]} {
        continue
    }
    if {[lsearch -exact $click_targets $pn] < 0} { lappend click_targets $pn }
}
ok "transcript click handlers found" {[llength $click_targets] > 0}

set click_offenders {}
foreach p $click_targets {
    # The proc body, from its definition to the next top-level `proc `.
    set d [string first "proc ${p} " $chat]
    if {$d < 0} { continue }
    set rest [string range $chat $d end]
    set e [string first "\nproc " [string range $rest 1 end]]
    set body [expr {$e < 0 ? $rest : [string range $rest 0 $e]}]
    if {[regexp {emit_message|log_insert} $body]} {
        lappend click_offenders $p
    }
}
if {[llength $click_offenders]} { puts "      appenders: $click_offenders" }
ok "no transcript click handler appends to the log" {[llength $click_offenders] == 0}

# ── the follow bracket is a chokepoint (R6) ─────────────────────────
#
# The follow bug class is always the same disease: an insert that
# escapes the prestash→insert→follow bracket. _live_break used to
# prestash and insert its own "\n"; the caller's prestash then
# OVERWROTE the good stash with a post-insert read, and one unbracketed
# newline disarmed follow for the rest of the turn (USER: "follow stops
# on a tool call when /reasoning is on"). The transcript spec pins the
# BEHAVIOUR (R6 cases); this scan pins the MECHANICS, so a new painter
# cannot reintroduce the class silently:
#   1. _live_break is PURE — no insert, no prestash, no follow in its
#      body; it may only compute and return the separator.
#   2. _at_bottom_prestash / _maybe_follow are called from exactly the
#      SIX procs that own live inserts: log_insert, on_reasoning,
#      emit_message, emit_note, on_delta, _flush_live_delta. Anything
#      else wanting to write to the live log goes through log_insert and
#      inherits the bracket.
puts "\nfollow bracket chokepoints"

# Proc body extractor: definition line through just before the next
# top-level `proc` (same shape the click-handler scan above uses).
proc proc_body {src name} {
    set d [string first "proc ::vchat::${name} " $src]
    if {$d < 0} { return "" }
    set rest [string range $src $d end]
    set e [string first "\nproc " [string range $rest 1 end]]
    return [expr {$e < 0 ? $rest : [string range $rest 0 $e]}]
}

set lb [proc_body $chat _live_break]
ok "_live_break exists" {[string length $lb] > 0}
set impure {}
foreach pat {_run {^\s*\$w insert} _writable _at_bottom_prestash _maybe_follow log_insert} {
    foreach line [split $lb \n] {
        if {[string match "*$pat*" $line] && ![string match "*#*" [string range $line 0 [string first $pat $line]]]} {
            lappend impure $pat
            break
        }
    }
}
if {[llength $impure]} { puts "      impure: $impure" }
ok "_live_break is pure — computes the separator, never writes" \
    {[llength $impure] == 0}

# Which proc is each prestash/follow call site in?
proc enclosing_proc {src line_no} {
    set p ""
    set i 0
    foreach line [split $src \n] {
        incr i
        if {[regexp {^proc ::vchat::([A-Za-z0-9_]+)} $line -> name]} { set p $name }
        if {$i == $line_no} { return $p }
    }
    return $p
}
# _flush_live_reasoning joined this list when live reasoning stopped
# painting per token: it is the reasoning half of what _flush_live_delta
# already is for the answer — a paced insert chokepoint that owns the
# prestash/follow bracket for the text it paints. on_reasoning keeps its
# place because it still brackets the one-off SEGMENT HEADER insert.
# on_tool joined when the tool END arm started painting the expandable
# result live (the `_collapsible`/`_render_patch_result` half of a round,
# replacing the old `_tool_preview` stub): that insert is a real live
# paint, so it owns the same bracket the other five do.
set allowed {log_insert on_reasoning emit_message emit_note on_delta \
    _flush_live_delta _flush_live_reasoning on_tool}
set stray {}
set i 0
foreach line [split $chat \n] {
    incr i
    if {![regexp {_at_bottom_prestash|_maybe_follow} $line]} { continue }
    if {[string match "proc ::vchat::*" $line]} { continue }
    if {[regexp {^\s*#|;#} $line]} { continue }
    set p [enclosing_proc $chat $i]
    if {$p eq "" || [lsearch -exact $allowed $p] < 0} {
        lappend stray "$i ($p): [string trim $line]"
    }
}
if {[llength $stray]} { puts "      stray bracket calls:" ; foreach s $stray { puts "        $s" } }
ok "prestash/follow are called only from the four insert chokepoints" \
    {[llength $stray] == 0}

puts "\nuitest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
