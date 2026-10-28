# veles-desktop/lib/keymap.tcl — keyboard binding modes.
#
# Three schemes, switchable via the palette (Ctrl-K → Keybindings, or
# Alt-x → Keybindings) and persisted in desktop_state.json:
#
#   normie  the current defaults: Shift-Enter newline, Ctrl-K palette,
#           Ctrl-* app shortcuts.
#   emacs   C-j newline, M-x palette, app shortcuts on the M-* prefix
#           so the C-* chord stays readline-style editing. Emacs
#           doctrine: C-g aborts, C-h help, C-x is the window/dock
#           prefix, M-o opens overlays.
#   vi      emacs chords + Esc normal mode (j/k navigate sessions).
#
# ALL bindings come from lib/bindings.tcl (the authoritative registry).
# This module only translates registry rows to Tk binds, manages the
# armed-prefix state machine, and persists the mode choice. There is no
# second copy of any binding anywhere — help.tcl, whichkey.tcl and the
# palette hints all read the same table.
#
# M-x (Alt-x) is bound in ALL modes — the command palette is the
# universal "do anything" entry point. C-g (abort) and C-h (help) are
# bound in ALL modes too (editline.tcl installs them once).

namespace eval ::vkeymap {
    variable current "normie"
    variable vi_normal 0
    variable armed ""   ;# display-string of the armed prefix ("C-x"), "" = none
    variable meta_armed 0  ;# ESC pressed, waiting for the key it modifies
    variable persisted_mode 1 ;# does a use() persist? boot's fallback call turns this off
}

# ── display-key → Tk event sequence ─────────────────────────────────
# "C-x" → <Control-Key-x> · "Ctrl-K" → <Control-Key-k>
# "Alt-1" → <Alt-Key-1> · "Shift-Enter" → <Shift-Key-Return>
#
# THE DETAIL IS ALWAYS "Key"-QUALIFIED, and that is not cosmetic.
#
# Tk resolves an unqualified single-digit detail to a MOUSE BUTTON:
# `bind . <Alt-1>` is stored as <Alt-Button-1>, i.e. Alt + left click.
# So the five pane hotkeys this app advertises everywhere — the tab
# strip is literally labelled "1 Files", "2 Review" … and the help
# overlay, which-key and the palette all list Alt-1..Alt-5 — were bound
# to Alt-CLICK and had never once fired from the keyboard. Worse, they
# were bound on `all`, so Alt-clicking anywhere in the app silently
# switched the right-hand pane.
#
# Alt-0 worked, which is what made it look like the digits were fine:
# there is no Button-0, so Tk fell back to the key for that one alone.
#
# Verified under Xvfb before the fix:
#     _event Alt-1 -> <Alt-1>   Tk stores: <Alt-Button-1>
#     _event Alt-0 -> <Alt-0>   Tk stores: <Alt-Key-0>
#     pressing Alt+1 fired: nothing · ALT-CLICKING fired: the binding
#
# Qualifying every detail — letters and named keysyms too, not just
# digits — means no future row can drift into a mouse binding by being
# spelled with a character Tk happens to overload. uitest asserts it by
# asking Tk what it STORED, because the string we build looks fine.
proc ::vkeymap::_event {keys} {
    if {$keys eq "Esc"} { return "<Key-Escape>" }
    # A multi-key chord ("C-x h", "C-c C-t"): each word is its own
    # event, concatenated — Tk's form for a sequence. Splitting on "-"
    # alone used to collapse "C-x h" into <Control-Key-x h>, which is
    # not bindable at all, so a chord help row advertised a binding no
    # widget could ever receive.
    if {[string first " " $keys] >= 0} {
        set out ""
        foreach word $keys {
            append out [_event $word]
        }
        return $out
    }
    set parts [split $keys "-"]
    set mods {}
    set key [lindex $parts end]
    set shifted 0
    foreach p [lrange $parts 0 end-1] {
        switch -- $p {
            C - Ctrl - Control { lappend mods Control }
            M - Meta { lappend mods Alt }
            Alt { lappend mods Alt }
            S - Shift { lappend mods Shift ; set shifted 1 }
        }
    }
    # Canonical key spelling for Tk. "/" is not a keysym — Tk's is
    # "slash", and a binding spelled <Control-Key-/> is unbindable,
    # which is how a C-/ help row would advertise an undo that never
    # fired.
    switch -- $key {
        Enter - Return { set key "Return" }
        /               { set key "slash" }
        default { set key [string tolower $key] }
    }
    # Tk wants <Alt-Up>, not <Alt-up>.
    foreach sp {up down left right} {
        if {$key eq $sp} { set key [string toupper $key 0] }
    }
    # Function keys are keysyms F1..F12, not letters. This used to
    # special-case F1 alone and lowercase everything else, so a row
    # spelled F2 produced <Key-f2> — an event Tk never delivers, and a
    # binding that therefore silently did nothing.
    if {[regexp {^f([0-9]{1,2})$} $key -> n]} { set key "F$n" }
    # With Shift held, X delivers the UPPERCASE keysym: a binding on
    # <Control-Shift-Key-p> never matches, because the event carries
    # keysym P. Without this, every C-S-<letter> row would be dead —
    # and worse, `S` used to be dropped entirely, collapsing C-S-P onto
    # <Control-Key-p>, which is already Ctrl-P (composer preview).
    if {$shifted && [string length $key] == 1 && [string is alpha $key]} {
        set key [string toupper $key]
    }
    set sep [expr {[llength $mods] ? "-" : ""}]
    return "<[join $mods {-}]${sep}Key-$key>"
}

proc ::vkeymap::_clear {} {
    variable armed ""
    # Unbind every sequence the registry knows about, in every mode
    # (a mode switch never leaves stale bindings behind). Rows with an
    # empty action are DOCUMENTATION rows (Shift-Enter, C-j) — their
    # real bindings live in chat.tcl/editline.tcl and must NOT be
    # touched here.
    foreach mode {normie emacs vi} {
        foreach row [::vbindings::table $mode] {
            lassign $row keys target action help category
            # A documentation-only row for a widget-local target: the
            # module that owns the widget does the binding, and there is
            # nothing global to clear. Was `input`-specific; `irc` rows
            # need the same treatment and any future target will too.
            #
            # Checked BEFORE _event, not after: the Dev-pane rows spell
            # a two-key chord ("C-c C-t"), which _event cannot render and
            # which would otherwise be turned into some unrelated single
            # binding and cleared out from under whoever owns it. With
            # or without an action: the Dev rows carry one now, for the
            # command tree, and their binding is still dev.tcl's.
            if {$target ni {all input}} { continue }
            set ev [_event $keys]
            if {$category eq "prefix"} {
                catch { bind all $ev {} }
                continue
            }
            # A row with no action is DOCUMENTATION, whatever its target:
            # some other module owns the real binding and this row exists so
            # help, which-key and the palette can name it. Clearing it is
            # always wrong.
            #
            # This used to read `$target ne "all" && $action eq ""`, which
            # let the one doc row targeting `all` — C-g, "abort / cancel
            # (emacs law)" — fall through to `bind all <Control-g> {}`
            # below. editline.tcl binds C-g at startup and `use` never
            # rebinds an empty action, so boot order was the only thing
            # keeping C-g alive: the FIRST keymap switch (F4, palette,
            # anything calling ::vkeymap::use) killed it for the session and
            # the whole abort ladder went with it. USER: "the mark/selection
            # sticks forever and C-g doesn't cancel it".
            if {$action eq ""} { continue }
            if {$target eq "input"} {
                catch { bind .mid.chatwrap.chat.input $ev {} }
            } else {
                catch { bind all $ev {} }
            }
        }
    }
    # Prefix arm-time binds + disarm catch-all (removed on disarm, but
    # a stale set from a mid-switch armed prefix must not survive).
    _disarm
    # vi normal-mode composer hooks.
    foreach seq {<Key-j> <Key-k> <Key-i>} {
        catch { bind .mid.chatwrap.chat.input $seq {} }
    }
    # ESC-as-meta. Not a registry row (it modifies OTHER rows rather than
    # naming an action of its own), so the loop above never clears it.
    _disarm_meta
    catch { bind all <Key-Escape> {} }
}

# ── ESC is Meta ─────────────────────────────────────────────────────
#
# emacs law: `ESC x` is `M-x`. Every terminal emacs has worked this way
# since terminals had no Meta key, and it is still how you reach a Meta
# chord on a keyboard where Alt is spoken for.
#
# USER (spot test, 2026-08-25): "Esc+\d should do the same thing as C-\d
# or M-\d". It does M-\d — `ESC 1` is `M-1`, which is "pane 1 of this
# tab". C-1 is a different chord (tab 1) and stays that way; conflating
# them would make one of the two unreachable.
#
# EMACS MODE ONLY. In vi, Escape is normal mode — that is the entire
# point of vi — and normie has no Meta chords to reach.
#
# The lookup is over the REGISTRY, by the display string the key would
# have had. So ESC inherits every M-/Alt- binding automatically, and
# cannot drift from them: there is no second table.
proc ::vkeymap::_arm_meta {} {
    variable meta_armed
    # A completion popup owns Escape first — dismissing it is what the
    # user means, and chat.tcl binds that on the composer.
    if {[llength [info commands ::vcomp::open]] && [::vcomp::open]} {
        catch { ::vcomp::_close }
        return -code break
    }
    # ESC ESC cancels. This is the ONLY place it can happen: `bind all
    # <Key-Escape>` is more specific than the `<KeyPress>` catch-all
    # armed below, and Tk fires exactly one binding per tag — the most
    # specific. A second Escape therefore comes back HERE, never to
    # _meta_dispatch. (A mutation test caught the mirror of this check
    # sitting in _meta_dispatch as unreachable code.)
    if {$meta_armed} { _disarm_meta ; return -code break }
    _disarm                                                   ;# never both at once
    set meta_armed 1
    ::vstatus::set_msg [::vi18n::mc {ESC- … (Meta prefix)}]
    bind all <KeyPress> [list ::vkeymap::_meta_dispatch %K]
    return -code break
}

proc ::vkeymap::_disarm_meta {} {
    variable meta_armed
    if {!$meta_armed} { return }
    set meta_armed 0
    catch { bind all <KeyPress> {} }
    ::vstatus::set_msg ""
}

# Modifier keys pressed on their own must not consume the prefix — you
# are allowed to reach for Shift on the way to the key you meant.
proc ::vkeymap::_meta_ignorable {keyname} {
    return [expr {[string match "Shift_*" $keyname]
               || [string match "Control_*" $keyname]
               || [string match "Alt_*" $keyname]
               || [string match "Meta_*" $keyname]
               || [string match "Super_*" $keyname]
               || [string match "Hyper_*" $keyname]
               || [string match "ISO_*" $keyname]
               || $keyname eq "Caps_Lock" || $keyname eq "Num_Lock"}]
}

proc ::vkeymap::_meta_dispatch {keyname} {
    variable meta_armed
    variable current
    if {!$meta_armed} { return }
    if {[_meta_ignorable $keyname]} { return }
    _disarm_meta
    # `_event` renders M-x and Alt-x as the SAME Tk sequence, so the
    # registry may spell a row either way. Try both spellings of the
    # chord this key would have made.
    foreach spelling [list "M-$keyname" "Alt-$keyname"] {
        foreach row [::vbindings::table $current] {
            lassign $row keys target action help category
            if {$keys ne $spelling} { continue }
            if {$category eq "prefix"} {
                _arm_prefix $keys
                return -code break
            }
            if {$action eq ""} { return -code break }
            ::vstatus::set_msg [format [::vi18n::mc {ESC %1$s — %2$s}] $keyname $help]
            _run_row $action
            return -code break
        }
    }
    ::vstatus::set_msg [format [::vi18n::mc {ESC %s is not bound}] $keyname]
    return -code break
}

# ── prefix state machine ────────────────────────────────────────────
# Arming a prefix shows which-key IMMEDIATELY (breadcrumb = "C-x-").
# Continuation keys are bound ONLY while armed (on `bind all` so they
# win over every widget class binding); disarming removes them, so an
# unarmed app never eats ordinary typing. Any key that is not a
# continuation disarms the prefix (emacs feel).

proc ::vkeymap::_arm_prefix {prefix} {
    variable armed $prefix
    ::vstatus::set_msg [format [::vi18n::mc {%s- …}] $prefix]
    ::vwhichkey::arm $prefix
    # Arm-time dispatcher: bound while armed ONLY.
    foreach row [::vbindings::prefix_continuations $prefix] {
        set k [lindex $row 0]
        bind all "<Key-$k>" [list ::vkeymap::_armed_dispatch $prefix $k]
    }
    # Catch-all disarm for every other key.
    bind all <KeyPress> {+ ::vkeymap::_armed_check_disarm %K}
}

# Remove the arm-time binds (idempotent).
proc ::vkeymap::_disarm {} {
    variable armed ""
    ::vwhichkey::hide
    foreach row [::vbindings::prefix_continuations "C-x"] {
        catch { bind all "<Key-[lindex $row 0]>" {} }
    }
    foreach row [::vbindings::prefix_continuations "M-o"] {
        catch { bind all "<Key-[lindex $row 0]>" {} }
    }
    catch { bind all <KeyPress> {} }
    ::vstatus::set_msg ""
}

# Dispatch a key pressed while PREFIX is armed.
proc ::vkeymap::_armed_dispatch {prefix key} {
    if {$key eq "g"} { _disarm; return -code break }   ;# explicit cancel
    set hit [::vbindings::prefix_action $prefix $key]
    if {$hit eq ""} { _disarm; return }
    lassign $hit action help
    _disarm
    if {$action eq ""} { return -code break }
    # Breadcrumb for the resolved chord.
    ::vstatus::set_msg [format [::vi18n::mc {%1$s %2$s — %3$s}] $prefix $key $help]
    catch { ::vcoach::note_key $action }
    uplevel #0 $action
    return -code break
}

# Any key that is not a continuation of the armed prefix disarms it.
proc ::vkeymap::_armed_check_disarm {keyname} {
    variable armed
    if {$armed eq ""} { return }
    foreach row [::vbindings::prefix_continuations $armed] {
        if {[lindex $row 0] eq $keyname} { return }
    }
    _disarm
}

proc ::vkeymap::use {name} {
    variable current
    variable vi_normal
    variable persisted_mode
    set current $name
    set vi_normal 0
    _clear

    foreach row [::vbindings::table $name] {
        lassign $row keys target action help category

        if {$category eq "prefix"} {
            set ev [_event $keys]
            bind all $ev [list ::vkeymap::_arm_prefix $keys]
            continue
        }
        # Widget-local rows (irc, dev) name chords whose real binding
        # lives in the module that builds the widget, whether or not
        # they carry an action. Some spell a two-key sequence _event
        # cannot render, so bail before asking it to.
        if {$target ni {all input}} { continue }

        set ev [_event $keys]
        if {$target eq "input"} {
            # Composer-only bindings.
            switch -- $keys {
                C-j {
                    bind .mid.chatwrap.chat.input $ev {
                        .mid.chatwrap.chat.input insert insert "\n"
                        break
                    }
                }
                Esc {
                    # vi: enter normal mode — but a completion popup
                    # gets dismissed first (it replaces chat.tcl's own
                    # Escape binding on this widget).
                    bind .mid.chatwrap.chat.input $ev {
                        if {[::vcomp::open]} {
                            ::vcomp::_close
                            break
                        }
                        set ::vkeymap::vi_normal 1
                        ::vstatus::set_msg [::vi18n::mc {vi normal (i to insert)}]
                        break
                    }
                }
                default {
                    # Shift-Enter and friends: already owned by
                    # chat.tcl/editline.tcl — registry row is docs.
                }
            }
            continue
        }
        if {$action eq ""} { continue }   ;# documentation-only row
        # break inside the binding script stops later tags from acting.
        bind all $ev [list ::vkeymap::_run_row $action]
        # Tk tag order is widget → class → toplevel → all: a `bind all`
        # shortcut loses to the Text CLASS bindings when the composer
        # has focus (Ctrl-K would open the palette AND kill to EOL).
        # Normie users don't expect emacs text keys, so bind the
        # composer widget too. emacs/vi modes WANT the Text class keys
        # (C-a/C-e/C-k/…), so they stay `all`-only.
        if {$name eq "normie"} {
            bind .mid.chatwrap.chat.input $ev \
                [list ::vkeymap::_run_row_break $action]
        }
    }

    # ESC is Meta — emacs only; see _arm_meta.
    if {$name eq "emacs"} {
        bind all <Key-Escape> ::vkeymap::_arm_meta
    }

    # vi normal-mode session navigation (composer-local).
    if {$name eq "vi"} {
        bind .mid.chatwrap.chat.input <Key-j> {::vkeymap::_vi_key j}
        bind .mid.chatwrap.chat.input <Key-k> {::vkeymap::_vi_key k}
        bind .mid.chatwrap.chat.input <Key-i> {::vkeymap::_vi_key i}
    }

    # Emacs editing keys follow the mode: C-k is kill-line in emacs but
    # the command palette in normie, so they cannot both be live.
    catch { ::vemacs::apply_mode $name }
    # A BOOT FALLBACK must not overwrite the saved choice (2026-09-20:
    # "something simply reset my keybindings to normie"). boot() used
    # to call use("normie") whenever the state read failed — a gateway
    # that was busy at launch — and use() persisted the guess, so a
    # transient stall rewrote the preference. Only a DELIBERATE switch
    # (picker, /keymap, palette) persists; boot turns the flag off and
    # this re-arms it, so exactly boot's one call is exempt.
    variable persisted_mode
    if {$persisted_mode} {
        _persist
    }
}

# Row runner for bind scripts: execute the registry action at global
# scope. Bound as a plain command so the binding script itself is one
# call (Tk breaks cleanly if the proc returns -code break).
#
# This is also the one place that can say "the KEY was used", which is
# what retires a coach hint. It has to be here rather than in the
# action: the same command reached by clicking must not count, or the
# hint would teach itself away without you ever pressing the key.
proc ::vkeymap::_run_row {action} {
    catch { ::vcoach::note_key $action }
    uplevel #0 $action
}

# Composer-widget variant: also break, so the Text class bindings
# (emacs line editing) never double-act on the same keystroke.
proc ::vkeymap::_run_row_break {action} {
    catch { ::vcoach::note_key $action }
    uplevel #0 $action
    return -code break
}

proc ::vkeymap::_persist {} {
    variable current
    if {![::vstate::put keymap $current]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {keymap not saved: %s}] $::vstate::error] }
    }
}

# In vi normal mode, j/k navigate sessions; i returns to insert. In
# insert mode this returns "" so the composer's default typing wins.
proc ::vkeymap::_vi_key {char} {
    variable vi_normal
    if {!$vi_normal} { return "" }
    switch -- $char {
        j { ::vsidebar::select_rel 1 }
        k { ::vsidebar::select_rel -1 }
        i { set vi_normal 0; ::vstatus::set_msg [::vi18n::mc {vi insert}] }
    }
    return "break"
}

# ── command → keys, for the ACTIVE mode ─────────────────────────────
#
# Every user-visible key hint goes through here, so switching keymap
# switches the hints with it. The lookup itself lives in bindings.tcl
# (::vbindings::keys_for) — this only supplies the current mode.
#
# Deliberately NOT called `key`, the name it replaces. The old one took
# a made-up keyword ("source", "preview_pane") and fuzzy-matched it
# against help prose, so a call site that was never migrated would keep
# compiling and quietly return the wrong chord. Under the new name it
# fails loudly instead.
#
# Takes the COMMAND the affordance runs, verbatim:
#     ::vkeymap::key_for ::vchat::toggle_source      → Ctrl-R / M-r
#     ::vkeymap::key_for {::voverlay::open models}   → M-o m  (emacs)
#                                                    → ""     (normie)
# "" means "no key in this mode" — render the action without a hint
# rather than inventing one.
proc ::vkeymap::key_for {command} {
    variable current
    # A phone names the control on its screen instead, or nothing: the
    # palette is its ⋮, and "Ctrl-K for the command palette" named a key
    # it has not got. Its palette rows carried a column of chords, cut off
    # at the screen's edge.
    if {[llength [info commands ::vandroid::on]] && [::vandroid::on]} {
        return [::vandroid::control_for $command]
    }
    return [::vbindings::keys_for $command $current]
}

# The default keymap: env VELES_KEYMAP, else the persisted choice, else
# normie. Called at boot after the layout is built.
proc ::vkeymap::boot {} {
    variable current
    variable persisted_mode
    # Deliberate switches persist; THIS call may be a fallback whose
    # only justification is a state read that failed. use() reads the
    # flag and re-arms it before its own _persist.
    set persisted_mode 0
    set name "normie"
    if {[info exists ::env(VELES_KEYMAP)] && $::env(VELES_KEYMAP) ne ""} {
        set name $::env(VELES_KEYMAP)
    } elseif {![catch {::vapi::get_json /api/desktop/state} st] && [dict exists $st keymap]} {
        set name [dict get $st keymap]
    }
    if {$name ni {normie emacs vi}} { set name "normie" }
    use $name
    # Re-arm: every use() after this one is a deliberate switch.
    set persisted_mode 1
}

# ── keybindings picker (from the palette) ───────────────────────────
# A small modal listing the three schemes; ↑↓+Enter or click applies
# and persists. The preview pane renders the FULL binding table for the
# highlighted mode straight from the registry — every key documented.

proc ::vkeymap::_summary {name} {
    set lines {}
    set last_cat ""
    foreach entry [::vbindings::describe $name] {
        lassign $entry keys help category
        if {$category ne $last_cat} {
            lappend lines ""
            lappend lines "── $category ──"
            set last_cat $category
        }
        lappend lines [format "  %-12s %s" $keys $help]
    }
    return [join $lines "\n"]
}

proc ::vkeymap::_preview {list_w} {
    set sel [lindex [$list_w curselection] 0]
    if {$sel eq ""} return
    set row [$list_w get $sel]
    set name [string trim [string map {"◆ " "" "(active)" ""} $row]]
    if {$name ni {normie emacs vi}} { return }
    set w .keymap_picker.preview
    if {![winfo exists $w]} { return }
    # `.preview` is a TEXT widget, which has no -text option — this
    # used to `configure -text` and threw on every selection change, so
    # the preview pane has never once rendered. Text widgets are filled
    # by delete+insert, and this one is kept read-only.
    $w configure -state normal
    $w delete 1.0 end
    $w insert end [::vkeymap::_summary $name]
    $w configure -state disabled
}

proc ::vkeymap::picker {} {
    variable current
    set w .keymap_picker
    if {[winfo exists $w]} { destroy $w }
    toplevel $w
    wm title $w "Keybindings"
    wm geometry $w "440x520+[expr {[winfo rootx .]+200}]+[expr {[winfo rooty .]+60}]"
    ttk::label $w.hint -text [::vi18n::mc {pick a binding scheme (↑↓ Enter · Esc)}] -foreground $::vt::T(fg_dim)
    listbox $w.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -bd 0 -activestyle none
    foreach name {normie emacs vi} {
        if {$name eq $current} {
            $w.list insert end "◆ $name (active)"
        } else {
            $w.list insert end "  $name"
        }
    }
    $w.list selection set 0
    text $w.preview -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -font {monospace 9} -wrap none -relief flat -highlightthickness 0
    $w.preview insert end [::vkeymap::_summary $current]
    ::vtext::readonly $w.preview
    pack $w.hint -fill x -padx 8 -pady {8 2}
    pack $w.list -fill x -padx 8 -pady 2
    pack $w.preview -fill both -expand 1 -padx 8 -pady {2 8}
    bind $w.list <<ListboxSelect>> {::vkeymap::_preview %W}
    bind $w.list <ButtonRelease-1> {::vkeymap::_pick_apply %W}
    bind $w.list <Return> {::vkeymap::_pick_apply .keymap_picker.list}
    bind $w.list <Escape> {destroy .keymap_picker; break}
    focus $w.list
}

proc ::vkeymap::_pick_apply {list_w} {
    set sel [lindex [$list_w curselection] 0]
    if {$sel eq ""} return
    set row [$list_w get $sel]
    set name [string trim [string map {"◆ " "" "(active)" ""} $row]]
    if {$name in {normie emacs vi}} {
        ::vkeymap::use $name
    }
    ::vkeymap::_mark $list_w
    $list_w selection set $sel
    $list_w see $sel
    ::vkeymap::_preview $list_w
}

proc ::vkeymap::_mark {list_w} {
    for {set i 0} {$i < [$list_w size]} {incr i} {
        set row [$list_w get $i]
        set name [string trim [string map {"◆ " "" "(active)" ""} $row]]
        if {$name in {normie emacs vi}} {
            set new [expr {$name eq $::vkeymap::current ? "◆ $name (active)" : "  $name"}]
            $list_w delete $i
            $list_w insert $i $new
        }
    }
}
