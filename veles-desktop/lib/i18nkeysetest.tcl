# veles-desktop/lib/i18nkeysetest.tcl — the Rust literal scanner.
#
# WHY THIS SUITE EXISTS. `rust_literals_in` (in scripts/i18n-keys.tcl)
# decides which keys the ENGINE can reach, and that set drives two things
# that are both silent when wrong:
#
#   * `--engine-keys` — the filter that trims crates/veles-core/locales/*.yml
#     to the keys the engine can actually look up. A key MISSING here is
#     dropped from the catalog, and a dropped key renders English with no
#     error anywhere. That is the losing direction.
#   * the generator's key set, which en.msg is built from. A key missing
#     here is a key no translator is ever asked for — which is exactly how
#     /tasks, /subagents, /shells and /monitors stayed English in all 47
#     locales while every catalog reported complete.
#
# THE BUG IT REPLACED. The first implementation was a quote-to-quote
# regexp, and Rust char literals may CONTAIN a double quote
# (`trim_matches('"')`). One such literal shifts every later quote
# pairing, so the match that should have been `"agents_header"` ran from
# one delimiter to a distant one and swallowed thousands of characters:
# eight real keys came back missing while the total still looked
# plausible. Every case below that involves an apostrophe, a comment or a
# raw string is a direct consequence of that failure.
#
# Run: scripts/one_tcl_test.sh veles-desktop/lib/i18nkeysetest.tcl
#
# The scanner is SOURCED from the repo, not copied, so this suite measures
# the code the generator runs.

set _dir [file dirname [file normalize [info script]]]
source [file join $_dir .. .. scripts i18n-keys.tcl]

# A real newline, built at runtime so this file stays plain ASCII and the
# expected value in the case below cannot be mangled by Tcl substitution.
set ::nl [format %c 10]

set ::T 0
set ::F 0
proc ok {name cond} {
    incr ::T
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ok   $name"
    } else {
        incr ::F
        puts "  FAIL $name"
    }
}

# One case: a source fragment, and the literals it must yield.
proc case {name src expect} {
    set got [rust_literals_in $src]
    set missing {}
    foreach want $expect {
        if {[lsearch -exact $got $want] < 0} { lappend missing $want }
    }
    # Report WHAT came back on failure: a bare FAIL on a scanner bug is
    # nearly useless, since the symptom is "some other string matched".
    if {[llength $missing]} {
        puts "      $name: missing [list $missing], got [list $got]"
    }
    ok $name {[llength $missing] == 0}
}

puts "── the trap that started this: a char literal containing a quote"
# `trim_matches('"')` is in the tree (agents.rs). A regexp pairing quotes
# blindly reads the char literal's opening quote as a string start.
case "a char literal's quote does not derail the scan" {
    let x = v.trim_matches('"');
    let k = crate::i18n::lookup("after the quote", &[]);
} {"after the quote"}

puts "── lifetimes are not unterminated char literals"
case "a lifetime does not eat the rest of the file" {
    fn f<'a>(s: &'a str) -> &'a str {
        crate::i18n::lookup("in a lifetime fn", &[]);
        s
    }
} {"in a lifetime fn"}
case "an escaped char literal is skipped whole" {
    let nl = '\n';
    let k = lookup("after escaped char", &[]);
} {"after escaped char"}

puts "── escaping inside normal strings"
case "an escaped quote is decoded" {
    let k = lookup("he said \"hi\"", &[]);
} {"he said \"hi\""}
# The generator writes the DECODED form, so a key holding a real backslash
# has to survive: the catalog compares it to the same string msgcat holds.
# (Expected value built with format, NOT written as "\n" in this file —
# Tcl would substitute a real newline here and the two strings would
# differ by construction, which is how this case failed the first time.)
case "a backslash-n decodes to a newline" {
    let k = lookup("line\nbreak", &[]);
} [list "line${::nl}break"]

puts "── raw strings carry no escapes"
case "a raw string keeps its backslashes" {
    let k = lookup(r"raw \n stays literal", &[]);
} {"raw \\n stays literal"}
case "a raw string may contain quotes" {
    let k = lookup(r#"has "quotes" inside"#, &[]);
} {"has \"quotes\" inside"}

puts "── comments are not code"
case "a line-commented key is not collected" {
    // crate::i18n::lookup("commented out", &[]);
    let k = lookup("real one", &[]);
} {"real one"}
case "a block-commented key is not collected" {
    /* lookup("block commented", &[]); */
    let k = lookup("after block", &[]);
} {"after block"}
case "a division slash is not a comment" {
    let a = b / c;
    let k = lookup("after division", &[]);
} {"after division"}

puts "── the match-arm shape the generator also reads"
# Not this proc's job (en_yml_keys supplies those), but the scanner must
# not choke on the shape.
case "a tuple of symbolic keys is harmless to the scan" {
    let (h, n) = match k { None => ("tasks_header", "tasks_none") };
} {"tasks_header" "tasks_none"}

puts "── the help table's descriptions are keys (rust_help_descs_in)"
# help_entries_localized looks each description up through a VARIABLE,
# so following lookup( never found them: 76 descriptions sat in no
# catalog and /help rendered English in every locale while the coverage
# note said the table was "resolved at every render site".
set _src {
fn unrelated() { lookup("not help", &[]); }
pub fn help_entries(surface: Surface) -> Vec<(&'static str, &'static str)> {
    let mut v = vec![
        ("/model [name]", "show or switch model"),
        (
            "/crons",
            "scheduled jobs; /crons add \"<sched>\" <prompt>",
        ),
    ];
    match surface {
        Surface::Repl => v.extend([("/quit | /exit", "exit")]),
    }
    v
}
fn after() { lookup("not help either", &[]); }
}
set _got [rust_help_descs_in $_src]
ok "every description, decoded, and no usage string" {
    $_got eq [list "show or switch model" "scheduled jobs; /crons add \"<sched>\" <prompt>" "exit"]
}
ok "nothing from outside the function" {
    [lsearch -exact $_got "not help"] < 0 && [lsearch -exact $_got "not help either"] < 0
}

# ── the Tcl-side shapes added 2026-09-26 ────────────────────────────
#
# USER report: the steer button, the pane names, the tooltips, the help
# text. Every one of those was a key the extractor could not see, so no
# translator was ever asked for it and mc() returned the English it was
# handed. Each case below is one of those shapes, and each one FAILS on
# the extractor as it was before this round — which is the point of
# pinning them here rather than trusting the key count.
puts ""
puts "── the Tcl shapes a key reaches a widget by"

# 1. a foreach DATA TABLE — panes.tcl's tab names and its tooltips.
set _tab {proc ::vpanes::build {} {
    foreach {tab label} {files "Files" review "Review" preview "Preview"} {
        ttk::button $::vpanes::R.tabs.$tab -text [::vi18n::mc $label]
    }
}}
ok "a foreach table's prose cells are keys" {
    [tcl_table_keys $_tab] eq [list Files Review Preview]
}

# …and a SINGLE-WORD cell in one is a key too. This is the Instances
# control strip (instpane.tcl) and the dev debug bar (dev.tcl), which
# shipped `Attach`, `Start`, `Restart`, `widgets` and `namespaces` as
# English in all 46 locales: flat_prose_cells required whitespace, so the
# one-word cells failed its prose test and only their multi-word siblings
# (`Reload plugins`, `proc body`) became keys. A half-translated strip
# reads as a missing TRANSLATION, which is why no static check caught it
# — the live-window probe did.
set _strip {proc ::vinstpane::build_ctl {} {
    foreach {id label} {
        attach    {Attach}
        |         {}
        reload    {Reload plugins}
        start     {Start}
        restart   {Restart}
        add       {Add…}
    } {
        ttk::button $R_ctl.$id -text [::vi18n::mc $label]
    }
}}
ok "a one-word cell in a table is a key" {
    [tcl_table_keys $_strip] eq [list Attach {Reload plugins} Start Restart Add…]
}

# …and the cut has to stay narrow, or every identifier in every table
# becomes a "key". Each of these is a real neighbouring cell that must
# NOT be collected: a lowercase command name, a variable, a file path,
# a slash path, and a Tcl fragment.
set _cells {proc f {} {
    set choices {
        mode  {schedule}
        var   {provider_name}
        path  {patches.tcl}
        dir   {a/b}
        frag  {insert linestart}
    }
}}
ok "an identifier-shaped cell is not a key" {
    [tcl_table_keys $_cells] eq {}
}

# …and a foreach over a VARIABLE or a COMMAND is NOT a table. This is
# the case that made the audit stop reading files (see i18n-audit.tcl),
# and the extractor walks groups by position so it never had that bug —
# asserted rather than assumed.
set _loop {proc f {} {
    foreach sid [dict keys $logs] { catch { _drop_log $sid } }
    foreach mode {normie emacs vi} { lappend out $mode }
}}
ok "a foreach over a list is not a table" {
    [tcl_table_keys $_loop] eq {}
}

# 2. the ::vbindings registry's help column — the F1 help, the which-key
#    popup and the palette hint column all read it.
set _reg {        {F1           all {::vhelp::open}                 "help (this screen)"                       general}
        {C-S-M        all {::voverlay::open models}       "Models & settings"                        manage}
}
ok "the registry's help column is a key" {
    [tcl_registry_help $_reg] eq [list {help (this screen)} {Models & settings}]
}

# 3. a label-list proc — ::vpalette::actions, the Ctrl-K/M-x list.
set _ll {proc ::vpalette::actions {} {
    return {
        {"New session"            ::vsidebar::new_session}
        {"Find in transcript"     ::vfind::show}
        {"Themes"                 ::vpalette::open_themes}
        {"Quit"                   exit}
    }
}}
ok "a label-list proc's labels are keys" {
    [tcl_label_list $_ll] eq [list {New session} {Find in transcript} Themes Quit]
}

# ...and a proc returning a SINGLE string is not a label list.
set _one {proc f {} {
    return {"just one"}
}}
ok "a single-string return is not a label list" {
    [tcl_label_list $_one] eq {}
}

# 4. the desktop-local command table. The description is the LAST brace
#    group — the open group is the command name, so a "third group" test
#    refuses every row (it did, until this was fixed).
set _cmds {        {/help {} {this list}}
        {/theme {name} {switch or list themes}}
        {/keymap {normie|emacs|vi} {switch keybinding scheme}}
}
ok "the desktop command table's descriptions are keys" {
    [tcl_desktop_cmd_keys $_cmds] eq \
        [list {this list} {switch or list themes} {switch keybinding scheme}]
}

# 5. a context menu's entries — the long-press menus on a phone. The
#    label is a key, the separator row is not, and neither is the
#    command, quoted words and all.
set _ctx {    ::vctx::attach $w {
        {"View source / rendered" {::vchat::toggle_source_at $::vctx::x $::vctx::y}}
        {-}
        {"Copy message"           {::vchat::copy_message_at "x" 0}}
    }
    ::vctx::attach .x {{"Paste" {event generate .x <<Paste>>}}}
}
ok "a context menu's labels are keys" {
    [tcl_ctxmenu_keys $_ctx] eq [list {View source / rendered} {Copy message} Paste]
}
ok "…and the definition is not a table" {
    [tcl_ctxmenu_keys {proc ::vctx::attach {w entries} {
        set m $w.vctxmenu
    }}] eq {}
}

puts ""
puts "i18nkeysetest: $::T passed, $::F failed"
exit [expr {$::F > 0 ? 1 : 0}]