# veles-desktop/lib/palette.tcl — Ctrl-K command palette.
# Actions (keyboard-first, like hermes' command palette):
#   new session · find · models/settings · cron · skills · memory ·
#   plugins · usage · hot-reload patches · quit
# Plus session jump: typing filters sessions (FTS snippets via the
# search endpoint when >2 chars, local title filter otherwise).

namespace eval ::vpalette {
    variable wins {}
    variable items {}   ;# list of {label action}
    # The cursor lives in ::vlist (lib/vlist.tcl), not here — one owner
    # for list behaviour across every picker.
}

# Two-column rendering: label left, key hint right, padded to a shared
# width so the hints align into a readable column.
#
# The hint comes from ::vkeymap::key_for on the row's OWN action, so
# there is nothing here to keep in sync with bindings.tcl. This replaced
# a hand-maintained label→keyword table that fuzzy-matched help prose;
# 7 of the 18 rows that have a key were advertising the wrong one, and
# the two that resolved to nothing printed a literal "?".
#
# An empty hint means the action has no key IN THIS MODE — render the
# row bare rather than inventing a chord the mode does not answer to.
proc ::vpalette::_format_row {label hint width} {
    if {$hint eq ""} {
        return "⌘ $label"
    }
    set pad [expr {$width - [string length $label]}]
    if {$pad < 2} { set pad 2 }
    set row "⌘ $label"
    append row [string repeat " " $pad] $hint
    return $row
}

# Widest visible action label (for the padding math above).
#
# USES THE TRANSLATED LABEL, not the English key: the padding exists so
# the key-hint column lines up, and it lines up against what is actually
# painted. Measuring the English made the hint column ragged in every
# locale whose words are longer.
proc ::vpalette::_label_width {} {
    set w 0
    foreach a [actions] {
        set l [string length [::vi18n::mc [lindex $a 0]]]
        if {$l > $w} { set w $l }
    }
    return $w
}

# {label action category}. The category is where the row sits in the
# command tree behind ⋮ (lib/cmdtree.tcl); the palette lists them flat.
# No label names its key: the hint column does, from the registry, and
# a key written into a label ("Help (F1)") is one a phone has not got.
proc ::vpalette::_table {} {
    return {
        {"New session"            ::vsidebar::new_session sessions}
        {"Browse sessions"        {::voverlay::open sessions} sessions}
        {"Find in transcript"     ::vfind::show view}
        {"Refetch this transcript from the store" ::vchat::force_refresh sessions}
        {"Models & settings"      {::voverlay::open models} agent}
        {"Cron jobs"              {::voverlay::open cron} agent}
        {"Triggers"               {::voverlay::open triggers} agent}
        {"Running tasks"          {::voverlay::open tasks} agent}
        {"Skills"                 {::voverlay::open skills} agent}
        {"Memory"                 {::voverlay::open memory} agent}
        {"Access control (ACL)"  {::voverlay::open acl} agent}
        {"Sandboxes"             {::voverlay::open sandbox} agent}
        {"Apps"                   {::voverlay::open apps} agent}
        {"Plugins"                {::voverlay::open plugins} agent}
        {"Hooks (what plugins run on)" {::voverlay::open hooks} agent}
        {"Tools & MCP servers"    {::voverlay::open tools} agent}
        {"Usage"                  {::voverlay::open usage} agent}
        {"Task list"              {::vpanes::show tasks} agent}
        {"Checkpoints (undo)"     {::voverlay::open checkpoints} agent}
        {"Log (engine + status)"  {::voverlay::open log} agent}
        {"Subagents (/agents)"    {::vchat::slash_command "/agents"} agent}
        {"Failover mode (/failover)" {::vchat::slash_command "/failover"} agent}
        {"Pane: Files"            {::vpanes::show files} panes}
        {"Pane: Review (git)"     {::vpanes::show review} panes}
        {"Pane: Preview"          {::vpanes::show preview} panes}
        {"Pane: Terminal"         {::vpanes::show terminal} panes}
        {"Pane: Tasks (todo + plan)" {::vpanes::show tasks} panes}
        {"Pane: Dev (REPL + LSP)"  {::vpanes::show dev} panes}
        {"Dev: session for the live scripts" {::vdev::new_session} dev}
        {"Dev: arm / disarm live patching" {::vdev::toggle_arm} dev}
        {"Dev: new buffer"        {::vpanes::show dev ; ::vdev::buf_new} dev}
        {"Dev: next buffer"       {::vpanes::show dev ; ::vdev::buf_next 1} dev}
        {"Dev: previous buffer"   {::vpanes::show dev ; ::vdev::buf_next -1} dev}
        {"Dev: close buffer"      {::vpanes::show dev ; ::vdev::buf_close} dev}
        {"Dev: peek the definition"  {::vpanes::show dev ; ::vdev::_peek_def} dev}
        {"Dev: code actions"         {::vpanes::show dev ; ::vdev::code_action} dev}
        {"Dev: fold / unfold at point" {::vpanes::show dev ; ::vdev::toggle_fold} dev}
        {"Dev: unfold everything"    {::vpanes::show dev ; ::vdev::unfold_all} dev}
        {"Dev: pop buffer out into its own window" {::vpanes::show dev ; ::vdev::pop_out} dev}
        {"Dev: re-attach a popped buffer" ::vdev::reattach dev}
        {"Hide right panes"       ::vpanes::hide panes}
        {"View markdown source"   ::vchat::toggle_source view}
        {"Composer preview"       ::vchat::toggle_preview view}
        {"Themes"                 ::vpalette::open_themes settings}
        {"Language…"              ::vlang::picker settings}
        {"Setup wizard…"          ::vsetup::open settings}
        {"Install components…"    ::vsetup::open_install settings}
        {"Installation profiles…"  ::vsetup::open_profiles settings}
        {"Keybindings"            ::vkeymap::picker settings}
        {"Mechanisms (what the engine does on its own)" {::voverlay::open mechanisms} agent}
        {"Failover blueprint (edit the route chain)" ::vfg::open agent}
        {"Failover hook script (what `script` mode runs)" ::vfg::edit_script agent}
        {"Toggle native title bars" ::vtitle::toggle_native view}
        {"Restore hidden windows"   ::vtitle::restore_all view}
        {"Help"              ::vhelp::open help}
        {"Show every key"    ::vwhichkey::show_all help}
        {"Gateway status"         ::vstatus::show_gateway agent}
        {"Reload UI patches"      ::vhotpatch::load dev}
        {"All settings"           {::voverlay::open settings} settings}
        {"Phone: permissions, battery, storage" {::voverlay::open phone} settings}
        {"Quit"                   exit top}
    }
}

# The palette's rows here. A phone's own switches (lib/phone.tcl) mean
# nothing elsewhere; their row stays in the table, a proc that returns
# it, which is the shape scripts/i18n-keys.tcl reads labels from.
proc ::vpalette::actions {} {
    set rows [_table]
    if {!([llength [info commands ::vandroid::on]] && [::vandroid::on])} {
        set rows [lsearch -all -inline -not -exact -index 1 $rows {::voverlay::open phone}]
    }
    return $rows
}

# Theme picker — a small modal listing all palettes (built-ins + user
# themes from themes/*.tcl under the config dir). Click or ↑↓+Enter to apply.
proc ::vpalette::open_themes {} {
    set w .theme_picker
    if {[winfo exists $w]} { destroy $w }
    toplevel $w
    wm title $w "Themes"
    wm geometry $w "320x300+[expr {[winfo rootx .]+200}]+[expr {[winfo rooty .]+80}]"
    ttk::label $w.hint -text [::vi18n::mc {pick a palette (↑↓ Enter · Esc)}] -foreground $::vt::T(fg_dim)
    listbox $w.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -bd 0 -activestyle none
    foreach name [::vt::theme_names] {
        if {$name eq $::vt::current_theme} {
            $w.list insert end "◆ $name (active)"
        } else {
            $w.list insert end "  $name"
        }
    }
    $w.list selection set 0
    pack $w.hint -fill x -padx 8 -pady {8 2}
    pack $w.list -fill both -expand 1 -padx 8 -pady 2
    # Radio-select semantics: highlight moves on click/arrows; applying
    # happens on button release (after the class binding settles the
    # selection) or Enter — no double-selection races.
    bind $w.list <ButtonRelease-1> {::vpalette::pick_theme_apply %W}
    bind $w.list <Return> {::vpalette::pick_theme_apply .theme_picker.list}
    bind $w.list <Escape> {destroy .theme_picker; break}
    # Don't destroy on FocusOut — opening the picker from the palette
    # transfers focus, and the re-open loop (after 50 open_themes)
    # fought the FocusOut, creating a flicker-destroy cycle.
    focus $w.list
}

# Apply the selected theme without closing the picker, so the user can
# cycle through palettes and see each one live.
proc ::vpalette::pick_theme_apply {list_w} {
    set sel [lindex [$list_w curselection] 0]
    if {$sel eq ""} return
    set row [$list_w get $sel]
    set name [string trim [string map {"◆ " "" "(active)" ""} $row]]
    catch {::vt::use_theme $name}
    # Refresh the active markers IN PLACE at the same indices so the
    # cursor sticks on the picked row.
    set themes [::vt::theme_names]
    for {set i 0} {$i < [$list_w size]} {incr i} {
        set r [$list_w get $i]
        set n [string trim [string map {"◆ " "" "(active)" ""} $r]]
        if {$n ni $themes} continue
        set new [expr {$n eq $::vt::current_theme ? "◆ $n (active)" : "  $n"}]
        $list_w delete $i
        $list_w insert $i $new
    }
    $list_w selection set $sel
    $list_w see $sel
}

proc ::vpalette::open {} {
    variable wins
    if {[winfo exists .palette]} { focus .palette.entry; return }
    variable items
    set items [actions]

    # Remember what had focus so Esc/close can restore it. Without this,
    # closing the palette when the root window held focus leaves focus
    # empty — every bind-all shortcut goes dead until the user clicks.
    variable _prev_focus [focus -displayof .]
    if {$_prev_focus eq "" || $_prev_focus eq "."} {
        set _prev_focus .mid.chatwrap.chat.input
    }

    toplevel .palette
    wm title .palette [::vi18n::mc {commands}]
    wm overrideredirect .palette 1
    # Centred on the main window, always. The palette is the whole
    # app's command list, not an annotation on anything, so its position
    # must not depend on what happens to hold focus — anchoring it to
    # the focused widget put it off-screen when focus was in the
    # transcript. ::vplace clamps to the display.
    ::vplace::centered .palette 640 380
    raise .palette

    ttk::entry .palette.entry
    listbox .palette.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -bd 0 -activestyle none
    ttk::label .palette.hint -text [::vi18n::mc {↑↓ select · Enter run · Esc close}] \
        -foreground $::vt::T(fg_dim)
    catch { ::va11y::name .palette.entry [::vi18n::mc {command search}] }
    catch { ::va11y::name .palette.list [::vi18n::mc {command list}] }
    # Explicit cycle: entry → list → back. The default traversal order is
    # creation order here, but pinning it makes that a guarantee rather
    # than an accident if the build order ever changes.
    ::va11y::tab_order [list .palette.entry .palette.list]
    pack .palette.entry -fill x -padx 6 -pady {6 2}
    pack .palette.list -fill both -expand 1 -padx 6 -pady 2
    pack .palette.hint -fill x -padx 6 -pady {0 4}

    # One shared list behaviour (lib/vlist.tcl): filter, wrap-around
    # cursor, click-to-run, and — the part that used to be wrong
    # everywhere — arrows that do not refill and therefore do not throw
    # the cursor back to the first row.
    ::vlist::manage .palette.entry .palette.list \
        -refill {::vpalette::filter} \
        -run    {::vpalette::run_selected} \
        -close  {::vpalette::close}
    # FocusOut on the whole palette window (not on individual widgets —
    # clicking the listbox fires FocusOut on the entry, killing the
    # window before the click registers). %X/%Y substitute to "??" when
    # the event has no pointer position — winfo containing would then
    # throw "bad screen distance" and take the app down via bgerror.
    # Keep the palette open in that case (a stray FocusOut without
    # coords is usually the WM shuffling focus, not a click-away).
    # NOTE: this runs in an `after` callback, NOT in a proc — `return`
    # inside it is illegal (Tcl reports "command returned bad code: 2"
    # to bgerror and can take the app down). Structure it as nested
    # if/else so the callback always evaluates to a plain result.
    bind .palette <FocusOut> {
        after 10 {
            # Runs in an `after` callback, NOT a proc: `return`/`break`
            # inside are illegal (Tcl reports "command returned bad code:
            # 2" to bgerror and can take the app down). Structure as
            # nested if/else so the callback always evaluates to a plain
            # result. %X/%Y can be "??" (event generate without pointer)
            # or empty — winfo containing throws "bad screen distance"
            # on either; catch shields bgerror.
            if {[winfo exists .palette]} {
                if {[catch {winfo containing %X %Y} over] || $over eq ""} {
                    # no usable pointer position (WM focus shuffle, not a
                    # click-away) — keep the palette open
                } elseif {$over ne ".palette" && ![string match ".palette.*" $over]} {
                    destroy .palette
                }
            }
        }
    }
    focus .palette.entry
    filter
}

proc ::vpalette::filter {} {
    variable items
    variable last_q
    # The palette can be destroyed (Esc, FocusOut, run) while a stale
    # <KeyRelease> event is still queued — guard before touching widgets.
    if {![winfo exists .palette.list]} { return "" }
    set q [string tolower [.palette.entry get]]
    .palette.list delete 0 end
    set shown {}
    set width [_label_width]
    # FUZZY, not substring: "tasks pane" is found from "tskpn", and a
    # model name from its initials. Ranked by the one matcher every
    # picker in the app shares (::vedit::fuzzy_score), best first.
    #
    # MATCHING USES BOTH LANGUAGES. The table's label is the English
    # catalog key, and the row is DISPLAYED translated — so matching only
    # the English would leave a German user unable to find "Neue
    # Sitzung" by typing it, while matching only the translation would
    # break every muscle-memory query typed in English. Either word
    # finds the row; the displayed one is the translation.
    if {$q eq ""} {
        set acts [actions]
    } else {
        set scored {}
        set i 0
        foreach a [actions] {
            lassign $a label action
            set shown_label [::vi18n::mc $label]
            if {[::vedit::fuzzy_match $q $label] || [::vedit::fuzzy_match $q $shown_label]} {
                lappend scored [list [::vedit::fuzzy_score $q $label] $i $a]
            }
            incr i
        }
        set acts {}
        foreach row [lsort -decreasing -integer -index 0 $scored] {
            lappend acts [lindex $row 2]
        }
    }
    foreach a $acts {
        lassign $a label action
        .palette.list insert end [_format_row [::vi18n::mc $label] [::vkeymap::key_for $action] $width]
        lappend shown $a
    }
    # Session jump: FTS when query is substantive.
    if {[string length $q] > 2 && ![catch {::vapi::get_json "/api/search?q=[::vapi::json_escape $q]&limit=8"} resp]} {
        foreach h [dict get $resp hits] {
            set sid [dict get $h session_id]
            set snip [dict get $h snippet]
            .palette.list insert end "↪ $sid · $snip"
            lappend shown [list "goto $sid" [list ::vchat::open_session $sid]]
        }
    }
    set items $shown
    # The cursor belongs to ::vlist now. A changed query starts from the
    # top; the same query re-rendered (the async session search lands
    # late) keeps the row the user had walked to.
    if {![info exists last_q] || $q ne $last_q} {
        set last_q $q
        ::vlist::reset .palette.list
    } else {
        ::vlist::keep .palette.list
    }
}

proc ::vpalette::run_selected {} {
    variable items
    if {![winfo exists .palette]} { return }
    set sel [::vlist::get .palette.list]
    if {$sel >= 0 && $sel < [llength $items]} {
        set item [lindex $items $sel]
        set action [lindex $item 1]
        ::vpalette::close
        uplevel #0 $action
    } else {
        ::vpalette::close
    }
}

# Close the palette and restore focus to what had it before opening
# (composer when nothing else held focus — never leave the keyboard
# dead on an unfocused root window).
proc ::vpalette::close {} {
    variable _prev_focus
    catch { destroy .palette }
    if {[info exists _prev_focus] && [winfo exists $_prev_focus]} {
        # -force: the palette toplevel held the X input focus; destroying
        # it can leave the DISPLAY's focus unset, in which case a plain
        # `focus` only records the toplevel's choice and the app's
        # keyboard stays dead until the WM intervenes (no WM under Xvfb,
        # some WM edge cases on Wayland). -force claims it back now.
        catch { focus -force $_prev_focus }
    }
}
