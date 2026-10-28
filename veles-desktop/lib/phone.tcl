# veles-desktop/lib/phone.tcl — the Phone page (PLAN-sessions-and-phone.md
# A2, round P2): what veles may do on this phone, each with the button
# that changes it in Android's own settings.
#
# A phone that kills veles in the background, or never shows its
# notifications, looks broken, and the person holding it cannot tell
# why. Cheap phones (Transsion's, Xiaomi's) kill background apps
# hardest, and they are the phones this is for. So: one page, every
# switch that matters, and a button beside each.
#
# Since P4 two rows more: whether veles may read other apps'
# notifications (for the person's triggers, android/java/…/
# NotificationListener.java), and whether it starts with the phone (the
# `boot-start` file, which WakeReceiver.java reads at boot).
#
# What only Java can see (the notification and battery settings, the
# location grant, notification access, the maker, free space)
# DesktopActivity writes to
# `<files>/phone-status` on every resume. Coming back from Android's
# settings is a resume, so the page watches that file and refreshes
# itself.

namespace eval ::vphone {
    variable pkg chat.janitor.veles
    # The status file's mtime when the page last drew it.
    variable drawn_mtime -1
    variable poll ""
    # Makers whose phones stop apps beyond Android's own battery rules.
    variable strict_makers {
        xiaomi redmi poco tecno infinix itel oppo realme oneplus vivo
        huawei honor meizu asus lenovo nokia
    }
}

proc ::vphone::status_file {} {
    if {[info exists ::env(INTERNAL_STORAGE)]} {
        return [file join $::env(INTERNAL_STORAGE) phone-status]
    }
    return ""
}

# "Start with the phone": this file's presence (WakeReceiver.bootFlag).
proc ::vphone::boot_flag {} {
    if {[info exists ::env(INTERNAL_STORAGE)]} {
        return [file join $::env(INTERNAL_STORAGE) boot-start]
    }
    return ""
}

# What DesktopActivity wrote, as a dict; empty when there is none.
proc ::vphone::status {{path ""}} {
    if {$path eq ""} { set path [status_file] }
    set d [dict create]
    if {$path eq "" || [catch {::open $path r} f]} { return $d }
    fconfigure $f -encoding utf-8
    set text [read $f]
    close $f
    foreach line [split $text \n] {
        set i [string first = $line]
        if {$i > 0} {
            dict set d [string range $line 0 [expr {$i - 1}]] \
                [string range $line [expr {$i + 1}] end]
        }
    }
    return $d
}

proc ::vphone::_get {d key {default ""}} {
    if {[dict exists $d $key]} { return [dict get $d $key] }
    return $default
}

proc ::vphone::_gb {bytes} {
    format %.1f [expr {$bytes / 1e9}]
}

proc ::vphone::build {w} {
    toplevel $w
    wm title $w [::vi18n::mc {Phone}]
    wm geometry $w 520x640
    wm minsize $w 320 360
    ::voverlay::_header $w {Phone}
    bind $w <Escape> [list destroy $w]

    ttk::label $w.intro -justify left -wraplength 480
    ::vi18n::label $w.intro {What veles may do on this phone. Each button opens the Android setting that changes it.}
    pack $w.intro -fill x -padx 10 -pady {8 6}

    ttk::frame $w.rows
    pack $w.rows -fill x -padx 10
    foreach {id name action} {
        notifications {Notifications}             notifications
        background    {Running in the background} background
        reading       {Reading notifications, for triggers} reading
        boot          {Start with the phone}      boot
        driver        {Using the phone, for the agent} driver
        location      {Location}                  location
        network       {Network}                   ""
        storage       {Free space}                ""
        engine        {Engine}                    ""
    } {
        set r $w.rows.$id
        ttk::frame $r
        ttk::label $r.name -font TkHeadingFont
        ::vi18n::label $r.name $name
        ttk::label $r.state -justify left -wraplength 300
        grid $r.name -row 0 -column 0 -sticky w
        grid $r.state -row 1 -column 0 -sticky w
        if {$action ne ""} {
            ttk::button $r.go -command [list ::vphone::settings $action]
            grid $r.go -row 0 -column 1 -rowspan 2 -sticky e -padx {8 0}
        }
        grid columnconfigure $r 0 -weight 1
        pack $r -fill x -pady 4
    }

    ttk::label $w.maker -justify left -wraplength 480
    pack $w.maker -fill x -padx 10 -pady {6 0}

    ttk::label $w.capsname -font TkHeadingFont
    ::vi18n::label $w.capsname {What the agent can do here}
    ttk::label $w.caps -justify left -wraplength 480
    pack $w.capsname -anchor w -padx 10 -pady {10 0}
    pack $w.caps -fill x -padx 10

    ttk::button $w.join -command ::vphone::join_page
    ::vi18n::label $w.join {Join a pool}
    pack $w.join -anchor w -padx 10 -pady {10 0}

    ttk::frame $w.bar
    ttk::button $w.bar.app -command [list ::vphone::settings app]
    ::vi18n::label $w.bar.app {Open veles in Android's settings}
    ttk::button $w.bar.reload -command [list ::vphone::render $w]
    ::vi18n::label $w.bar.reload {Reload}
    pack $w.bar.app -side left
    pack $w.bar.reload -side right
    pack $w.bar -fill x -padx 10 -pady 10

    bind $w <Destroy> [list ::vphone::_closed $w %W]
    render $w
    _watch $w
    return $w
}

# Fill every row from the status file, the network, the engine and the
# device catalog. `status` stands in for the file in tests.
proc ::vphone::render {w {status ""}} {
    variable drawn_mtime
    variable strict_makers
    if {![winfo exists $w]} { return }
    if {$status eq ""} {
        set status [status]
        set drawn_mtime [_mtime]
    }
    set n $w.rows.notifications
    switch -- [_get $status notifications] {
        1 {
            ::vi18n::label $n.state {on}
            ::vi18n::label $n.go {Settings}
        }
        0 {
            ::vi18n::label $n.state {off}
            ::vi18n::label $n.go {Turn on}
        }
        default {
            ::vi18n::label $n.state {unknown}
            ::vi18n::label $n.go {Settings}
        }
    }
    set b $w.rows.background
    switch -- [_get $status background] {
        1 {
            ::vi18n::label $b.state {allowed}
            ::vi18n::label $b.go {Settings}
        }
        0 {
            ::vi18n::label $b.state {limited by Android}
            ::vi18n::label $b.go {Allow}
        }
        default {
            ::vi18n::label $b.state {unknown}
            ::vi18n::label $b.go {Settings}
        }
    }
    set rd $w.rows.reading
    switch -- [_get $status listener] {
        1 {
            ::vi18n::label $rd.state {on}
            ::vi18n::label $rd.go {Settings}
        }
        0 {
            ::vi18n::label $rd.state {off}
            ::vi18n::label $rd.go {Turn on}
        }
        default {
            ::vi18n::label $rd.state {unknown}
            ::vi18n::label $rd.go {Settings}
        }
    }
    set bt $w.rows.boot
    set flag [boot_flag]
    if {$flag ne "" && [file exists $flag]} {
        ::vi18n::label $bt.state {on}
        ::vi18n::label $bt.go {Turn off}
    } else {
        ::vi18n::label $bt.state {off}
        ::vi18n::label $bt.go {Turn on}
    }

    # The accessibility driver (AccessibilityDriver.java, P5): only the
    # sideload build has it, so the row is shown only where it exists.
    set dr $w.rows.driver
    switch -- [_get $status driver absent] {
        1 {
            ::vi18n::label $dr.state {on}
            ::vi18n::label $dr.go {Settings}
        }
        0 {
            ::vi18n::label $dr.state {off}
            ::vi18n::label $dr.go {Turn on}
        }
    }
    if {[_get $status driver absent] in {0 1}} {
        if {![winfo ismapped $dr] && [catch {pack info $dr}]} {
            pack $dr -fill x -pady 4 -after $w.rows.boot
        }
    } else {
        pack forget $dr
    }

    set l $w.rows.location
    switch -- [_get $status location] {
        precise {
            ::vi18n::label $l.state {precise}
            ::vi18n::label $l.go {Settings}
        }
        approximate {
            ::vi18n::label $l.state {approximate}
            ::vi18n::label $l.go {Allow}
        }
        off {
            ::vi18n::label $l.state {off}
            ::vi18n::label $l.go {Allow}
        }
        default {
            ::vi18n::label $l.state {unknown}
            ::vi18n::label $l.go {Allow}
        }
    }

    set net ""
    if {[llength [info commands ::borg]]} { catch { set net [borg networkinfo] } }
    if {$net eq "none"} {
        ::vi18n::label $w.rows.network.state {offline}
    } elseif {$net eq ""} {
        ::vi18n::label $w.rows.network.state {unknown}
    } else {
        # Android's own words ("wifi", "mobile LTE"): not a catalog key.
        catch { unset ::vi18n::wkeys($w.rows.network.state) }
        $w.rows.network.state configure -text $net
    }

    set free [_get $status free]
    set total [_get $status total]
    if {[string is wide -strict $free] && [string is wide -strict $total] && $total > 0} {
        ::vi18n::label $w.rows.storage.state {%s GB free of %s GB} [_gb $free] [_gb $total]
    } else {
        ::vi18n::label $w.rows.storage.state {unknown}
    }

    if {[catch {::vapi::get_json /api/health} h] || ![dict exists $h uptime_s]} {
        ::vi18n::label $w.rows.engine.state {not running}
    } else {
        set since [clock format [expr {[clock seconds] - [dict get $h uptime_s]}] -format %H:%M]
        ::vi18n::label $w.rows.engine.state {running since %s} $since
    }

    set maker [string tolower [_get $status maker]]
    if {$maker in $strict_makers} {
        ::vi18n::label $w.maker {Phones made by %s also stop apps on their own. In veles' app settings, let it start by itself and run in the background.} [string totitle $maker]
    } else {
        $w.maker configure -text ""
        catch { unset ::vi18n::wkeys($w.maker) }
    }

    set names {}
    catch { set names [lsort [dict keys $::vdevice::caps]] }
    $w.caps configure -text [join $names ", "]
}

proc ::vphone::_mtime {} {
    set f [status_file]
    if {$f eq "" || [catch {file mtime $f} m]} { return 0 }
    return $m
}

# While the page is open, redraw when DesktopActivity rewrites the file
# (the person came back from Android's settings).
proc ::vphone::_watch {w} {
    variable poll
    variable drawn_mtime
    if {![winfo exists $w]} { return }
    if {[_mtime] != $drawn_mtime} { render $w }
    set poll [after 1000 [list ::vphone::_watch $w]]
}

proc ::vphone::_closed {w W} {
    variable poll
    if {$W ne $w} { return }
    after cancel $poll
    set poll ""
}

# One of Android's settings screens, for this app.
proc ::vphone::settings {what} {
    variable pkg
    switch -- $what {
        notifications {
            _intent android.settings.APP_NOTIFICATION_SETTINGS {} \
                [list android.provider.extra.APP_PACKAGE $pkg]
        }
        background {
            # Android's one-tap "let it run in the background?" dialog;
            # the app's own page where a phone refuses that intent.
            if {[catch {_intent android.settings.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS package:$pkg {}}]} {
                _intent android.settings.APPLICATION_DETAILS_SETTINGS package:$pkg {}
            }
        }
        reading {
            # Android's own switch for this one app (Android 11 on);
            # the list of every app that may read notifications before.
            set listener $pkg/$pkg.NotificationListener
            if {[catch {_intent android.settings.NOTIFICATION_LISTENER_DETAIL_SETTINGS {} \
                    [list android.provider.extra.NOTIFICATION_LISTENER_COMPONENT_NAME $listener]}]} {
                _intent android.settings.ACTION_NOTIFICATION_LISTENER_SETTINGS {} {}
            }
        }
        driver {
            # Android's own switch for the driver (Android 12 on); the
            # accessibility list before. A sideloaded app may first need
            # "Allow restricted settings" in its app page (docs/phone.md).
            set driver $pkg/$pkg.AccessibilityDriver
            if {[catch {_intent android.settings.ACCESSIBILITY_DETAILS_SETTINGS {} \
                    [list android.intent.extra.COMPONENT_NAME $driver]}]} {
                _intent android.settings.ACCESSIBILITY_SETTINGS {} {}
            }
        }
        boot {
            # Not Android's to grant: veles' own choice, kept as a file
            # the boot receiver reads.
            set flag [boot_flag]
            if {$flag eq ""} { return }
            if {[file exists $flag]} {
                file delete $flag
            } else {
                close [::open $flag w]
            }
            foreach w [winfo children .] {
                if {[winfo exists $w.rows.boot]} { render $w }
            }
        }
        location {
            set perms {android.permission.ACCESS_FINE_LOCATION android.permission.ACCESS_COARSE_LOCATION}
            # Android asks once or twice; after a "don't ask again" only
            # the app's own page can change it.
            if {[_get [status] location] eq "precise"
                || [catch {borg checkpermission {*}$perms 1}]} {
                _intent android.settings.APPLICATION_DETAILS_SETTINGS package:$pkg {}
            }
        }
        app {
            _intent android.settings.APPLICATION_DETAILS_SETTINGS package:$pkg {}
        }
    }
}

# `borg activity action uri type ?cats comp args?`: the extras are the
# seventh word. Passed sixth they named a COMPONENT, and Android refused
# to start "android.provider.extra.APP_PACKAGE/chat.janitor.veles" (the
# real phone, 2026-10-11; the test's stub had taken any order).
proc ::vphone::_intent {action uri extras} {
    borg activity $action $uri {} {} {} $extras
}

# ── joining the pool by QR (PLAN-sessions-and-phone.md P2d) ──────────
#
# The arbiter's `veles pool invite` shows a WireGuard code (for the
# WireGuard app) and then the veles code. This page reads the second with
# the camera — the gateway decodes the photo (POST /api/pool/scan) — or
# takes its text pasted. It shows what joining means and joins only on
# the person's yes (POST /api/pool/join).

namespace eval ::vphone {
    # The invite text a scan found, waiting for the person's yes.
    variable scanned ""
    variable camera 0
}

proc ::vphone::join_page {} {
    set w .ov_pooljoin
    if {[winfo exists $w]} { raise $w ; return $w }
    toplevel $w
    wm title $w [::vi18n::mc {Join a pool}]
    wm geometry $w 520x700
    ::voverlay::_header $w {Join a pool}
    bind $w <Escape> [list destroy $w]

    ttk::label $w.intro -justify left -wraplength 480
    ::vi18n::label $w.intro {Scan the veles code from the machine that invited this phone. If it showed a WireGuard code first, scan that one with the WireGuard app before this.}
    pack $w.intro -fill x -padx 10 -pady {8 6}

    image create photo ::vphone::campic -width 1 -height 1
    label $w.view -image ::vphone::campic -background black
    pack $w.view -padx 10 -pady 4

    ttk::frame $w.bar
    ttk::button $w.bar.scan -command [list ::vphone::snap $w]
    ::vi18n::label $w.bar.scan {Scan}
    ttk::button $w.bar.paste -command [list ::vphone::_show_paste $w]
    ::vi18n::label $w.bar.paste {Paste the code instead}
    pack $w.bar.scan -side left
    pack $w.bar.paste -side left -padx 8
    pack $w.bar -fill x -padx 10 -pady 4

    ttk::frame $w.paste
    text $w.paste.t -height 4 -wrap char -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -insertbackground $::vt::T(pink)
    ::va11y::name $w.paste.t [::vi18n::mc {Paste the code instead}]
    ttk::button $w.paste.read -command [list ::vphone::read_pasted $w]
    ::vi18n::label $w.paste.read {Read}
    pack $w.paste.t -fill x
    pack $w.paste.read -anchor e -pady 4

    ttk::label $w.status -justify left -wraplength 480
    pack $w.status -fill x -padx 10 -pady 6

    ttk::frame $w.confirm
    ttk::label $w.confirm.what -justify left -wraplength 480
    ttk::button $w.confirm.yes -command [list ::vphone::join_now $w]
    ::vi18n::label $w.confirm.yes {Join}
    ttk::button $w.confirm.no -command [list ::vphone::_cancel_join $w]
    ::vi18n::label $w.confirm.no {Cancel}
    pack $w.confirm.what -fill x
    pack $w.confirm.yes $w.confirm.no -side left -pady 6 -padx {0 8}

    bind $w <<ImageCapture>> [list ::vphone::_frame $w]
    bind $w <<PictureTaken>> [list ::vphone::_picture $w]
    bind $w <Destroy> [list ::vphone::_join_closed $w %W]
    _camera_on $w
    return $w
}

proc ::vphone::_show_paste {w} {
    if {![winfo ismapped $w.paste]} {
        pack $w.paste -fill x -padx 10 -after $w.bar
    }
    focus $w.paste.t
}

proc ::vphone::_camera_on {w} {
    variable camera
    if {![llength [info commands ::borg]]} {
        _show_paste $w
        return
    }
    if {1 ni [borg checkpermission android.permission.CAMERA 0]} {
        catch { borg checkpermission android.permission.CAMERA 1 }
        ::vi18n::label $w.status {Allow the camera to scan the code, then open this page again.}
        _show_paste $w
        return
    }
    if {[catch { borg camera open } opened] || !$opened} {
        ::vi18n::label $w.status {The camera did not open; paste the code instead.}
        _show_paste $w
        return
    }
    set camera 1
    catch { borg camera start }
}

proc ::vphone::_camera_off {} {
    variable camera
    if {!$camera} { return }
    set camera 0
    catch { borg camera stop }
    catch { borg camera close }
}

proc ::vphone::_join_closed {w W} {
    if {$W ne $w} { return }
    _camera_off
    catch { image delete ::vphone::campic }
}

# A preview frame: show it.
proc ::vphone::_frame {w} {
    catch { borg camera image ::vphone::campic }
}

# Scan: take a picture; AndroWish answers with <<PictureTaken>>. Not
# `scan` or `join`: a proc of that name here would shadow Tcl's own for
# every caller in this namespace (render joins the action names).
proc ::vphone::snap {w} {
    variable camera
    if {!$camera} { _show_paste $w ; return }
    if {[catch { borg camera takejpeg } took] || !$took} {
        ::vi18n::label $w.status {The camera did not open; paste the code instead.}
        _show_paste $w
    }
}

proc ::vphone::_picture {w} {
    if {[catch { borg camera jpeg } jpeg]} { return }
    set b64 [binary encode base64 -maxlen 0 $jpeg]
    _scanned $w [_post_scan "{\"image_b64\":\"$b64\"}"]
    # The preview stops at a picture on some phones: go on showing it.
    catch { borg camera start }
}

proc ::vphone::read_pasted {w} {
    set text [string trim [$w.paste.t get 1.0 end]]
    if {$text eq ""} { return }
    _scanned $w [_post_scan "{\"text\":\"[::vapi::json_escape $text]\"}"]
}

# POST /api/pool/scan; the answer as a dict, or one with `error`.
proc ::vphone::_post_scan {body} {
    if {[catch { ::json::json2dict [::vapi::post /api/pool/scan $body] } r]} {
        return [dict create error [::vapi::reason $r]]
    }
    return $r
}

# What a scan found: why it failed, in this window's language, or what
# joining would do, for the person's yes.
proc ::vphone::_scanned {w r} {
    variable scanned
    set scanned ""
    pack forget $w.confirm
    if {[dict exists $r error]} {
        set kind ""
        catch { set kind [dict get $r kind] }
        switch -- $kind {
            no_code    { ::vi18n::label $w.status {No code in the picture. Hold the phone closer, straight on, and scan again.} }
            unreadable { ::vi18n::label $w.status {The code could not be read. Try again in better light.} }
            wireguard  { ::vi18n::label $w.status {That is the WireGuard code: scan it with the WireGuard app. The veles code is the second one.} }
            not_invite { ::vi18n::label $w.status {That is not a veles pool code.} }
            default    { ::vi18n::label $w.status {Could not join: %s} [dict get $r error] }
        }
        return
    }
    if {[dict get $r expires_in] <= 0} {
        ::vi18n::label $w.status {This code has expired. Ask for a new one.}
        return
    }
    set scanned [dict get $r text]
    $w.status configure -text ""
    catch { unset ::vi18n::wkeys($w.status) }
    ::vi18n::label $w.confirm.what {Join the pool of %1$s as %2$s? The pool will reach this phone at %3$s.} \
        [dict get $r arbiter] [dict get $r name] [dict get $r url]
    pack $w.confirm -fill x -padx 10 -after $w.status
}

proc ::vphone::_cancel_join {w} {
    variable scanned
    set scanned ""
    pack forget $w.confirm
}

proc ::vphone::join_now {w} {
    variable scanned
    if {$scanned eq ""} { return }
    ::vi18n::label $w.status {Joining…}
    update idletasks
    set body "{\"text\":\"[::vapi::json_escape $scanned]\"}"
    if {[catch { ::json::json2dict [::vapi::post /api/pool/join $body 120000] } r]} {
        set r [dict create error [::vapi::reason $r]]
    }
    pack forget $w.confirm
    set scanned ""
    if {[dict exists $r error]} {
        ::vi18n::label $w.status {Could not join: %s} [dict get $r error]
        return
    }
    _camera_off
    ::vi18n::label $w.status {Joined %1$s and %2$d other machine(s). Close veles and open it again so the pool can reach this phone.} \
        [dict get $r arbiter] [llength [dict get $r members]]
}
