# veles-desktop/lib/apps.tcl — apps the agent builds for the person
# (PLAN-sessions-and-phone.md A4; USER decisions D5, D6, 2026-10-11).
#
# The person asks ("make me an app that counts the eggs"), the agent
# writes a small Tk script (veles_core::apps), and it runs HERE, in the
# window, in a safe interpreter with Tk: no exec, no open, no socket, no
# load. What it can reach is a `veles::` API, and every call is checked
# against the capabilities its app.toml declared and the person allowed
# on its first run (all of them, or it does not run). A changed
# declaration asks again; changed code under the same one does not.
#
# The window also offers the agent `app.run`, `app.stop`, `app.reload`,
# `app.logs` and `app.screenshot` as device actions (lib/device.tcl), so
# the agent's `app` tool can run what it wrote, read its errors and look
# at it.

namespace eval ::vapps {
    # name -> dict {interp, top, info, log}
    variable running [dict create]
    # name -> list of log lines (the last 200)
    variable logs [dict create]
    variable max_log 200
}

# What each capability lets an app do, as the grant prompt shows it.
proc ::vapps::cap_text {cap} {
    switch -glob -- $cap {
        notify    { return [::vi18n::mc {post notifications}] }
        toast     { return [::vi18n::mc {show short messages}] }
        vibrate   { return [::vi18n::mc {vibrate the phone}] }
        speak     { return [::vi18n::mc {read text aloud}] }
        open      { return [::vi18n::mc {open links}] }
        share     { return [::vi18n::mc {open the share sheet}] }
        clipboard { return [::vi18n::mc {read and set the clipboard}] }
        location  { return [::vi18n::mc {know where the phone is}] }
        store     { return [::vi18n::mc {keep its own data}] }
        ask       { return [::vi18n::mc {ask the agent}] }
        http:*    { return [format [::vi18n::mc {fetch from %s}] [string range $cap 5 end]] }
    }
    return $cap
}

proc ::vapps::_log {name line} {
    variable logs
    variable max_log
    set l {}
    if {[dict exists $logs $name]} { set l [dict get $logs $name] }
    lappend l "[clock format [clock seconds] -format %H:%M:%S] $line"
    if {[llength $l] > $max_log} { set l [lrange $l end-[expr {$max_log - 1}] end] }
    dict set logs $name $l
}

proc ::vapps::log_text {name} {
    variable logs
    if {![dict exists $logs $name]} { return "" }
    return [join [dict get $logs $name] \n]
}

# ── fetching ─────────────────────────────────────────────────────────

proc ::vapps::fetch {name} {
    return [::vapi::get_json "/api/apps/$name/source"]
}

proc ::vapps::listing {} {
    return [dict get [::vapi::get_json /api/apps] apps]
}

# ── grants ───────────────────────────────────────────────────────────

proc ::vapps::granted {info} {
    set g [::vstate::get app_grants {}]
    set name [dict get $info name]
    expr {[dict exists $g $name] && [dict get $g $name] eq [dict get $info grant_key]}
}

proc ::vapps::grant {info} {
    set g [::vstate::get app_grants {}]
    dict set g [dict get $info name] [dict get $info grant_key]
    ::vstate::put app_grants $g
}

# Ask the person, once per declaration. Returns 1 to run. An app that
# declares nothing needs no question.
proc ::vapps::ask_grant {info {answer ""}} {
    set caps [dict get $info capabilities]
    if {![llength $caps] || [granted $info]} { return 1 }
    set lines {}
    foreach c $caps { lappend lines "• [cap_text $c]" }
    if {$answer eq ""} {
        set answer no
        catch {
            set answer [tk_messageBox -type yesno -icon question -default no \
                -title [dict get $info title] \
                -message [format [::vi18n::mc {Let %s:}] [dict get $info title]] \
                -detail [join $lines \n]]
        }
    }
    if {$answer ne "yes"} { return 0 }
    grant $info
    return 1
}

# ── running ──────────────────────────────────────────────────────────

# Run `name` (stop it first if it runs). `payload` stands in for the
# gateway's /api/apps/:name/source in tests; `answer` for the person's
# reply to the grant question. Returns "" or why it did not run.
proc ::vapps::run {name {payload ""} {answer ""}} {
    variable running
    if {[dict exists $running $name]} { stop $name }
    if {$payload eq ""} {
        if {[catch { fetch $name } payload]} {
            return [format [::vi18n::mc {could not read %s: %s}] $name [::vapi::reason $payload]]
        }
    }
    set info [dict get $payload info]
    set files [dict get $payload files]
    set entry [dict get $info entry]
    if {[llength [dict get $info unknown]]} {
        _log $name "unknown capabilities refused: [dict get $info unknown]"
    }
    if {![ask_grant $info $answer]} {
        return [::vi18n::mc {not allowed}]
    }

    set top .app_[string map {- _} $name]
    catch { destroy $top }
    toplevel $top
    wm title $top [dict get $info title]
    wm geometry $top 480x640
    bind $top <Destroy> [list ::vapps::_closed $name $top %W]

    # Embedded in a container this window manages (on a phone: full
    # screen, with the window's own Back and close), or, where Tk cannot
    # embed, in a toplevel of its own.
    set s [::safe::interpCreate]
    set embedded 0
    if {![catch {
        frame $top.c -container 1 -background black
        pack $top.c -fill both -expand 1
        update idletasks
        ::safe::loadTk $s -use [winfo id $top.c]
    }]} {
        set embedded 1
    } else {
        catch { destroy $top.c }
        ::safe::loadTk $s
    }
    dict set running $name [dict create interp $s top $top info $info embedded $embedded]
    _api $s $name $info

    # A runaway loop is stopped: the limit is pushed forward from here
    # once a second, so only a stretch of more than two seconds without
    # coming back to the event loop runs out.
    interp limit $s time -seconds [expr {[clock seconds] + 2}]
    _tick $name

    if {[catch { $s eval [dict get $files $entry] } err]} {
        _log $name "error: $err"
        catch { $s eval [list ttk::label .veles_error -text $err -wraplength 400 -foreground red] }
        catch { $s eval {pack .veles_error -fill x} }
        return [format [::vi18n::mc {%s failed: %s}] [dict get $info title] $err]
    }
    _log $name "started"
    return ""
}

proc ::vapps::_tick {name} {
    variable running
    if {![dict exists $running $name]} { return }
    set s [dict get $running $name interp]
    if {![interp exists $s]} { return }
    catch { interp limit $s time -seconds [expr {[clock seconds] + 2}] }
    after 1000 [list ::vapps::_tick $name]
}

proc ::vapps::stop {name} {
    variable running
    if {![dict exists $running $name]} { return 0 }
    set r [dict get $running $name]
    dict unset running $name
    catch { interp delete [dict get $r interp] }
    catch { destroy [dict get $r top] }
    _log $name "stopped"
    return 1
}

proc ::vapps::_closed {name top W} {
    variable running
    if {$W ne $top} { return }
    if {[dict exists $running $name]} { stop $name }
}

# ── the veles:: API, checked per call ────────────────────────────────

proc ::vapps::_api {s name info} {
    foreach cmd {notify toast vibrate speak open share clipboard location store ask http log app} {
        $s alias veles::$cmd ::vapps::_call $name $cmd
    }
    # An app's own errors (a bad -command, a failed after) land in its
    # log, not in a dialog on the person's screen.
    $s alias bgerror ::vapps::_bgerror $name
}

proc ::vapps::_bgerror {name msg} {
    _log $name "error: $msg"
}

proc ::vapps::_allowed {name cap} {
    variable running
    if {![dict exists $running $name]} { return 0 }
    expr {$cap in [dict get $running $name info capabilities]}
}

proc ::vapps::_need {name cap} {
    if {![_allowed $name $cap]} {
        error "$name may not $cap: its app.toml does not declare it"
    }
}

# Run a device action this window offers (lib/device.tcl's providers).
proc ::vapps::_device {action args_dict} {
    if {![dict exists $::vdevice::caps $action]} {
        error "this window cannot $action"
    }
    return [{*}[dict get $::vdevice::caps $action proc] $args_dict]
}

proc ::vapps::_call {name cmd args} {
    variable running
    switch -- $cmd {
        log {
            _log $name [join $args " "]
            return
        }
        app {
            set info [dict get $running $name info]
            return [dict create name $name title [dict get $info title]]
        }
        notify {
            _need $name notify
            lassign $args title text
            if {[llength $args] == 1} { set text $title ; set title [dict get $running $name info title] }
            return [_device notify [dict create title $title text $text]]
        }
        toast { _need $name toast ; return [_device toast [dict create text [lindex $args 0]]] }
        vibrate { _need $name vibrate ; return [_device vibrate [dict create ms [lindex $args 0]]] }
        speak { _need $name speak ; return [_device speak [dict create text [lindex $args 0]]] }
        open { _need $name open ; return [_device open [dict create uri [lindex $args 0]]] }
        share { _need $name share ; return [_device share [dict create text [lindex $args 0]]] }
        clipboard {
            _need $name clipboard
            if {[llength $args]} { return [_device clipboard [dict create text [lindex $args 0]]] }
            return [_device clipboard [dict create]]
        }
        location { _need $name location ; return [_device location [dict create wait 20]] }
        store {
            _need $name store
            lassign $args op key value
            set body [dict create op $op key $key]
            if {$op eq "set"} {
                set body "{\"op\":\"set\",\"key\":\"[::vapi::json_escape $key]\",\"value\":\"[::vapi::json_escape $value]\"}"
            } else {
                set body "{\"op\":\"[::vapi::json_escape $op]\",\"key\":\"[::vapi::json_escape $key]\"}"
            }
            set r [::json::json2dict [::vapi::post /api/apps/$name/store $body]]
            set v [dict get $r value]
            return [expr {$v eq "null" ? "" : $v}]
        }
        ask {
            _need $name ask
            lassign $args text callback
            _ask $name $text $callback
            return
        }
        http {
            lassign $args url callback
            _http $name $url $callback
            return
        }
    }
    error "unknown veles:: command $cmd"
}

# `veles::ask text callback`: the agent's reply comes to `callback reply`
# in the app, when it comes. Asynchronous: a turn can take minutes, and
# the window — the app's and everyone else's — stays live meanwhile.
proc ::vapps::_ask {name text callback} {
    set body "{\"text\":\"[::vapi::json_escape $text]\"}"
    set conn [::vapi::_conn]
    if {[catch {
        http::geturl "[dict get $conn url]/api/apps/$name/ask" -method POST \
            -type application/json -binary 1 -query [::vapi::_utf8_out $body] \
            -timeout 310000 -command [list ::vapps::_ask_done $name $callback] \
            {*}[::vapi::_conn_headers $conn]
    } err]} {
        _to_app $name $callback [format [::vi18n::mc {could not ask: %s}] $err]
    }
}

proc ::vapps::_ask_done {name callback tok} {
    set status [http::status $tok]
    set data [::vapi::_utf8 [http::data $tok]]
    http::cleanup $tok
    if {$status ne "ok"} {
        set reply [format [::vi18n::mc {could not ask: %s}] $status]
    } elseif {[catch { ::json::json2dict $data } r]} {
        set reply [format [::vi18n::mc {could not ask: %s}] $data]
    } elseif {[dict exists $r error]} {
        set reply [dict get $r error]
    } else {
        set reply [dict get $r reply]
    }
    _to_app $name $callback $reply
}

# `veles::http url callback`: only from a host the app declared.
proc ::vapps::_http {name url callback} {
    if {![regexp {^https?://([^/:]+)} $url -> host]} {
        error "veles::http takes an http(s) URL"
    }
    _need $name http:$host
    ::http::geturl $url -timeout 30000 -command [list ::vapps::_http_done $name $callback]
}

proc ::vapps::_http_done {name callback tok} {
    set body [::http::data $tok]
    set code [::http::ncode $tok]
    ::http::cleanup $tok
    _to_app $name $callback $body $code
}

proc ::vapps::_to_app {name callback args} {
    variable running
    if {$callback eq "" || ![dict exists $running $name]} { return }
    set s [dict get $running $name interp]
    if {[catch { $s eval [list {*}$callback {*}$args] } err]} {
        _log $name "error: $err"
    }
}

# ── what the window offers the agent (lib/device.tcl) ────────────────

proc ::vapps::cap_run {a} {
    set name [dict get $a name]
    set why [run $name]
    if {$why ne ""} { error $why }
    return "running $name"
}

proc ::vapps::cap_stop {a} {
    set name [dict get $a name]
    if {[stop $name]} { return "stopped $name" }
    return "$name was not running"
}

proc ::vapps::cap_reload {a} {
    return [cap_run $a]
}

proc ::vapps::cap_logs {a} {
    set name [dict get $a name]
    set t [log_text $name]
    if {$t eq ""} { return "no log lines from $name" }
    return $t
}

# `app.screenshot`: the app's window as a PNG, base64. Tk cannot read
# its own pixels; on a phone Android copies them (DesktopActivity's SNAP
# broadcast, PixelCopy).
proc ::vapps::cap_screenshot {a} {
    variable running
    set name [dict get $a name]
    if {![dict exists $running $name]} { error "$name is not running; run it first" }
    if {![llength [info commands ::borg]]} {
        error "app screenshots are taken on a phone; this window cannot read its own pixels"
    }
    set top [dict get $running $name top]
    update idletasks
    set dir [file join $::env(INTERNAL_STORAGE) snaps]
    file mkdir $dir
    set path [file join $dir $name-[clock milliseconds].png]
    borg broadcast send chat.janitor.veles.SNAP {} {} {} \
        [list path $path x [winfo rootx $top] y [winfo rooty $top] \
             w [winfo width $top] h [winfo height $top]]
    for {set i 0} {$i < 50 && ![file exists $path] && ![file exists $path.err]} {incr i} {
        set ::vapps::_snap_wait 0
        after 100 {set ::vapps::_snap_wait 1}
        vwait ::vapps::_snap_wait
    }
    if {[file exists $path.err]} {
        set f [open $path.err] ; set why [read $f] ; close $f
        file delete $path.err
        error "the picture failed: $why"
    }
    if {![file exists $path]} { error "the picture did not come within five seconds" }
    set f [open $path rb] ; set png [read $f] ; close $f
    file delete $path
    return [binary encode base64 -maxlen 0 $png]
}

# A pinned shortcut opened the desktop: run the app once the window is
# attached to its gateway (a cold start attaches a moment after).
proc ::vapps::open_from_shortcut {name {tries 30}} {
    if {[llength [info commands ::vapi::unattached]] && [::vapi::unattached] && $tries > 0} {
        after 1000 [list ::vapps::open_from_shortcut $name [expr {$tries - 1}]]
        return
    }
    set why [run $name]
    if {$why ne ""} { catch { ::vstatus::set_msg $why } }
}

# Pin to the home screen (PinActivity.java): Android asks the person.
proc ::vapps::pin {name title {icon ""}} {
    if {![llength [info commands ::borg]]} { return 0 }
    borg activity android.intent.action.MAIN {} {} {} \
        [list chat.janitor.veles chat.janitor.veles.PinActivity] \
        [list name $name label $title icon $icon]
    return 1
}

# Ask the agent to change an app: its words in the composer, for the
# person to finish.
proc ::vapps::ask_change {name title} {
    catch { ::vcmdtree::compose [format [::vi18n::mc {Change my app “%s” (%s): }] $title $name] }
}

proc ::vapps::register {} {
    if {![llength [info commands ::vdevice::provide]]} { return }
    set one {{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}
    ::vdevice::provide app.run "Run one of the person's apps in this window." $one ::vapps::cap_run -foreground 1
    ::vdevice::provide app.stop "Stop a running app." $one ::vapps::cap_stop
    ::vdevice::provide app.reload "Stop an app and run it again with its files as they are now." $one ::vapps::cap_reload -foreground 1
    ::vdevice::provide app.logs "An app's log: its errors and veles::log lines." $one ::vapps::cap_logs
    ::vdevice::provide app.screenshot "A picture of a running app's window (PNG, base64)." $one ::vapps::cap_screenshot -foreground 1
}

# ── the launcher ─────────────────────────────────────────────────────

proc ::vapps::launcher {{w .ov_apps} {payload ""}} {
    if {[winfo exists $w]} { raise $w ; return $w }
    toplevel $w
    wm title $w [::vi18n::mc {Apps}]
    wm geometry $w 520x640
    ::voverlay::_header $w {Apps}
    bind $w <Escape> [list destroy $w]
    ttk::label $w.status -justify left -wraplength 480
    pack $w.status -side bottom -fill x -padx 10 -pady 6
    set body [::vmanage::scrollframe $w body]
    pack $w.body -fill both -expand 1 -padx 8 -pady 8
    fill $w $payload
    return $w
}

proc ::vapps::fill {w {payload ""}} {
    set body $w.body.c.inner
    foreach c [winfo children $body] { destroy $c }
    if {$payload eq ""} {
        if {[catch { listing } payload]} {
            ::vi18n::label $w.status {unavailable: %s} [::vapi::reason $payload]
            return
        }
    }
    if {![llength $payload]} {
        ttk::label $body.empty -justify left -wraplength 440
        ::vi18n::label $body.empty {No apps yet. Ask the agent to make one: “make me an app that…”}
        pack $body.empty -fill x -pady 20
        return
    }
    set i 0
    foreach a $payload {
        set name [dict get $a name]
        set b $body.a[incr i]
        if {[dict exists $a error]} {
            ttk::button $b -text "$name ⚠" -command [list ::vapps::_broken $w $name [dict get $a error]] ;# i18n-exempt: the app's own name and a sign
        } else {
            set img ""
            if {[dict exists $a icon_png] && [dict get $a icon_png] ni {"" null}} {
                catch { set img [image create photo -data [dict get $a icon_png]] }
            }
            ttk::button $b -text [dict get $a title] -compound top \
                -command [list ::vapps::_launch $w $name]
            if {$img ne ""} { $b configure -image $img }
            set icon ""
            if {[dict exists $a icon_png] && [dict get $a icon_png] ni {"" null}} {
                set icon [dict get $a icon_png]
            }
            # Touch and hold (or the right button): what else to do with it.
            set items {}
            foreach row [_menu_rows] {
                lassign $row label what
                switch -- $what {
                    run    { lappend items [list $label [list ::vapps::_launch $w $name]] }
                    stop   { lappend items [list $label [list ::vapps::stop $name]] }
                    change { lappend items [list $label [list ::vapps::ask_change $name [dict get $a title]]] }
                    pin {
                        if {[llength [info commands ::borg]]} {
                            lappend items [list $label [list ::vapps::pin $name [dict get $a title] $icon]]
                        }
                    }
                }
            }
            catch { ::vctx::attach $b $items }
        }
        grid $b -row [expr {($i - 1) / 3}] -column [expr {($i - 1) % 3}] -padx 6 -pady 6 -sticky nsew
    }
    foreach c {0 1 2} { grid columnconfigure $body $c -weight 1 -uniform apps }
}

# The launcher's menu for one app (labels are catalog keys; the menu
# translates them when it opens).
proc ::vapps::_menu_rows {} {
    return {
        {"Run"                        run}
        {"Stop"                       stop}
        {"Ask the agent to change it" change}
        {"Pin to home screen"         pin}
    }
}

proc ::vapps::_launch {w name} {
    set why [run $name]
    if {$why ne ""} { $w.status configure -text $why }
}

proc ::vapps::_broken {w name err} {
    $w.status configure -text "$name: $err" ;# i18n-exempt: the app's name and the engine's own error
}
