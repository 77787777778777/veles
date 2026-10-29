# veles-desktop/lib/panes.tcl — the right-pane shell (hermes-desktop
# right-sidebar parity): Files / Review / Preview / Terminal tabs.
#
# Layout: one vertical ttk::notebook-style strip on the right of .mid.chatwrap.chat.
# Built as a plain frame with a tab bar + one body frame per tab (Tk's
# ttk::notebook fights the theme system; hand-rolled tabs theme cleanly
# and give us the collapse behavior).
#
# Backend: /api/fs/tree, /api/fs/read, /api/git/*, /api/preview,
# /api/focus (frontends/panes.rs). The agent drives this through its
# open_preview / focus_pane tools; we poll /api/preview + /api/focus on
# a slow timer so agent-initiated previews pop in live.
#
# Terminal: launches the user's terminal emulator (wezterm → konsole →
# xterm fallback) in the current repo dir. A full embedded PTY is out
# of scope for Tk — honest about it in the tab's empty state.

namespace eval ::vpanes {
    variable current "preview"
    variable visible 0
    variable poll_after ""
    variable last_preview ""
    variable preview_img ""
    variable last_focus ""
    variable _seen_preview ""
    # The session whose agent preview the pane shows; "" when it shows
    # nothing, or a file the user opened themselves (_poll_tick).
    variable preview_owner ""
    variable root_dir "~"
    # Widget root of the pane shell. Normally .panes (docked in the .mid
    # pane); while undocked it is the frame inside the float toplevel.
    # Every reference goes through $R so rebuild-on-undock works (Tk
    # cannot reparent a widget into another toplevel, so undocking
    # destroys + rebuilds the shell at the new root).
    variable R ".panes"
    # Tab caption tables: the ENGLISH cell (which is the catalog key) and
    # the hotkey digit for the current keymap mode. Kept so a language
    # swap can rebuild the caption from the source string rather than
    # from the string it last painted (see _tab_text).
    variable tablabel
    variable tabhotkey
    array set tablabel {}
    array set tabhotkey {}
}

# Paint ONE tab's caption: "<digit> <translated name>".
#
# The digit comes from the keymap registry and may legitimately be empty
# (a mode with no binding for this pane), so the format has two arms.
proc ::vpanes::_tab_text {tab} {
    variable R
    variable tablabel
    variable tabhotkey
    set w $R.tabs.$tab
    if {![winfo exists $w]} { return }
    set name [::vi18n::mc $tablabel($tab)]
    set hot $tabhotkey($tab)
    catch { $w configure -text [expr {$hot eq "" ? $name : "$hot $name"}] }
}

# Re-paint every tab caption after a language swap.
#
# Registered with ::vi18n::on_locale_change — see the build loop for why
# a registered label would not do. Idempotent, so a rebuild that
# re-registers the hook is harmless (on_locale_change dedupes).
proc ::vpanes::relabel_tabs {} {
    variable tablabel
    foreach tab [array names tablabel] { _tab_text $tab }
}

# Rebuild the whole shell at a new root (undock/redock). Remembers the
# active tab so the float lands on the same content. Packs the root
# only when it lives directly in a float toplevel — when docked, the
# .mid pane is the geometry manager.
proc ::vpanes::rebuild {root} {
    variable R
    variable current
    if {[winfo exists $R]} { destroy $R }
    set R $root
    build
    set parent [winfo parent $R]
    if {[winfo toplevel $parent] eq $parent} {
        pack $R -fill both -expand 1
    }
    show $current
}

# ── build ────────────────────────────────────────────────────────────

proc ::vpanes::build {} {
    ttk::frame $::vpanes::R

    # Tab bar — labels carry their hotkey (Alt-1..4 in every keymap
    # mode), so the relevant key is visible at the level where it acts.
    ttk::frame $::vpanes::R.tabs
    # The digit comes from the REGISTRY, through the command this button
    # runs — not from a counter. A counter is right only while the
    # registry happens to agree with it, which is exactly the drift that
    # made 7 palette hints name the wrong key.
    #
    # The LABEL COLUMN IS THE KEY (USER report 2026-09-26: "all the pane
    # names are untranslated"). `-text $label` carries no brace for the
    # extractor to find, so these six names sat in no catalog while every
    # visible string around them was wrapped — and the fix is not to wrap
    # the call site but to make the TABLE the key list.
    #
    # THE TEXT IS COMPUTED, so it follows a swap through a locale hook
    # rather than through ::vi18n::label. A registered label stores
    # {key args} and re-renders `format(mc(key), args)` — but one arg
    # here is the TRANSLATED name, so a re-render would re-format the old
    # language's word and the button would stay stale. The hook rebuilds
    # from the English cell, which is exactly what i18n.tcl documents
    # on_locale_change for (the status bar's chip is the other user).
    foreach {tab label} {files "Files" review "Review" preview "Preview" terminal "Term" tasks "Tasks" dev "Dev"} {
        set hotkey ""
        catch { set hotkey [::vkeymap::key_for [list ::vpanes::show $tab]] }
        # "Alt-1" is too wide for this strip; the digit alone is enough
        # HERE because the strip sits inside the panes it switches.
        regexp {(\d)$} $hotkey -> hotkey
        set ::vpanes::tablabel($tab) $label
        set ::vpanes::tabhotkey($tab) $hotkey
        ttk::button $::vpanes::R.tabs.$tab -width 9 \
            -command [list ::vpanes::show $tab]
        ::vpanes::_tab_text $tab
        pack $::vpanes::R.tabs.$tab -side left -padx 1
    }
    ::vi18n::on_locale_change ::vpanes::relabel_tabs
    ttk::button $::vpanes::R.tabs.hide -text [::vi18n::mc {» hide}] -width 7 -command ::vpanes::hide
    pack $::vpanes::R.tabs.hide -side right -padx {4 0}
    # These six are exactly the ones whose keys were dead until Tk
    # stopped reading <Alt-1> as a mouse button, and the labels have
    # been advertising them the whole time. Clicking one now says which
    # key it was, until you have used it.
    foreach _t {files review preview terminal tasks dev hide} {
        catch { ::vcoach::attach $::vpanes::R.tabs.$_t }
    }
    # Every tab says what it shows and which key opens it — the labels
    # are one word ("Review"), which is not an explanation.
    foreach {_t tip} {files {browse the repo's files}
                      review {git status and the current diff}
                      preview {what the agent last opened for you}
                      terminal {run a one-shot command or open a terminal}
                      tasks {the agent's todo list and its plan}
                      dev {REPL + editor for live patches}
                      hide {collapse the whole right pane}} {
        catch { ::va11y::name $::vpanes::R.tabs.$_t [::vi18n::mc $tip] }
    }

    # Bodies — all six exist at all times; show raises one.
    _build_files $::vpanes::R.files
    _build_review $::vpanes::R.review
    _build_preview $::vpanes::R.preview
    _build_terminal $::vpanes::R.terminal
    _build_tasks $::vpanes::R.tasks
    _build_dev $::vpanes::R.dev

    pack $::vpanes::R.tabs -fill x -padx 2 -pady {2 0}
    pack $::vpanes::R.files -fill both -expand 1
    # Only one body packed at a time (single-pack discipline, pitfall 18).

    # pack of $::vpanes::R itself happens in main.tcl AFTER .mid.chatwrap.chat, so it sits
    # to the right (side right). Hidden until revealed.
}

# ── tab switching / visibility ───────────────────────────────────────

# The right-hand pane strip's tabs, in Alt-1..Alt-6 order.
proc ::vpanes::_ordinal {tab} {
    set i [lsearch -exact {files review preview terminal tasks dev} $tab]
    return [expr {$i < 0 ? 0 : $i + 1}]
}

proc ::vpanes::show {tab} {
    # Pane bodies are built lazily, so coach whatever this one
    # just created. Idempotent, so re-showing a pane is free.
    after idle [list catch [list ::vcoach::attach_all $::vpanes::R]]
    # A TAB IS A SAVED LAYOUT, and .panes belongs to the layouts that
    # list it. Docking it into one that does not — which is what the
    # `redock`/`dock` below does unconditionally — makes Alt-1..5 rewrite
    # the tab you are in. In the IRC tab it dropped a Files pane into a
    # three-region layout that had no slot for one.
    #
    # In a tab without .panes, Alt-N keeps its documented meaning ("pane
    # within the current tab") by focusing that tab's Nth region: in IRC
    # that is buffers / messages / roster.
    #
    # The guard is here rather than at the binding because it is a
    # property of the LAYOUT, not of the caller. A button inside .panes
    # can only be clicked while .panes is visible, so it never trips.
    if {[llength [info commands ::vtabs::has_region]]
        && ![::vtabs::has_region .panes]} {
        return [::vtabs::focus_nth [_ordinal $tab]]
    }
    variable current $tab
    variable visible 1
    variable R
    foreach t {files review preview terminal tasks dev} {
        catch {pack forget $R.$t}
    }
    pack $R.$tab -fill both -expand 1 -padx 2 -pady 2
    # Only dock when the region is not already on screen — during a
    # rebuild into the float window R is the float body, and docking
    # THAT would fight the toplevel geometry. The canonical region is
    # .panes; dock it when it's hidden in neither world.
    if {![winfo ismapped $R] && ![::vdock::visible .panes]} {
        catch { ::vdock::redock .panes }
        ::vdock::dock .panes
    }
    _highlight_tab $tab
    # Lazy-fill the tab on first reveal.
    switch -- $tab {
        files   { if {[llength [$::vpanes::R.files.tree children {}]] == 0} { refresh_files } }
        review  { refresh_review }
        preview { if {[$::vpanes::R.preview.body get 1.0 end-1c] eq ""} { _preview_empty_state } }
        tasks   {
            # _tasks_poll does the first refresh AND arms the 5s one;
            # it self-cancels once another tab becomes current.
            variable tasks_poll_after
            if {![info exists tasks_poll_after]} { set tasks_poll_after "" }
            if {$tasks_poll_after eq ""} { _tasks_poll }
        }
        terminal { }
        dev     { }
    }
}

# ── tab-remembered state ─────────────────────────────────────────────
#
# Which pane this shell is showing is a property of the TAB, not of the
# shell: the Chat tab's right-hand pane is Files while the Dev tab is
# the editor, and they are the same widget. ::vtabs captures this on the
# way out of a tab and puts it back on the way in.
proc ::vpanes::capture_state {} {
    variable current
    return [dict create tab $current]
}

proc ::vpanes::apply_state {st} {
    if {[dict exists $st tab]} {
        set t [dict get $st tab]
        if {$t in {files review preview terminal tasks dev}} {
            if {[::vdock::visible .panes]} {
                show $t
            } else {
                # Remember the pane WITHOUT docking .panes into the
                # incoming layout. The Chat tab remembers its right pane
                # AND opens closed; `show`'s dock side effect used to
                # force it open on the first select and persist that, so
                # the tab's `initial` (opens closed) never survived a
                # restart. The remembered tab lands on the next reveal.
                variable current $t
                catch { _highlight_tab $t }
            }
        }
    }
}

# The widget that holds the ACTUAL CONTENT of the visible pane — the
# thing a user would call a buffer, never a toolbar or a tab strip.
#
# `::vdock::focus_target` used to return `$R.tabs.files`, a tab BUTTON,
# so `C-x o` and Shift-arrows cycled onto chrome instead of onto
# something you can read or type in. Emacs `other-window` moves between
# windows showing buffers; this is the list it walks.
proc ::vpanes::content_widget {} {
    variable current
    variable R
    set candidates [dict create \
        files    [list $R.files.tree] \
        review   [list $R.review.files $R.review.diff] \
        preview  [list $R.preview.body] \
        terminal [list $R.terminal.out] \
        dev      [list $R.dev.edit.body $R.dev.repl.out]]
    if {[dict exists $candidates $current]} {
        foreach c [dict get $candidates $current] {
            if {[winfo exists $c]} { return $c }
        }
    }
    # Unknown or not yet built: fall back to any content-ish descendant
    # rather than to a button.
    foreach c [_descendant_content $R] { return $c }
    return ""
}

# Depth-first search for the first text/listbox/treeview under $w.
proc ::vpanes::_descendant_content {w} {
    set out {}
    if {![winfo exists $w]} { return $out }
    foreach child [winfo children $w] {
        if {[winfo class $child] in {Text Listbox Treeview}} {
            lappend out $child
        }
        foreach d [_descendant_content $child] { lappend out $d }
    }
    return $out
}

# Copy the path of the row the context menu was opened on. ::vctx has
# already selected it, so read the selection.
proc ::vpanes::_ctx_copy_path {} {
    set w $::vctx::widget
    if {$w eq "" || ![winfo exists $w]} { return }
    set item [lindex [$w selection] 0]
    if {$item eq ""} { return }
    set path ""
    catch { set path [$w set $item path] }
    if {$path eq ""} { catch { set path [$w item $item -text] } }
    if {$path eq ""} { return }
    clipboard clear
    clipboard append $path
    ::vstatus::set_msg [format [::vi18n::mc {copied: %s}] $path]
}

proc ::vpanes::hide {} {
    variable visible 0
    # $::vpanes::R lives in the .mid pane (or a float) — remove it from
    # whichever manages it.
    if {[winfo exists $::vpanes::R]} {
        catch { .mid forget $::vpanes::R }
        if {[info exists ::vdock::floats] && [dict exists $::vdock::floats $::vpanes::R]} {
            set t [dict get $::vdock::floats $::vpanes::R]
            pack forget $::vpanes::R
            catch { destroy $t }
            dict unset ::vdock::floats $::vpanes::R
        }
    }
}

proc ::vpanes::toggle {} {
    variable visible
    if {$visible} { hide } else { show $::vpanes::current }
}

proc ::vpanes::_highlight_tab {tab} {
    foreach t {files review preview terminal tasks dev} {
        if {$t eq $tab} {
            $::vpanes::R.tabs.$t state pressed
        } else {
            $::vpanes::R.tabs.$t state !pressed
        }
    }
}

# ── Files pane ───────────────────────────────────────────────────────

proc ::vpanes::_build_files {w} {
    ttk::frame $w
    ttk::frame $w.bar
    ttk::entry $w.bar.path -textvariable ::vpanes::root_dir
    ttk::button $w.bar.go -text [::vi18n::mc {Go}] -command ::vpanes::refresh_files
    pack $w.bar.go -side right -padx 2
    pack $w.bar.path -side left -fill x -expand 1
    # helm-find-files style: type to filter the tree (substring match
    # over a depth-3 flattening of the root); clear restores the
    # hierarchy. Alt-1 focuses this pane's tab from anywhere.
    ttk::entry $w.filter
    ttk::label $w.fhint -text [::vi18n::mc {type to filter (helm)}] -foreground $::vt::T(fg_dim)
    ttk::treeview $w.tree -show tree -selectmode browse
    pack $w.bar -fill x -padx 2 -pady 2
    pack $w.filter -fill x -padx 2
    pack $w.fhint -anchor w -padx 4
    pack $w.tree -fill both -expand 1
    bind $w.tree <<TreeviewSelect>> { ::vpanes::_file_selected }
    ::vctx::attach $w.tree {
        {"Open"           {::vpanes::_file_selected}}
        {"Copy path"      {::vpanes::_ctx_copy_path}}
        {-}
        {"Preview here"   {::vpanes::show preview}}
        {"Refresh"        {::vpanes::refresh_files}}
    }
    bind $w.bar.path <Return> { ::vpanes::refresh_files }
    bind $w.filter <KeyRelease> { ::vpanes::_files_filter }
    bind $w.filter <Escape> { set %W ""; ::vpanes::refresh_files; focus %W }
    bind $w.filter <Return> { ::vpanes::_files_open_match }
}

# helm-style: flatten the root's tree (depth ≤3, server-capped at 400
# per level), keep substring matches, show them flat. Empty filter =
# normal hierarchy.
proc ::vpanes::_files_filter {} {
    set w $::vpanes::R.files
    set q [string tolower [$w.filter get]]
    if {[string trim $q] eq ""} {
        $w.fhint configure -text [::vi18n::mc {type to filter (helm)}]
        refresh_files
        return
    }
    set root [string trim $::vpanes::root_dir]
    if {$root eq ""} { set root "~" }
    if {[catch {::vapi::get_json "/api/fs/tree?path=[::vapi::json_escape $root]&depth=3"} resp]} {
        $w.fhint configure -text [::vi18n::mc {filter error}]
        return
    }
    if {[dict exists $resp error]} {
        $w.fhint configure -text "[dict get $resp error]" ;# i18n-exempt: server error passthrough
        return
    }
    set flat {}
    _files_flatten [dict get $resp entries] flat
    set matches {}
    foreach e $flat {
        if {[string match "*$q*" [string tolower [dict get $e path]]]} {
            lappend matches $e
        }
    }
    $w.tree delete [$w.tree children {}]
    set n 0
    foreach e $matches {
        if {$n >= 200} break
        set name [dict get $e name]
        set path [dict get $e path]
        set is_dir [dict get $e is_dir]
        set id [$w.tree insert {} end -text $path -values [list $path $is_dir]]
        incr n
    }
    ::vi18n::label $w.fhint {%s match(es) — Enter opens first, Esc clears} $n
}

proc ::vpanes::_files_flatten {entries outvar} {
    upvar 1 $outvar out
    foreach e $entries {
        lappend out $e
        if {[dict exists $e children]} {
            _files_flatten [dict get $e children] out
        }
    }
}

# Enter in the filter: open the first match (file → preview; dir → set
# root + refresh).
proc ::vpanes::_files_open_match {} {
    set w $::vpanes::R.files
    set kids [$w.tree children {}]
    if {[llength $kids] == 0} return
    set first [lindex $kids 0]
    lassign [$w.tree item $first -values] path is_dir
    if {$is_dir} {
        set ::vpanes::root_dir $path
        $w.filter delete 0 end
        refresh_files
    } else {
        show preview
        open_file_preview $path
    }
}

proc ::vpanes::refresh_files {} {
    variable root_dir
    set w $::vpanes::R.files.tree
    $w delete [$w children {}]
    set root [string trim $root_dir]
    if {$root eq ""} { set root "~" }
    if {[catch {::vapi::get_json "/api/fs/tree?path=[::vapi::json_escape $root]&depth=2"} resp]} {
        # A TREEVIEW ITEM's text, not a widget option — there is no
        # registration channel for it, so a swap cannot re-text it. It
        # is rebuilt by the next refresh_files, which answers in the
        # current locale, so the staleness is bounded by the next
        # refresh rather than lasting the session. See
        # docs/i18n-runtime.md.
        $w insert {} end -text [::vi18n::mc {error: %s} $resp] ;# i18n-exempt: treeview item, rebuilt on refresh
        return
    }
    if {[dict exists $resp error]} {
        $w insert {} end -text [::vi18n::mc {error: %s} [dict get $resp error]] ;# i18n-exempt: treeview item, rebuilt on refresh
        return
    }
    foreach e [dict get $resp entries] {
        _tree_insert $w {} $e
    }
}

proc ::vpanes::_tree_insert {w parent e} {
    set name [dict get $e name]
    set path [dict get $e path]
    set is_dir [dict get $e is_dir]
    set id [$w insert $parent end -text $name -values [list $path $is_dir]]
    if {$is_dir} {
        # Placeholder child makes the disclosure arrow appear.
        $w insert $id end -id "$id.ph" -text "…"
        if {[dict exists $e children]} {
            $w delete "$id.ph"
            foreach c [dict get $e children] {
                _tree_insert $w $id $c
            }
        }
    }
}

proc ::vpanes::_file_selected {} {
    set w $::vpanes::R.files.tree
    set sel [$w selection]
    if {$sel eq ""} return
    set vals [$w item $sel -values]
    if {[llength $vals] < 2} return
    lassign $vals path is_dir
    if {$is_dir} {
        # Expand lazily: load one level if only the placeholder is there.
        set kids [$w children $sel]
        if {[llength $kids] == 1 && [$w item [lindex $kids 0] -text] eq "…"} {
            $w delete [lindex $kids 0]
            if {![catch {::vapi::get_json "/api/fs/tree?path=[::vapi::json_escape $path]&depth=1"} resp]} {
                if {[dict exists $resp entries]} {
                    foreach c [dict get $resp entries] {
                        _tree_insert $w $sel $c
                    }
                }
            }
        }
        return
    }
    show preview
    open_file_preview $path
}

# ── Review pane (git) ────────────────────────────────────────────────

namespace eval ::vpanes {
    variable review_repo ""
}

proc ::vpanes::_build_review {w} {
    ttk::frame $w
    ttk::frame $w.bar
    ttk::entry $w.bar.repo -textvariable ::vpanes::review_repo
    ttk::button $w.bar.go -text [::vi18n::mc {Status}] -command ::vpanes::refresh_review
    ttk::button $w.bar.stage -text [::vi18n::mc {Stage}] -command {::vpanes::_git_mutate stage}
    ttk::button $w.bar.unstage -text [::vi18n::mc {Unstage}] -command {::vpanes::_git_mutate unstage}
    ttk::button $w.bar.revert -text [::vi18n::mc {Revert}] -command ::vpanes::_git_revert_confirm
    pack $w.bar.go $w.bar.stage $w.bar.unstage $w.bar.revert -side right -padx 1
    pack $w.bar.repo -side left -fill x -expand 1

    ttk::treeview $w.files -columns {st path} -show headings -selectmode browse
    $w.files heading st -text " "
    $w.files heading path -text [::vi18n::mc {changed file}]
    $w.files column st -width 30 -stretch 0
    $w.files column path -width 200 -stretch 1

    text $w.diff -wrap none -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -font {monospace 10} \
        -highlightthickness 0 -relief flat -bd 0
    ttk::scrollbar $w.dsb -orient vertical -command [list $w.diff yview]
    $w.diff configure -yscrollcommand [list $w.dsb set]

    pack $w.bar -fill x -padx 2 -pady 2
    pack $w.files -fill x -padx 2
    pack $w.dsb -side right -fill y
    pack $w.diff -fill both -expand 1 -padx 2 -pady 2

    $w.diff tag configure add -foreground #50fa7b
    $w.diff tag configure del -foreground #ff5555
    $w.diff tag configure hdr -foreground $::vt::T(cyan) -font {monospace 10 bold}

    bind $w.files <<TreeviewSelect>> { ::vpanes::_diff_selected }
    ::vctx::attach $w.files {
        {"Show diff"  {::vpanes::_diff_selected}}
        {-}
        {"Stage"      {::vpanes::_git_mutate stage}}
        {"Unstage"    {::vpanes::_git_mutate unstage}}
        {"Revert…"    {::vpanes::_git_revert_confirm}}
        {-}
        {"Copy path"  {::vpanes::_ctx_copy_path}}
        {"Refresh"    {::vpanes::refresh_review}}
    }
    bind $w.bar.repo <Return> { ::vpanes::refresh_review }

    # Sensible default: the home dir's git repos are unknown until the
    # user picks one; empty state explains.
    if {$::vpanes::review_repo eq ""} {
        set ::vpanes::review_repo "~/veles-agent"
    }
}

proc ::vpanes::refresh_review {} {
    variable review_repo
    set w $::vpanes::R.review
    $w.files delete [$w.files children {}]
    set repo [string trim $review_repo]
    if {$repo eq ""} return
    if {[catch {::vapi::get_json "/api/git/status?repo=[::vapi::json_escape $repo]"} resp]} {
        _review_set_diff "error: $resp"
        return
    }
    if {[dict exists $resp error]} {
        _review_set_diff "git error: [dict get $resp error]"
        return
    }
    set branch [dict get $resp branch]
    _review_set_diff "# branch: $branch\n# select a file to see its diff\n"
    foreach f [dict get $resp files] {
        $w.files insert {} end -values [list [dict get $f status] [dict get $f path]]
    }
}

proc ::vpanes::_diff_selected {} {
    variable review_repo
    set w $::vpanes::R.review
    set sel [$w.files selection]
    if {$sel eq ""} return
    set vals [$w.files item $sel -values]
    set path [lindex $vals 1]
    if {[catch {::vapi::get_json "/api/git/diff?repo=[::vapi::json_escape $review_repo]&path=[::vapi::json_escape $path]"} resp]} {
        _review_set_diff "error: $resp"
        return
    }
    if {[dict exists $resp error]} {
        _review_set_diff "git error: [dict get $resp error]"
        return
    }
    _review_set_diff [dict get $resp diff]
}

proc ::vpanes::_review_set_diff {text} {
    set w $::vpanes::R.review.diff
    $w configure -state normal
    $w delete 1.0 end
    # Insert line-by-line and tag as we go (search-after-insert is O(n²)
    # and misfires on duplicate lines — a diff is full of them).
    foreach line [split $text \n] {
        set start [$w index end-1c]
        $w insert end $line\n
        set tag ""
        if {[string match "+*" $line] && ![string match "+++" $line]} { set tag add }
        if {[string match "-*" $line] && ![string match "---" $line]} { set tag del }
        if {[string match "@@*" $line] || [string match "diff *" $line]} { set tag hdr }
        if {$tag ne ""} {
            $w tag add $tag $start end-1c
        }
    }
    $w configure -state disabled
}

proc ::vpanes::_git_mutate {action} {
    variable review_repo
    set w $::vpanes::R.review
    set sel [$w.files selection]
    if {$sel eq ""} { return }
    set path [lindex [$w.files item $sel -values] 1]
    set body "{\"repo\":\"[::vapi::json_escape $review_repo]\",\"action\":\"$action\",\"path\":\"[::vapi::json_escape $path]\"}"
    if {[catch {::vapi::post /api/git/mutate $body} e]} {
        ::vstatus::set_msg [format [::vi18n::mc {git: %s}] $e]
    }
    refresh_review
}

proc ::vpanes::_git_revert_confirm {} {
    set sel [$::vpanes::R.review.files selection]
    if {$sel eq ""} return
    set path [lindex [$::vpanes::R.review.files item $sel -values] 1]
    if {[tk_messageBox -type yesno -icon warning -title "Revert" \
         -message [format [::vi18n::mc {Revert %s to HEAD? Uncommitted changes in this file are lost.}] $path]] eq "yes"} {
        _git_mutate revert
    }
}

# ── Preview pane ─────────────────────────────────────────────────────

proc ::vpanes::_build_preview {w} {
    ttk::frame $w
    ttk::frame $w.bar
    ttk::label $w.bar.label -text [::vi18n::mc {preview}] -foreground $::vt::T(fg_dim)
    ttk::button $w.bar.open -text [::vi18n::mc {Open in browser}] -command ::vpanes::_preview_open_browser
    # Only meaningful for a file the pane can RENDER — packed and
    # unpacked by _preview_paint rather than sitting there dead.
    ttk::button $w.bar.src -text [::vi18n::mc {Source}] -command ::vpanes::toggle_preview_source
    pack $w.bar.open -side right -padx 2
    pack $w.bar.label -side left -fill x -expand 1
    text $w.body -wrap word -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -font {monospace 10} \
        -highlightthickness 0 -relief flat -bd 0
    ttk::scrollbar $w.sb -orient vertical -command [list $w.body yview]
    $w.body configure -yscrollcommand [list $w.sb set]
    pack $w.bar -fill x -padx 2 -pady 2
    pack $w.sb -side right -fill y
    pack $w.body -fill both -expand 1
}

# What can this pane render, going by the file name?
#
# USER: "preview pane doesn't render markdown". It never had: every file
# went in as raw text in a fixed font, which is why the agent's attempt
# to work around the table bug by writing a .md file and previewing it
# showed the pipes and asterisks instead of a document.
proc ::vpanes::preview_kind_for {label} {
    # The label can carry a suffix ("path — url"); the path is the head.
    set path [lindex [split $label " "] 0]
    switch -- [string tolower [file extension $path]] {
        .md - .markdown - .mdown - .mkd - .mdwn { return markdown }
        default {
            # ::vmedia owns the decoder question — the preview pane must
            # not carry a second, staler list of image extensions.
            if {[::vmedia::is_image $path]} { return image }
            return plain
        }
    }
}

proc ::vpanes::toggle_preview_source {} {
    variable preview_source
    set preview_source [expr {![info exists preview_source] || !$preview_source}]
    _preview_paint
}

proc ::vpanes::_preview_empty_state {} {
    variable preview_raw
    variable preview_owner ""
    variable preview_kind plain
    variable preview_source 0
    set preview_raw "Nothing previewed yet.\n\nClick a file in the Files pane, or the agent can open_preview a URL or file here.\n"
    $::vpanes::R.preview.bar.label configure -text [::vi18n::mc {preview}]
    _preview_paint
}

proc ::vpanes::open_file_preview {path} {
    # Images render inline (Tk-native PNG/GIF). Don't hand them to the fs
    # text reader, which would just report "binary — not previewable":
    # USER: "the preview also doesn't preview pngs".
    if {[::vmedia::is_image $path] && [file isfile $path]} {
        _preview_set_image $path
        return
    }
    if {[catch {::vapi::get_json "/api/fs/read?path=[::vapi::json_escape $path]&limit=400"} resp]} {
        _preview_set "error: $resp" $path
        return
    }
    if {[dict exists $resp error]} {
        _preview_set "error: [dict get $resp error]" $path
        return
    }
    if {[dict exists $resp binary] && [dict get $resp binary]} {
        _preview_set "(binary file, [dict get $resp size] bytes — not previewable)" $path
        return
    }
    _preview_set [dict get $resp text] $path
}

proc ::vpanes::_preview_set_image {path} {
    variable last_preview
    variable preview_owner ""
    variable preview_raw
    variable preview_kind image
    variable preview_source 0
    set preview_raw $path
    $::vpanes::R.preview.bar.label configure -text $path
    _preview_paint
    set last_preview [list file $path]
}

proc ::vpanes::_preview_set {text label {kind auto}} {
    variable last_preview
    variable preview_owner ""
    variable preview_raw
    variable preview_kind
    variable preview_source
    set preview_raw $text
    set preview_kind [expr {$kind eq "auto" ? [preview_kind_for $label] : $kind}]
    # A new file starts rendered. Carrying the previous file's Source
    # state over is the kind of stickiness that reads as a bug.
    set preview_source 0
    $::vpanes::R.preview.bar.label configure -text $label
    _preview_paint
    set last_preview [list file $label]
}

proc ::vpanes::_preview_paint {} {
    variable preview_raw
    variable preview_kind
    variable preview_source
    variable preview_img
    if {![info exists preview_raw]}    { set preview_raw "" }
    if {![info exists preview_kind]}   { set preview_kind plain }
    if {![info exists preview_source]} { set preview_source 0 }
    set w $::vpanes::R.preview.body
    if {![winfo exists $w]} { return }
    set bar $::vpanes::R.preview.bar
    $w configure -state normal
    # Link tags are minted per rendering pass and keyed by tag name, so
    # the old file's have to go with the old file's text. Same for a
    # previous image: named Tk images are not GC'd, so drop it or every
    # re-preview leaks one.
    catch { ::vmd::forget_links $w }
    if {[info exists preview_img] && $preview_img ne ""} { catch { image delete $preview_img } }
    set preview_img ""
    $w delete 1.0 end
    if {$preview_kind eq "image"} {
        $w configure -font {monospace 10}
        # Fit the pane, not the file: a 1280x1577 photo inserted at its
        # native size makes the preview a horizontal scrollbar with a
        # picture behind it. Width is the pane's own, minus the padding.
        set cap [expr {[winfo width $w] - 24}]
        if {$cap < 80} { set cap 480 }
        set preview_img [::vmedia::load $preview_raw $cap 0]
        if {$preview_img ne ""} {
            $w image create end -image $preview_img -padx 6 -pady 6
            $w insert end "\n"
            ::vmedia::card $w $preview_raw -caption 1 -preview 0
        } else {
            # The ONLY thing shown for a format with no decoder (.webp,
            # .heic) — a card that names it and carries Open / Save as…
            # / Copy path, rather than a bare error string.
            ::vmedia::card $w $preview_raw -preview 0
        }
    } elseif {$preview_kind eq "markdown" && !$preview_source} {
        # Proportional: the whole point of rendering is that it stops
        # looking like a source listing.
        $w configure -font {TkDefaultFont 10}
        ::vmd::render $w $preview_raw
    } else {
        $w configure -font {monospace 10}
        $w insert end $preview_raw
    }
    $w configure -state disabled
    if {[winfo exists $bar.src]} {
        if {$preview_kind eq "plain" || $preview_kind eq "image"} {
            pack forget $bar.src
        } else {
            if {$preview_source} { $bar.src configure -text [::vi18n::mc {Rendered}] } else { $bar.src configure -text [::vi18n::mc {Source}] }
            pack $bar.src -side right -padx 2
        }
    }
    return
}

proc ::vpanes::_preview_open_browser {} {
    variable last_preview
    if {[llength $last_preview] == 2 && [lindex $last_preview 0] eq "url"} {
        ::vmd::open_url [lindex $last_preview 1]
    }
}

# Apply a preview item coming from the agent (POST /api/preview).
proc ::vpanes::_apply_preview_item {item} {
    set kind [dict get $item kind]
    show preview
    if {$kind eq "url"} {
        set url [dict get $item url]
        variable last_preview [list url $url]
        set label [dict get $item url]
        if {[dict exists $item label] && [dict get $item label] ne ""} {
            set label "[dict get $item label] — $url"
        }
        set w $::vpanes::R.preview.body
        variable preview_raw "URL preview: $url\n\nOpening in the browser…\n"
        variable preview_kind plain
        variable preview_source 0
        $::vpanes::R.preview.bar.label configure -text $label
        _preview_paint
        # The agent supplies this URL and nobody clicked anything, so
        # the scheme check is the only thing between a tool result and
        # xdg-open acting on it. `open_url` refuses anything that is not
        # http/https/mailto and says so in the status bar.
        ::vmd::open_url $url
    } elseif {$kind eq "file"} {
        open_file_preview [dict get $item path]
    }
}

# ── Terminal pane ────────────────────────────────────────────────────
# Tk has no embeddable terminal widget; the honest design is a launcher
# for the user's real terminal (wezterm on this box) pointed at the
# review repo dir, plus a command-runner for quick one-shot output.

proc ::vpanes::_build_terminal {w} {
    ttk::frame $w
    ttk::frame $w.bar
    ttk::label $w.bar.lbl -text [::vi18n::mc {run in:}] -foreground $::vt::T(fg_dim)
    ttk::entry $w.bar.dir -textvariable ::vpanes::review_repo
    ttk::button $w.bar.launch -text [::vi18n::mc {Open terminal}] -command ::vpanes::_term_launch
    pack $w.bar.launch -side right -padx 2
    pack $w.bar.lbl -side left
    pack $w.bar.dir -side left -fill x -expand 1 -padx 4

    ttk::frame $w.run
    ttk::entry $w.run.cmd
    ttk::button $w.run.go -text [::vi18n::mc {Run}] -command ::vpanes::_term_run
    pack $w.run.go -side right -padx 2
    pack $w.run.cmd -side left -fill x -expand 1

    text $w.out -wrap none -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -font {monospace 10} \
        -highlightthickness 0 -relief flat -bd 0
    ttk::scrollbar $w.osb -orient vertical -command [list $w.out yview]
    $w.out configure -yscrollcommand [list $w.osb set]

    pack $w.bar -fill x -padx 2 -pady 2
    pack $w.run -fill x -padx 2 -pady {0 2}
    pack $w.osb -side right -fill y
    pack $w.out -fill both -expand 1 -padx 2 -pady 2

    bind $w.run.cmd <Return> { ::vpanes::_term_run; break }
    $w.out insert end "Open terminal: launches wezterm/konsole in the repo dir (a full embedded PTY is not a Tk thing).\nRun: quick one-shot command, output shown below.\n"
}

proc ::vpanes::_term_launch {} {
    variable review_repo
    set dir [string trim $review_repo]
    if {$dir eq ""} { set dir "~" }
    if {[string match "~*" $dir]} {
        set dir [string replace $dir 0 0 $::env(HOME)]
    }
    foreach term {wezterm konsole xterm} {
        if {![catch {exec which $term}]} {
            # Not `spawn`: that sanitises EVERY argument, and a literal
            # flag must not be. `$dir` is the operator's repo path rather
            # than remote data, so this is uniformity, not a fix.
            catch {exec $term --directory [::vsafe::arg $dir] &}
            return
        }
    }
    $::vpanes::R.terminal.out configure -state normal
    $::vpanes::R.terminal.out insert end "\nno terminal emulator found (wezterm/konsole/xterm)\n"
    ::vtext::readonly $::vpanes::R.terminal.out
}

# The shell the Run line uses: bash where there is one, else the system's
# sh. A phone has /system/bin/sh and no bash, so the Run line answered
# every command there with "couldn't execute bash".
proc ::vpanes::_term_shell {} {
    foreach sh {bash sh} {
        set p [auto_execok $sh]
        if {$p ne ""} { return [lindex $p 0] }
    }
    if {[file executable /system/bin/sh]} { return /system/bin/sh }
    return sh
}

# Run one command, in the repo dir when there is one, WITHOUT blocking the
# window: its output arrives as the command writes it. A synchronous exec
# froze the whole desktop for as long as the command ran — on a phone,
# where the desktop is the only window, that read as a hang.
proc ::vpanes::_term_run {} {
    variable review_repo
    set cmd [string trim [$::vpanes::R.terminal.run.cmd get]]
    if {$cmd eq ""} return
    set dir [string trim $review_repo]
    if {[string match "~*" $dir]} {
        set dir [string replace $dir 0 0 $::env(HOME)]
    }
    if {$dir eq "" || ![file isdirectory $dir]} { set dir [pwd] }
    set out_w $::vpanes::R.terminal.out
    $out_w configure -state normal
    $out_w insert end "\n\$ $cmd\n"
    $out_w see end
    $out_w configure -state disabled
    # The directory and the command travel as the shell's positional
    # parameters, never spliced into its script.
    set script {cd "$1" || exit 1; eval "$2"}
    if {[catch {open [list | [_term_shell] -c $script veles $dir $cmd 2>@1] r} fh]} {
        _term_append $out_w "(error: $fh)\n"
        return
    }
    fconfigure $fh -blocking 0 -encoding utf-8 -translation auto
    fileevent $fh readable [list ::vpanes::_term_read $fh $out_w]
}

proc ::vpanes::_term_read {fh out_w} {
    set chunk [read $fh]
    if {$chunk ne ""} { _term_append $out_w $chunk }
    if {[eof $fh]} {
        fconfigure $fh -blocking 1
        if {[catch {close $fh} err opts]} {
            set code [lindex [dict get $opts -errorcode] end]
            if {[string is integer -strict $code]} {
                _term_append $out_w "(exit $code)\n"
            }
        }
        set ::vpanes::term_done [clock milliseconds]
    }
}

proc ::vpanes::_term_append {out_w text} {
    if {![winfo exists $out_w]} { return }
    $out_w configure -state normal
    $out_w insert end $text
    $out_w see end
    $out_w configure -state disabled
}

# ── Tasks pane: the agent's todo list + plan, where they belong ─────
#
# The todo list was only reachable through an overlay (C-S-O) that
# fetched once and closed — so an agent working through a multi-step
# task had its plan effectively invisible unless you kept re-opening
# it. USER: "agent todo/task list is not actually shown to user,
# should be accessible in right side panel, same for a plan if there
# is any plan".
#
# One endpoint feeds it: /api/sessions/:id/todos answers todos +
# plan_mode + the last presented plan together (the engine persists
# the plan text when present_plan runs, so it outlives the ask that
# carried it).

proc ::vpanes::_build_tasks {w} {
    ttk::frame $w
    ttk::frame $w.bar
    ttk::button $w.bar.refresh -text "⟳" -width 3 -command ::vpanes::tasks_refresh
    ttk::label $w.bar.mode -text "" -foreground $::vt::T(gold)
    ttk::label $w.bar.stale -text "" -foreground $::vt::T(fg_dim)
    pack $w.bar.refresh -side left -padx 2
    pack $w.bar.mode -side left -padx 6
    pack $w.bar.stale -side right -padx 4
    catch { ::va11y::name $w.bar.refresh [::vi18n::mc {refresh the task list}] }

    text $w.body -wrap word -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -font {TkDefaultFont 10} \
        -highlightthickness 0 -relief flat -bd 0 -padx 8 -pady 6
    catch { ::va11y::name $w.body [::vi18n::mc {the agent's task list and plan}] }
    pack $w.bar -fill x -padx 2 -pady 2
    pack $w.body -fill both -expand 1 -padx 2 -pady 2
}

# Fetch + repaint. Called on reveal, by the refresh button, and from
# chat.tcl when a `todo` tool round finishes or a turn ends — the list
# changes exactly then, and polling a loopback endpoint every 5s while
# visible covers everything else.
proc ::vpanes::tasks_refresh {} {
    variable tasks_payload
    if {![info exists tasks_payload]} { set tasks_payload "" }
    set w $::vpanes::R.tasks
    if {![winfo exists $w]} { return }
    set sid ""
    catch { set sid $::vchat::session }
    $w.bar.stale configure -text [clock format [clock seconds] -format %H:%M:%S]
    if {$sid eq ""} {
        $w.bar.mode configure -text ""
        ::vpanes::_tasks_body "no session selected — the list follows the session on screen" 1
        return
    }
    if {[catch {::vapi::get_json /api/sessions/$sid/todos} d]} {
        ::vpanes::_tasks_body "tasks unavailable: $d" 1
        return
    }
    set plan_mode 0
    catch { set plan_mode [dict get $d plan_mode] }
    $w.bar.mode configure -text [expr {$plan_mode ? [::vi18n::mc {▲ PLAN MODE}] : ""}]
    set lines {}
    set plan ""
    catch { set plan [dict get $d plan] }
    if {$plan ne "" && $plan ne "null"} {
        # PLAN MODE FIX 2 (USER 2026-09-13: "The Tasks pane doesn't
        # render the markdown for the plan"). The plan used to be
        # two-space-prefixed raw source — headings, bullets, fences,
        # bold all printed as `**…**` literals, and the indent
        # additionally killed markdown block structure. The plan
        # renders through ::vmd now, verbatim, like every settled
        # transcript text (chat.tcl on_done is the precedent).
        lappend lines "── plan ────────────────────────────"
        lappend lines ""
        lappend lines [string trimright $plan]
        lappend lines ""
    }
    lappend lines "── tasks ───────────────────────────"
    set items {}
    catch { set items [dict get $d todos] }
    if {[llength $items] == 0} {
        lappend lines "  (empty — the agent keeps its task list here when it plans multi-step work)"
    }
    foreach it $items {
        set status "pending"
        set content ""
        catch { set status [dict get $it status] }
        catch { set content [dict get $it content] }
        set mark "☐"
        switch -- $status {
            in_progress { set mark "▶" }
            completed   { set mark "✓" }
            cancelled   { set mark "✕" }
        }
        lappend lines "  $mark $content"
    }
    ::vpanes::_tasks_body [join $lines "\n"]
}

# Paint the Tasks body.
#
# PLAN MODE FIX 3 (USER 2026-09-13: "The Task pane returns to the top
# after a few seconds so it's impossible to scroll down and read the
# plan fully"). The 5s poll used to `delete 1.0 end` + insert
# unconditionally: the repaint discarded the view, so reading a long
# plan was a race against the timer. Two fixes, both here:
#   (a) a payload identical to what is on screen is a NO-OP — the
#       common case (nothing changed) costs no repaint at all;
#   (b) a real change restores the view anchor (`index @0,0` + the
#       x-view) after the repaint, so the reader stays where they
#       were. The todo/plan rows live in the same widget, so a task
#       list update mid-read no longer yanks the plan out from under
#       them either.
#
# `force` (error paths + explicit refresh button) repaints even when
# the payload matches, and still preserves the anchor.
#
# The payload renders through ::vmd::render: the plan section is real
# markdown (FIX 2), and the task rows are plain text that survives
# markdown unchanged (☐/▶/✓/✕ are not markup).
proc ::vpanes::_tasks_body {txt {force 0}} {
    variable tasks_payload
    if {![info exists tasks_payload]} { set tasks_payload "" }
    variable tasks_last_mark ""
    set w $::vpanes::R.tasks
    if {!$force && $txt eq $tasks_payload && [winfo ismapped $w.body]} {
        return
    }
    # An identical payload on a NOT-yet-mapped widget still paints
    # once (first reveal).
    set tasks_payload $txt
    $w.body configure -state normal
    # Save the reader's place: the first visible line and the x-view.
    set anchor [$w.body index @0,0]
    set xview [lindex [$w.body xview] 0]
    $w.body delete 1.0 end
    if {[catch { ::vmd::render $w.body $txt } err]} {
        # The renderer is not loadable in some harnesses — the plain
        # insert is the fallback, not the primary path.
        $w.body insert end "$txt\n"
    }
    # Restore the reader's place: never past the new end. `yview
    # <index>` puts the anchor line back at the TOP of the window;
    # `see` only made it visible, and Tk centres a line that is far
    # from the (just reset) view, so the restored position sat half a
    # window below the old one — a distance that depends on the font
    # height, which is why panetest's anchor case went red on the
    # laptop the day its monospace font changed (2026-10-20), on a
    # tree that was green the day before.
    catch { $w.body yview $anchor }
    catch { $w.body xview moveto $xview }
    ::vtext::readonly $w.body
}

# The 5s visibility poll: cheap loopback GET, and only while the pane
# is actually looking at it.
proc ::vpanes::_tasks_poll {} {
    variable current
    variable tasks_poll_after
    if {![info exists tasks_poll_after]} { set tasks_poll_after "" }
    if {$current eq "tasks" && [winfo exists $::vpanes::R.tasks]} {
        tasks_refresh
        set tasks_poll_after [after 5000 ::vpanes::_tasks_poll]
    } else {
        set tasks_poll_after ""
    }
}

# ── agent-driven polling (preview + focus) ───────────────────────────
# The agent's open_preview / focus_pane tools write state we pick up
# here. 1.5s poll — cheap, local loopback, and it keeps the window
# reactive without SSE plumbing for two rarely-changing values.

proc ::vpanes::poll_start {} {
    _poll_tick
}

# The phone, backgrounded (lib/android.tcl): the 1.5 s poll stops;
# poll_start re-arms it when the app comes back.
proc ::vpanes::poll_stop {} {
    variable poll_after
    if {[info exists poll_after] && $poll_after ne ""} {
        catch { after cancel $poll_after }
        set poll_after ""
    }
}

# ── attach ──────────────────────────────────────────────────────────
#
# The Files/Review/Preview panes hold gateway-local data (the file
# tree, git status, the last preview). None of it means anything
# against a different gateway, and the preview POLL would keep pulling
# the old gateway's blob stamps into the new one's pane.

proc ::vpanes::detach_clear {} {
    variable _seen_preview ""
    variable last_focus ""
    catch { $::vpanes::R.files.tree delete [$::vpanes::R.files.tree children {}] }
    catch { _review_set_diff "" }
    catch { _preview_empty_state }
    # File-tree + review + preview labels go back to their defaults.
    catch { $::vpanes::R.review.bar.repo configure -text "" }
}

proc ::vpanes::_poll_tick {} {
    variable poll_after
    variable last_focus
    # Unattached: no request leaves the process. The poll re-arms so
    # attaching revives it without extra plumbing.
    if {[::vapi::unattached]} {
        set poll_after [after 1500 ::vpanes::_poll_tick]
        return
    }
    # Asked, not waited on: a synchronous GET every 1.5 s was a nested
    # event loop that fired inside other waits (the 2026-10-04 hang).
    #
    # NOT passive, on purpose: it runs only while the desktop is on the
    # screen (a phone's `background` stops it, lib/android.tcl), and a
    # desktop on the screen is someone using the gateway — its next tap
    # needs the engine there. Marked passive (6c2eeb8) the phone's engine
    # stopped under an open, untouched desktop, and a message sent after
    # five minutes of reading met "gateway unreachable" until the app
    # went to the background and back (only a resume starts it).
    #
    # The preview asked for is the SHOWN session's. Asked for bare, the
    # last preview any session opened came back on every start (a fresh
    # window has seen no stamp), raised the pane and handed a days-old
    # url to the browser (owner, 2026-10-29). The gateway answers only
    # with a preview that session opened, and only while it is fresh
    # (panes.rs preview_for); nothing on screen, nothing asked.
    set sid [_shown_session]
    _preview_drop_foreign $sid
    if {$sid ne ""} {
        if {[catch {
            ::vapi::get_json_async "/api/preview?session=[::vapi::urlencode $sid]" \
                [list ::vpanes::_got_preview $sid]
        } err]} {
            after idle [list ::vpanes::_got_preview $sid 0 $err]
        }
    }
    catch { ::vapi::get_json_async /api/focus ::vpanes::_got_focus }
    set poll_after [after 1500 ::vpanes::_poll_tick]
}

proc ::vpanes::_shown_session {} {
    set sid ""
    catch { set sid $::vchat::session }
    return $sid
}

# The pane shows another session's agent preview: put it away. A file
# the user opened themselves has no owner and stays.
proc ::vpanes::_preview_drop_foreign {sid} {
    variable preview_owner
    if {$preview_owner ne "" && $preview_owner ne $sid} {
        catch { _preview_empty_state }
    }
}

proc ::vpanes::_got_preview {sid ok p} {
    variable preview_owner
    if {!$ok || $p eq "null" || $p eq ""} { return }
    # Asked for a session the window has since left.
    if {$sid ne [_shown_session]} { return }
    catch {
        set stamp [dict get $p set_at]
        if {$stamp ne $::vpanes::_seen_preview} {
            set ::vpanes::_seen_preview $stamp
            _apply_preview_item $p
            set preview_owner $sid
        }
    }
}

proc ::vpanes::_got_focus {ok f} {
    variable last_focus
    if {!$ok || $f eq "null" || $f eq ""} { return }
    catch {
        set stamp [dict get $f set_at]
        if {$stamp ne $last_focus} {
            set last_focus $stamp
            set pane [dict get $f pane]
            # chat/sessions are handled by the main window; the pane
            # shell owns files/review/terminal/preview.
            if {$pane in {files review terminal preview}} {
                show $pane
            }
            raise .
        }
    }
}
