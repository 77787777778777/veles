# scripts/i18n-keys.tcl — collect every translatable key in the tree.
#
# args: <repo root>  → lines of "<source>:<key>" on stdout, or with
#                     --keys-only, deduped keys alone.
#
# THE KEY IS A BALANCED BRACE GROUP, not everything up to the next
# closing brace.
#
# This lived inline in i18n-sync.sh as a regexp whose key capture was
# "one or more non-closing-brace characters". That stops at the first
# closing brace, so a key CONTAINING a brace came out truncated.
# acl.tcl has one — it wraps a literal bracket in braces to get it past
# msgcat's format pass — and the extractor turned that key into a
# two-character fragment ending in an unmatched OPEN brace. The fragment
# was written into en.msg still wearing it, msgcat threw
# "missing close-brace" while sourcing the catalog, and ::vi18n::init
# (called unguarded from main.tcl) carried that out of app startup.
#
# Counting depth is the only way to read Tcl's own quoting correctly.
#
# It is a FILE rather than a heredoc because it is dense with escaped
# braces, and the unquoted heredoc it used to live in mangled those
# before tclsh ever saw them.
#
# NOTE, the hard way: Tcl counts braces inside COMMENTS too, which is
# what "possible unbalanced brace in comment" means. Every brace in the
# prose above is therefore spelled out in words.
#
# CALL SHAPES COVERED (the wrap contract, enforced by i18ntest.tcl):
#
#   Desktop (Tcl, lib/*.tcl, non-test):
#     ::vi18n::mc {Key}
#     format [::vi18n::mc {Key}] args
#     ::vi18n::label $w {Key}            (registered, re-texts on swap)
#     ::vi18n::aname $w {Key}
#     a foreach DATA TABLE               (2026-09-26, below)
#     ::vbindings registry help column   (2026-09-26, below)
#     ::vpalette::actions labels         (2026-09-26, below)
#
#   Engine (Rust, crates/**/*.rs):
#     i18n::lookup("Key", …)             (direct)
#     i18n!("Key", name = val)           (macro)
#
# Every call whose key is a brace group in Tcl and a string literal in
# Rust is extracted; the Rust side additionally reports a symbolic key
# (an identifier with no spaces) verbatim so en.yml's older keys keep
# resolving.
#
# ── WHY THE THREE 2026-09-26 SHAPES ARE HERE ────────────────────────
#
# USER report, five findings that were one cause: the steer button, the
# pane names, "many system messages", the right-panel tooltips, and the
# help text. Every one of those sites is either wrapped already or is
# not a call site — and the extractor could see none of them, because
# its rule was "a key spelled as a literal brace group AT one of three
# call shapes". A key that travels any other way never entered en.msg,
# so no translator was ever asked for it, and ::vi18n::mc returned the
# English text it was handed. The gap is invisible from the call site,
# and i18ntest's "nothing left unwrapped" scan passes on all of it
# because that scan looks for the CALL.
#
# scripts/i18n-audit.tcl is the companion: it enumerates the visible
# call sites and reports every literal that is NOT a key in en.msg, so
# this list can be re-derived instead of remembered.

# SOURCED or RUN?
#
# This file is both: the generator runs it as a script (`tclsh
# i18n-keys.tcl <root> --keys-only`), and uitest.tcl SOURCES it to reuse
# `tcl_keys_in` — so the gate measures exactly what the generator writes
# instead of a second copy of the walk that can drift. A sourced file
# must define the procs and do nothing else: `$argv` is not even set in
# that case, and an unguarded `lindex $argv 0` would either error or,
# worse, pick up the host script's argv.
proc i18n_keys_main {argv} {
    set root [file normalize [lindex $argv 0]]
    set keys_only 0
    set engine_keys 0
    foreach a $argv {
        if {$a eq "--keys-only"} { set keys_only 1 }
        if {$a eq "--engine-keys"} { set engine_keys 1 }
    }

    # ── ENGINE KEY SET ─────────────────────────────────────────────
    # The keys the Rust side can hand to lookup(), for trimming a
    # translated yml down to what the engine can actually read.
    #
    # WHY THE TRIM IS WORTH DOING: each translated catalog carried ~930
    # entries while the engine reaches ~150; at 47 locales that is tens
    # of thousands of pairs no code can read. On disk: 3.3 MB -> 448 KB
    # (42022 -> 6144 entries). The catalogs are runtime files now
    # (crates/veles-core/src/i18n.rs); the trim was first made when
    # rust-i18n compiled them into the binary.
    #
    # What the compiled-in catalogs cost, settled at last on 2026-09-28
    # with each compile measured ALONE by a RUSTC_WRAPPER under GNU time
    # (after two wrong published figures, ~1.3 GB and ~1-6 MB): +0.34 GB
    # on veles-core's release library, +0.60 GB on its test binary. That
    # is why they moved to runtime files. The trim stands on its own
    # terms: 7x smaller, no dead entries, no translation lost.
    #
    # WHAT IS NOT TRIMMED: the desktop's .msg catalogs are untouched —
    # the desktop reads .msg and never .yml, and its completeness
    # contract (i18ntest.tcl) is unchanged. That is the whole reason
    # this trim is safe: the two catalogs share a key space but not a
    # reader.
    if {$engine_keys} {
        set rs {}
        walk_rs [file join $root crates] rs
        foreach f $rs {
            set fh [open $f r] ; set src [read $fh] ; close $fh
            foreach k [rust_literals_in $src] { puts $k }
        }
        return
    }

    # Desktop modules.
    set lib [file join $root veles-desktop lib]
    set tcl_files [files_under $lib *.tcl *test.tcl]
    set i18n_self [file join $lib i18n.tcl]
    foreach f $tcl_files {
        if {$f eq $i18n_self} { continue }
        set fh [open $f r] ; set src [read $fh] ; close $fh
        foreach k [tcl_keys_in $src] {
            if {$keys_only} { puts $k } else { puts "[file tail $f]:$k" }
        }
    }

    # Engine sources.
    set rs {}
    walk_rs [file join $root crates] rs
    foreach f $rs {
        set fh [open $f r] ; set src [read $fh] ; close $fh
        foreach k [rust_keys_in $src] {
            if {$keys_only} { puts $k } else { puts "[file tail $f]:$k" }
        }
    }

    # The engine's HELP TABLE. `help_entries_localized` hands each row's
    # description to lookup() through a variable, so walking call sites
    # never found them — and 76 descriptions sat in NO catalog while the
    # coverage note said "help_entries resolved at every render site".
    # The lookups were real; there was nothing for them to find. Every
    # locale rendered /help, the REPL's Tab completion, the TUI palette,
    # Telegram's `/` menu and the desktop's command table in English.
    foreach f $rs {
        if {[file tail $f] ne "commands.rs"} { continue }
        set fh [open $f r] ; set src [read $fh] ; close $fh
        foreach k [rust_help_descs_in $src] {
            if {$keys_only} { puts $k } else { puts "commands.rs(help):$k" }
        }
    }

    # The symbolic keys — the ones that reach lookup() through a
    # VARIABLE and so cannot be found by walking call sites. en.yml is
    # their written list (rust_keys_in's header has the whole story:
    # these seven were missing from keys.txt, so /tasks, /subagents,
    # /shells and /monitors stayed English in all 47 locales).
    foreach k [en_yml_keys $root] {
        if {$keys_only} { puts $k } else { puts "en.yml:$k" }
    }
}

# ─ Tcl keys: balanced brace walk ───────────────────────────────────
proc tcl_keys_in {src} {
    # Comments first. Tcl counts braces inside comments too, and a
    # comment that mentions a call shape would otherwise inject a key
    # that no locale should have to translate. A line whose first
    # non-blank character is a hash is a comment line; the extractor
    # drops those before walking.
    set keep {}
    foreach line [split $src \n] {
        if {[string match "#*" [string trimleft $line]]} { continue }
        lappend keep $line
    }
    set src [join $keep \n]

    # The open-brace character, built at RUNTIME.
    #
    # Spelling it in the source as an escaped brace inside a quoted
    # regexp does not work: the proc body is a brace group, and Tcl
    # counts braces in it before any substitution happens — a backslash
    # followed by an escaped backslash leaves the brace unescaped, so
    # the counter sees an opener that never closes and the whole FILE
    # fails to parse ("missing close-brace: possible unbalanced brace in
    # comment"). This file's own convention warns about exactly this.
    set OB [format %c 123]

    set out {}
    set i 0
    while {1} {
        # The nearest call-of-interest at or after $i, in every shape.
        # A single line can carry two keys (irc.tcl's ternary is the
        # live case), so this scans the whole file rather than per line.
        set hit -1
        foreach pat [list "vi18n::mc\\s*\\$OB" "vi18n::label\\s+\\S+\\s*\\$OB" \
                          "vi18n::aname\\s+\\S+\\s*\\$OB"] {
            set at [regexp -indices -start $i -inline $pat $src]
            if {$at eq ""} { continue }
            # The opening brace is the last char of the match.
            set start [lindex [lindex $at 0] 1]
            if {$hit < 0 || $start < $hit} { set hit $start }
        }
        if {$hit < 0} { break }
        # `proc ::vi18n::mc {s args}` is the DEFINITION, not a call, and its
        # parameter list has the same shape as a key: the extractor
        # reported `s args` as a translatable string with no catalog
        # entry. Comments are the same trap — this file's own header
        # documents the call as `# Usage in widgets:  ::vi18n::mc {Key}`,
        # and a comment is not a call site. Both are skipped by looking
        # at the line the brace sits on: a `proc` header ends with the
        # brace, a comment starts with `#`.
        set lstart [expr {[string last "\n" [string range $src 0 $hit]] + 1}]
        # $hit IS the opening brace, so the prefix must stop before it:
        # including it makes the header end in an open brace and every
        # $-anchored test miss.
        set lpre [string range $src $lstart [expr {$hit - 1}]]
        if {[string trimleft $lpre] eq "" || [regexp {^\s*#} $lpre]} {
            set i [expr {$hit + 1}]
            continue
        }
        if {[regexp {proc\s+\S*vi18n::(mc|label|aname)\s*$} $lpre]} {
            set i [expr {$hit + 1}]
            continue
        }
        # $hit indexes the opening brace; walk to its partner.
        set depth 0
        set close -1
        set len [string length $src]
        for {set j $hit} {$j < $len} {incr j} {
            set c [string index $src $j]
            if {$c eq "\\"} { incr j ; continue }
            if {$c eq "\{"} { incr depth ; continue }
            if {$c eq "\}"} {
                incr depth -1
                if {$depth == 0} { set close $j ; break }
            }
        }
        if {$close < 0} { set i [expr {$hit + 1}] ; continue }
        lappend out [string range $src [expr {$hit + 1}] [expr {$close - 1}]]
        set i [expr {$close + 1}]
    }

    # ── the three shapes a key reaches a widget by DATA ─────────────
    #
    # See the header note. Each of these is a place where a visible
    # string lives one hop from the call site, so the brace-at-mc walk
    # above cannot see it — and i18ntest's unwrapped scan cannot either,
    # because there is no unwrapped CALL to find.
    foreach k [tcl_table_keys $src]   { lappend out $k }
    foreach k [tcl_registry_help $src] { lappend out $k }
    foreach k [tcl_label_list $src]   { lappend out $k }
    foreach k [tcl_desktop_cmd_keys $src] { lappend out $k }
    foreach k [tcl_ctxmenu_keys $src] { lappend out $k }
    return $out
}

# ── a foreach DATA TABLE ────────────────────────────────────────────
#
#     foreach {tab label} {files "Files" review "Review" preview "Preview"} {
#         ttk::button ... -text [::vi18n::mc $label]
#
# The key is the CELL, not the call: `-text $label` has no brace to
# match, and the cells are data. panes.tcl's six tab names and its six
# right-panel tooltips are both this shape.
#
# A data list is distinguished from a SCRIPT by what it contains: a
# script references variables (`$row`) or substitutes commands
# (`[expr]`). A table is a flat run of literal words. Measured across
# the tree, that test separates `{files "Files" review "Review"}` from
# `[::vbindings::table $mode]` and `{ lappend mods Control }` with no
# false positives either way.
#
# Only the two PROSE spellings of a cell are keys, never bare words. A
# bare word in a data list is a token — `files`, `en`, `model`, `ask` —
# and extracting those measured 2754 hits across the tree, nearly all
# identifiers, which is the same usable-signal collapse the audit's
# whole-literal version had.
#
# FOUND BY POSITION, not by parsing Tcl: a data list is the brace group
# immediately after a group that a `foreach` keyword introduces. That is
# one pass over the file's balanced groups, and it needs no notion of
# statements or lines.
#
# EVERY close is examined, not only the outermost ones. The first
# version tested `depth == 0` alone — which is true for a whole `proc`
# body and never for a table inside one, so it found nothing in any real
# module while passing on a bare `foreach` at top level. A table only
# ever appears nested, so a top-level-only rule is a rule that can never
# fire.
proc tcl_table_keys {src} {
    set out {}
    set depth 0
    set start -1
    set len [string length $src]
    # Stack of open-group start offsets: a group's text runs from its own
    # start to its close, whatever the nesting.
    set starts {}
    for {set i 0} {$i < $len} {incr i} {
        set ch [string index $src $i]
        if {$ch eq "\\"} { incr i ; continue }
        if {$ch eq "\{"} {
            incr depth
            lappend starts [expr {$i + 1}]
            continue
        }
        if {$ch ne "\}"} { continue }
        incr depth -1
        if {$depth < 0} { break }
        set start [lindex $starts end]
        set starts [lrange $starts 0 end-1]
        set gtext [string range $src $start [expr {$i - 1}]]
        # Is THIS group a foreach data list? Then the text just before it
        # ends with the foreach's variable-list group — `foreach {a b} `
        # — or with a bare `foreach ` for the single-variable form.
        set before [string range $src 0 [expr {$start - 2}]]
        if {[regexp {foreach\s*\{[^{}]*\}\s*$} $before]
            || [regexp {foreach\s*$} $before]} {
            foreach cell [flat_prose_cells $gtext] { lappend out $cell }
        }
    }
    return $out
}

# The prose cells of a data list, or {} when it is really a script.
#
# THREE tests, and each one is a false positive that got in:
#   * `[` / `$`  — the data references a command or a variable, so it is
#                  a script (`[::vbindings::table $mode]`).
#   * `;`       — a statement separator. emacs.tcl's install_readonly
#                  table is `foreach {seq body} [list <Control-a> {%W mark
#                  set insert {insert linestart}; %W see insert} …]`, and
#                  without this test the NESTED `{insert linestart}`
#                  passed every prose rule and became a catalog key.
proc flat_prose_cells {data} {
    if {[string first "\[" $data] >= 0} { return {} }
    if {[string first "\$" $data] >= 0} { return {} }
    if {[string first ";" $data] >= 0} { return {} }
    set out {}
    foreach w [regexp -all -inline {"[^"]*"} $data] {
        set t [string range $w 1 end-1]
        if {[regexp {[A-Za-z]{2}} $t]} { lappend out $t }
    }
    foreach g [regexp -all -inline {\{[^{}\n]*\}} $data] {
        # `regexp` returns the match INCLUDING its delimiters, so the
        # braces come off here. Leaving them on made the key
        # `{browse the repo's files}` — a string no catalog row and no
        # call site uses, so the key would sit in en.msg matching nothing
        # and the real gap would still read as covered.
        set t [string range $g 1 end-1]
        set t [string trim $t]
        # PROSE, not Tcl. A cell that is a SCRIPT shares every surface
        # feature of a label: several lowercase words, no variable, no
        # bracket. `%W mark set insert {insert linestart}` and `lappend
        # mods Control` are commands; "browse the repo's files" is a
        # sentence. What separates them is decidable — a Tcl fragment
        # opens with a Tk path substitution (%W), an option (-foreground),
        # a statement separator, or a known subcommand verb. Measured:
        # this rule keeps emacs.tcl's twelve readline bodies and
        # keymap.tcl's modifier loop out of the key set.
        if {[regexp {^%[A-Z]|^[-;]|;\s|^\s*(set|append|lappend|return|bind|puts|if|foreach|proc|expr|string|list|catch|upvar|global|incr|array|dict|mark|see)\s} $t]} {
            continue
        }
        if {[regexp {[a-z]{3}} $t] && [regexp {\s} $t] \
            && ![regexp {[\[\]$;%+:@]} $t]} {
            lappend out $t
        }
        # A SINGLE-WORD cell is a label too, and leaving them out is how
        # the Instances control strip shipped English in 46 locales:
        # `Attach`, `Detach`, `Start`, `Stop`, `Restart`, `Remove`, `Add…`
        # failed the whitespace test above, so only `Reload plugins` and
        # friends became keys — and a strip that is half translated reads
        # as a missing TRANSLATION rather than a missing KEY.
        #
        # The risk of accepting one word is a one-word Tcl fragment. What
        # separates a label from an identifier is decidable, and this is
        # the narrowest cut that admits the labels: a capital first letter
        # (identifiers here are lowercase), letters only, and an optional
        # trailing ellipsis, which is what the two Add…/Edit… cells carry.
        # Measured against every two-column table in the shipping tree:
        # it admits exactly the labels and no cell that is a path
        # (`patches.tcl`), a variable (`provider_name`) or a command name
        # (`schedule`, `network pattern` — those start lowercase).
        if {[regexp {^[A-Z][A-Za-z]*…?$} $t]} {
            lappend out $t
        }
    }
    return $out
}

# ── a context menu's entries ────────────────────────────────────────
#
#     ::vctx::attach $w {
#         {"Copy message"  {::vchat::copy_message_at $::vctx::x $::vctx::y}}
#         {-}
#         {"Find in transcript…"  {::vfind::show}}
#     }
#
# ::vctx::_rebuild hands each entry's label to mc() when the menu opens.
# Until 2026-10-01 it set the English label as it stood, and no shape
# here could see the table either, so every long-press menu on the
# phone was English in all 46 locales: the touch help could only name
# its "Copy" because the selection's own entry was a call site.
#
# The table is the brace group after `vctx::attach <path>`; a row is a
# line that opens with a quoted label. The separator row `{-}` has no
# quote and drops out by itself.
proc tcl_ctxmenu_keys {src} {
    set OB [format %c 123]
    set out {}
    set i 0
    while {[regexp -indices -start $i "vctx::attach\\s+\\S+\\s*\\$OB" $src at]} {
        set hit [lindex $at 1]
        set depth 0
        set close -1
        set len [string length $src]
        for {set j $hit} {$j < $len} {incr j} {
            set c [string index $src $j]
            if {$c eq "\\"} { incr j ; continue }
            if {$c eq "\{"} { incr depth ; continue }
            if {$c eq "\}"} {
                incr depth -1
                if {$depth == 0} { set close $j ; break }
            }
        }
        if {$close < 0} { break }
        foreach line [split [string range $src $hit $close] \n] {
            if {[regexp {^\s*\{?\s*\{"([^"]*)"} $line -> label]
                    && [regexp {[A-Za-z]{2}} $label]} {
                lappend out $label
            }
        }
        set i [expr {$close + 1}]
    }
    return $out
}

# ── the desktop-local command table ─────────────────────────────────
#
#     variable desktop_local {
#         {/help {} {this list}}
#         {/theme {name} {switch or list themes}}
#     }
#
# Row shape is `{/name {args} {description}}` — the command name, its
# argument spec, then the description. The description is a KEY;
# ::vcmds::help_text resolves it through mc() and the engine's own rows
# pass through unchanged (they arrive already localized).
#
# The trailing-group rule: the description is the LAST brace group on
# the row, whatever the earlier columns are.
#
# Counting groups to a minimum does not work: `{/theme {name} {switch
# or list themes}}` matches only TWO groups, because the open group is
# the COMMAND NAME (`{/theme …}`) — it contains a `/`, so it is not
# `[^{}]*`-clean… and more importantly the command-name group is the one
# that wraps the whole row, so a "third group" test refuses every row of
# this table. Taking the last group instead is positional, exact, and
# needs no count.
proc tcl_desktop_cmd_keys {src} {
    set out {}
    foreach line [split $src \n] {
        if {[string match "#*" [string trimleft $line]]} { continue }
        if {![regexp {^\s*\{/[a-z]+} $line]} { continue }
        set groups [regexp -all -inline {\{[^{}]*\}} $line]
        if {![llength $groups]} { continue }
        set last [lindex $groups end]
        set t [string range $last 1 end-1]
        if {[regexp {[A-Za-z]{2}} $t]} { lappend out $t }
    }
    return $out
}

# ── the ::vbindings registry's help column ──────────────────────────
#
#     {F1  all {::vhelp::open}  "help (this screen)"  general}
#
# One string drives three surfaces: the F1 help overlay, the which-key
# popup's description column, and the palette's hint column. It is data
# in a table, so no call-site walk reaches it — and until 2026-09-26 the
# help screen was English in every locale.
#
# The rule is "the LAST quoted string on a row that is brace-delimited
# and ends in a category", not a positional regexp. Measured: 113 of the
# 115 row lines carry exactly one quoted string and 2 carry two (a quoted
# action column plus the help); the last is the help in both.
proc tcl_registry_help {src} {
    set out {}
    foreach line [split $src \n] {
        if {[string match "#*" [string trimleft $line]]} { continue }
        if {![regexp {^\s+\{.*\}\s*$} $line]} { continue }
        if {![regexp {[a-z_]+\s*\}?\s*$} $line]} { continue }
        # A row that OPENS with a quoted string is a label list's
        # (::vpalette::actions, tcl_label_list below), not the registry,
        # whose rows open with a key. The palette's rows end in a
        # category word too since 2026-10-01, and this rule took
        # "/agents" out of {::vchat::slash_command "/agents"} as a key.
        if {[regexp {^\s*\{"} $line]} { continue }
        set qs [regexp -all -inline {"[^"]*"} $line]
        if {![llength $qs]} { continue }
        set h [string range [lindex $qs end] 1 end-1]
        if {[regexp {[A-Za-z]{2}} $h]} { lappend out $h }
    }
    return $out
}

# ── the label column of a named label-list proc ─────────────────────
#
#     proc ::vpalette::actions {} {
#         return {
#             {"New session"       ::vsidebar::new_session}
#             {"Find in transcript" ::vfind::show}
#         }
#     }
#
# `::vpalette::actions` is the Ctrl-K palette's whole visible list. Its
# label column is a key; the command column is not.
#
# Discovered rather than enumerated by name: any proc whose body returns
# a table of brace groups that START with a quoted label is a label
# list. That is the shape `::vpalette::actions` and `::vdev::_popup_actions`
# share, and it is specific enough that the rest of the tree does not
# match it.
proc tcl_label_list {src} {
    set out {}
    # Walk every proc body, looking for a return of a row table.
    set at 0
    while {1} {
        set at [string first "return \{" $src $at]
        if {$at < 0} { break }
        # Is there a `proc` before it on the enclosing lines? A label
        # list is a proc that returns its table, not an inline return.
        set before [string range $src [expr {[string last "proc " [string range $src 0 $at]]}] $at]
        if {![string match "proc *" $before]} { incr at ; continue }
        # The group that the return statement opens.
        set gstart [expr {$at + 8}]
        set depth 0
        set close -1
        for {set j [expr {$gstart - 1}]} {$j < [string length $src]} {incr j} {
            set c [string index $src $j]
            if {$c eq "\\"} { incr j ; continue }
            if {$c eq "\{"} { incr depth ; continue }
            if {$c eq "\}"} {
                incr depth -1
                if {$depth == 0} { set close $j ; break }
            }
        }
        if {$close < 0} { incr at ; continue }
        set body [string range $src $gstart [expr {$close - 1}]]
        # Rows: a line that opens with a quoted label.
        set rows 0 ; set labels {}
        foreach line [split $body \n] {
            if {[regexp {^\s*\{"([^"]*)"} $line -> label]} {
                incr rows
                if {[regexp {[A-Za-z]{2}} $label]} { lappend labels $label }
            }
        }
        # A TABLE, not a one-off: a proc returning a single quoted string
        # is a different (already-covered) shape.
        if {$rows >= 3} { foreach l $labels { lappend out $l } }
        set at [expr {$close + 1}]
    }
    return $out
}

# ── Rust keys: string literals after lookup( / i18n!( ───────────────
proc rust_keys_in {src} {
    set out {}
    # i18n::lookup("...", …)  |  i18n!("...", …)
    # The literal is a normal Rust string; escapes are rare in prose
    # ("\\n" appears in a couple of format strings) and are decoded by
    # nobody — the catalog stores the ESCAPED form's meaning, so a
    # literal backslash-n in the key means the string contains a real
    # newline at runtime. Keeping the escaped spelling is therefore
    # WRONG; decode the two escapes that exist in practice.
    #
    # `\s*` between the paren and the quote is load-bearing: `cargo fmt`
    # breaks a long call across lines —
    #     Some(crate::i18n::lookup(
    #         "unknown theme '%{name}'",
    #         &[…],
    #     ))
    # — and without it the extractor saw only the short calls. Three
    # real keys went missing that way, and because the generator is the
    # source of en.msg the catalog then carried entries no "key" was
    # found for: the orphan direction of the drift check, which is how
    # this was caught.
    foreach pat {{i18n::lookup\(\s*"([^"\\]*(?:\\.[^"\\]*)*)"} {i18n!\(\s*"([^"\\]*(?:\\.[^"\\]*)*)"}} {
        foreach {- lit} [regexp -all -inline $pat $src] {
            set lit [string map {\\n "\n" \\t "\t" \\\" "\"" {\\\\} "\\"} $lit]
            lappend out $lit
        }
    }

    # ─ AND THE KEYS THAT REACH lookup() THROUGH A VARIABLE ──────────
    #
    # Nothing above can see these: they arrive as an argument, not a
    # literal in the call. tasks.rs is the live case —
    #
    #     let (header, none) = match kind {
    #         None => ("tasks_header", "tasks_none"),
    #         Some(TaskKind::Subagent) => ("subagents_header", …),
    #     };
    #     crate::i18n::lookup(header, &[])
    #
    # — and the cost of the blind spot was REAL, not theoretical: those
    # seven keys (tasks_*/subagents_*/shells_*/monitors_*) never entered
    # keys.txt, so no translator was ever asked for them, and /tasks,
    # /subagents, /shells and /monitors stayed English in all 47
    # locales while the catalogs reported complete. Found 2026-09-21 by
    # asking why de.yml carried `tasks_none` but not `tasks_header`.
    #
    # THE SOURCE IS en.yml, NOT A SHAPE. A first attempt matched bare
    # snake_case words in tuples and pulled in 176 identifiers that are
    # not keys at all — tool names (`web_search`), config fields
    # (`tls_ca_cert`), test fixtures (`vau_owner`). en.yml is the
    # engine's identity catalog and its whole stated purpose is to BE
    # this list ("a symbolic key reaches lookup() through a VARIABLE …
    # so it is hand-maintained prose, not a generated artifact"), and
    # i18n::tests::catalog_keys_and_code_do_not_drift already asserts
    # every entry in it is looked up from Rust source. Reading it here
    # makes the generator agree with that test instead of guessing.
    #
    # Read by the caller (i18n_keys_main) rather than here: this proc
    # takes a SOURCE STRING and has no repo root to find en.yml from.
    return $out
}

# The descriptions in commands.rs `help_entries`: every literal in that
# function's body that is not a usage string (usages start with "/").
# The body is a table of (usage, description) pairs and nothing else, so
# "not a usage" is exactly "a description" — pinned in i18nkeysetest.
proc rust_help_descs_in {src} {
    set start [string first "pub fn help_entries(" $src]
    if {$start < 0} { return {} }
    set end [string first "\n\}\n" $src $start]
    if {$end < 0} { return {} }
    set out {}
    foreach lit [rust_literals_in [string range $src $start $end]] {
        if {[string index $lit 0] ne "/"} { lappend out $lit }
    }
    return $out
}

proc en_yml_keys {root} {
    # The symbolic keys, from the engine's identity catalog. See
    # rust_keys_in for why this is the authoritative list.
    #
    # A MISSING FILE IS AN ERROR, not an empty list. Returning {} would
    # silently subtract these keys from the caller's set — and every
    # consumer treats a smaller set as "these keys do not exist", so the
    # failure mode is a catalog that quietly loses 31 translations or a
    # trim that deletes them. That is the same losing direction the
    # scanner note describes. The caller that legitimately has no tree
    # (a bare check in a sandbox) gets a clear message instead of a
    # mystery gap. `i18n_keys_main` calls this once per run, and the
    # flake's tclCheck copies locales/ precisely so this file is there.
    set f [file join $root crates veles-core locales en.yml]
    if {![file exists $f]} {
        error "en_yml_keys: no identity catalog at $f — the symbolic key\
 set cannot be derived, and an empty set would silently drop those keys"
    }
    set fh [open $f r]
    set data [read $fh]
    close $fh
    set out {}
    foreach line [split $data \n] {
        # A top-level bare `key: "…"` row. Quoted keys are English-as-key
        # rows, which the literal walk already covers.
        if {[regexp {^([a-z][a-z0-9]*(?:_[a-z0-9]+)+):\s*"} $line -> k]} {
            lappend out $k
        }
    }
    if {![llength $out]} {
        error "en_yml_keys: $f parsed to zero keys — refusing to return an\
 empty set, which would silently drop every symbolic key"
    }
    return $out
}

proc rust_literals_in {src} {
    # EVERY string literal in a Rust source file. This is the ENGINE KEY
    # SET (`--engine-keys`), and it is deliberately a SUPERSET of what
    # lookup() can be handed.
    #
    # Why a superset is the only safe shape: a key reaches lookup()
    # through a VARIABLE as often as through a literal — the
    # help_entries table's descriptions (`lookup(desc, &[])`) and
    # tasks.rs's match arms (`("shells_header", …)`) are both real, and
    # neither can be found by following `lookup(`. Anything this returns
    # that no code path can actually ask for costs a few bytes in a
    # catalog; anything it MISSES silently costs a translation, because
    # a key with no entry just renders English with no error anywhere.
    #
    # A REAL SCANNER, not a quote-to-quote regexp. The regexp version
    # was wrong in exactly that losing direction and a count check is
    # what caught it: Rust char literals may CONTAIN a double quote
    # (`trim_matches('"')`), and one stray quote shifts every later
    # pairing — the match that should have been `"agents_header"` ran
    # from one delimiter to a distant one and swallowed thousands of
    # characters. Eight real keys (agents_header, agents_none and the
    # seven doctor_*) came back missing while the total still looked
    # plausible.
    #
    # So the file is walked, and anything not inside a string is
    # skipped: line comments, block comments, char literals (which also
    # must not eat a LIFETIME — `'a` is not an unterminated `'a'`), and
    # every string form (with escapes, raw `r#"…"#`, and the byte
    # variants of both).
    set out {}
    set n [string length $src]
    set i 0
    while {$i < $n} {
        # Jump to the next byte that can start something interesting.
        # Everything between two of those is opaque code (identifiers,
        # operators, whitespace) and is skipped in one step — a
        # per-character crawl over several MB of Rust is slow enough to
        # notice at build time.
        if {![regexp -indices -start $i {[\"'/]} $src hit]} { break }
        set i [lindex $hit 0]
        set c [string index $src $i]
        set nxt [string index $src [expr {$i + 1}]]

        if {$c eq "/" && $nxt eq "/"} {
            # Line comment: to the end of the line.
            set j [string first "\n" $src $i]
            if {$j < 0} { break }
            set i [expr {$j + 1}]
            continue
        }
        if {$c eq "/" && $nxt eq "*"} {
            # Block comment. Rust nests them; one level is what the tree
            # actually contains, and a missed nesting level would only
            # over-include (the safe direction).
            set j [string first "*/" $src [expr {$i + 2}]]
            if {$j < 0} { break }
            set i [expr {$j + 2}]
            continue
        }

        if {$c eq "'"} {
            # A char literal or a lifetime. `'a` (lifetime) must not be
            # read as an unterminated char literal, or the scan would
            # run to the next apostrophe in the file — the same class of
            # bug as the regexp's.
            if {$nxt eq "\\"} {
                # Escaped: definitely a char literal ('\n', '\'', '"').
                set j [expr {$i + 2}]
                while {$j < $n} {
                    set ch [string index $src $j]
                    if {$ch eq "\\"} { incr j 2 ; continue }
                    if {$ch eq "'"} { break }
                    incr j
                }
                set i [expr {$j + 1}]
                continue
            }
            if {[string index $src [expr {$i + 2}]] eq "'"} {
                # 'x' or '"' — a plain char literal, quote and all.
                set i [expr {$i + 3}]
                continue
            }
            # A lifetime like 'a or 'static: consume just the apostrophe.
            incr i
            continue
        }

        if {$c eq "\""} {
            # Is this the opening quote of a RAW string? Its prefix is
            # `r`, `br`, or either followed by `#`s, so the quote is
            # preceded by `r` or by a run of `#` ending at an `r`.
            set h 0
            set k [expr {$i - 1}]
            while {$k >= 0 && [string index $src $k] eq "#"} { incr h ; incr k -1 }
            set pre [string index $src $k]
            set prev [expr {$k > 0 ? [string index $src [expr {$k - 1}]] : ""}]
            set is_raw [expr {$pre eq "r" && ![regexp {[A-Za-z0-9_]} $prev]}]
            if {$is_raw} {
                # Ends at a quote followed by the SAME number of hashes.
                set needle "\"[string repeat # $h]"
                set j [string first $needle $src [expr {$i + 1}]]
                if {$j < 0} { break }
                lappend out [string range $src [expr {$i + 1}] [expr {$j - 1}]]
                set i [expr {$j + 1 + $h}]
                continue
            }
            # A normal string (or a byte string b"...", whose prefix
            # needs no special handling). Scan to the closing quote,
            # honouring escapes.
            set j [expr {$i + 1}]
            while {$j < $n} {
                set ch [string index $src $j]
                if {$ch eq "\\"} { incr j 2 ; continue }
                if {$ch eq "\""} { break }
                incr j
            }
            set lit [string range $src [expr {$i + 1}] [expr {$j - 1}]]
            lappend out [string map {\\n "\n" \\t "\t" \\\" "\"" {\\\\} "\\"} $lit]
            set i [expr {$j + 1}]
            continue
        }

        # A lone `/` that starts nothing (division): step over it.
        incr i
    }
    return $out
}

proc files_under {dir ext {skip {}}} {
    set out {}
    foreach f [lsort [glob -nocomplain [file join $dir *]]] {
        if {[file isdir $f]} { continue }
        if {[string match $skip [file tail $f]]} { continue }
        if {[string match $ext [file tail $f]]} { lappend out $f }
    }
    return $out
}

proc walk_rs {dir out_var} {
    upvar 1 $out_var out
    foreach entry [lsort [glob -nocomplain [file join $dir *]]] {
        if {[file isdir $entry]} {
            if {[string match "*target*" $entry]} { continue }
            walk_rs $entry out
        } elseif {[string match *.rs [file tail $entry]]} {
            lappend out $entry
        }
    }
}

# Run only when executed as a script. `info exists ::argv` is not enough
# (a sourced file inherits the host's argv); the guard is on the script
# path tclsh reports for the top-level file.
if {[info exists ::argv0] && [file normalize $::argv0] eq [file normalize [info script]] } {
    i18n_keys_main $::argv
}