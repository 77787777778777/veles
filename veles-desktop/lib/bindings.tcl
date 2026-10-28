# veles-desktop/lib/bindings.tcl — THE authoritative keybinding registry.
#
# One table per mode. Everything that displays bindings (help overlay,
# which-key popups, palette hints, status bar) reads from here — there
# is no second copy anywhere. keymap.tcl installs these bindings; when
# a binding changes, this file is the only edit.
#
# Row shape: {keys target action help category}
#   keys      display string, Tk-ish (C-x, M-o s, Alt-←, Shift-Enter)
#   target    all | input (composer)
#   action    command prefix to run ("" = structural, e.g. a prefix arm)
#   help      one-line description
#   category  grouping for help/palette display
#
# Prefix rows have the category "prefix"; their continuation keys live
# in ::vbindings::prefix_table keyed by the prefix display string.

namespace eval ::vbindings {
    variable mode_tables {}
    variable prefix_table {}
}

# Rows shared by EVERY mode. Style-neutral only: function keys and the
# numeric pane/tab switches belong to no tradition in particular.
#
# C-1..C-9 select a tab, which is emacs `tab-bar-select-tab` — and is
# also just what a Ctrl-digit does in every tabbed application, so it
# is genuinely universal rather than an emacs import. Alt-1..Alt-5 keep
# meaning "pane within the current tab", so nothing already learned
# changes meaning.
#
# These are Key events, not mouse buttons, only because _event
# qualifies the detail. `bind . <Control-1>` is Ctrl+left-click.
#
# This used to also carry Alt-x, the C-x and M-o prefixes, and BOTH
# Alt-arrows and Shift-arrows. That is why all three modes felt like a
# mix of all three styles: normie advertised Ctrl-K but silently
# answered to M-x, C-x and M-o as well, and because `_event` maps `M-x`
# and `Alt-x` to the SAME Tk event, emacs bound <Alt-x> twice and listed
# "command palette" twice in the help.
#
# Anything with an emacs or vi flavour now lives in those modes only.
# The style-neutral chords for surfaces that had NO key at all.
#
# USER: "a lot of stuff doesn't even seem to have a keyboard shortcut
# because it was never added." It hadn't been: 23 of 43 palette actions
# resolved to no key in any mode. ACL and Mechanisms shipped without
# bindings.tcl being touched at all, and the Dev-pane chords were raw
# `bind` calls outside the registry, so help/which-key/the palette hint
# column were blind to them.
#
# These are flat Ctrl-Shift and function keys, NOT a leader prefix:
# normie is deliberately "pure Ctrl" (commit 267d355) and a prefix would
# undo that decision. Ctrl-Shift-<letter> is free in every mode, so the
# rows live in `_universal` and all three modes get them at once.
proc ::vbindings::_managers {} {
    return {
        {F2           all {::vwhichkey::show_all}         "show every key in this mode"              general}
        {C-S-P        all {::voverlay::open plugins}      "Plugins"                                  manage}
        {C-S-H        all {::voverlay::open hooks}        "Hooks"                                    manage}
        {C-S-S        all {::voverlay::open skills}       "Skills"                                   manage}
        {C-S-M        all {::voverlay::open models}       "Models & settings"                        manage}
        {C-S-E        all {::voverlay::open memory}       "Memory"                                   manage}
        {C-S-A        all {::voverlay::open acl}          "Access control (ACL)"                     manage}
        {C-S-D        all {::voverlay::open sandbox}      "Sandboxes"                                manage}
        {C-S-X        all {::voverlay::open settings}     "All settings"                             manage}
        {C-S-N        all {::voverlay::open mechanisms}   "Mechanisms (what the engine does on its own)" manage}
        {C-S-I        all {::voverlay::open sessions}     "Browse sessions"                          session}
        {C-S-J        all {::voverlay::open cron}         "Cron jobs"                                manage}
        {C-S-Q        all {::voverlay::open triggers}     "Triggers"                                 manage}
        {C-S-T        all {::voverlay::open tools}        "Tools & MCP servers"                      manage}
        {C-S-K        all {::voverlay::open checkpoints}  "Checkpoints (undo)"                       manage}
        {C-S-U        all {::voverlay::open usage}        "Usage"                                    manage}
        {C-S-O        all {::vpanes::show tasks}          "Task list (right pane)"                   manage}
        {C-S-L        all {::voverlay::open log}          "Log (engine + status)"                    manage}
        {C-S-B        all {::voverlay::open tasks}        "Running tasks (subagents, shells, monitors)" manage}
        {F3           all {::vstatus::show_gateway}       "Gateway status"                           general}
        {C-S-V        all {::vchat::force_refresh}        "Refetch this transcript from the store"   general}
        {F4           all {::vkeymap::picker}             "Keybindings"                              general}
        {C-S-Y        all {::vlang::picker}               "Language (i18n)"                          general}
        {C-S-W        all {::vsetup::open}                "Setup wizard"                             manage}
        {C-S-C        all {::vsetup::open_install}        "Install components…"                     manage}
        {C-S-Z        all {::vsetup::open_profiles}       "Installation profiles…"                   manage}
        {F5           all {::vhotpatch::load}             "Reload UI patches"                        dev}
        {F6           all {::vdev::new_session}           "Dev: session for the live scripts"        dev}
        {F7           all {::vdev::toggle_arm}            "Dev: arm / disarm live patching"          dev}
        {F8           all {::vfg::open}                   "Failover blueprint (edit the route chain)" manage}
        {F9           all {::vpalette::open_themes}       "Themes"                                   general}
        {F10          all {::voverlay::open apps}         "Apps"                                     manage}
        {C-S-G        all {::vchat::slash_command "/agents"}   "Subagents"                            manage}
        {C-S-F        all {::vchat::slash_command "/failover"} "Failover mode for this session"       manage}
        {C-S-R        all {::vdev::reattach}               "Dev: re-attach a popped buffer"           dev}
    }
}

# Dev-pane chords: the Dev editor's and its REPL's.
#
# `target` is `dev` (the editor) or `repl` (the REPL's input): dev.tcl
# owns the real `bind` (the widgets only exist when that pane is built),
# exactly as `_irc_composer` does for the IRC composer, and keymap.tcl
# binds no widget-local row. Two widgets, so one chord can mean two
# things: C-c C-a is code actions in the editor, the app target in the
# REPL. The editor's C-x chords are its own, in every mode, normie too. They are
# here so the help overlay, the F2 sheet and the palette's hint column
# can see chords that previously existed only as text hand-typed into
# each button's label — which is the drift the registry was created to
# end.
#
# The ACTION is what runs the same thing from outside the editor: the
# command tree behind ⋮ (lib/cmdtree.tcl) and the palette run it, and
# the join gives their rows the chord. Until 2026-10-01 these rows had
# none, and three of them named the wrong thing: C-c C-s is signature
# help (C-x C-s saves), C-c C-o the outline (not "open a file"), and
# C-c C-g sets the REPL's target to the gateway (it evaluates nothing).
# whichkeytest.tcl now reads dev.tcl's binds against these rows.
proc ::vbindings::_dev_pane {} {
    return {
        {{C-x C-s}      dev {::vpanes::show dev ; ::vdev::editor_save}       "save the buffer"                            dev}
        {{C-c C-c}      dev {::vpanes::show dev ; ::vdev::editor_source_app} "source the buffer into the app interpreter" dev}
        {{C-c C-d}      dev {::vpanes::show dev ; ::vdev::_goto_def}         "go to definition"                           dev}
        {{C-c C-r}      dev {::vpanes::show dev ; ::vdev::_references}       "find references"                            dev}
        {{C-c C-s}      dev {::vpanes::show dev ; ::vdev::_signature}        "signature help"                             dev}
        {{C-c C-o}      dev {::vpanes::show dev ; ::vdev::_outline}          "outline of the buffer"                      dev}
        {{C-c C-e}      dev {::vpanes::show dev ; ::vdev::_rename}           "rename the symbol"                          dev}
        {{C-c C-h}      dev {::vpanes::show dev ; ::vdev::_highlight}        "highlight occurrences"                      dev}
        {{C-c C-p}      dev {::vpanes::show dev ; ::vdev::_peek_def}         "peek the definition"                        dev}
        {{C-c C-a}      dev {::vpanes::show dev ; ::vdev::code_action}       "code actions"                               dev}
        {{C-c C-f}      dev {::vpanes::show dev ; ::vdev::toggle_fold}       "fold / unfold at point"                     dev}
        {{C-c C-u}      dev {::vpanes::show dev ; ::vdev::unfold_all}        "unfold everything"                          dev}
        {{C-x Right}    dev {::vpanes::show dev ; ::vdev::buf_next 1}        "next buffer"                                dev}
        {{C-x Left}     dev {::vpanes::show dev ; ::vdev::buf_next -1}       "previous buffer"                            dev}
        {{C-x k}        dev {::vpanes::show dev ; ::vdev::buf_close}         "close the buffer"                           dev}
        {{C-x 4}        dev {::vpanes::show dev ; ::vdev::pop_out}           "pop the buffer out into its own window"     dev}
        {{C-c C-t}      repl {::vpanes::show dev ; ::vdev::toggle_target}     "toggle the REPL target (app / gw)"          dev}
        {{C-c C-a}      repl {::vpanes::show dev ; ::vdev::set_target app}    "REPL target: the app"                       dev}
        {{C-c C-g}      repl {::vpanes::show dev ; ::vdev::set_target gw}     "REPL target: the gateway"                   dev}
    }
}

proc ::vbindings::_universal {} {
    return {
        {F1           all {::vhelp::open}                 "help (this screen)"                       general}
        {C-a          all {}                              "normie: select all · emacs: start of line" general}
        {C-/          all {}                              "undo"                                     general}
        {C-z          all {}                              "undo"                                     general}
        {Shift-Left   all {::vdock::focus_left}           "focus previous region"                    navigation}
        {Shift-Right  all {::vdock::focus_right}          "focus next region"                        navigation}
        {Alt-1        all {::vpanes::show files}          "Files pane"                               panes}
        {Alt-2        all {::vpanes::show review}         "Review (git) pane"                        panes}
        {Alt-3        all {::vpanes::show preview}        "Preview pane"                             panes}
        {Alt-4        all {::vpanes::show terminal}       "Terminal pane"                            panes}
        {Alt-5        all {::vpanes::show tasks}          "Tasks pane (agent todo list + plan)"       panes}
        {Alt-6        all {::vpanes::show dev}            "Dev pane (REPL + LSP)"                    panes}
        {Alt-0        all {::vpanes::hide}                "hide right panes"                         panes}
        {C-1          all {::vtabs::select_index 1}       "tab 1"                                    tabs}
        {C-2          all {::vtabs::select_index 2}       "tab 2"                                    tabs}
        {C-3          all {::vtabs::select_index 3}       "tab 3"                                    tabs}
        {C-4          all {::vtabs::select_index 4}       "tab 4"                                    tabs}
        {C-5          all {::vtabs::select_index 5}       "tab 5"                                    tabs}
        {C-6          all {::vtabs::select_index 6}       "tab 6"                                    tabs}
        {C-7          all {::vtabs::select_index 7}       "tab 7"                                    tabs}
        {C-8          all {::vtabs::select_index 8}       "tab 8"                                    tabs}
        {C-9          all {::vtabs::select_index 9}       "tab 9"                                    tabs}
    }
}

# The emacs prefixes. emacs and vi only — a normie user who never asked
# for modal chords should not have C-x swallow their keystrokes.
proc ::vbindings::_emacs_prefixes {} {
    return {
        {Alt-Left     all {::vdock::focus_left}           "focus previous region"                    navigation}
        {Alt-Right    all {::vdock::focus_right}          "focus next region"                        navigation}
        {C-x          all {}                              "prefix: window & dock commands"           prefix}
        {M-o          all {}                              "prefix: open overlays"                    prefix}
    }
}

proc ::vbindings::_prefixes {} {
    # prefix display → {key action help}
    return {
        {C-x {
            {s {::vdock::toggle .sidebar} "dock/undock sidebar"}
            {p {::vdock::toggle .panes}   "dock/undock right panes"}
            {o {::vdock::focus_right}     "other window (next region)"}
            {b {::vdock::focus_left}      "back (previous region)"}
            {t {::vtabs::next}            "next tab"}
            {T {::vtabs::previous}        "previous tab"}
            {g {}                         "cancel prefix"}
        }}
        {M-o {
            {m {::voverlay::open models}      "models & settings"}
            {c {::voverlay::open cron}        "cron jobs"}
            {r {::voverlay::open triggers}    "triggers"}
            {s {::voverlay::open skills}      "skills"}
            {i {::vsetup::open_install}       "install components"}
            {e {::voverlay::open memory}      "memory"}
            {p {::voverlay::open plugins}     "plugins"}
            {u {::voverlay::open usage}       "usage"}
            {t {::vpanes::show tasks}          "task list"}
            {k {::voverlay::open checkpoints} "checkpoints"}
            {l {::voverlay::open log}         "engine log"}
            {b {::voverlay::open tasks}       "running tasks"}
            {g {}                             "cancel prefix"}
        }}
    }
}

# Composer-local rows for the IRC pane. `target` is `irc`, and their
# action is empty: irc.tcl owns the actual bind (the widget only exists
# when that pane is built), and these rows are here so the help overlay,
# which-key and the palette can SEE them. Same arrangement as
# Shift-Enter, whose real binding lives in chat.tcl.
proc ::vbindings::_irc_composer {} {
    return {
        {C-v  irc {} "insert the next key literally (^B bold, ^C colour, ^O reset)" irc}
        {C-y  irc {} "paste — C-v is quoted-insert in this composer"                irc}
    }
}

# Append rows for actions this mode cannot already reach.
#
# The manager chords are flat (Ctrl-Shift-*, F-keys) so normie — which
# deliberately has no prefix — can reach the overlays at all. emacs and
# vi already reach nine of them through M-o, and a flat row would WIN
# the `keys_for` lookup because non-prefix rows are searched first: the
# palette would start advertising C-S-M to an emacs user whose muscle
# memory is M-o m. So each mode takes only the rows that fill a gap in
# it, and the emacs idiom stays the emacs answer.
#
# `mode` is the name of the list variable in the caller's scope.
proc ::vbindings::_add_missing {mode args} {
    upvar 1 $mode rows
    foreach row $args {
        lassign $row keys target action help category
        if {[_action_in $rows $action] ne ""} { continue }
        lappend rows $row
    }
    return
}

# The display chord an already-collected row list gives this action,
# prefix continuations included, or "".
#
# Deliberately NOT `keys_for`: that reads the FINISHED mode_tables, and
# this runs while the table is still being built.
proc ::vbindings::_action_in {rows action} {
    variable prefix_table
    set want [_norm $action]
    if {$want eq ""} { return "" }
    foreach row $rows {
        lassign $row keys target a help category
        if {$category eq "prefix"} {
            foreach pair $prefix_table {
                lassign $pair p conts
                if {$p ne $keys} { continue }
                foreach prow $conts {
                    lassign $prow k pa ph
                    if {[_norm $pa] eq $want} { return "$keys $k" }
                }
            }
            continue
        }
        if {[_norm $a] eq $want} { return $keys }
    }
    return ""
}

proc ::vbindings::init {} {
    variable mode_tables
    variable prefix_table
    set prefix_table [_prefixes]

    set normie [list]
    lappend normie {*}{
        {Ctrl-K       all {::vpalette::open}            "command palette"                  general}
        {Ctrl-F       all {::vfind::show}               "find in transcript"               view}
        {Ctrl-N       all {::vsidebar::new_session}     "new session"                      session}
        {Ctrl-L       all {focus .mid.chatwrap.chat.input} "focus composer"                navigation}
        {Ctrl-E       all {::vdock::focus_transcript}   "jump to transcript"               navigation}
        {Ctrl-D       all {::vsidebar::delete_selected} "delete session"                   session}
        {Ctrl-R       all {::vchat::toggle_source}      "markdown source ⇄ rendered"       view}
        {Ctrl-P       all {::vchat::toggle_preview}     "composer preview"                 view}
        {Alt-u        all {::vdock::toggle .panes}      "toggle right panes"               panes}
        {Alt-Up       all {::vsidebar::select_rel -1}   "previous session"                 session}
        {Alt-Down     all {::vsidebar::select_rel 1}    "next session"                     session}
        {Shift-Enter  input {}                          "newline (do not send)"            composer}
    }
    foreach row [_universal] { lappend normie $row }
    foreach row [_irc_composer] { lappend normie $row }
    _add_missing normie {*}[_managers]
    foreach row [_dev_pane] { lappend normie $row }
    dict set mode_tables normie $normie

    set emacs [list]
    # The C-x h row is braced because "C-x h" is ONE chord, not keys
    # "C-x" + target "h"; emacs-table-only, so normie advertises no
    # emacs chords.
    lappend emacs {*}{
        {M-x          all {::vpalette::open}            "command palette"                  general}
        {M-f          all {::vfind::show}               "find in transcript"               view}
        {M-n          all {::vsidebar::new_session}     "new session"                      session}
        {M-l          all {focus .mid.chatwrap.chat.input} "focus composer"                navigation}
        {M-e          all {::vdock::focus_transcript}   "jump to transcript"               navigation}
        {M-d          all {::vsidebar::delete_selected} "delete session"                   session}
        {M-r          all {::vchat::toggle_source}      "markdown source ⇄ rendered"       view}
        {M-p          all {::vchat::toggle_preview}     "composer preview"                 view}
        {M-u          all {::vdock::toggle .panes}      "toggle right panes"               panes}
        {Alt-Up       all {::vsidebar::select_rel -1}   "previous session"                 session}
        {Alt-Down     all {::vsidebar::select_rel 1}    "next session"                     session}
        {C-j          input {}                          "newline (do not send)"            composer}
        {C-g          all {}                            "abort / cancel (emacs law)"       general}
        {{C-x h}      all {}                            "mark whole buffer (emacs select-all)" general}
    }
    foreach row [_emacs_prefixes] { lappend emacs $row }
    foreach row [_universal] { lappend emacs $row }
    foreach row [_irc_composer] { lappend emacs $row }
    _add_missing emacs {*}[_managers]
    foreach row [_dev_pane] { lappend emacs $row }
    dict set mode_tables emacs $emacs

    # vi = emacs chords + Esc normal mode (j/k session nav, i insert).
    set vi [list]
    foreach row $emacs { lappend vi $row }
    lappend vi {Esc input {} "enter normal mode (j/k sessions, i insert)" composer}
    dict set mode_tables vi $vi
}

# ── command → keys ──────────────────────────────────────────────────
#
# The join key is the COMMAND STRING ITSELF, because the same string is
# already what every affordance invokes:
#
#   registry   {Ctrl-N all {::vsidebar::new_session} "new session" …}
#   button     ttk::button …    -command ::vsidebar::new_session
#   ctxmenu    {"New session"   {::vsidebar::new_session}}
#   palette    {"New session"    ::vsidebar::new_session}
#
# So a hint cannot drift from its binding: moving the binding moves the
# action string with it. No ids, no lookup table, nothing to keep in
# sync — which is exactly what the thing this replaces got wrong. The
# old ::vkeymap::key matched the HELP PROSE through a hand-maintained
# keyword map, with a second label→keyword map stacked on it in
# palette.tcl, and 7 of 18 palette hints were wrong or missing: "Pane:
# Review" advertised Ctrl-P (which is composer preview), "Pane: Preview"
# and "Composer preview" showed a literal "?", "Pane: Dev" showed
# nothing, "Hide right panes" advertised Alt-u (which is toggle, not
# hide), "Gateway status" advertised M-o l (which is the log overlay),
# and every M-o chord was offered to normie, which has no M-o prefix.
#
# EXACT match, never a prefix. "Dev: new buffer" runs
# {::vpanes::show dev ; ::vdev::buf_new}; a prefix match would promise
# that Alt-5 does it, when Alt-5 only does the first half.
#
# Prefix chords are searched in the MODE'S OWN prefix rows, so normie —
# which deliberately has neither C-x nor M-o — is never told about a
# chord it does not answer to.
proc ::vbindings::_norm {command} {
    return [string trim [regsub -all {\s+} $command " "]]
}

proc ::vbindings::keys_for {command mode} {
    set want [_norm $command]
    if {$want eq ""} { return "" }
    foreach row [table $mode] {
        lassign $row keys target action help category
        if {$category eq "prefix"} { continue }
        if {[_norm $action] eq $want} { return $keys }
    }
    foreach row [table $mode] {
        lassign $row keys target action help category
        if {$category ne "prefix"} { continue }
        foreach prow [prefix_continuations $keys] {
            lassign $prow k a h
            if {[_norm $a] eq $want} { return "$keys $k" }
        }
    }
    return ""
}

proc ::vbindings::table {mode} {
    variable mode_tables
    if {![dict exists $mode_tables $mode]} { return {} }
    return [dict get $mode_tables $mode]
}

# Continuation keys for an armed prefix ("" if the prefix is unknown).
# The table is a list of {prefix rows} PAIRS (one element per prefix),
# so walk elements with lassign — flat `foreach {p conts}` and dict
# accessors both misread the nesting.
proc ::vbindings::prefix_continuations {prefix} {
    variable prefix_table
    foreach pair $prefix_table {
        lassign $pair p conts
        if {$p eq $prefix} { return $conts }
    }
    return {}
}

# {action help} for prefix+key, or {} when the key is not bound there.
proc ::vbindings::prefix_action {prefix key} {
    foreach row [prefix_continuations $prefix] {
        lassign $row k action help
        if {$k eq $key} { return [list $action $help] }
    }
    return {}
}

# Human-readable full binding list for the help overlay / docs.
proc ::vbindings::describe {mode} {
    set out {}
    foreach row [table $mode] {
        lassign $row keys target action help category
        if {$category eq "prefix"} {
            foreach prow [prefix_continuations $keys] {
                lassign $prow k a h
                lappend out [list "$keys $k" [::vi18n::mc $h] $category]
            }
        } else {
            lappend out [list $keys [::vi18n::mc $help] $category]
        }
    }
    return $out
}

::vbindings::init
