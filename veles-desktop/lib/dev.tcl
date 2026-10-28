# veles-desktop/lib/dev.tcl — the Dev pane: REPL + mini-editor +
# inspector aimed at veles' GUTS, not just its plugin sandbox.
#
# Layer model (what's actually veles):
#   1. app   — THIS wish interpreter. The whole desktop (chat, panes,
#              keymap, dock) is Tcl; every proc is live and can be
#              redefined/redfined in place. This is the emacs-doctrine
#              primary target: the app is its own lisp.
#   2. core  — the Rust engine, reached through the loopback API
#              (/api/guards, /api/status, /api/sessions). Read-only
#              state views in the inspector.
#   3. gw    — the gateway's separate plugin interpreter
#              (POST /api/tcl/eval). For plugin development — a
#              SANDBOX, not the guts; secondary target.
#
# REPL: target toggle (C-c a app / C-c g gateway), prompt shows which.
# Editor: open any file; "Source buf (app)" evaluates the buffer in the
# live app — edit a lib/*.tcl, save, source, and the running window
# upgrades itself (the hot-patch contract, but interactive). "Eval buf
# (gw)" sends it to the gateway interpreter for plugin work.
# Inspector: widget tree / namespaces / proc body / engine state (app),
# procs & hooks (gw).

namespace eval ::vdev {
    variable repl_history {}
    variable repl_hidx 0
    variable current_file ""
    variable last_error ""
    # REPL + editor eval target: app | gw
    variable target "app"
    # Captured AT SOURCE TIME — inside procs `info script` is "" during
    # event handling, so lib-dir lookups must use this snapshot.
    variable lib_dir [file dirname [info script]]

    # ── THE EDITOR IN FOCUS ─────────────────────────────────────────
    #
    # Every helper below used to name $::vpanes::R.dev.edit.body
    # directly, so the entire editing apparatus — LSP navigation,
    # hover, completion, signature help, diagnostics, check, C-c C-c —
    # existed exactly once, in the Dev pane, and any other code buffer
    # in the app got a bare Text widget.
    #
    # USER: "hopefully the new script editor shares all the conveniences
    # of the repl/dev pane". It does, because there is now ONE editor
    # implementation and `::vdev::attach` points it at a widget.
    variable editor_w ""
    variable editor_file      ;# widget -> the path it edits
    variable editor_sha       ;# widget -> bytes loaded/saved last ("" = new file)
    variable editor_status_w  ;# widget -> where its status line goes
    variable editor_save_cmd  ;# widget -> what C-x C-s runs instead
    variable editor_dirty     ;# widget -> unsaved changes
    variable _mirror_root ""  ;# slopdrop mirror root, cached per process
    variable _mirror_root_fetched 0 ;# probed at least once

    # ── BUFFERS ─────────────────────────────────────────────────────
    #
    # USER: "make the dev editor detachable and re-attachable" — and
    # then: "Tab strip would also be nice."
    #
    # The Dev pane held exactly ONE editor, so opening a second file
    # threw the first one away: chase a definition across files and the
    # buffer you came from was gone, unsaved edits with it. Detach only
    # made that sharper — one editor, in a different window.
    #
    # Each buffer is its own Text widget, stacked in the same slot with
    # only the current one packed. That is what makes tabs cheap: the
    # undo stack, the insert mark, the scroll position, the syntax tags
    # and the LSP state are all per widget already, so switching is a
    # pack/unpack and nothing has to be saved and restored. It is also
    # what makes EVERY tab detachable rather than just "the editor" —
    # popping one out moves that buffer's widget to a toplevel and the
    # others carry on where they are.
    variable buffers {}       ;# ordered buffer ids — the strip's order
    variable buf_seq 0
    variable buf_w            ;# id -> its Text widget, docked or floating
    variable buf_path         ;# id -> the file it edits ("" = scratch)
    variable buf_title        ;# id -> what the tab says
    variable buf_float        ;# id -> its toplevel, or "" when docked
    variable buf_kill         ;# id -> armed to close despite being dirty
    variable buf_cur ""       ;# the id showing in the pane
}

# The Dev pane's own editor: the widget of whichever buffer is showing.
# Falls back to the historic single-editor path so a harness that builds
# one by hand — and the pane before its first buffer exists — still
# resolves. Not a constant anywhere else, so undocking (which rebuilds
# the pane at a new root) cannot leave a stale path.
proc ::vdev::dev_editor {} {
    variable buf_cur
    variable buf_w
    if {$buf_cur ne "" && [info exists buf_w($buf_cur)] && [winfo exists $buf_w($buf_cur)]} {
        return $buf_w($buf_cur)
    }
    return $::vpanes::R.dev.edit.body
}

# Which editor do the commands act on? The last one focused, falling
# back to the Dev pane's — so nothing changes for the Dev pane itself.
proc ::vdev::editor_widget {} {
    variable editor_w
    if {$editor_w ne "" && [winfo exists $editor_w]} { return $editor_w }
    return [dev_editor]
}

proc ::vdev::editor_path {} {
    variable editor_file
    variable current_file
    set w [editor_widget]
    if {[info exists editor_file($w)] && $editor_file($w) ne ""} {
        return $editor_file($w)
    }
    if {$w eq [dev_editor]} { return $current_file }
    return ""
}

proc ::vdev::editor_dirty {} {
    variable editor_dirty
    set w [editor_widget]
    return [expr {[info exists editor_dirty($w)] && $editor_dirty($w)}]
}

proc ::vdev::set_editor_dirty {v} { return [_set_dirty_for [editor_widget] $v] }

proc ::vdev::_set_dirty_for {w v} {
    variable editor_dirty
    variable buf_kill
    set editor_dirty($w) $v
    # An edit un-arms a pending close: "close again to discard" must not
    # stay armed across the next twenty keystrokes.
    set id [_buf_of_widget $w]
    if {$id ne "" && $v} { set buf_kill($id) 0 }
    catch { _buf_paint_strip }
    return
}

# Which buffer owns this widget, if any.
proc ::vdev::_buf_of_widget {w} {
    variable buffers
    variable buf_w
    foreach id $buffers { if {$buf_w($id) eq $w} { return $id } }
    return ""
}

# Give ANY text widget the Dev pane's editing apparatus.
#
#   -path    the file it edits (drives LSP didOpen, check and status)
#   -status  a label to write the status line into; without one the
#            messages go to the app status bar rather than into the Dev
#            pane, which would be somebody else's window
#   -save    what C-x C-s runs. An editor whose buffer is not a plain
#            file on disk — the failover hook goes through an endpoint
#            that SOURCES it before installing it — supplies its own.
proc ::vdev::attach {w args} {
    variable editor_file
    variable editor_status_w
    variable editor_save_cmd
    array set o {-path "" -status "" -save ""}
    array set o $args
    set editor_file($w) $o(-path)
    if {$o(-status) ne ""} { set editor_status_w($w) $o(-status) }
    if {$o(-save) ne ""}   { set editor_save_cmd($w) $o(-save) }

    bind $w <FocusIn> {+::vdev::_editor_focus %W}
    bind $w <Destroy> {+::vdev::_editor_gone %W}
    bind $w <<Modified>> { ::vdev::_editor_modified %W }
    # lsp-mode feel: tree-sitter highlighting via tcl-lsp (live server),
    # regex fallback (::vsyn::highlight) when the server is absent.
    bind $w <KeyRelease> { ::vdev::_edit_sync_schedule }
    bind $w <Control-x><Control-s> { ::vdev::editor_save ; break }
    bind $w <Control-c><Control-c> { ::vdev::editor_source_app ; break }
    bind $w <Alt-period> { ::vdev::_goto_def ; break }
    bind $w <Control-c><Control-d> { ::vdev::_goto_def ; break }
    # xref-find-references, and signature help on demand.
    bind $w <Alt-question> { ::vdev::_references ; break }
    bind $w <Control-c><Control-r> { ::vdev::_references ; break }
    bind $w <Control-c><Control-s> { ::vdev::_signature ; break }
    bind $w <Control-c><Control-o> { ::vdev::_outline ; break }
    bind $w <Control-c><Control-e> { ::vdev::_rename ; break }
    bind $w <Control-c><Control-h> { ::vdev::_highlight ; break }
    # lsp-ui-peek: read the definition where it is without going there.
    bind $w <Control-c><Control-p> { ::vdev::_peek_def ; break }
    # Folding and quick-fixes, the last two of the plan's Phase C.
    bind $w <Control-c><Control-f> { ::vdev::toggle_fold ; break }
    bind $w <Control-c><Control-u> { ::vdev::unfold_all ; break }
    bind $w <Control-c><Control-a> { ::vdev::code_action ; break }
    # Buffer motion, emacs-shaped: C-x <arrow> walks the strip, C-x k
    # kills the buffer, C-x 4 pops it into its own window. Bound on the
    # widget rather than in the keymap registry for the same reason
    # every other chord here is — they belong to an editor, not a mode.
    bind $w <Control-x><Right> { ::vdev::buf_next 1 ; break }
    bind $w <Control-x><Left>  { ::vdev::buf_next -1 ; break }
    bind $w <Control-x>k       { ::vdev::buf_close ; break }
    bind $w <Control-x>4       { ::vdev::pop_out ; break }
    # Typing an open paren is when you want the signature; moving off
    # the call is when you want it gone.
    bind $w <KeyRelease-parenleft> { ::vdev::_signature }
    bind $w <KeyRelease-space> { ::vdev::_signature }
    bind $w <Escape> { ::vdev::_signature_hide }
    bind $w <FocusOut> { ::vdev::_signature_hide }
    # Hover (lsp-ui-doc style): Motion -> debounced -> tooltip.
    bind $w <Motion> { ::vdev::_hover_schedule %x %y }
    bind $w <Leave> { ::vdev::_leave_editor }
    # Shift (anywhere in the editor) holds the tooltip open; releasing hands
    # it back to the hide timer — unless the pointer found its way inside.
    # %s is the numeric modifier mask on the event (NOT an `event state`
    # command — that does not exist and raising inside a binding pops the
    # bgerror dialog). 0x1 = ShiftLock/shift held: a release that still
    # carries the bit belongs to the OTHER shift (or a shift+letter), and
    # must not unpin.
    bind all <KeyPress-Shift_L> { ::vdev::_tip_shift 1 }
    bind all <KeyPress-Shift_R> { ::vdev::_tip_shift 1 }
    bind all <KeyRelease-Shift_L> { ::vdev::_tip_shift 0 }
    bind all <KeyRelease-Shift_R> { ::vdev::_tip_shift 0 }
    # Tab: LSP completion at point.
    bind $w <Tab> { ::vdev::_complete_at_point ; break }
    # Emacs motion, kill ring and region, same as every other editable
    # buffer in the app.
    catch { ::vemacs::install $w }
    _editor_focus $w
    # Paint what is already in the buffer. An editor that only
    # highlights once you type looks broken for as long as you are
    # reading rather than editing — which, for a hook you opened to see
    # what it does, is most of the time.
    if {$o(-path) ne ""} {
        catch { _lsp_open }
    } else {
        catch { _edit_sync_now }
    }
    # Right-click menu — the Dev editor AND the failover script editor
    # (which shares this attach) get the same menu as the transcript:
    # standard edit ops plus the LSP jumps. USER: "right-click context
    # menu in the editors/repl/dev would be nice".
    if {![catch { namespace which ::vctx::attach } ok] && $ok ne ""} {
        ::vctx::attach $w {
            {"Cut"    {event generate $::vctx::widget <<Cut>>}}
            {"Copy"   {event generate $::vctx::widget <<Copy>>}}
            {"Paste"  {event generate $::vctx::widget <<Paste>>}}
            {-}
            {"Go to definition" {::vdev::_goto_def}}
            {"Find references"  {::vdev::_references}}
            {"Rename symbol"    {::vdev::_rename}}
            {"Highlight symbol" {::vdev::_highlight}}
            {"Peek definition"  {::vdev::_peek_def}}
            {"Code actions"     {::vdev::code_action}}
            {"Fold / unfold"    {::vdev::toggle_fold}}
        }
    }
    return
}

# Tk queues <<Modified>> rather than firing it inline, so a
# programmatic fill — opening a file, restoring a buffer into a popped
# window — was reported as an EDIT one event-loop pass later, after the
# loader had already cleared the flag. Every freshly opened file came up
# marked dirty; invisible while the only sign was a status line nobody
# re-read, obvious the moment a tab carried a "•" and refused to close.
proc ::vdev::_editor_modified {w} {
    if {![winfo exists $w]} { return }
    # THE guard. Tk queues <<Modified>> rather than firing it inline, so
    # by the time this runs a programmatic fill has already put the flag
    # back to 0 — and an event whose flag is 0 is our own reset, not the
    # user typing. Without this every freshly opened file came up marked
    # dirty: invisible while the only sign was a status line nobody
    # re-read, obvious the moment a tab carried a "•" and refused to
    # close.
    if {![$w edit modified]} { return }
    _editor_focus $w
    _set_dirty_for $w 1
    editor_status "[editor_status_base] (modified)"
    $w edit modified 0
}

# Put TEXT in a buffer without it counting as an edit: reset Tk's own
# modified flag last, so the <<Modified>> events this generates are
# already stale when _editor_modified gets them.
proc ::vdev::_editor_fill {w text} {
    $w delete 1.0 end
    if {$text ne ""} { $w insert 1.0 $text }
    $w edit modified 0
    _set_dirty_for $w 0
    return
}

proc ::vdev::_editor_focus {w} {
    variable editor_w
    if {[winfo exists $w]} { set editor_w $w }
    return
}

# A destroyed editor must not stay "the editor in focus": every command
# would then act on a widget that is gone.
proc ::vdev::_editor_gone {w} {
    variable editor_w
    variable editor_file
    variable editor_status_w
    variable editor_save_cmd
    variable editor_dirty
    if {$editor_w eq $w} { set editor_w "" }
    foreach a {editor_file editor_status_w editor_save_cmd editor_dirty} {
        catch { unset ${a}($w) }
    }
    return
}

# ── the buffer list ─────────────────────────────────────────────────

# Where docked buffer widgets live. One frame, one packed child.
proc ::vdev::buf_stack {} { return $::vpanes::R.dev.edit.stack }
# The tabs live in a frame INSIDE a canvas, because a row of them
# eventually stops fitting: pack just clips the overflow, and a buffer
# you cannot see or click is a buffer you have lost. The canvas scrolls,
# the wheel drives it, and the active tab is scrolled into view.
proc ::vdev::buf_strip {} { return $::vpanes::R.dev.edit.tabs.f }
proc ::vdev::buf_strip_canvas {} { return $::vpanes::R.dev.edit.tabs }

# Is the tab machinery actually built? The Dev pane may not exist yet
# (nothing has opened it), and devtest drives ::vdev::attach against a
# hand-built widget with no pane at all.
proc ::vdev::buf_ready {} {
    return [expr {[winfo exists [buf_stack]] && [winfo exists [buf_strip]]}]
}

proc ::vdev::buf_list {} { variable buffers ; return $buffers }
proc ::vdev::buf_current {} { variable buf_cur ; return $buf_cur }

proc ::vdev::buf_path_of {id} {
    variable buf_path
    if {[info exists buf_path($id)]} { return $buf_path($id) }
    return ""
}

proc ::vdev::buf_widget {id} {
    variable buf_w
    if {[info exists buf_w($id)]} { return $buf_w($id) }
    return ""
}

# The buffer already editing PATH, if any. One tab per file: opening a
# file twice would give you two buffers whose unsaved edits race, and
# whichever saved last would win silently.
proc ::vdev::buf_for_path {path} {
    variable buffers
    variable buf_path
    if {$path eq ""} { return "" }
    set norm [file normalize $path]
    foreach id $buffers {
        if {$buf_path($id) ne "" && [file normalize $buf_path($id)] eq $norm} { return $id }
    }
    return ""
}

# Does the showing buffer hold anything worth keeping? An untouched
# *scratch* is a slot to fill, not a buffer to preserve.
proc ::vdev::_buf_current_is_used {} {
    variable buf_cur
    variable buf_path
    if {$buf_cur eq ""} { return 0 }
    if {$buf_path($buf_cur) ne ""} { return 1 }
    set w [buf_widget $buf_cur]
    if {$w eq "" || ![winfo exists $w]} { return 0 }
    return [expr {[string trim [$w get 1.0 end-1c]] ne ""}]
}

# Point an existing buffer at a different file (the *scratch* case).
proc ::vdev::_buf_retitle {id path} {
    variable buf_path
    variable buf_title
    variable buf_float
    if {![info exists buf_path($id)]} { return }
    set buf_path($id) $path
    set buf_title($id) [_buf_title_for $path]
    if {[buf_detached $id]} {
        catch { wm title $buf_float($id) [::vdev::_window_title $path] }
        catch { $buf_float($id).bar.path configure -text $path }
    }
    _buf_paint_strip
}

proc ::vdev::_buf_title_for {path} {
    if {$path eq ""} { return "*scratch*" }
    set t [file tail $path]
    # Bounded, because one long filename would otherwise push every
    # other tab off the visible strip.
    if {[string length $t] > 22} { set t "[string range $t 0 20]…" }
    return $t
}

# The WM title for an editor window showing $path ("" = scratch).
#
# ONE PLACE, because three call sites build it — buf_detach, the editor
# float and .vdevfloat — and they had drifted into three copies of the
# same ternary. Two of the three were plain literals, so the window said
# "veles editor" in every language (USER report 2026-09-26).
proc ::vdev::_window_title {path} {
    if {$path eq ""} { return [::vi18n::mc {veles editor}] }
    return [format [::vi18n::mc {veles — %s}] [file tail $path]]
}

# Make a new buffer and show it. Returns its id.
proc ::vdev::buf_new {{path ""}} {
    variable buffers
    variable buf_seq
    variable buf_w
    variable buf_path
    variable buf_title
    variable buf_float
    variable buf_kill
    if {![buf_ready]} { return "" }
    incr buf_seq
    set id "b$buf_seq"
    set w [buf_stack].$id
    text $w -wrap none -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -font {monospace 10} -highlightthickness 0 -relief flat \
        -insertbackground $::vt::T(pink)
    set buf_w($id) $w
    set buf_path($id) $path
    set buf_title($id) [_buf_title_for $path]
    set buf_float($id) ""
    set buf_kill($id) 0
    lappend buffers $id
    # -path is deliberately empty for a docked Dev buffer: the pane's
    # path entry writes into ::vdev::current_file, and buf_show keeps
    # that in step with whichever tab is showing.
    attach $w -status $::vpanes::R.dev.edit.status
    buf_show $id
    return $id
}

# Show buffer ID in the pane. A detached buffer is raised in its own
# window instead — it is not in the stack to pack.
proc ::vdev::buf_show {id} {
    variable buf_cur
    variable buf_w
    variable buf_path
    variable buf_float
    variable current_file
    if {![info exists buf_w($id)] || ![winfo exists $buf_w($id)]} { return 0 }
    if {$buf_float($id) ne "" && [winfo exists $buf_float($id)]} {
        raise $buf_float($id)
        focus $buf_w($id)
        _buf_paint_strip
        return 1
    }
    foreach child [winfo children [buf_stack]] { catch { pack forget $child } }
    pack $buf_w($id) -fill both -expand 1
    set buf_cur $id
    # The path entry is bound to current_file, so it has to follow the
    # tab or it would offer to save the file you are NOT looking at.
    set current_file $buf_path($id)
    _editor_focus $buf_w($id)
    focus $buf_w($id)
    editor_status [editor_status_base]
    _buf_paint_strip
    # Re-point the language server at the buffer now on screen. Without
    # this, doc_path still names the previously opened file and every LSP
    # query (hover, M-., peek, references, rename, signature, highlight,
    # code actions) answers for the wrong document at this buffer's cursor
    # coordinates — silently. The root probe inside ::vlsp::start is a
    # no-op when the project is unchanged (gateway reuses the server), so
    # a same-project tab switch costs one didClose + didOpen.
    #
    # The `count > 1` guard skips the empty widget that buf_new shows
    # before editor_open fills it — editor_open runs _lsp_open itself once
    # the text is in, and a didOpen of "" here would only make it flicker.
    if {[::vlsp::running] && $buf_path($id) ne "" \
            && $buf_path($id) ne [::vlsp::doc_path] \
            && [$buf_w($id) count -chars 1.0 end] > 1} {
        _lsp_open
    }
    return 1
}

proc ::vdev::buf_next {{delta 1}} {
    variable buffers
    variable buf_cur
    if {[llength $buffers] < 2} { return }
    set i [lsearch -exact $buffers $buf_cur]
    if {$i < 0} { set i 0 }
    set n [expr {(($i + $delta) % [llength $buffers] + [llength $buffers]) % [llength $buffers]}]
    buf_show [lindex $buffers $n]
}

# Close a buffer. A dirty one refuses ONCE and says so; asking again
# closes it — the emacs bargain, rather than a modal that steals focus
# or a silent discard.
proc ::vdev::buf_close {{id ""}} {
    variable buffers
    variable buf_cur
    variable buf_w
    variable buf_path
    variable buf_title
    variable buf_float
    variable buf_kill
    variable editor_dirty
    if {$id eq ""} { set id $buf_cur }
    if {![info exists buf_w($id)]} { return 0 }
    set w $buf_w($id)
    if {[info exists editor_dirty($w)] && $editor_dirty($w) && !$buf_kill($id)} {
        set buf_kill($id) 1
        editor_status "$buf_title($id) has unsaved changes — close again to discard"
        _buf_paint_strip
        return 0
    }
    set was_current [expr {$buf_cur eq $id}]
    set i [lsearch -exact $buffers $id]
    set buffers [lreplace $buffers $i $i]
    if {$buf_float($id) ne ""} { catch { destroy $buf_float($id) } }
    catch { destroy $w }
    foreach a {buf_w buf_path buf_title buf_float buf_kill} {
        catch { unset ${a}($id) }
    }
    if {$was_current} {
        set buf_cur ""
        if {[llength $buffers]} {
            buf_show [lindex $buffers [expr {$i > 0 ? $i - 1 : 0}]]
        } else {
            # Never leave the pane with no editor in it: an empty Dev
            # pane looks broken, and every command would have nothing to
            # act on.
            buf_new
        }
    }
    _buf_paint_strip
    return 1
}

# ── attach teardown ─────────────────────────────────────────────────
#
# Close every buffer on an attach switch: a buffer's PATH is a path on
# the gateway, and an unsaved buffer is content the old gateway may
# still want. buf_kill marks each buffer discardable, so no unsaved
# prompt interrupts a switch the user already chose.
proc ::vdev::close_all {} {
    variable buffers
    variable buf_kill
    foreach id $buffers {
        catch { set buf_kill($id) 1 }
    }
    foreach id [lreverse $buffers] {
        catch { buf_close $id }
    }
    # buf_close re-creates one empty buffer when the last one goes;
    # that is the unattached Dev pane's empty state, so leave it.
    return 1
}

# ── one tab, one window ─────────────────────────────────────────────
#
# Tk cannot reparent a widget across toplevels, so detaching is a
# REBUILD: capture the content, destroy the widget, build it in the
# other place, restore. Everything else about the buffer — its path, its
# dirty flag, its place in the strip — is keyed by id, not by widget
# path, so only the text has to move.

proc ::vdev::_buf_float_name {id} { return ".vdevfloat_$id" }

proc ::vdev::buf_detached {id} {
    variable buf_float
    return [expr {[info exists buf_float($id)] && $buf_float($id) ne ""
                  && [winfo exists $buf_float($id)]}]
}

# Marked as floating, whether or not the window is still there. The two
# differ exactly when the window went away behind our back — and a tab in
# that state is the one that must not be left stranded with a widget that
# no longer exists.
proc ::vdev::_buf_marked_float {id} {
    variable buf_float
    return [expr {[info exists buf_float($id)] && $buf_float($id) ne ""}]
}

proc ::vdev::buf_pop {{id ""}} {
    variable buf_cur
    variable buf_w
    variable buf_path
    variable buf_title
    variable buf_float
    variable editor_dirty
    if {$id eq ""} { set id $buf_cur }
    if {![info exists buf_w($id)] || [buf_detached $id]} { return 0 }
    set src $buf_w($id)
    if {![winfo exists $src]} { return 0 }
    set content [$src get 1.0 end-1c]
    set dirty [expr {[info exists editor_dirty($src)] && $editor_dirty($src)}]
    set path $buf_path($id)

    set top [_buf_float_name $id]
    catch { destroy $top }
    toplevel $top
    wm title $top [::vdev::_window_title $path]
    wm geometry $top 800x580
    wm minsize $top 440 280
    ttk::frame $top.bar
    ttk::label $top.bar.path -text $path -foreground $::vt::T(fg_dim) -font {monospace 9}
    ttk::button $top.bar.in -text [::vi18n::mc {⤢ attach}] -command [list ::vdev::buf_dock $id]
    pack $top.bar.in -side right -padx 4
    pack $top.bar.path -side left -fill x -expand 1 -padx 8 -pady 6
    pack $top.bar -fill x
    ttk::label $top.status -text "" -foreground $::vt::T(fg_dim)
    pack $top.status -fill x -padx 8
    text $top.body -wrap none -font {monospace 10} -highlightthickness 0 \
        -relief flat -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -insertbackground $::vt::T(pink)
    pack $top.body -fill both -expand 1 -padx 6 -pady {2 6}
    catch { destroy $src }
    set buf_w($id) $top.body
    set buf_float($id) $top
    attach $top.body -path $path -status $top.status
    _editor_fill $top.body $content
    set editor_dirty($top.body) $dirty
    # Closing the window is a re-attach, not a lost buffer: the tab is
    # still in the strip and the text has nowhere else to be.
    wm protocol $top WM_DELETE_WINDOW [list ::vdev::buf_dock $id]
    bind $top <Escape> [list ::vdev::buf_dock $id]
    # WM_DELETE_WINDOW covers the close button; <Destroy> covers
    # everything else (a `destroy` from a hot-patch, a session teardown).
    # Guarded on %W so a child's destroy does not re-enter.
    bind $top <Destroy> [list ::vdev::_buf_float_gone $id %W]
    catch { ::vtext::restyle_all $top }
    focus $top.body

    # The stack slot it left has to show SOMETHING.
    if {$buf_cur eq $id} {
        set other [_buf_first_docked $id]
        if {$other ne ""} { buf_show $other } else { buf_new }
    }
    _buf_paint_strip
    return 1
}

proc ::vdev::_buf_float_gone {id w} {
    variable buf_float
    if {![info exists buf_float($id)] || $w ne $buf_float($id)} { return }
    # Re-dock at idle: we are inside the toplevel's own destroy, and
    # rebuilding a widget tree from there is how you get a half-destroyed
    # window. By the time idle runs the teardown is finished.
    after idle [list ::vdev::buf_dock $id]
    return
}

proc ::vdev::_buf_first_docked {{except ""}} {
    variable buffers
    foreach id $buffers {
        if {$id eq $except} { continue }
        if {![buf_detached $id]} { return $id }
    }
    return ""
}

proc ::vdev::buf_dock {{id ""}} {
    variable buf_cur
    variable buf_w
    variable buf_path
    variable buf_float
    variable editor_dirty
    if {$id eq ""} {
        # No id: whichever one is floating, so a bare re-attach from a
        # keybinding still does the obvious thing.
        variable buffers
        foreach b $buffers { if {[_buf_marked_float $b]} { set id $b ; break } }
    }
    if {$id eq "" || ![info exists buf_w($id)] || ![_buf_marked_float $id]} { return 0 }
    # The window may already be gone — destroyed by something other than
    # the ⤢ button or the WM close hook. Then there is no text to
    # salvage, but the TAB must still come back: a buffer whose widget
    # does not exist is one every command would act on and miss.
    set src $buf_w($id)
    set content ""
    set dirty 0
    if {[winfo exists $src]} {
        catch { set content [$src get 1.0 end-1c] }
        set dirty [expr {[info exists editor_dirty($src)] && $editor_dirty($src)}]
    }
    set top $buf_float($id)
    set buf_float($id) ""
    catch { destroy $top }

    set w [buf_stack].$id
    catch { destroy $w }
    text $w -wrap none -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -font {monospace 10} -highlightthickness 0 -relief flat \
        -insertbackground $::vt::T(pink)
    set buf_w($id) $w
    attach $w -status $::vpanes::R.dev.edit.status
    _editor_fill $w $content
    set editor_dirty($w) $dirty
    buf_show $id
    return 1
}

# ── the strip ───────────────────────────────────────────────────────

proc ::vdev::_buf_paint_strip {} {
    variable buffers
    variable buf_cur
    variable buf_title
    variable buf_kill
    variable editor_dirty
    variable buf_w
    set f [buf_strip]
    if {![winfo exists $f]} { return }
    foreach c [winfo children $f] { destroy $c }
    set n 0
    foreach id $buffers {
        incr n
        set t $buf_title($id)
        set w $buf_w($id)
        if {[info exists editor_dirty($w)] && $editor_dirty($w)} { append t " •" }
        if {[buf_detached $id]} { append t " ⧉" }
        if {$buf_kill($id)} { append t " ?" }
        ttk::frame $f.t$n
        ttk::button $f.t$n.b -text $t -style [expr {$id eq $buf_cur
            ? "DevTabOn.TButton" : "DevTab.TButton"}] \
            -command [list ::vdev::buf_show $id]
        # Per-tab pop-out, which is the point: not "the editor" detaches,
        # any tab does, and the rest stay where they are.
        ttk::button $f.t$n.p -text [expr {[buf_detached $id] ? "⤢" : "⧉"}] -width 2 \
            -style DevTab.TButton \
            -command [list ::vdev::[expr {[buf_detached $id] ? "buf_dock" : "buf_pop"}] $id]
        ttk::button $f.t$n.x -text "×" -width 2 -style DevTab.TButton \
            -command [list ::vdev::buf_close $id]
        pack $f.t$n.b $f.t$n.p $f.t$n.x -side left
        pack $f.t$n -side left -padx {0 4}
        catch { ::vi18n::aname $f.t$n.p {pop %s out into its own window} $buf_title($id) }
        catch { ::vi18n::aname $f.t$n.x {close %s} $buf_title($id) }
    }
    ttk::button $f.new -text "+" -width 2 -style DevTab.TButton \
        -command { ::vdev::buf_new }
    pack $f.new -side left
    catch { ::va11y::name $f.new [::vi18n::mc {new buffer}] }
    _buf_strip_fit
    return
}

# Keep the canvas's scrollregion honest and the active tab on screen.
proc ::vdev::_buf_strip_fit {} {
    variable buffers
    variable buf_cur
    set c [buf_strip_canvas]
    set f [buf_strip]
    if {![winfo exists $c] || ![winfo exists $f]} { return }
    update idletasks
    set h [winfo reqheight $f]
    if {$h > 1} { $c configure -height $h }
    set wid [winfo reqwidth $f]
    $c configure -scrollregion [list 0 0 $wid $h]
    # Scroll the active tab into view. A strip that silently clips the
    # tab you are editing is the same failure as not having tabs.
    set i [lsearch -exact $buffers $buf_cur]
    if {$i < 0} { return }
    set tab $f.t[expr {$i + 1}]
    if {![winfo exists $tab]} { return }
    set view [winfo width $c]
    if {$view <= 1 || $wid <= $view} { $c xview moveto 0 ; return }
    set x [expr {[winfo x $tab] * 1.0}]
    set right [expr {$x + [winfo reqwidth $tab]}]
    set left [expr {[lindex [$c xview] 0] * $wid}]
    if {$x < $left} {
        $c xview moveto [expr {$x / $wid}]
    } elseif {$right > $left + $view} {
        $c xview moveto [expr {($right - $view) / $wid}]
    }
    return
}

proc ::vdev::_buf_strip_wheel {c delta button} {
    # X11 sends wheel as Button-4/5 with no %D; Windows/macOS send %D.
    set step 0
    if {$button eq "4"} { set step -1 } elseif {$button eq "5"} { set step 1 }
    if {$step == 0 && $delta ne "??" && $delta ne ""} {
        set step [expr {$delta > 0 ? -1 : 1}]
    }
    if {$step != 0} { $c xview scroll $step units }
    return
}

# dev.tcl is sourced before panes.tcl; declare the namespace so the
# _build_dev proc can live in ::vpanes.
namespace eval ::vpanes {}

# ── the eval core: one dispatcher, two interpreters ─────────────────

# Evaluate code in the chosen interpreter. Returns a two-element list
# {ok result} — ok=0 carries the error text.
proc ::vdev::eval_in {tgt code} {
    if {$tgt eq "app"} {
        # The emacs-doctrine path: run in THIS interpreter, global
        # scope. The app rewrites itself.
        if {[catch {uplevel #0 $code} result]} {
            global errorInfo
            return [list 0 "$result\n$errorInfo"]
        }
        return [list 1 $result]
    }
    # gw: the gateway's plugin interpreter over loopback. Build the
    # JSON manually — arbitrary code looks dict-shaped to _json_value.
    set body "{\"code\":\"[::vapi::json_escape $code]\"}"
    if {[catch {::json::json2dict [::vapi::post /api/tcl/eval $body]} resp]} {
        return [list 0 "transport: $resp"]
    }
    if {[dict exists $resp ok] && [dict get $resp ok]} {
        return [list 1 [dict get $resp result]]
    }
    # `[dict exists $resp result ? ... : ...]` was here: Tcl has no
    # ternary inside a command, so `dict exists` was handed four
    # arguments and threw — the gw-eval FAILURE path was itself broken,
    # which is why a bad plugin eval reported nothing useful.
    if {[dict exists $resp result]} { return [list 0 [dict get $resp result]] }
    return [list 0 "unknown error"]
}

# ── build ────────────────────────────────────────────────────────────

proc ::vpanes::_build_dev {w} {
    ttk::frame $w
    panedwindow $w.pw -orient vertical -sashwidth 5 -sashrelief flat \
        -background $::vt::T(hairline) -borderwidth 0

    # ── REPL ──
    ttk::frame $w.repl
    ttk::frame $w.repl.bar
    ttk::label $w.repl.bar.mode -textvariable ::vdev::_mode_label \
        -foreground $::vt::T(accent) -font {TkDefaultFont 9 bold}
    set ::vdev::_mode_label "app"
    ;# keymap-literal: widget-local emacs chords, bound directly on the
    ;# REPL/editor widgets, so they work in every mode and are not
    ;# registry rows. The label said "C-c t" while the binding is
    ;# <Control-c><Control-t> — C-c C-t.
    ttk::button $w.repl.bar.toggle -text [::vi18n::mc {toggle (C-c C-t)}] -width 16 \
        -command { ::vdev::toggle_target }
    ttk::label $w.repl.bar.hint -text [::vi18n::mc {↑/↓ history · app = this window, gw = plugins}] \
        -foreground $::vt::T(fg_dim)
    # The gate on agent-driven evals into THIS interpreter. Unchecked at
    # every start: an arm that survived a restart would be a standing
    # invitation nobody remembered issuing.
    ttk::checkbutton $w.repl.bar.arm -text [::vi18n::mc {arm live patching}] \
        -variable ::vdev::armed \
        -command { ::vdev::set_arm $::vdev::armed }
    catch { ::va11y::name $w.repl.bar.arm \
        [::vi18n::mc {let an agent evaluate Tcl in this window}] }
    pack $w.repl.bar.mode -side left -padx {0 6}
    pack $w.repl.bar.toggle -side left
    pack $w.repl.bar.arm -side left -padx {8 0}
    pack $w.repl.bar.hint -side right
    text $w.repl.out -height 6 -wrap word -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -font {monospace 10} -state disabled \
        -highlightthickness 0 -relief flat
    entry $w.repl.in -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -insertbackground $::vt::T(pink) -font {monospace 10}
    pack $w.repl.bar -fill x
    pack $w.repl.out -fill both -expand 1
    pack $w.repl.in -fill x -pady {2 0}

    bind $w.repl.in <Return> { ::vdev::repl_eval [string trim [%W get]] ; %W delete 0 end ; break }
    bind $w.repl.in <Up>   { ::vdev::repl_hist -1 ; break }
    bind $w.repl.in <Down> { ::vdev::repl_hist 1 ; break }
    bind $w.repl.in <Control-c><Control-t> { ::vdev::toggle_target ; break }
    bind $w.repl.in <Control-c><Control-a> { ::vdev::set_target app ; break }
    bind $w.repl.in <Control-c><Control-g> { ::vdev::set_target gw ; break }

    # Right-click menus: the REPL output is read-only (copy), the input is
    # an entry (cut/copy/paste). USER: "right-click context menu in the
    # editors/repl/dev would be nice".
    catch {
        ::vctx::attach $w.repl.out {
            {"Copy" {event generate $::vctx::widget <<Copy>>}}
        }
        ::vctx::attach $w.repl.in {
            {"Cut"    {event generate $::vctx::widget <<Cut>>}}
            {"Copy"   {event generate $::vctx::widget <<Copy>>}}
            {"Paste"  {event generate $::vctx::widget <<Paste>>}}
            {-}
            {"Clear"  {$::vctx::widget delete 0 end}}
        }
    }

    # ── editor ──
    ttk::frame $w.edit
    ttk::frame $w.edit.bar
    ttk::entry $w.edit.bar.path -textvariable ::vdev::current_file -font {monospace 9}
    ttk::button $w.edit.bar.open -text [::vi18n::mc {Open}] -command { ::vdev::editor_open }
    ;# keymap-literal: widget-local, see the REPL toolbar above.
    ttk::button $w.edit.bar.save -text [::vi18n::mc {Save (C-x C-s)}] -command { ::vdev::editor_save }
    ;# keymap-literal: widget-local, see the REPL toolbar above.
    ttk::button $w.edit.bar.source -text [::vi18n::mc {Source→app (C-c C-c)}] \
        -command { ::vdev::editor_source_app }
    ttk::button $w.edit.bar.evalgw -text [::vi18n::mc {Eval→gw}] -command { ::vdev::editor_eval_gw }
    ttk::button $w.edit.bar.check -text [::vi18n::mc {Check (lsp)}] -command { ::vdev::editor_check }
    ttk::button $w.edit.bar.outline -text [::vi18n::mc {Outline}] -command { ::vdev::_outline }
    ttk::button $w.edit.bar.def -text [::vi18n::mc {Def (M-.)}] -command { ::vdev::_goto_def }
    ;# keymap-literal: widget-local, see the REPL toolbar above.
    ttk::button $w.edit.bar.peek -text [::vi18n::mc {Peek (C-c C-p)}] -command { ::vdev::_peek_def }
    ;# keymap-literal: widget-local, Dev editor update chords follow emacs, not the keymap.
    ttk::button $w.edit.bar.rename -text [::vi18n::mc {Rename (C-c C-e)}] -command { ::vdev::_rename }
    ttk::button $w.edit.bar.hl -text [::vi18n::mc {Highlight (C-c C-h)}] -command { ::vdev::_highlight }
    ttk::button $w.edit.bar.pop -text "⧉" -command { ::vdev::pop_out }
    ::va11y::name $w.edit.bar.pop [::vi18n::mc {pop the current buffer out into its own window}]

    # ── slopdrop mirror (plan item 3) ──
    # One button, honest about what it does: it PULLS slopdrop's live
    # state into the mirror, overwriting local proc files. Local edits
    # never ride this button — only /slopdrop save deploys them.
    ttk::button $w.edit.bar.slopdrop -text [::vi18n::mc {⟳ slopdrop sync}] \
        -command { ::vdev::_slopdrop_sync_click }
    ::va11y::name $w.edit.bar.slopdrop [::vi18n::mc {re-sync the slopdrop state mirror}]

    pack $w.edit.bar.slopdrop -side left -padx 1
    pack $w.edit.bar.open $w.edit.bar.save $w.edit.bar.source \
        $w.edit.bar.evalgw $w.edit.bar.def $w.edit.bar.peek $w.edit.bar.outline \
        $w.edit.bar.rename $w.edit.bar.hl $w.edit.bar.check $w.edit.bar.pop \
        -side right -padx 1
    pack $w.edit.bar.path -side left -fill x -expand 1 -padx {0 4}
    # Quick-open row: the guts' own source files, one click each.
    ttk::frame $w.edit.lib
    ttk::label $w.edit.lib.h -text [::vi18n::mc {guts:}] -foreground $::vt::T(fg_dim)
    pack $w.edit.lib.h -side left
    foreach f {chat.tcl panes.tcl dev.tcl keymap.tcl dock.tcl editline.tcl theme.tcl overlays.tcl} {
        ttk::button $w.edit.lib.[string map {. _} $f] -text $f -width 12 \
            -command [list ::vdev::open_lib $f]
        pack $w.edit.lib.[string map {. _} $f] -side left -padx 1
    }
    ttk::button $w.edit.lib.patches -text [::vi18n::mc {patches.tcl}] -width 11 \
        -command { ::vdev::open_patches }
    pack $w.edit.lib.patches -side left -padx {6 1}
    # The tab strip. Buffers live in .stack, one packed at a time; the
    # strip is the only thing that knows they are plural.
    canvas $w.edit.tabs -height 26 -highlightthickness 0 -borderwidth 0 \
        -background $::vt::T(bg)
    ttk::frame $w.edit.tabs.f
    $w.edit.tabs create window 0 0 -anchor nw -window $w.edit.tabs.f -tags stripwin
    # The wheel over the strip scrolls the strip — the natural gesture
    # when a row of tabs runs off the edge.
    foreach ev {<Button-4> <Button-5> <MouseWheel>} {
        bind $w.edit.tabs $ev { ::vdev::_buf_strip_wheel %W %D %b }
    }
    ttk::frame $w.edit.stack
    ttk::label $w.edit.status -text [::vi18n::mc {no file — pick a guts file or type a path}] \
        -foreground $::vt::T(fg_dim)
    pack $w.edit.bar -fill x
    pack $w.edit.tabs -fill x -pady {2 0}
    pack $w.edit.lib -fill x -pady {2 0}
    pack $w.edit.status -fill x
    pack $w.edit.stack -fill both -expand 1
    # The Dev pane is now just the first editor rather than the only
    # one: every binding lives in ::vdev::attach, which any other code
    # buffer in the app can call, and every buffer here is one of those.
    # Its path stays in `current_file` (the path entry writes straight
    # into it) and buf_show keeps that in step with the visible tab.
    ::vdev::buf_new
    bind $w.edit.bar.path <Return> { ::vdev::editor_open ; break }

    # ── inspector ──
    ttk::frame $w.dbg
    ttk::label $w.dbg.h -text [::vi18n::mc {inspector — app internals · core state · gw plugins}] \
        -foreground $::vt::T(fg_dim)
    ttk::frame $w.dbg.bar
    # The label reaches the button through ::vi18n::label, and it must:
    # `-text $label` never consulted the catalog, so `widgets` and
    # `namespaces` were English in all 46 locales while their multi-word
    # siblings (`proc body`, `engine state`) WERE translated. That split
    # is what made it look like a missing translation rather than a
    # missing CALL — a probe of the LIVE window named the two strings,
    # every static check in the tree had passed them.
    #
    # label() also registers the widget, so the bar re-texts on a locale
    # swap; mc() resolves once, at creation.
    foreach {id label} {
        widgets "widgets" namespaces "namespaces" procsrc "proc body"
        engine "engine state" gwprocs "gw procs" gwhooks "gw hooks"
        err "last error"
    } {
        ttk::button $w.dbg.bar.$id -text [::vi18n::mc $label] \
            -command [list ::vdev::inspect $id]
        ::vi18n::label $w.dbg.bar.$id $label
        pack $w.dbg.bar.$id -side left -padx 1
    }
    entry $w.dbg.arg -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -insertbackground $::vt::T(pink) -font {monospace 9} -relief flat \
        -highlightthickness 0
    pack $w.dbg.bar -fill x
    pack $w.dbg.arg -fill x -pady {2 0}
    text $w.dbg.out -height 5 -wrap word -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -font {monospace 9} -state disabled \
        -highlightthickness 0 -relief flat
    pack $w.dbg.h -fill x
    pack $w.dbg.out -fill both -expand 1
    bind $w.dbg.arg <Return> { ::vdev::inspect_from_arg ; break }

    $w.pw add $w.repl -minsize 80 -stretch never
    $w.pw add $w.edit -minsize 140 -stretch always
    $w.pw add $w.dbg -minsize 70 -stretch never
    pack $w.pw -fill both -expand 1
}

# ── target switching ─────────────────────────────────────────────────

proc ::vdev::set_target {tgt} {
    variable target $tgt
    variable _mode_label
    set _mode_label $tgt
}

proc ::vdev::toggle_target {} {
    variable target
    set_target [expr {$target eq "app" ? "gw" : "app"}]
}

# ── REPL ─────────────────────────────────────────────────────────────

proc ::vdev::_repl_print {tag text} {
    set w $::vpanes::R.dev.repl.out
    $w configure -state normal
    $w insert end "$text\n" $tag
    $w see end
    $w configure -state disabled
}

proc ::vdev::repl_eval {code_str} {
    variable repl_history
    variable repl_hidx
    variable target
    if {[string trim $code_str] eq ""} return
    lappend repl_history $code_str
    set repl_hidx [llength $repl_history]
    _repl_print in "$target» $code_str"
    lassign [eval_in $target $code_str] ok result
    if {$ok} {
        if {$result ne ""} { _repl_print out $result }
    } else {
        variable last_error $result
        _repl_print err "error: $result"
    }
}

proc ::vdev::repl_hist {delta} {
    variable repl_history
    variable repl_hidx
    set n [llength $repl_history]
    if {$n == 0} return
    set repl_hidx [expr {$repl_hidx + $delta}]
    if {$repl_hidx < 0} { set repl_hidx 0 }
    if {$repl_hidx > $n} { set repl_hidx $n }
    set w $::vpanes::R.dev.repl.in
    $w delete 0 end
    if {$repl_hidx < $n} {
        $w insert 0 [lindex $repl_history $repl_hidx]
    }
}

# ── editor ───────────────────────────────────────────────────────────

proc ::vdev::editor_status_base {} {
    set f [editor_path]
    if {$f eq ""} { return "no file" }
    return "[file tail $f][expr {[editor_dirty] ? " •" : ""}]"
}

proc ::vdev::editor_status {msg} {
    variable editor_status_w
    set w [editor_widget]
    if {[info exists editor_status_w($w)] && [winfo exists $editor_status_w($w)]} {
        $editor_status_w($w) configure -text $msg
        return
    }
    if {$w eq [dev_editor] && [winfo exists $::vpanes::R.dev.edit.status]} {
        $::vpanes::R.dev.edit.status configure -text $msg
        return
    }
    # An attached editor with no status line of its own says it in the
    # app status bar. Writing into the Dev pane's label would put the
    # message in a window the user is not looking at.
    catch { ::vstatus::set_msg $msg }
}

proc ::vdev::editor_open {} {
    variable current_file
    set path [string trim $current_file]
    if {$path eq ""} { editor_status "type a path first"; return }
    # One tab per file. Opening something already open focuses its tab
    # rather than stacking a second copy of it — two buffers on one file
    # race, and whichever saved last would win silently. A tab with
    # unsaved edits is shown as-is rather than re-read from disk.
    if {[buf_ready]} {
        set id [buf_for_path $path]
        if {$id ne ""} {
            buf_show $id
            if {[editor_dirty]} {
                editor_status "[editor_status_base] — unsaved changes, not re-read"
                return
            }
        } elseif {[_buf_current_is_used]} {
            # The showing buffer already holds something; give the new
            # file its own tab instead of throwing that away. This is
            # what makes a cross-file go-to-definition non-destructive.
            set id [buf_new $path]
        } else {
            _buf_retitle [buf_current] $path
        }
    }
    set w [editor_widget]
    # There is always meant to be a buffer, but "meant to" is not a
    # guarantee: with no Dev pane built, editor_widget falls back to a
    # path that does not exist, and `$w delete` would throw out of a
    # keybinding into bgerror rather than saying anything useful.
    if {![winfo exists $w]} { editor_status "no editor to open into"; return }
    # An editor needs the WHOLE file. /api/fs/read is the Preview pane's
    # HEAD endpoint (200 lines by default) — opening a buffer through it
    # truncated everything past the limit, and the LSP inherited the
    # truncation: didOpen carried the cut text, so the server diagnosed
    # "missing braces" on a document that ended mid-block.
    if {[catch {::vapi::get_json "/api/fs/file?path=[::vapi::json_escape $path]"} resp]} {
        editor_status "open failed: $resp"; return
    }
    if {[dict exists $resp error]} {
        editor_status "open failed: [dict get $resp error]"; return
    }
    if {[dict exists $resp binary] && [dict get $resp binary]} {
        editor_status "binary file — refusing to edit"; return
    }
    _editor_fill $w [dict get $resp text]
    variable editor_sha
    set editor_sha($w) [expr {[dict exists $resp sha256] ? [dict get $resp sha256] : ""}]
    _lsp_open
    # A go-to-definition that landed in another file jumps once the text
    # is actually on screen.
    if {$::vdev::_jump_after_open ne ""} {
        set pos $::vdev::_jump_after_open
        set ::vdev::_jump_after_open ""
        _apply_jump $pos
    }
}

# Quick-open the app's own source files (the guts).
proc ::vdev::open_lib {name} {
    variable current_file
    variable lib_dir
    set current_file [file normalize [file join $lib_dir $name]]
    editor_open
}

proc ::vdev::open_patches {} {
    variable current_file
    # The SAME path ::vhotpatch loads, from the same helper. These two used
    # to agree on ~/.veles/desktop/patches.tcl, which is why the bug was
    # invisible: the editor opened a file, the loader read it back, and
    # neither was where the migration had put the user's patches — so this
    # created a second, ignored stub on every migrated box.
    set current_file [::vpaths::patch_file]
    # patches.tcl may not exist yet — create a stub so the buffer opens.
    if {![file exists $current_file]} {
        catch {
            file mkdir [file dirname $current_file]
            set f [open $current_file w]
            puts $f "# hot-patch file — sourced at startup and on Reload UI patches.\n# Must be idempotent.\n"
            close $f
        }
    }
    editor_open
}

# ── slopdrop mirror (plan item 3) ────────────────────────────────────
#
# A buffer whose path lives under the mirror root is NOT a plain file:
# saving it deploys the proc through slopdrop's interpreter (the only
# write path that persists and commits correctly). `editor_save`
# intercepts on this; the status line says where the save went, because
# a save that reached a different place than the user thinks is the
# silent automation the notes doctrine forbids.

# The mirror root, from GET /api/plugins/slopdrop/status. Cached for the process —
# the mirror moves only on reconfig, which restarts the gateway and
# this window with it. "" = unknown (probe failed or never ran).
proc ::vdev::_slopdrop_mirror_root {} {
    variable _mirror_root
    variable _mirror_root_fetched
    if {!$_mirror_root_fetched} {
        set root ""
        if {![catch {::vapi::get_json /api/plugins/slopdrop/status} resp] && [dict exists $resp root]} {
            set root [dict get $resp root]
        }
        set _mirror_root $root
        set _mirror_root_fetched 1
    }
    return $_mirror_root
}

# Is this file a mirror proc? Deliberately a PREFIX test, not an exact
# path: [file normalize] collisions with symlinked layouts would make an
# exact membership test fragile, and the writer-side refuses anything
# outside procs/ anyway (belt and braces, the writer is the gate).
proc ::vdev::_slopdrop_is_mirror {path} {
    set root [_slopdrop_mirror_root]
    if {$root eq "" || $path eq ""} { return 0 }
    set n [file normalize $path]
    set r [file normalize $root]
    return [string match "$r/*" $n]
}

# The toolbar button: pull slopdrop's live state into the mirror. A
# dirty mirror buffer is NOT silently clobbered — the reload is
# refused and the user is told to reconcile (that is the same choice
# `editor_open` makes for any dirty tab).
proc ::vdev::_slopdrop_sync_click {} {
    variable editor_dirty
    set dirty ""
    if {[buf_ready]} {
        foreach id [buf_list] {
            set p [buf_path_of $id]
            set w [_buf_of_widget $id]
            if {$w ne "" && [info exists editor_dirty($w)] && $editor_dirty($w) &&
                [_slopdrop_is_mirror $p]} {
                lappend dirty [file tail $p]
            }
        }
    }
    if {[llength $dirty] > 0} {
        editor_status "not syncing — unsaved mirror buffers: [join $dirty {, }]"
        return
    }
    if {[catch {::json::json2dict [::vapi::post /api/plugins/slopdrop/sync {}]} resp]} {
        editor_status "slopdrop sync failed: $resp"
        return
    }
    if {[dict exists $resp ok] && ![dict get $resp ok]} {
        set why [expr {[dict exists $resp error] ? [dict get $resp error] : "refused"}]
        editor_status "slopdrop sync failed: $why"
        return
    }
    set procs [expr {[dict exists $resp procs] ? [dict get $resp procs] : "?"}]
    set vars [expr {[dict exists $resp vars] ? [dict get $resp vars] : "?"}]
    editor_status "slopdrop synced: $procs procs, $vars vars"
}

proc ::vdev::editor_save {} {
    variable editor_save_cmd
    # A buffer that is not a plain file on disk saves its own way.
    set ed [editor_widget]
    if {[info exists editor_save_cmd($ed)] && $editor_save_cmd($ed) ne ""} {
        uplevel #0 $editor_save_cmd($ed)
        return 1
    }
    set path [string trim [editor_path]]
    if {$path eq ""} { editor_status "no path to save to"; return }
    set content [[editor_widget] get 1.0 end-1c]
    # slopdrop mirror proc: deploy through the interpreter, not as a
    # plain file write.
    if {[_slopdrop_is_mirror $path]} {
        set sbody "{\"path\":\"[::vapi::json_escape $path]\",\"user\":\"local:owner\"}"
        if {[catch {::vapi::post /api/plugins/slopdrop/save $sbody} raw]} {
            editor_status "slopdrop save failed: $raw"; return
        }
        if {[catch {::json::json2dict $raw} resp]} {
            editor_status "slopdrop save: bad answer"; return
        }
        if {![dict exists $resp ok] || ![dict get $resp ok]} {
            set why [expr {[dict exists $resp error] ? [dict get $resp error] : "refused"}]
            editor_status "slopdrop save refused: $why"
            return
        }
        set_editor_dirty 0
        set msg [expr {[dict exists $resp message] ? [dict get $resp message] : "deployed"}]
        editor_status "slopdrop: $msg"
        return 1
    }
    set sha ""
    if {[info exists editor_sha($ed)]} { set sha $editor_sha($ed) }
    set base [expr {$sha eq "" ? "null" : "\"[::vapi::json_escape $sha]\""}]
    set body "{\"path\":\"[::vapi::json_escape $path]\",\"content\":\"[::vapi::json_escape $content]\",\"base_sha\":$base}"
    if {[catch {::json::json2dict [::vapi::post /api/fs/write $body]} resp]} {
        editor_status "save failed: $resp"; return
    }
    if {[dict exists $resp error]} {
        editor_status "save refused: [dict get $resp error]"; return
    }
    if {[dict exists $resp sha256]} { set editor_sha($ed) [dict get $resp sha256] }
    set_editor_dirty 0
    editor_status "[editor_status_base] saved ([dict get $resp bytes] bytes)"
    return 1
}

# THE emacs-doctrine feature: source the buffer into the LIVE app.
# Edit a lib proc, press C-c C-c, and the running window upgrades
# itself. Save first when dirty so disk and app stay in step.
proc ::vdev::editor_source_app {} {
    if {[editor_dirty] && [editor_save] != 1} {
        # A stale-save conflict (or a transport/write failure) leaves the
        # buffer deliberately unsaved. Do not turn C-c C-c into a way to
        # hide that refusal by sourcing the dirty text anyway.
        return 0
    }
    set content [[editor_widget] get 1.0 end-1c]
    if {[string trim $content] eq ""} { editor_status "buffer empty"; return }
    lassign [eval_in app $content] ok result
    if {$ok} {
        variable last_error ""
        editor_status "[editor_status_base] sourced into the live app"
        _repl_print out "sourced [editor_status_base]"
    } else {
        variable last_error $result
        editor_status "source error: [string range [lindex [split $result \n] 0] 0 70]"
        _repl_print err "source error: $result"
    }
}

# Send the buffer to the gateway's plugin interpreter (plugin work).
proc ::vdev::editor_eval_gw {} {
    set content [[editor_widget] get 1.0 end-1c]
    if {[string trim $content] eq ""} { editor_status "buffer empty"; return }
    lassign [eval_in gw $content] ok result
    if {$ok} {
        editor_status "[editor_status_base] evaluated on gateway"
    } else {
        variable last_error $result
        editor_status "gw eval error: [string range [lindex [split $result \n] 0] 0 70]"
        _repl_print err "gw eval error: $result"
    }
}

# Lint through our own tcl-lsp (one-shot stdio driver; the server has
# no --check mode). LIVE-VERIFIED: bad file → line:col diagnostics.
proc ::vdev::editor_check {} {
    set path [string trim [editor_path]]
    set content [[editor_widget] get 1.0 end-1c]
    set tmp ""
    if {$path eq ""} {
        set tmp "/tmp/veles-lsp-check.tcl"
        set f [open $tmp w]; puts -nonewline $f $content; close $f
        set path $tmp
    } elseif {[editor_dirty]} {
        editor_status "buffer modified — checking disk copy (save first to check the buffer)"
    }
    set tclsh [auto_execok tclsh]
    if {$tclsh eq ""} { editor_status "tclsh not on PATH"; return }
    variable lib_dir
    set driver [file join [file dirname $lib_dir] scripts tcl-lsp-check.tcl]
    if {![file exists $driver]} {
        editor_status "lsp driver not found (veles-desktop/scripts/tcl-lsp-check.tcl)"
        return
    }
    if {[catch {exec $tclsh $driver $path 2>@1} out]} {
        set out [string trim $out]
        if {$out eq ""} { set out "check produced no output" }
        _dbg_print $out
        editor_status "[editor_status_base] check: [string range [lindex [split $out \n] 0] 0 60]"
    } else {
        _dbg_print [string trim $out]
        editor_status "[editor_status_base] check clean"
    }
    if {$tmp ne ""} { catch { file delete $tmp } }
}

# ── inspector ────────────────────────────────────────────────────────

proc ::vdev::_dbg_print {text} {
    set w $::vpanes::R.dev.dbg.out
    $w configure -state normal
    $w delete 1.0 end
    $w insert end $text
    $w configure -state disabled
}

# Recursively dump the live widget tree (app internals).
proc ::vdev::_widget_tree {w depth} {
    set indent [string repeat "  " $depth]
    set class [winfo class $w]
    set line "$indent$w ($class)"
    catch { append line " \"[$w cget -text]\"" }
    set out [list $line]
    foreach c [winfo children $w] {
        lappend out {*}[::vdev::_widget_tree $c [expr {$depth + 1}]]
    }
    return $out
}

# proc body viewer: type a proc name in the arg box.
proc ::vdev::_proc_src {name} {
    set name [string trim $name]
    if {$name eq ""} { return "type a proc name in the box (e.g. ::vchat::send)" }
    if {[info procs $name] eq ""} {
        # Try with the leading ::v prefix conventions.
        foreach guess [list $name ::v$name ::vdev::$name] {
            if {[info procs $guess] ne ""} { set name $guess; break }
        }
        if {[info procs $name] eq ""} { return "no such proc: $name" }
    }
    set args [info args $name]
    return "proc $name \{$args\} \{\n[info body $name]\n\}"
}

proc ::vdev::inspect {what} {
    set arg [string trim [$::vpanes::R.dev.dbg.arg get]]
    switch -- $what {
        widgets {
            _dbg_print [join [_widget_tree . 0] \n]
        }
        namespaces {
            set out {}
            foreach ns [lsort [namespace children ::]] {
                set n_procs [llength [info procs ${ns}::*]]
                set n_vars [llength [info vars ${ns}::*]]
                lappend out [format "%-22s %3d procs · %3d vars" $ns $n_procs $n_vars]
            }
            _dbg_print [join $out \n]
        }
        procsrc {
            _dbg_print [_proc_src $arg]
        }
        engine {
            # Core (Rust) state via loopback API — guards + status.
            set out {}
            if {![catch {::vapi::get_json /api/guards} g]} {
                set cfg [dict get $g config]
                lappend out "guards: iter-cap=[dict get $cfg max_iterations] \
                    rail warn@[dict get $cfg guardrail_warn_at]/halt@[dict get $cfg guardrail_halt_at] \
                    verification=[expr {[dict get $cfg verification_nudge] ? "on" : "off"}]"
                foreach s [dict get $g active_sessions] {
                    set st [dict get $s state]
                    set it [dict get $st iterations]
                    lappend out "  session [string range [dict get $s session] 0 15]… \
                        iter [dict get $it used]/[dict get $it cap] \
                        edits=[dict get [dict get $st verification] edits_seen] \
                        fresh=[dict get [dict get $st verification] fresh_evidence]"
                }
            } else {
                lappend out "guards: unreachable ($g)"
            }
            if {![catch {::vapi::get_json /api/status} s]} {
                lappend out "gateway: up [dict get $s uptime_s]s · \
                    sessions=[dict get $s session_count] · \
                    platforms=[dict get $s platforms]"
            }
            _dbg_print [join $out \n]
        }
        gwprocs {
            lassign [eval_in gw "join \[lsort \[info procs ::*\]\] \n"] ok result
            _dbg_print [expr {$ok ? $result : "gw inspect failed: $result"}]
        }
        gwhooks {
            # ::veles::hooks is an ARRAY (tclrt.rs registers with
            # `array set`), so the `dict keys $::veles::hooks` this used
            # to send threw "can't read: variable is array" and the
            # button has never once printed a hook. The interpreter has
            # shipped ::veles::hooks_list since the bootstrap was
            # written — ask it instead of re-deriving the shape here.
            lassign [eval_in gw {if {[llength [info procs ::veles::hooks_list]]} {::veles::hooks_list} else {return "no hooks registry"}}] ok result
            if {$ok && [string trim $result] eq ""} { set result "no hooks registered" }
            _dbg_print [expr {$ok ? $result : "gw inspect failed: $result"}]
        }
        err {
            show_last_error
        }
    }
}

# Enter in the arg box: smart dispatch (proc name → source view).
proc ::vdev::inspect_from_arg {} {
    set arg [string trim [$::vpanes::R.dev.dbg.arg get]]
    if {$arg eq ""} return
    # Looks like a widget path → dump that subtree; else proc source.
    if {[string match ".*" $arg] && [winfo exists $arg]} {
        _dbg_print [join [_widget_tree $arg 0] \n]
    } else {
        _dbg_print [_proc_src $arg]
    }
}

proc ::vdev::show_last_error {} {
    variable last_error
    if {$last_error eq ""} {
        _dbg_print "no error recorded this session"
    } else {
        _dbg_print $last_error
    }
}

# ── tcl-lsp integration (lsp-mode feel) ─────────────────────────────
# One server per dev-pane session; semantic tokens drive the
# highlighting, publishDiagnostics drive underlines, hover drives the
# tooltip, completion drives Tab. Fallbacks: regex highlight
# (::vsyn::highlight) when tcl-lsp is absent; the one-shot
# tcl-lsp-check.tcl driver remains the "Check" button surface.

namespace eval ::vdev {
    variable lsp_sync_after ""
    variable lsp_hover_after ""
    variable lsp_hover_pos ""
    # Tooltip latches: tip_pin = Shift held or clicked to keep; tip_in =
    # pointer parked inside the tip. See _hover_result's lifetime comment.
    variable tip_pin 0
    variable tip_in 0
    variable tip_hide_after ""
    # A "line.char" position to jump to once a cross-file open finishes.
    variable _jump_after_open ""
    variable _peek_items {}
    variable _peek_cache
    variable _folds       ;# widget -> the server's folding ranges
    variable _folded      ;# widget -> the ranges currently collapsed
    variable _actions {}  ;# the code actions on offer
}

# Called after the buffer is (re)loaded: start the server on this file.
proc ::vdev::_lsp_open {} {
    set w [editor_widget]
    set text [$w get 1.0 end-1c]
    set path [editor_path]
    if {$path eq ""} { return }
    if {![catch { ::vlsp::start $path $text ::vdev::_lsp_diag }]} {
        # Pin the paint target AFTER start: a new document clears the
        # pin (it belongs to the buffer that opened it), so a pin
        # installed before the call was discarded every time — tokens
        # then resolved through whatever buffer happened to be current
        # at reply time, or nowhere at all.
        catch { ::vlsp::set_apply_widget $w }
        if {[::vlsp::running]} {
            editor_status "[editor_status_base] loaded (lsp)"
            return
        }
    }
    # No server → regex fallback highlight.
    ::vsyn::highlight $w
    editor_status "[editor_status_base] loaded"
}

# Diagnostics callback from the LSP client: underline + status count.
proc ::vdev::_lsp_diag {rows} {
    set w [editor_widget]
    if {![winfo exists $w]} { return }
    ::vsyn::diagnostics $w $rows
    set n [llength $rows]
    if {$n} {
        set errs 0
        foreach r $rows { if {[lindex $r 2] eq "ERROR"} { incr errs } }
        editor_status "[editor_status_base] $n diagnostic(s) ($errs error)"
        # Mirror the first few into the inspector too.
        set lines {}
        foreach r $rows {
            lassign $r line col sev msg
            lappend lines "$line:$col $sev $msg"
        }
        _dbg_print [join $lines \n]
    } else {
        editor_status "[editor_status_base] clean (lsp)"
    }
}

# Debounced re-highlight/re-sync on typing. Also clears the transient
# occurrence highlight — a keystroke means the symbol under the cursor
# changed, so last request's decorations are stale.
proc ::vdev::_edit_sync_schedule {} {
    _clear_hl
    variable lsp_sync_after
    if {$lsp_sync_after ne ""} { catch { after cancel $lsp_sync_after } }
    set lsp_sync_after [after 250 ::vdev::_edit_sync_now]
}

proc ::vdev::_edit_sync_now {} {
    set w [editor_widget]
    if {![winfo exists $w]} { return }
    # Sync ONLY the document the LSP session has open. A fresh scratch
    # buffer (or an editor on another file) used to fall through here
    # and didChange its own text — even "" — against the PREVIOUS
    # document's URI, blanking the server's copy mid-session. Anything
    # that is not the open document gets the regex fallback instead.
    set p [editor_path]
    set lp [::vlsp::doc_path]
    if {[::vlsp::running] && $p ne "" && $lp ne ""
        && [file normalize $p] eq [file normalize $lp]} {
        ::vlsp::sync [$w get 1.0 end-1c]
    } else {
        ::vsyn::highlight $w
    }
}

# Debounced hover: Motion → 300ms → query the server at that position.
# Motion also clears the occurrence highlight (it is keyed to where the
# cursor was).
proc ::vdev::_hover_schedule {x y} {
    _clear_hl
    # Reading wins over refresh: a pinned tip, or one the pointer is inside,
    # is not replaced just because the mouse twitched in the editor.
    variable tip_pin
    variable tip_in
    if {($tip_pin || $tip_in) && [winfo exists .vsyn_tip]} { return }
    variable lsp_hover_after
    variable lsp_hover_pos
    set lsp_hover_pos [list $x $y]
    if {$lsp_hover_after ne ""} { catch { after cancel $lsp_hover_after } }
    set lsp_hover_after [after 300 ::vdev::_hover_now]
}

proc ::vdev::_hover_now {} {
    variable lsp_hover_pos
    if {$lsp_hover_pos eq ""} { return }
    lassign $lsp_hover_pos x y
    set w [editor_widget]
    if {![winfo exists $w]} { return }
    if {[catch { $w index @$x,$y } idx]} { return }
    lassign [split $idx .] line char
    if {[::vlsp::running]} {
        # LSP positions are 0-based lines; Tk indices are 1-based.
        ::vlsp::hover_at [expr {$line - 1}] $char ::vdev::_hover_result
    } else {
        # Fallback: local proc signature tooltip.
        ::vsyn::hover $w $x $y
    }
}

proc ::vdev::_hover_result {result err} {
    variable tip_pin
    variable tip_in
    # A brand-new tip starts unpinned and pointer-outside: the previous
    # tip's latch must not silently protect this one. (While the pointer
    # is inside the old tip, motion refresh is blocked anyway.)
    set tip_pin 0
    set tip_in 0
    set text [::vlsp::render_hover $result]
    if {$text eq ""} { ::vdev::_tip_destroy; return }
    # Reuse the regex tooltip's window with server content, but read it
    # properly: no clipping, scroll without focus games, Shift to hold it.
    catch { destroy .vsyn_tip }
    set w [editor_widget]
    lassign $::vdev::lsp_hover_pos x y
    toplevel .vsyn_tip
    wm overrideredirect .vsyn_tip 1
    text .vsyn_tip.t -width 64 -wrap word \
        -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -font {monospace 9} -relief solid -borderwidth 1 \
        -highlightthickness 0 -padx 6 -pady 4 -cursor xterm -takefocus 1
    .vsyn_tip.t tag configure sig -foreground $::vt::T(cyan) -font {monospace 9 bold}
    scrollbar .vsyn_tip.sb -command {.vsyn_tip.t yview} \
        -background $::vt::T(bg_panel) -troughcolor $::vt::T(bg) \
        -elementborderwidth 0 -width 10
    .vsyn_tip.t configure -yscrollcommand {.vsyn_tip.sb set}
    .vsyn_tip.t insert end $text
    .vsyn_tip.t edit reset
    .vsyn_tip.t configure -state disabled
    pack .vsyn_tip.t -side left -fill both -expand 1
    pack .vsyn_tip.sb -side right -fill y

    # The tip is a reading widget, not a modal: the wheel scrolls it from
    # wherever the pointer is (no click-to-focus needed), entering it keeps
    # it alive, clicking pins it, Escape is good enough to dismiss it.
    bind .vsyn_tip <Enter> { ::vdev::_tip_enter }
    bind .vsyn_tip <Leave> { ::vdev::_tip_leave }
    # The guard matters: a rebuild (motion re-hover) destroys the tip
    # between an event being queued and processed, and an unguarded
    # `.vsyn_tip.t yview` on the corpse raises "bad window path name" —
    # wish answers with the bgerror dialog, which is what "a few errors
    # while scrolling" was.
    bind .vsyn_tip.t <MouseWheel> {
        if {[winfo exists .vsyn_tip.t]} {
            .vsyn_tip.t yview scroll [expr {int(round(-%D / 120.0))}] units
        }
        break
    }
    bind .vsyn_tip.t <Button-4> {
        if {[winfo exists .vsyn_tip.t]} { .vsyn_tip.t yview scroll -3 units }
        break
    }
    bind .vsyn_tip.t <Button-5> {
        if {[winfo exists .vsyn_tip.t]} { .vsyn_tip.t yview scroll 3 units }
        break
    }
    bind .vsyn_tip.t <Button-1> { focus -force .vsyn_tip.t ; ::vdev::_tip_pin }
    bind .vsyn_tip <Escape> { ::vdev::_tip_destroy }
    # A click moved focus INTO the tip, so the editor's Shift bindings are
    # out of reach; mirror them here so pressing and releasing Shift always
    # means "hold", whichever window holds focus.
    bind .vsyn_tip.t <KeyPress-Shift_L> { ::vdev::_tip_shift 1 }
    bind .vsyn_tip.t <KeyPress-Shift_R> { ::vdev::_tip_shift 1 }
    bind .vsyn_tip.t <KeyRelease-Shift_L> {
        if {(%s & 0x1) == 0} { ::vdev::_tip_shift 0 }
    }
    bind .vsyn_tip.t <KeyRelease-Shift_R> {
        if {(%s & 0x1) == 0} { ::vdev::_tip_shift 0 }
    }

    # Size to the content, clamped to the screen — a wrapped hover used to
    # be cut at ten lines and the window could hang off the bottom edge.
    update idletasks
    set one [font metrics TkFixedFont -linespace]
    set shown [lindex [.vsyn_tip.t index end-1c] 0]
    set maxh [expr {max(6, int([winfo screenheight .] * 0.65) / $one)}]
    .vsyn_tip.t configure -height [expr {min($maxh, max(2, $shown))}]
    update idletasks
    set tw [expr {[winfo reqwidth .vsyn_tip] + 4}]
    set th [expr {[winfo reqheight .vsyn_tip] + 4}]
    set px [expr {[winfo rootx $w] + $x + 14}]
    set py [expr {[winfo rooty $w] + $y + 18}]
    if {$px + $tw > [winfo screenwidth .] - 8} {
        set px [expr {[winfo screenwidth .] - 8 - $tw}]
    }
    if {$px < 0} { set px 0 }
    if {$py + $th > [winfo screenheight .] - 8} {
        # No room below: sit above the pointer instead of being chopped.
        set py [expr {[winfo rooty $w] + $y - $th - 10}]
        if {$py < 0} { set py [expr {[winfo screenheight .] - 8 - $th}] }
    }
    wm geometry .vsyn_tip "+$px+$py"
    raise .vsyn_tip
    ::vdev::_tip_schedule_hide 8000
}

# ── tooltip lifetime ─────────────────────────────────────────────────
# Three ways in, one latch: `tip_pin` (Shift held, or a click inside),
# `tip_in` (pointer parked over the tip). A hide is scheduled only when
# neither holds, and any re-entry cancels it — the crossing gap between
# editor and tooltip must not read as "user left".
variable tip_hide_after ""

proc ::vdev::_tip_schedule_hide {{ms 6000}} {
    variable tip_pin
    variable tip_in
    if {$tip_pin || $tip_in} { return }
    variable tip_hide_after
    ::vdev::_tip_cancel_hide
    set tip_hide_after [after $ms { ::vdev::_tip_destroy }]
}

proc ::vdev::_tip_cancel_hide {} {
    variable tip_hide_after
    if {$tip_hide_after ne ""} {
        catch { after cancel $tip_hide_after }
        set tip_hide_after ""
    }
}

proc ::vdev::_tip_destroy {} {
    variable tip_pin
    variable tip_in
    set tip_pin 0
    set tip_in 0
    ::vdev::_tip_cancel_hide
    # A click inside the tip moved focus there; giving the editor its
    # keyboard back is part of tearing the tip down, or the next keystroke
    # goes to a dead window.
    set had [expr {[winfo exists .vsyn_tip] && [focus] eq ".vsyn_tip.t"}]
    catch { destroy .vsyn_tip }
    if {$had} {
        set w [editor_widget]
        if {[winfo exists $w]} { focus -force $w }
    }
}

proc ::vdev::_tip_alive {} { expr {[winfo exists .vsyn_tip]} }

proc ::vdev::_tip_pin {} {
    variable tip_pin
    set tip_pin 1
    ::vdev::_tip_cancel_hide
}

proc ::vdev::_tip_enter {} {
    variable tip_in
    set tip_in 1
    ::vdev::_tip_cancel_hide
}

proc ::vdev::_tip_leave {} {
    variable tip_in
    variable tip_pin
    set tip_in 0
    if {$tip_pin} { return }
    ::vdev::_tip_schedule_hide 1200
}

# The editor's Leave: with a tip up, hand lifetime to the tip's own
# Enter/Leave (grace for the crossing); otherwise the old immediate hide.
proc ::vdev::_leave_editor {} {
    if {![winfo exists .vsyn_tip]} {
        ::vsyn::hide_tip
        return
    }
    # The tip sits 14-18px away: the walk to it must be survivable. This
    # grace being 350ms is exactly "it disappears when I move the mouse".
    ::vdev::_tip_schedule_hide 1500
}

# Shift-hold pinning, read from the EDITOR (the focused window): a bare
# Shift press/repeat pins the tip while held; releasing hands it back —
# unless the pointer made it inside the tooltip, which keeps it pinned
# until it leaves. Auto-repeat sends repeated presses; they are no-ops.
proc ::vdev::_tip_shift {press} {
    if {![winfo exists .vsyn_tip]} {
        # Tip already gone (or never built): a stale pin cannot guard
        # anything — clear it so the next tip starts clean.
        if {!$press} { ::vdev::_tip_destroy }
        return
    }
    variable tip_in
    if {$press} {
        ::vdev::_tip_pin
    } else {
        variable tip_pin
        if {!$tip_in} {
            set tip_pin 0
            ::vdev::_tip_schedule_hide 600
        }
    }
}

# Tab → LSP completion at point; results go to a small listbox.
proc ::vdev::_complete_at_point {} {
    set w [editor_widget]
    if {![::vlsp::running]} { return }
    set idx [$w index insert]
    lassign [split $idx .] line char
    ::vlsp::complete_at [expr {$line - 1}] $char ::vdev::_complete_result
}

proc ::vdev::_complete_result {result err} {
    set items [::vlsp::completion_items $result]
    if {![llength $items]} { return }
    catch { destroy .vdev_comp }
    set w [editor_widget]
    set bbox [$w bbox insert]
    if {$bbox eq ""} { set bbox {0 0 0 0} }
    lassign $bbox bx by bw bh
    toplevel .vdev_comp
    wm overrideredirect .vdev_comp 1
    wm geometry .vdev_comp "260x220+[expr {[winfo rootx $w] + $bx}]+[expr {[winfo rooty $w] + $by + $bh + 2}]"
    listbox .vdev_comp.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief solid -borderwidth 1 -font {monospace 10} \
        -exportselection 0
    # Carry the RAW labels in a parallel array — the display column is
    # padded and splitting the row text would mangle multi-word kinds.
    array unset ::vdev::_comp_labels
    set i 0
    foreach it $items {
        lassign $it label kind
        set ::vdev::_comp_labels($i) $label
        .vdev_comp.list insert end "[format %-12s $kind] $label"
        incr i
    }
    .vdev_comp.list selection set 0
    pack .vdev_comp.list -fill both -expand 1
    focus .vdev_comp.list
    bind .vdev_comp.list <Return> { ::vdev::_comp_insert ; break }
    bind .vdev_comp.list <Escape> { destroy .vdev_comp ; break }
    bind .vdev_comp.list <FocusOut> { after 200 { catch { destroy .vdev_comp } } }
}

proc ::vdev::_comp_insert {} {
    set sel [.vdev_comp.list curselection]
    if {$sel eq ""} { destroy .vdev_comp; return }
    set label $::vdev::_comp_labels([lindex $sel 0])
    set w [editor_widget]
    # Replace the current word with the completion.
    $w delete "insert wordstart" insert
    $w insert insert $label
    destroy .vdev_comp
    focus $w
}

# ── go-to-definition (M-.) ──────────────────────────────────────────
# xref-find-definitions feel: place the cursor on a call and jump to the
# proc/namespace that defines it. Same-file jumps move the editor
# cursor; cross-file jumps open the target and jump after load.

proc ::vdev::_goto_def {} {
    set w [editor_widget]
    if {![::vlsp::running]} {
        editor_status "lsp not running - open a file first"
        return
    }
    # Say WHY nothing happens rather than "no definition found" — the same
    # check _peek_def already makes.
    if {![::vlsp::supports definitionProvider]} {
        editor_status "this language server does not provide go-to-definition"
        return
    }
    set idx [$w index insert]
    lassign [split $idx .] line char
    ::vlsp::definition_at [expr {$line - 1}] $char ::vdev::_goto_def_result
}

proc ::vdev::_goto_def_result {result err} {
    variable current_file
    variable _jump_after_open
    if {$err ne ""} { editor_status "definition failed: $err"; return }
    set tgt [::vlsp::definition_target $result]
    if {![llength $tgt]} {
        editor_status "no definition found under the cursor"
        return
    }
    lassign $tgt uri line char
    if {![string match "file://*" $uri]} {
        editor_status "definition is not in a file"
        return
    }
    set path [file normalize [string range $uri 7 end]]
    set pos "[expr {$line + 1}].$char"
    # Same-file is decided against the FOCUSED editor, not the docked
    # pane's current_file: M-. inside a popped-out buffer or the failover
    # script editor lands on its own file, and comparing to current_file
    # would treat that as cross-file and yank the file into the dock.
    set here [editor_path]
    if {$here ne "" && $path eq [file normalize $here]} {
        _apply_jump $pos
        editor_status "[editor_status_base] -> [expr {$line + 1}]:$char"
        return
    }
    # Cross-file: open, then jump once the buffer is loaded.
    set _jump_after_open $pos
    set current_file $path
    editor_open
}

proc ::vdev::_apply_jump {pos} {
    set w [editor_widget]
    if {![winfo exists $w]} { return }
    $w mark set insert $pos
    $w see insert
    focus $w
}

# ── document symbols / outline ──────────────────────────────────────
# lsp-ui-imenu feel: a flat list of the buffer's procs/namespaces, click
# or Enter to jump, Escape to dismiss.

proc ::vdev::_outline {} {
    set w [editor_widget]
    if {![::vlsp::running]} {
        editor_status "lsp not running - open a file first"
        return
    }
    ::vlsp::document_symbols ::vdev::_outline_result
}

proc ::vdev::_outline_result {result err} {
    set items [::vlsp::symbol_list $result]
    if {$err ne ""} { editor_status "outline failed: $err"; return }
    if {![llength $items]} {
        editor_status "no symbols in this buffer"
        return
    }
    catch { destroy .vdev_out }
    set w [editor_widget]
    set bx [winfo rootx $w]
    set by [expr {[winfo rooty $w] + 24}]
    toplevel .vdev_out
    wm overrideredirect .vdev_out 1
    wm geometry .vdev_out "320x300+$bx+$by"
    listbox .vdev_out.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief solid -borderwidth 1 -font {monospace 10} \
        -exportselection 0
    array unset ::vdev::_out_lines
    set i 0
    foreach it $items {
        lassign $it name kind line
        set ::vdev::_out_lines($i) [expr {$line + 1}]
        .vdev_out.list insert end [format "%-14s %-20s L%d" $kind $name [expr {$line + 1}]]
        incr i
    }
    .vdev_out.list selection set 0
    pack .vdev_out.list -fill both -expand 1
    focus .vdev_out.list
    bind .vdev_out.list <Return> { ::vdev::_outline_jump ; break }
    bind .vdev_out.list <Escape> { destroy .vdev_out ; break }
    bind .vdev_out.list <FocusOut> { after 200 { catch { destroy .vdev_out } } }
}

# ── find references (M-?) ───────────────────────────────────────────
# xref-find-references feel: every use of the symbol under the cursor,
# in a picker that jumps on Return. Same list widget as the outline, so
# the two behave identically.

proc ::vdev::_references {} {
    set w [editor_widget]
    if {![::vlsp::running]} {
        editor_status "lsp not running - open a file first"
        return
    }
    if {![::vlsp::supports referencesProvider]} {
        editor_status "this language server does not provide references"
        return
    }
    set idx [$w index insert]
    lassign [split $idx .] line char
    ::vlsp::references_at [expr {$line - 1}] $char ::vdev::_references_result
}

proc ::vdev::_references_result {result err} {
    if {$err ne ""} { editor_status "references failed: $err"; return }
    set items {}
    foreach it [::vlsp::reference_list $result] {
        lassign $it uri line char
        set path $uri
        if {[string match "file://*" $uri]} { set path [string range $uri 7 end] }
        lappend items [list $path [expr {$line + 1}] $char]
    }
    if {![llength $items]} {
        editor_status "no references to the symbol under the cursor"
        return
    }
    peek_open "references" $items
}

# ── peek (lsp-ui-peek) ──────────────────────────────────────────────
#
# The old references popup was a listbox of "chat.tcl:412" and nothing
# else: you had to jump to each one to find out whether it was the hit
# you wanted, and jumping meant leaving the buffer you were reading.
#
# Peek shows the LINE at each location beside its name, and the lines
# around the selected one in a preview beside the list — so the common
# case (skim the hits, recognise the one you meant) never leaves the
# buffer at all. Enter jumps, Escape closes and you are where you were.

proc ::vdev::peek_items {} { variable _peek_items ; return $_peek_items }

# The source of a file, as a list of lines. The CURRENT buffer is read
# from the widget rather than from disk: peeking at a file you have
# edited must show what you are editing, not the last save.
proc ::vdev::_peek_source {path} {
    variable _peek_cache
    variable buffers
    set path [file normalize $path]
    if {[info exists _peek_cache($path)]} { return $_peek_cache($path) }
    set text ""
    set found 0
    foreach id [buf_list] {
        set bp [buf_path_of $id]
        if {$bp ne "" && [file normalize $bp] eq $path} {
            set bw [buf_widget $id]
            if {$bw ne "" && [winfo exists $bw]} {
                set text [$bw get 1.0 end-1c]
                set found 1
            }
            break
        }
    }
    if {!$found && [editor_path] ne "" && [file normalize [editor_path]] eq $path} {
        set text [[editor_widget] get 1.0 end-1c]
        set found 1
    }
    if {!$found} {
        if {[catch {
            # Whole file, like editor_open: a peek past line 200 of the
            # preview endpoint would show a line that is not there.
            set resp [::vapi::get_json "/api/fs/file?path=[::vapi::json_escape $path]"]
            if {[dict exists $resp text]} { set text [dict get $resp text] }
        } e]} {
            # An empty preview with no explanation looks like the peek is
            # broken; it usually means the file moved or is unreadable.
            editor_status "cannot read [file tail $path]: $e"
        }
    }
    set _peek_cache($path) [split $text "\n"]
    return $_peek_cache($path)
}

proc ::vdev::_peek_line {path line} {
    set lines [_peek_source $path]
    set i [expr {$line - 1}]
    if {$i < 0 || $i >= [llength $lines]} { return "" }
    return [string trim [lindex $lines $i]]
}

# ITEMS is a list of {path line char}, 1-based lines.
proc ::vdev::peek_open {title items} {
    variable _peek_items
    variable _peek_cache
    array unset _peek_cache
    set _peek_items $items
    catch { destroy .vdev_peek }
    set w [editor_widget]
    set bx [winfo rootx $w]
    set by [expr {[winfo rooty $w] + 24}]
    toplevel .vdev_peek
    wm overrideredirect .vdev_peek 1
    # Clamped like every other popup in the app rather than placed blind:
    # an editor near the bottom of the screen would otherwise put the
    # peek off it.
    if {[catch { ::vplace::popup .vdev_peek 900 340 -anchor $w }]} {
        wm geometry .vdev_peek "900x340+$bx+$by"
    }
    ttk::frame .vdev_peek.f -relief solid -borderwidth 1
    pack .vdev_peek.f -fill both -expand 1
    ttk::label .vdev_peek.f.h -foreground $::vt::T(fg_dim) -font {TkDefaultFont 9}
    ::vi18n::label .vdev_peek.f.h {%1$s — %2$s · Enter to jump, Esc to close} \
        $title [llength $items]
    pack .vdev_peek.f.h -fill x -padx 6 -pady {4 2}
    ttk::panedwindow .vdev_peek.f.pw -orient horizontal
    pack .vdev_peek.f.pw -fill both -expand 1
    ttk::frame .vdev_peek.f.pw.l
    listbox .vdev_peek.f.pw.l.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -font {monospace 9} -exportselection 0
    pack .vdev_peek.f.pw.l.list -fill both -expand 1
    ttk::frame .vdev_peek.f.pw.r
    text .vdev_peek.f.pw.r.body -wrap none -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -font {monospace 9} -highlightthickness 0 \
        -relief flat -state disabled
    pack .vdev_peek.f.pw.r.body -fill both -expand 1
    # The list carries file, line AND source, so it needs real width —
    # at 1:2 the snippet was clipped to a dozen characters and the column
    # that makes peek worth having was the one that did not fit.
    .vdev_peek.f.pw add .vdev_peek.f.pw.l -weight 2
    .vdev_peek.f.pw add .vdev_peek.f.pw.r -weight 3

    foreach it $items {
        lassign $it path line char
        set src [_peek_line $path $line]
        if {[string length $src] > 60} { set src "[string range $src 0 58]…" }
        set name [file tail $path]
        if {[string length $name] > 18} { set name "[string range $name 0 16]…" }
        .vdev_peek.f.pw.l.list insert end \
            [format "%-18s %4d  %s" $name $line $src]
    }
    .vdev_peek.f.pw.l.list selection set 0
    _peek_preview
    focus .vdev_peek.f.pw.l.list
    editor_status "[llength $items] $title"
    bind .vdev_peek.f.pw.l.list <<ListboxSelect>> { ::vdev::_peek_preview }
    bind .vdev_peek.f.pw.l.list <Return> { ::vdev::_peek_jump ; break }
    bind .vdev_peek.f.pw.l.list <Double-Button-1> { ::vdev::_peek_jump ; break }
    bind .vdev_peek.f.pw.l.list <Escape> { ::vdev::peek_close ; break }
    bind .vdev_peek.f.pw.l.list <FocusOut> { after 200 { ::vdev::peek_close } }
    return 1
}

proc ::vdev::peek_close {} {
    catch { destroy .vdev_peek }
    return
}

proc ::vdev::peek_selection {} {
    if {![winfo exists .vdev_peek.f.pw.l.list]} { return -1 }
    set sel [.vdev_peek.f.pw.l.list curselection]
    if {$sel eq ""} { return -1 }
    return [lindex $sel 0]
}

# The preview: the target line with a few either side, so a hit reads in
# context instead of as a number.
proc ::vdev::_peek_preview {{context 5}} {
    variable _peek_items
    set body .vdev_peek.f.pw.r.body
    if {![winfo exists $body]} { return }
    set i [peek_selection]
    if {$i < 0} { return }
    lassign [lindex $_peek_items $i] path line char
    set lines [_peek_source $path]
    set from [expr {$line - $context - 1}]
    if {$from < 0} { set from 0 }
    set to [expr {$line + $context - 1}]
    if {$to >= [llength $lines]} { set to [expr {[llength $lines] - 1}] }
    $body configure -state normal
    $body delete 1.0 end
    $body tag configure peekhit -background $::vt::T(select_bg) -foreground $::vt::T(fg)
    $body tag configure peekno -foreground $::vt::T(fg_dim)
    $body insert end "$path\n\n" peekno
    for {set n $from} {$n <= $to} {incr n} {
        set mark [expr {$n + 1 == $line ? "peekhit" : ""}]
        $body insert end [format "%5d  " [expr {$n + 1}]] peekno
        $body insert end "[lindex $lines $n]\n" $mark
    }
    $body configure -state disabled
    return
}

proc ::vdev::_peek_jump {} {
    variable _peek_items
    variable current_file
    variable _jump_after_open
    set i [peek_selection]
    if {$i < 0} { peek_close ; return 0 }
    lassign [lindex $_peek_items $i] path line char
    peek_close
    set path [file normalize $path]
    set pos "$line.$char"
    # Against the focused editor, not the dock (see _goto_def_result).
    set here [editor_path]
    if {$here ne "" && $path eq [file normalize $here]} {
        _apply_jump $pos
        editor_status "[editor_status_base] -> $line:$char"
        return 1
    }
    # Cross-file: open (its own tab now), then jump once the buffer is
    # in place. This used to call `open_file`, a proc that does not
    # exist anywhere in the app — every cross-file reference jump threw
    # straight into bgerror — and to park the position as a two-element
    # list that _apply_jump could not read even if it had got there.
    set _jump_after_open $pos
    set current_file $path
    editor_open
    return 1
}

# Peek the DEFINITION: read it where it is, without leaving the buffer
# you are in. M-. jumps; this shows.
proc ::vdev::_peek_def {} {
    set w [editor_widget]
    if {![::vlsp::running]} {
        editor_status "lsp not running - open a file first"
        return
    }
    if {![::vlsp::supports definitionProvider]} {
        editor_status "this language server does not provide definitions"
        return
    }
    lassign [split [$w index insert] .] line char
    ::vlsp::definition_at [expr {$line - 1}] $char ::vdev::_peek_def_result
}

proc ::vdev::_peek_def_result {result err} {
    if {$err ne ""} { editor_status "definition failed: $err"; return }
    set tgt [::vlsp::definition_target $result]
    if {![llength $tgt]} {
        editor_status "no definition found under the cursor"
        return
    }
    lassign $tgt uri line char
    if {![string match "file://*" $uri]} {
        editor_status "definition is not in a file"
        return
    }
    peek_open "definition" \
        [list [list [string range $uri 7 end] [expr {$line + 1}] $char]]
}

# ── signature help (typed, and on demand with M-s) ──────────────────
#
# A one-line popup under the cursor naming the proc you are calling and
# marking the argument you are on.

proc ::vdev::_signature {} {
    set w [editor_widget]
    if {![::vlsp::running]} { return }
    if {![::vlsp::supports signatureHelpProvider]} { return }
    set idx [$w index insert]
    lassign [split $idx .] line char
    ::vlsp::signature_at [expr {$line - 1}] $char ::vdev::_signature_result
}

proc ::vdev::_signature_result {result err} {
    if {$err ne ""} { _signature_hide ; return }
    set label [::vlsp::render_signature $result]
    if {$label eq ""} { _signature_hide ; return }
    set w [editor_widget]
    if {![winfo exists $w]} { return }
    # Anchor under the insertion point, not the widget corner.
    set bb [$w bbox insert]
    if {![llength $bb]} { _signature_hide ; return }
    lassign $bb bx by bw bh
    set x [expr {[winfo rootx $w] + $bx}]
    set y [expr {[winfo rooty $w] + $by + $bh + 2}]
    catch { destroy .vdev_sig }
    toplevel .vdev_sig
    wm overrideredirect .vdev_sig 1
    label .vdev_sig.l -text $label -background $::vt::T(bg_panel) \
        -foreground $::vt::T(fg) -font {monospace 10} \
        -relief solid -borderwidth 1 -padx 6 -pady 2 -justify left
    pack .vdev_sig.l
    wm geometry .vdev_sig "+$x+$y"
    catch { raise .vdev_sig }
}

proc ::vdev::_signature_hide {} {
    catch { destroy .vdev_sig }
}

proc ::vdev::_outline_jump {} {
    set sel [.vdev_out.list curselection]
    if {$sel eq ""} { destroy .vdev_out; return }
    set line $::vdev::_out_lines([lindex $sel 0])
    set pos "$line.0"
    destroy .vdev_out
    _apply_jump $pos
    editor_status "[editor_status_base] -> $line"
}

# ── rename symbol (C-c C-e) ─────────────────────────────────────────
# LSP rename: every occurrence of the symbol under the cursor, updated in
# one shot. The server returns a WorkspaceEdit; same-file edits apply
# here, and any OTHER file it touches is reported rather than silently
# half-renamed (this editor holds one buffer).

proc ::vdev::_rename {} {
    set w [editor_widget]
    if {![::vlsp::running]} {
        editor_status "lsp not running - open a file first"
        return
    }
    if {![::vlsp::supports renameProvider]} {
        editor_status "this language server does not provide rename"
        return
    }
    set idx [$w index insert]
    lassign [split $idx .] line char
    # Prefill with the symbol under the cursor, or the selection.
    set rng [$w tag ranges sel]
    if {[llength $rng]} {
        set name [string trim [$w get [lindex $rng 0] [lindex $rng 1]]]
    } else {
        set name [string trim [$w get "insert wordstart" "insert wordend"]]
    }
    if {$name eq ""} {
        editor_status "place the cursor on the symbol to rename"
        return
    }
    ::vdev::_rename_prompt $name [expr {$line - 1}] $char
}

proc ::vdev::_rename_prompt {name line char} {
    variable _rename_line
    variable _rename_char
    set _rename_line $line
    set _rename_char $char
    catch { destroy .vdev_rename }
    set w [editor_widget]
    set bx [winfo rootx $w]
    set by [expr {[winfo rooty $w] + 24}]
    toplevel .vdev_rename
    wm title .vdev_rename "Rename"
    wm geometry .vdev_rename "+$bx+$by"
    wm transient .vdev_rename .
    ttk::label .vdev_rename.l
    ::vi18n::label .vdev_rename.l {Rename %s to:} $name
    ttk::entry .vdev_rename.e
    .vdev_rename.e insert 0 $name
    .vdev_rename.e selection range 0 end
    bind .vdev_rename.e <Return> { ::vdev::_rename_apply ; break }
    bind .vdev_rename.e <Escape> { destroy .vdev_rename ; break }
    ttk::button .vdev_rename.ok -text [::vi18n::mc {Rename}] -command ::vdev::_rename_apply
    ttk::button .vdev_rename.cancel -text [::vi18n::mc {Cancel}] -command { destroy .vdev_rename }
    grid .vdev_rename.l -row 0 -column 0 -padx 8 -pady {8 0} -sticky w
    grid .vdev_rename.e -row 1 -column 0 -padx 8 -pady 4 -sticky ew
    grid .vdev_rename.ok .vdev_rename.cancel -row 2 -column 0 -padx 4 -pady {4 8}
    ::vi18n::aname .vdev_rename.e {new name for %s} $name
    focus .vdev_rename.e
}

proc ::vdev::_rename_apply {} {
    variable _rename_line
    variable _rename_char
    set new [string trim [.vdev_rename.e get]]
    destroy .vdev_rename
    if {$new eq ""} { return }
    ::vlsp::rename_at $_rename_line $_rename_char $new ::vdev::_rename_result
}

proc ::vdev::_rename_result {result err} {
    variable current_file
    if {$err ne ""} { editor_status "rename failed: $err"; return }
    set edits [::vlsp::rename_edit $result]
    if {![llength $edits]} {
        editor_status "rename returned no edits (symbol not renameable here?)"
        return
    }
    lassign [apply_edits $edits] n other
    if {$n < 0} { return }
    if {[llength $other]} {
        editor_status "renamed $n site(s) here; also touches [join $other {, }]"
    } else {
        editor_status "renamed $n site(s)"
    }
}

# Apply a flattened WorkspaceEdit to the CURRENT buffer and report what
# it could not reach. Returns {applied other_paths}, or {-1 {}} when
# there is no buffer to edit.
#
# Shared by rename and by code actions: both answer with a WorkspaceEdit,
# and a second copy of this would be a second place for the ordering bug
# below to come back.
proc ::vdev::apply_edits {edits} {
    variable current_file
    set w [editor_widget]
    if {![winfo exists $w]} { return [list -1 {}] }
    set cur ""
    catch { set cur [file normalize $current_file] }
    set mine {}
    set other {}
    foreach e $edits {
        lassign $e uri sl sc el ec newtext
        set path $uri
        if {[string match "file://*" $uri]} { set path [string range $uri 7 end] }
        set path [file normalize $path]
        if {$path eq $cur} {
            lappend mine [list $sl $sc $el $ec $newtext]
        } else {
            lappend other $path
        }
    }
    # Apply same-file edits back-to-front (later line, then later char)
    # so earlier offsets stay valid while later ranges shift.
    set mine [lsort -decreasing -command {apply {{a b} {
        lassign $a al ac
        lassign $b bl bc
        if {$al != $bl} { return [expr {$al - $bl}] }
        return [expr {$ac - $bc}]
    }}} $mine]
    foreach e $mine {
        lassign $e sl sc el ec newtext
        set start "[expr {$sl + 1}].$sc"
        set end   "[expr {$el + 1}].$ec"
        $w delete $start $end
        $w insert $start $newtext
    }
    _edit_sync_now
    return [list [llength $mine] [lsort -unique $other]]
}

# ── highlight occurrences (C-c C-h) ─────────────────────────────────
# The symbol under the cursor lights up everywhere it is used, until the
# next motion or keystroke clears it.

proc ::vdev::_highlight {} {
    set w [editor_widget]
    if {![::vlsp::running]} {
        editor_status "lsp not running - open a file first"
        return
    }
    if {![::vlsp::supports documentHighlightProvider]} {
        editor_status "this language server does not provide highlights"
        return
    }
    set idx [$w index insert]
    lassign [split $idx .] line char
    ::vlsp::document_highlight_at [expr {$line - 1}] $char ::vdev::_highlight_result
}

proc ::vdev::_highlight_result {result err} {
    if {$err ne ""} { _clear_hl; return }
    set w [editor_widget]
    if {![winfo exists $w]} { return }
    _clear_hl
    set items [::vlsp::highlight_list $result]
    if {![llength $items]} {
        editor_status "no occurrences of the symbol under the cursor"
        return
    }
    $w tag configure lsp_hl \
        -background [::vtext::blend $::vt::T(accent) $::vt::T(bg_input) 0.35]
    foreach it $items {
        lassign $it kind sl sc el ec
        set start "[expr {$sl + 1}].$sc"
        set end   "[expr {$el + 1}].$ec"
        $w tag add lsp_hl $start $end
    }
    editor_status "highlighted [llength $items] occurrence(s) — move or type to clear"
}

proc ::vdev::_clear_hl {} {
    set w [editor_widget]
    if {![winfo exists $w]} { return }
    catch { $w tag remove lsp_hl 1.0 end }
}

# ── folding (textDocument/foldingRange) ─────────────────────────────
#
# The server already knows where a proc, a namespace or a comment block
# starts and ends. Without asking it an editor either guesses from
# indentation or cannot fold at all — and in Tcl, where a proc body is
# just braces, guessing is wrong often enough to be useless.
#
# A fold elides from the end of its first line to the end of its last,
# so the header stays readable and a marker says how much is hidden.
# Elision is a tag property, not an edit: `get` still returns the whole
# file, so folding a region cannot change what you save.

proc ::vdev::fold_ranges {{w ""}} {
    variable _folds
    if {$w eq ""} { set w [editor_widget] }
    if {[info exists _folds($w)]} { return $_folds($w) }
    return {}
}

proc ::vdev::folded_ranges {{w ""}} {
    variable _folded
    if {$w eq ""} { set w [editor_widget] }
    if {[info exists _folded($w)]} { return $_folded($w) }
    return {}
}

proc ::vdev::_fold_tag {w sl} { return "vdevfold_$sl" }

# Fold [sl, el] — 1-based, inclusive. Returns 1 if it folded.
proc ::vdev::fold_apply {w sl el} {
    variable _folded
    if {![winfo exists $w]} { return 0 }
    if {$el <= $sl} { return 0 }
    set last [lindex [split [$w index end-1c] .] 0]
    if {$sl > $last} { return 0 }
    if {$el > $last} { set el $last }
    set tag [_fold_tag $w $sl]
    $w tag configure $tag -elide 1
    $w tag add $tag "$sl.0 lineend" "$el.0 lineend"
    set hidden [expr {$el - $sl}]
    catch {
        ::vsyn::annotate $w "$sl.0 lineend" "  … $hidden lines" \
            $::vt::T(fg_dim) fold$sl
    }
    if {![info exists _folded($w)]} { set _folded($w) {} }
    lappend _folded($w) [list $sl $el]
    return 1
}

proc ::vdev::fold_release {w sl} {
    variable _folded
    if {![winfo exists $w]} { return 0 }
    set tag [_fold_tag $w $sl]
    if {[lsearch -exact [$w tag names] $tag] < 0} { return 0 }
    $w tag delete $tag
    catch { ::vsyn::clear_annotations $w fold$sl }
    set keep {}
    foreach f [folded_ranges $w] {
        if {[lindex $f 0] != $sl} { lappend keep $f }
    }
    set _folded($w) $keep
    return 1
}

# The innermost fold containing LINE, from the server's ranges.
proc ::vdev::fold_at {line {w ""}} {
    if {$w eq ""} { set w [editor_widget] }
    set best {}
    foreach r [fold_ranges $w] {
        lassign $r sl el
        # Server ranges are 0-based; the editor counts from 1.
        set s [expr {$sl + 1}]
        set e [expr {$el + 1}]
        if {$line < $s || $line > $e} { continue }
        if {![llength $best] || ($e - $s) < ([lindex $best 1] - [lindex $best 0])} {
            set best [list $s $e]
        }
    }
    return $best
}

proc ::vdev::toggle_fold {} {
    set w [editor_widget]
    if {![winfo exists $w]} { return }
    set line [lindex [split [$w index insert] .] 0]
    # Already folded here? Then this is an unfold, and it must work
    # whether or not a server is running — you must always be able to
    # get your text back.
    foreach f [folded_ranges $w] {
        lassign $f sl el
        if {$line >= $sl && $line <= $el} {
            fold_release $w $sl
            editor_status "unfolded $sl-$el"
            return
        }
    }
    if {![::vlsp::running]} {
        editor_status "lsp not running - open a file first"
        return
    }
    if {![::vlsp::supports foldingRangeProvider]} {
        editor_status "this language server does not provide folding ranges"
        return
    }
    ::vlsp::folding_ranges ::vdev::_fold_result
}

proc ::vdev::_fold_result {result err} {
    variable _folds
    if {$err ne ""} { editor_status "folding failed: $err"; return }
    set w [editor_widget]
    set _folds($w) [::vlsp::folding_list $result]
    if {![llength $_folds($w)]} {
        editor_status "no foldable range here"
        return
    }
    set line [lindex [split [$w index insert] .] 0]
    set r [fold_at $line $w]
    if {![llength $r]} {
        editor_status "nothing to fold at line $line"
        return
    }
    lassign $r sl el
    if {[fold_apply $w $sl $el]} {
        editor_status "folded $sl-$el ([expr {$el - $sl}] lines hidden)"
    }
}

proc ::vdev::unfold_all {} {
    set w [editor_widget]
    set n 0
    foreach f [folded_ranges $w] {
        if {[fold_release $w [lindex $f 0]]} { incr n }
    }
    editor_status [expr {$n ? "unfolded $n region(s)" : "nothing was folded"}]
    return $n
}

# ── code actions (textDocument/codeAction) ──────────────────────────
#
# The quick-fix the server is already computing for the diagnostic it
# just sent. Without this the editor can show you that a line is wrong
# and the fix the server has ready, and make you type it yourself.

proc ::vdev::code_action {} {
    set w [editor_widget]
    if {![winfo exists $w]} { return }
    if {![::vlsp::running]} {
        editor_status "lsp not running - open a file first"
        return
    }
    if {![::vlsp::supports codeActionProvider]} {
        editor_status "this language server does not provide code actions"
        return
    }
    # The region if there is one, the cursor's line otherwise — the same
    # bargain every other editor makes about what "here" means.
    set from [$w index insert]
    set to $from
    if {![catch { $w index sel.first }]} {
        set from [$w index sel.first]
        set to [$w index sel.last]
    }
    lassign [split $from .] sl sc
    lassign [split $to .] el ec
    ::vlsp::code_action_at [expr {$sl - 1}] $sc [expr {$el - 1}] $ec \
        ::vdev::_code_action_result
}

proc ::vdev::code_actions {} { variable _actions ; return $_actions }

proc ::vdev::_code_action_result {result err} {
    variable _actions
    if {$err ne ""} { editor_status "code actions failed: $err"; return }
    set _actions [::vlsp::code_action_list $result]
    if {![llength $_actions]} {
        editor_status "no code actions here"
        return
    }
    catch { destroy .vdev_actions }
    set w [editor_widget]
    toplevel .vdev_actions
    wm overrideredirect .vdev_actions 1
    if {[catch { ::vplace::popup .vdev_actions 480 200 -anchor $w }]} {
        wm geometry .vdev_actions "480x200+[winfo rootx $w]+[expr {[winfo rooty $w] + 24}]"
    }
    ttk::frame .vdev_actions.f -relief solid -borderwidth 1
    pack .vdev_actions.f -fill both -expand 1
    ttk::label .vdev_actions.f.h -text [::vi18n::mc {code actions — Enter to apply, Esc to close}] \
        -foreground $::vt::T(fg_dim) -font {TkDefaultFont 9}
    pack .vdev_actions.f.h -fill x -padx 6 -pady {4 2}
    listbox .vdev_actions.f.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -font {TkDefaultFont 10} -exportselection 0
    pack .vdev_actions.f.list -fill both -expand 1
    foreach a $_actions {
        lassign $a title kind edit command
        set suffix ""
        if {$kind ne ""} { set suffix "  ($kind)" }
        .vdev_actions.f.list insert end "$title$suffix"
    }
    .vdev_actions.f.list selection set 0
    focus .vdev_actions.f.list
    editor_status "[llength $_actions] code action(s)"
    bind .vdev_actions.f.list <Return> { ::vdev::apply_code_action ; break }
    bind .vdev_actions.f.list <Double-Button-1> { ::vdev::apply_code_action ; break }
    bind .vdev_actions.f.list <Escape> { destroy .vdev_actions ; break }
    bind .vdev_actions.f.list <FocusOut> { after 200 { catch { destroy .vdev_actions } } }
}

proc ::vdev::apply_code_action {{i ""}} {
    variable _actions
    if {$i eq ""} {
        set i -1
        if {[winfo exists .vdev_actions.f.list]} {
            set sel [.vdev_actions.f.list curselection]
            if {$sel ne ""} { set i [lindex $sel 0] }
        }
    }
    catch { destroy .vdev_actions }
    if {$i < 0 || $i >= [llength $_actions]} { return 0 }
    lassign [lindex $_actions $i] title kind edit command
    if {$edit ne ""} {
        set edits [::vlsp::workspace_edit $edit]
        if {[llength $edits]} {
            lassign [apply_edits $edits] n other
            if {$n < 0} { return 0 }
            if {[llength $other]} {
                editor_status "$title: $n edit(s) here; also touches [join $other {, }]"
            } else {
                editor_status "$title: $n edit(s)"
            }
            return 1
        }
    }
    if {$command ne ""} {
        # A command runs ON THE SERVER and comes back as a workspace edit
        # request, or as nothing at all. Say which one we sent rather
        # than claiming the action was applied.
        ::vlsp::execute_command $command ::vdev::_command_result
        editor_status "$title: sent to the server"
        return 1
    }
    editor_status "$title: nothing to apply"
    return 0
}

proc ::vdev::_command_result {result err} {
    if {$err ne ""} { editor_status "command failed: $err"; return }
    editor_status "command finished"
}

# ── pop out / re-attach the editor ───────────────────────────────────
# USER: "make the dev editor detachable and re-attachable, together with
# the script editor". Tk cannot reparent a widget across toplevels, so
# this is a REBUILD: capture content + path, destroy the editor, build
# the other one, restore. The LSP / emacs apparatus is re-attached via
# ::vdev::attach, so the popped editor keeps every convenience — and the
# failover script editor (.vfgscript), which shares the same attach, is
# the same kind of detachable buffer.

# The in-pane editor frame (moves with ::vpanes::R on undock).
proc ::vdev::dev_frame {} { return $::vpanes::R.dev.edit }

proc ::vdev::is_detached {} {
    variable _detached
    variable buffers
    if {[buf_ready]} {
        foreach id $buffers { if {[buf_detached $id]} { return 1 } }
        return 0
    }
    if {![info exists _detached]} { set _detached 0 }
    return $_detached
}

proc ::vdev::pop_out {} {
    variable _detached
    variable current_file
    # With a tab strip, "pop out the editor" means "pop out the tab you
    # are looking at" — every other tab stays where it is.
    if {[buf_ready] && [buf_current] ne ""} { return [buf_pop [buf_current]] }
    if {[is_detached]} { return }
    set src [dev_editor]
    if {![winfo exists $src]} { return }
    set parent [winfo parent $src]
    set content [$src get 1.0 end-1c]
    set path ""
    catch { set path $current_file }
    set _detached 1

    toplevel .vdevfloat
    wm title .vdevfloat [::vdev::_window_title $path]
    wm geometry .vdevfloat 800x580
    wm minsize .vdevfloat 440 280
    ttk::frame .vdevfloat.bar
    ttk::label .vdevfloat.bar.path -text $path -foreground $::vt::T(fg_dim) -font {monospace 9}
    ttk::button .vdevfloat.bar.in -text [::vi18n::mc {⤢ attach}] -command { ::vdev::reattach }
    pack .vdevfloat.bar.in -side right -padx 4
    pack .vdevfloat.bar.path -side left -fill x -expand 1 -padx 8 -pady 6
    pack .vdevfloat.bar -fill x
    ttk::label .vdevfloat.status -text "" -foreground $::vt::T(fg_dim)
    pack .vdevfloat.status -fill x -padx 8
    text .vdevfloat.body -wrap none -font {monospace 10} -highlightthickness 0 \
        -relief flat -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -insertbackground $::vt::T(pink)
    pack .vdevfloat.body -fill both -expand 1 -padx 6 -pady {2 6}
    .vdevfloat.body insert end $content
    ::vdev::attach .vdevfloat.body -path $path -status .vdevfloat.status
    bind .vdevfloat <Escape> { ::vdev::reattach }
    catch { ::vtext::restyle_all .vdevfloat }
    focus .vdevfloat.body

    # Dock side: the body is gone; a note points at the window.
    destroy $src
    ttk::label $parent.popped -text [::vi18n::mc {editor popped out — use its window, or ⤢ attach}] \
        -foreground $::vt::T(fg_dim)
    pack $parent.popped -fill both -expand 1
}

proc ::vdev::reattach {} {
    variable _detached
    variable current_file
    if {[buf_ready]} { return [buf_dock] }
    set _detached 0
    if {![winfo exists .vdevfloat.body]} { return }
    set content [.vdevfloat.body get 1.0 end-1c]
    set path [::vdev::editor_path]
    catch { set path $current_file }
    destroy .vdevfloat

    set f [dev_frame]
    catch { destroy $f.popped }
    text $f.body -wrap none -font {monospace 10} -highlightthickness 0 \
        -relief flat -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -insertbackground $::vt::T(pink)
    pack $f.body -fill both -expand 1
    $f.body insert end $content
    ::vdev::attach $f.body -status $f.status
    focus $f.body
}

# ── a session that can work on the live scripts ─────────────────────
#
# USER: "It would be nice if it was possible to start a dedicated
# session specifically for debugging/programming/extending the live
# scripts in the dev tab."
#
# Two halves.
#
# THE SESSION is a preset: a new chat seeded with a system preamble that
# says where the sources are, what the two interpreters are, and how to
# verify an edit before evaluating it. That is context the agent would
# otherwise have to be told every time.
#
# THE CHANNEL is the part that did not exist. The gateway's Tcl
# interpreter is reachable over HTTP (/api/tcl/eval); THIS process's is
# not, because veles-desktop is an HTTP client with no listener. So the
# desktop subscribes to /api/desktop/eval/events, runs what arrives
# through the same ::vdev::eval_in the REPL uses, and posts the answer
# back. The agent's `tcl_eval` tool with target=app rides that channel.
#
# ARMING. An app eval can redefine any proc in the running window, with
# no restart and no undo, so it is refused unless the user has switched
# live patching on here. The gateway mirrors the flag only so it can
# refuse early with a useful message — THIS side is the gate, and it
# checks on every request rather than trusting what it last reported.
#
# Nothing happens invisibly: the request and its result are printed into
# the REPL transcript exactly as if they had been typed.

namespace eval ::vdev {
    variable armed 0
    variable eval_tok ""
    variable eval_seen 0
}

# Subscribe to the work queue. Called at boot and after an attach — the
# stream belongs to a gateway, and attaching to another one needs a new
# one.
proc ::vdev::eval_listen {} {
    variable eval_tok
    eval_unlisten
    if {[::vapi::unattached]} { return "" }
    if {[catch { ::vapi::sse_open /api/desktop/eval/events ::vdev::_eval_event } tok]} {
        return ""
    }
    set eval_tok $tok
    return $tok
}

proc ::vdev::eval_unlisten {} {
    variable eval_tok
    if {$eval_tok ne ""} {
        catch { ::vapi::sse_close $eval_tok }
        set eval_tok ""
    }
}

# One request off the stream.
proc ::vdev::_eval_event {event data} {
    variable armed
    variable eval_seen
    if {$event ne "eval"} { return }
    set id ""
    set code ""
    set note ""
    catch { set id [dict get $data id] }
    catch { set code [dict get $data code] }
    catch { set note [dict get $data note] }
    if {$id eq "" || $code eq ""} { return }
    incr eval_seen

    # THE GATE. Checked here, on every request — the server's copy of
    # `armed` is a convenience for producing a good error message, not
    # an authority. Disarming must stop evaluation even if the server
    # never hears about it.
    if {!$armed} {
        _eval_answer $id 0 "live patching is not armed in the Dev pane"
        catch { ::vstatus::set_msg [::vi18n::mc {refused an agent eval — live patching is off}] }
        return
    }

    # Show it BEFORE running it. If the eval wedges the window, the last
    # thing on screen is the code that did it.
    catch { _repl_print in "agent> $code" }
    if {$note ne ""} { catch { _repl_print out "# $note" } }
    lassign [eval_in app $code] ok result
    catch { _repl_print [expr {$ok ? "out" : "err"}] $result }
    _eval_answer $id $ok $result
}

proc ::vdev::_eval_answer {id ok result} {
    # A failed answer leaves the agent waiting for the full timeout with
    # no idea why, so say it here rather than swallowing it — this is the
    # one place that knows the post did not land.
    if {[catch {
        ::vapi::post_json /api/desktop/eval/result \
            [dict create id $id ok [::vapi::json_bool $ok] result [::vapi::json_string $result]]
    } e]} {
        catch { _repl_print err "could not return the result to the gateway: $e" }
    }
}

# The arm switch. Also tells the gateway, so an agent asking for an app
# eval while disarmed gets "switch it on in the Dev pane" instead of a
# twenty-second timeout.
proc ::vdev::set_arm {on} {
    variable armed
    set armed [expr {$on ? 1 : 0}]
    catch {
        ::vapi::post_json /api/desktop/eval/arm [dict create armed [::vapi::json_bool $armed]]
    }
    if {$armed} {
        catch { ::vdev::eval_listen }
        catch { ::vstatus::set_msg [::vi18n::mc {live patching ARMED — an agent can now redefine procs in this window}] }
    } else {
        catch { ::vstatus::set_msg [::vi18n::mc {live patching disarmed}] }
    }
    _arm_paint
    return $armed
}

proc ::vdev::toggle_arm {} {
    variable armed
    catch { ::vpanes::show dev }
    return [set_arm [expr {!$armed}]]
}

# Keep the checkbox and its warning colour in step with the variable,
# however the state was reached (palette, F7, or the checkbox itself).
proc ::vdev::_arm_paint {} {
    variable armed
    set b $::vpanes::R.dev.repl.bar.arm
    if {![winfo exists $b]} { return }
    catch {
        $b configure -text [expr {$armed
            ? [::vi18n::mc {live patching ARMED}]
            : [::vi18n::mc {arm live patching}]}]
    }
}

# ── the preset session ──────────────────────────────────────────────

# What the agent needs to know that it cannot read off the screen.
proc ::vdev::session_preamble {} {
    set root [file normalize [file join [file dirname [info script]] ..]]
    return "You are working on veles-desktop itself: the Tcl/Tk window this\
conversation is displayed in.

THREE LAYERS, and they are not interchangeable:

  app  the running wish process — this window. Its source is
       $root/main.tcl and $root/lib/*.tcl. Every proc in it is live and
       redefinable. `tcl_eval` with target=app evaluates HERE, and the
       change takes effect immediately with no restart and no undo. It
       is refused unless the user has armed live patching in the Dev
       pane; if you get that refusal, ask rather than retrying.
  gw   the gateway's plugin interpreter — a separate process. Plugin
       procs, registered hooks, the veles:: bridge. `tcl_eval` with
       target=gw, or /api/tcl/eval.
  core the Rust engine. Read-only from here, over the loopback API.

HOW TO WORK

  * Prefer editing the SOURCE FILE and re-sourcing it over a one-off app
    eval. An eval that is not in a file is lost on restart, and the next
    person to read the file will not find the behaviour they are seeing.
  * [::vpaths::patch_file] is sourced at startup and on demand, so
    it is where a change belongs that should survive a restart without
    editing the app's own source. It re-runs on every reload, so it MUST
    be idempotent — redefining a proc is fine, appending to a global
    list twice is not.
  * The Dev pane has a real LSP over the Tcl sources: check a buffer
    before evaluating it rather than finding out by breaking the window.
  * Tk detail worth knowing: bindtags order is {widget class toplevel
    all}, `bind all <KeyPress>` is the keymap's prefix-dispatch slot,
    and lib/bindings.tcl is the ONE registry every key hint reads from —
    a binding added anywhere else is invisible to help, which-key and
    the coach.

Ask before doing anything to the running window that you cannot undo by
re-sourcing a file."
}

# Create the session, seed it, open it, and put the Dev pane up beside it.
proc ::vdev::new_session {} {
    if {[::vapi::unattached]} {
        catch { ::vstatus::set_msg [::vi18n::mc {not attached to a gateway}] }
        return ""
    }
    set body [dict create \
        title [::vapi::json_string [::vi18n::mc {Dev: the live scripts}]] \
        preamble [::vapi::json_string [session_preamble]]]
    if {[catch { ::vapi::post_json /api/sessions/new $body } resp]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {could not start a dev session: %s}] \
            [::vmanage::_reason $resp]] }
        return ""
    }
    set sid ""
    catch { set sid [dict get $resp session_id] }
    if {$sid eq ""} { return "" }
    catch { ::vsidebar::refresh }
    catch { ::vchat::open_session $sid }
    # The editor, the REPL and the conversation, all visible at once:
    # the point of the preset is that you can watch what it does.
    catch { ::vpanes::show dev }
    catch { ::vstatus::set_msg [::vi18n::mc {dev session ready — arm live patching to let it touch this window}] }
    return $sid
}
