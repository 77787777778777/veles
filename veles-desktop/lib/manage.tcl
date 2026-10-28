# veles-desktop/lib/manage.tcl — the shared shape of a manager overlay.
#
# USER: "Why is there still no gui mechanism for managing plugins, hooks
# or mechanisms? There is only placeholder bullshit that tells you to
# edit config.toml manually."
#
# The overlays that DID manage something (ACL) and the ones that only
# listed (plugins, skills, memory) differed by about two hundred lines of
# toolbar, tree and error plumbing each. Writing that four more times is
# how the fifth one ends up being a text dump again, so it lives here
# once:
#
#   ::vmanage::window       toplevel + header + Escape + toolbar + status
#   ::vmanage::button       a toolbar button, named for a11y and coached
#   ::vmanage::tree         a columned tree with scrollbars
#   ::vmanage::say          the status line (plain / ok / error tinted)
#   ::vmanage::post         POST + the API's error shape, unwrapped
#   ::vmanage::del          DELETE, same
#   ::vmanage::effect       render a config write's honest effect
#   ::vmanage::confirm      the modal a destructive action needs
#
# THE ERROR SHAPE. ::vapi::post throws on any status >= 400 with the
# literal "HTTP <code>: <body>", and the body is JSON carrying the real
# message. Every manager used to re-derive that; `post` unwraps it once
# so a refusal reads "name already exists" rather than
# "HTTP 400: {"error":"name already exists"}".

namespace eval ::vmanage {
    # ::vmanage::ask is modal (grab + tkwait), so its result crosses the
    # wait in a namespace variable rather than a return value.
    variable _ask_result ""
    variable _ask_ok_path ""
}

# ── chrome ──────────────────────────────────────────────────────────

# Build the standard manager toplevel. Returns the toolbar frame path.
proc ::vmanage::window {w title geometry {minsize {520 340}}} {
    toplevel $w
    wm title $w $title
    wm geometry $w $geometry
    wm minsize $w {*}$minsize
    ::voverlay::_header $w $title
    bind $w <Escape> [list destroy $w]
    ttk::frame $w.bar
    pack $w.bar -fill x -padx 8 -pady {4 0}
    ttk::label $w.bar.status -text "" -foreground $::vt::T(fg_dim)
    pack $w.bar.status -side right -fill x -expand 1 -padx 8
    return $w.bar
}

# A toolbar button. `name` is the widget suffix; `label` is shown; `help`
# is the a11y/coach name (defaults to the label).
proc ::vmanage::button {bar name label cmd {help ""}} {
    set b $bar.$name
    ttk::button $b -text $label -command $cmd
    if {$help eq ""} { set help $label }
    catch { ::va11y::name $b $help } ;# i18n-exempt: caller-supplied label
    catch { ::vcoach::attach $b }
    pack $b -side left -padx {0 6}
    return $b
}

proc ::vmanage::separator {bar name} {
    ttk::separator $bar.$name -orient vertical
    pack $bar.$name -side left -fill y -padx 6 -pady 2
}

# The status line. `kind` is info | ok | warn — colour only, so a
# screen reader gets the same sentence either way.
proc ::vmanage::say {w msg {kind info}} {
    if {![winfo exists $w.bar.status]} { return }
    set fg $::vt::T(fg_dim)
    switch -- $kind {
        ok   { set fg $::vt::T(accent) }
        warn { set fg $::vt::T(gold) }
    }
    $w.bar.status configure -text $msg -foreground $fg
}

# A columned tree with both scrollbars, packed into `parent`.
proc ::vmanage::tree {parent name columns headings {widths {}}} {
    set f $parent.$name
    ttk::frame $f
    ttk::treeview $f.t -columns $columns -show headings -selectmode browse
    foreach c $columns h $headings {
        $f.t heading $c -text $h
    }
    foreach c $columns wdt $widths {
        if {$wdt ne ""} { $f.t column $c -width $wdt -stretch 1 }
    }
    ttk::scrollbar $f.sb -orient vertical -command [list $f.t yview]
    ttk::scrollbar $f.hsb -orient horizontal -command [list $f.t xview]
    $f.t configure -yscrollcommand [list $f.sb set] -xscrollcommand [list $f.hsb set]
    grid $f.t   -row 0 -column 0 -sticky nsew
    grid $f.sb  -row 0 -column 1 -sticky ns
    grid $f.hsb -row 1 -column 0 -sticky ew
    grid rowconfigure    $f 0 -weight 1
    grid columnconfigure $f 0 -weight 1
    return $f
}

# ── talking to the API ──────────────────────────────────────────────

# Pull the real message out of whatever ::vapi threw. The decoder is
# ::vapi::reason (api.tcl), shared with the setup wizard.
proc ::vmanage::_reason {err} {
    return [::vapi::reason $err]
}

# POST and report. Returns the response dict on success, "" on failure
# (having already put the reason in the status line).
proc ::vmanage::post {w path body} {
    if {[catch { ::vapi::post_json $path $body } resp]} {
        say $w [format [::vi18n::mc {failed: %s}] [_reason $resp]] warn
        return ""
    }
    # A 2xx that still carries an error: the operation half-happened and
    # saying "done" would be a lie.
    if {[dict exists $resp error]} {
        say $w [format [::vi18n::mc {partly failed: %s}] [dict get $resp error]] warn
        return $resp
    }
    return $resp
}

proc ::vmanage::del {w path} {
    if {[catch { ::json::json2dict [::vapi::delete $path] } resp]} {
        say $w [format [::vi18n::mc {failed: %s}] [_reason $resp]] warn
        return ""
    }
    return $resp
}

# GET, or "" with the reason on screen.
proc ::vmanage::get {w path} {
    if {[catch { ::vapi::get_json $path } resp]} {
        say $w [format [::vi18n::mc {unreachable: %s}] [_reason $resp]] warn
        return ""
    }
    return $resp
}

# What a config write actually did, in the engine's own words.
#
# The API answers `effect` (live | new_sessions | restart_gateway) and a
# `note` spelling it out. Surfacing it is the difference between a
# settings pane you trust and one you restart the gateway after "just in
# case" — see configedit::Effect.
proc ::vmanage::effect {w resp {prefix ""}} {
    if {$resp eq ""} { return }
    set note ""
    catch { set note [dict get $resp note] }
    set kind ok
    if {[catch { dict get $resp effect } eff]} { set eff "" }
    if {$eff eq "restart_gateway"} { set kind warn }
    set msg [string trim "$prefix $note"]
    if {$msg eq ""} { set msg [::vi18n::mc {saved}] }
    say $w $msg $kind
}

# ── destructive actions ─────────────────────────────────────────────

# A modal yes/no. Returns 1 for yes.
#
# tk_messageBox rather than a hand-built toplevel: it is modal for real,
# it is keyboard-operable without extra binding, and this is the one
# place in the app where getting the default button wrong deletes
# something. `-default no` is deliberate.
proc ::vmanage::confirm {w title detail} {
    set answer no
    catch {
        set answer [tk_messageBox -parent $w -type yesno -icon warning \
            -default no -title $title -message $title -detail $detail]
    }
    return [expr {$answer eq "yes"}]
}

# A one-field prompt. Returns the entered string, or "" if cancelled.
proc ::vmanage::ask {w title label {initial ""} {hint ""}} {
    set d .vmanage_ask
    catch { destroy $d }
    toplevel $d
    wm title $d $title
    wm transient $d $w
    ::vplace::centered $d 520 [expr {$hint eq "" ? 170 : 210}]
    bind $d <Escape> [list destroy $d]

    ttk::frame $d.f
    pack $d.f -fill both -expand 1 -padx 12 -pady 12
    ttk::label $d.f.l -text $label
    ttk::entry $d.f.e
    $d.f.e insert 0 $initial
    catch { ::va11y::name $d.f.e $label } ;# i18n-exempt: caller-supplied label
    pack $d.f.l -anchor w
    pack $d.f.e -fill x -pady {2 6}
    if {$hint ne ""} {
        ttk::label $d.f.hint -text $hint -foreground $::vt::T(fg_dim) -wraplength 470 -justify left
        pack $d.f.hint -anchor w -pady {0 6}
    }
    ttk::frame $d.f.b
    pack $d.f.b -fill x
    set ::vmanage::_ask_result ""
    ttk::button $d.f.b.ok -text [::vi18n::mc {OK}] -command {
        set ::vmanage::_ask_result [.vmanage_ask.f.e get]
        destroy .vmanage_ask
    }
    ttk::button $d.f.b.cancel -text [::vi18n::mc {Cancel}] -command { destroy .vmanage_ask }
    pack $d.f.b.ok $d.f.b.cancel -side left -padx {0 6}
    bind $d.f.e <Return> { $::vmanage::_ask_ok_path invoke }
    set ::vmanage::_ask_ok_path $d.f.b.ok
    focus $d.f.e
    # Modal: the caller is a button handler and wants an answer, not a
    # callback. grab+tkwait is the Tk idiom for that.
    catch { grab set $d }
    tkwait window $d
    catch { grab release $d }
    return $::vmanage::_ask_result
}

# ── selection ───────────────────────────────────────────────────────

# The values of the selected row, or "" (with a nudge on screen).
proc ::vmanage::selected {w tree {what "a row"}} {
    set sel [$tree selection]
    if {![llength $sel]} {
        say $w [format [::vi18n::mc {select %s first}] $what]
        return ""
    }
    return [$tree item [lindex $sel 0] -values]
}

# ── a container that scrolls ────────────────────────────────────────
#
# USER: "the mechanisms window doesn't have a scrollbar and you can't
# actually see the bottom options".
#
# Packed widgets do not scroll: when the window is shorter than its
# content, Tk simply clips from the bottom and the rows down there
# become unreachable — no scrollbar, no wheel, no keyboard. The
# Mechanisms overlay stacks eight two-line mechanism rows, five guard
# knobs and a guards report, which is more than fits on a laptop.
#
# The Tk idiom for this is a canvas with a frame in it, because a canvas
# is the only widget with a scrollregion. `scrollframe` returns the
# INNER frame: pack into that and it scrolls.
proc ::vmanage::scrollframe {parent name} {
    set f $parent.$name
    ttk::frame $f
    canvas $f.c -highlightthickness 0 -borderwidth 0 \
        -background $::vt::T(bg_panel) -yscrollincrement 12
    ttk::scrollbar $f.sb -orient vertical -command [list $f.c yview]
    $f.c configure -yscrollcommand [list $f.sb set]
    grid $f.c  -row 0 -column 0 -sticky nsew
    grid $f.sb -row 0 -column 1 -sticky ns
    grid rowconfigure    $f 0 -weight 1
    grid columnconfigure $f 0 -weight 1

    set inner $f.c.inner
    ttk::frame $inner
    set win [$f.c create window 0 0 -anchor nw -window $inner]
    # Two bindings, two jobs: the content changing height resets the
    # scrollregion, and the canvas changing width re-widths the content
    # so `-fill x` inside behaves the way it would in a plain frame.
    bind $inner <Configure> [list ::vmanage::_sf_content $f.c]
    bind $f.c   <Configure> [list ::vmanage::_sf_width $f.c $win]

    # Wheel scrolling, in the app's existing spelling: X11 delivers
    # Button-4/5 with no %D, everything else sends %D.
    foreach ev {<Button-4> <Button-5> <MouseWheel>} {
        bind $f.c $ev [list ::vmanage::_sf_wheel $f.c %D %b]
        bind $inner $ev [list ::vmanage::_sf_wheel $f.c %D %b]
    }
    return $inner
}

proc ::vmanage::_sf_content {c} {
    catch { $c configure -scrollregion [$c bbox all] }
}

proc ::vmanage::_sf_width {c win} {
    catch { $c itemconfigure $win -width [winfo width $c] }
    _sf_content $c
    # A shorter window (on a phone, the keyboard coming up) keeps the
    # field being typed into in sight.
    set f [focus]
    if {$f ne "" && [string match $c.inner.* $f]} { scroll_see $f 1 }
}

# The scrollframe canvas a widget is in, or "".
proc ::vmanage::_sf_canvas {w} {
    for {set p $w} {$p ne "" && [winfo exists $p]} {set p [winfo parent $p]} {
        if {[winfo class $p] eq "Canvas" && [winfo exists $p.inner]} { return $p }
    }
    return ""
}

# Scrolls w's scrollframe until w is in sight: a field that takes the
# focus (a tap, Tab) is not typed into out of view. After a resize
# (settle) as far up as keeps it in sight: a frame that grows (the
# keyboard going down, a step's first layout) shows what is above the
# field again; a step opened scrolled below its heading.
proc ::vmanage::scroll_see {w {settle 0}} {
    set c [_sf_canvas $w]
    if {$c eq "" || $w eq $c} { return 0 }
    set inner $c.inner
    set full [winfo height $inner]
    set view [winfo height $c]
    if {$full <= $view || $view <= 1} { return 0 }
    set top [expr {[winfo rooty $w] - [winfo rooty $inner]}]
    set bottom [expr {$top + [winfo height $w]}]
    set y0 [expr {int([$c canvasy 0])}]
    if {$top < $y0} {
        set to $top
    } elseif {$bottom > $y0 + $view} {
        set to [expr {min($top, $bottom - $view)}]
    } elseif {$settle && $bottom - $view < $y0} {
        set to [expr {max(0, $bottom - $view)}]
    } else {
        return 0
    }
    # The canvas moves in whole increments, to the nearest one: round
    # the way that keeps all of w in sight.
    set inc [winfo pixels $c [$c cget -yscrollincrement]]
    if {$inc > 0} {
        if {$to == $top} {
            set to [expr {$inc * ($to / $inc)}]
        } else {
            set to [expr {$inc * (($to + $inc - 1) / $inc)}]
        }
    }
    $c yview moveto [expr {double($to) / $full}]
    return 1
}
# Only for the field itself: each frame round it hears of the focus too
# (a Virtual detail), and a step taller than its window scrolled to its
# own frame, below its heading.
bind VScrollKid <FocusIn> {
    if {![string match *Virtual %d]} { ::vmanage::scroll_see %W }
}

proc ::vmanage::_sf_wheel {c delta button} {
    set step 0
    if {$button eq "4"} { set step -3 } elseif {$button eq "5"} { set step 3 }
    if {$step == 0 && $delta ne "??" && $delta ne ""} {
        set step [expr {$delta > 0 ? -3 : 3}]
    }
    if {$step != 0} { catch { $c yview scroll $step units } }
    return
}

# Wheel events land on whatever widget is under the pointer, and the
# rows inside a scrollframe are ordinary widgets that know nothing about
# it — so without this the wheel does nothing over the content itself,
# only over the bare canvas. Call after filling one.
proc ::vmanage::scroll_children {inner} {
    set c [winfo parent $inner]
    foreach w [_descendants $inner] {
        # One that scrolls itself keeps its wheel: the wizard's config
        # preview and language list would otherwise move with the frame.
        if {[winfo class $w] in {Text Listbox Treeview}} { continue }
        foreach ev {<Button-4> <Button-5> <MouseWheel>} {
            catch { bind $w $ev [list ::vmanage::_sf_wheel $c %D %b] }
        }
    }
    # Focus brings a field into sight, and on a phone a finger moves the
    # frame (VScrollKid, lib/android.tcl).
    foreach w [concat $c [_descendants $inner]] {
        if {"VScrollKid" ni [bindtags $w]} { bindtags $w [linsert [bindtags $w] 0 VScrollKid] }
    }
    _sf_content $c
}

proc ::vmanage::_descendants {w} {
    set out [list $w]
    foreach c [winfo children $w] {
        foreach d [_descendants $c] { lappend out $d }
    }
    return $out
}
