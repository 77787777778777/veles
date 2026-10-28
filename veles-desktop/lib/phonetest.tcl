# veles-desktop/lib/phonetest.tcl — the Phone page (lib/phone.tcl,
# PLAN-sessions-and-phone.md round P2): the real window, fed the status
# file DesktopActivity writes, with `borg` recorded instead of Android.
# What each row says, which Android screen each button opens, and that
# coming back from Android's settings redraws the page by itself.
#
# Run: scripts/tcl_tests.sh

package require Tk
set _dir [file dirname [file normalize [info script]]]

set failed 0
set passed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

package require json
source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
namespace eval ::voverlay { proc _header {w title} {} }
namespace eval ::va11y { proc name {w t} {} }

# ── stubs, with the REAL signatures ──────────────────────────────────
# Android, recorded. `fail` names borg calls that throw, as a phone
# without that settings screen does.
set ::borg_calls {}
set ::borg_fail {}
set ::network "mobile LTE"
proc borg {args} {
    lappend ::borg_calls $args
    # AndroWish's own arity: action uri type ?cats comp args callback?.
    # Which word is which is pinned by the calls' recorded shape below:
    # extras in the component's place were refused by Android, not by
    # borg (the real phone, 2026-10-11).
    if {[lindex $args 0] eq "activity" && ([llength $args] < 4 || [llength $args] > 8)} {
        error "wrong # args: $args"
    }
    foreach pat $::borg_fail {
        if {[string match $pat $args]} { error "no activity for $args" }
    }
    switch -- [lindex $args 0] {
        networkinfo { return $::network }
        checkpermission {
            if {[lindex $args 1] eq "android.permission.CAMERA"} { return $::camera_perm }
            return {}
        }
        camera {
            switch -- [lindex $args 1] {
                open { return 1 }
                takejpeg {
                    after idle {event generate .ov_pooljoin <<PictureTaken>>}
                    return 1
                }
                jpeg { return "JPEGDATA" }
            }
            return 1
        }
    }
    return {}
}
set ::camera_perm 1
# The gateway's two pool routes, recorded; replies by route.
set ::pool_posts {}
set ::pool_reply [dict create]
proc ::vapi::post {path body {ms 0}} {
    lappend ::pool_posts [list $path $body]
    return [dict get $::pool_reply $path]
}
set ::health [dict create status ok uptime_s 600 version 0.1.0]
namespace eval ::vapi {
    proc get_json {path} {
        if {$path ne "/api/health"} { error "unexpected GET $path" }
        if {$::health eq ""} { error "connection refused" }
        return $::health
    }
}
namespace eval ::vdevice {
    variable caps [dict create notify {} info {} toast {}]
}

# The app's files directory, where DesktopActivity writes the status.
set ::files [file join [expr {[info exists ::env(TMPDIR)] ? $::env(TMPDIR) : "/tmp"}] \
    phonetest-[pid]]
file mkdir $::files
set ::env(INTERNAL_STORAGE) $::files
proc write_status {lines {bump 0}} {
    set f [open [file join $::files phone-status] w]
    puts -nonewline $f [join $lines \n]
    close $f
    if {$bump} {
        file mtime [file join $::files phone-status] [expr {[clock seconds] + $bump}]
    }
}
proc wait_ms {ms} {
    set ::_t 0
    after $ms {set ::_t 1}
    vwait ::_t
}

source [file join $_dir phone.tcl]

# ── the status file ──────────────────────────────────────────────────
ok "no file: nothing known" {[dict size [::vphone::status]] == 0}
write_status {notifications=0 background=0 location=off listener=0 maker=Xiaomi \
    free=12300000000 total=64000000000 odd=a=b sdk=34}
set s [::vphone::status]
ok "the file reads as a dict" {[dict get $s notifications] eq "0" && [dict get $s maker] eq "Xiaomi"}
ok "a value may hold =" {[dict get $s odd] eq "a=b"}

# ── a phone that holds veles back ────────────────────────────────────
set w [::vphone::build .ov_phone]
update
proc txt {path} { .ov_phone.$path cget -text }
ok "notifications off, and the button turns them on" \
    {[txt rows.notifications.state] eq "off" && [txt rows.notifications.go] eq "Turn on"}
ok "background limited by Android, and the button allows it" \
    {[txt rows.background.state] eq "limited by Android" && [txt rows.background.go] eq "Allow"}
ok "location off, and the button asks" \
    {[txt rows.location.state] eq "off" && [txt rows.location.go] eq "Allow"}
ok "reading notifications off, and the button turns it on" \
    {[txt rows.reading.state] eq "off" && [txt rows.reading.go] eq "Turn on"}
ok "not started with the phone" \
    {[txt rows.boot.state] eq "off" && [txt rows.boot.go] eq "Turn on"}
ok "no driver row where the build has no driver" {[catch {pack info .ov_phone.rows.driver}]}
ok "the network in Android's words" {[txt rows.network.state] eq "mobile LTE"}
ok "free space in GB" {[txt rows.storage.state] eq "12.3 GB free of 64.0 GB"}
set since [clock format [expr {[clock seconds] - 600}] -format %H:%M]
ok "the engine and when it started" {[txt rows.engine.state] eq "running since $since"}
ok "a maker that stops apps gets the extra step" \
    {[string match "Phones made by Xiaomi also stop apps*" [txt maker]]}
ok "what the agent can do here" {[txt caps] eq "info, notify, toast"}

# ── each button opens the right Android screen ───────────────────────
set ::borg_calls {}
.ov_phone.rows.notifications.go invoke
ok "notifications: the app's notification settings" \
    {[lindex $::borg_calls 0] eq {activity android.settings.APP_NOTIFICATION_SETTINGS {} {} {} {} {android.provider.extra.APP_PACKAGE chat.janitor.veles}}}
set ::borg_calls {}
.ov_phone.rows.background.go invoke
ok "background: Android's one-tap dialog for this app" \
    {[lindex $::borg_calls 0] eq {activity android.settings.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS package:chat.janitor.veles {} {} {} {}}}
set ::borg_calls {}
set ::borg_fail {{activity android.settings.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS*}}
.ov_phone.rows.background.go invoke
ok "a phone without that dialog gets the app's own page" \
    {[lindex $::borg_calls end] eq {activity android.settings.APPLICATION_DETAILS_SETTINGS package:chat.janitor.veles {} {} {} {}}}
set ::borg_fail {}
set ::borg_calls {}
.ov_phone.rows.location.go invoke
ok "location off: Android asks for it" \
    {[lindex $::borg_calls 0] eq {checkpermission android.permission.ACCESS_FINE_LOCATION android.permission.ACCESS_COARSE_LOCATION 1}
     && [llength $::borg_calls] == 1}
set ::borg_calls {}
.ov_phone.rows.reading.go invoke
ok "reading: Android's notification access switch for veles' listener" \
    {[lindex $::borg_calls 0] eq {activity android.settings.NOTIFICATION_LISTENER_DETAIL_SETTINGS {} {} {} {} {android.provider.extra.NOTIFICATION_LISTENER_COMPONENT_NAME chat.janitor.veles/chat.janitor.veles.NotificationListener}}}
set ::borg_calls {}
set ::borg_fail {{activity android.settings.NOTIFICATION_LISTENER_DETAIL_SETTINGS*}}
.ov_phone.rows.reading.go invoke
ok "an older Android gets the list of every app with access" \
    {[lindex $::borg_calls end] eq {activity android.settings.ACTION_NOTIFICATION_LISTENER_SETTINGS {} {} {} {} {}}}
set ::borg_fail {}

# "Start with the phone" is veles' own switch: the file the boot
# receiver reads (WakeReceiver.bootFlag), no Android screen.
set ::borg_calls {}
.ov_phone.rows.boot.go invoke
update
ok "start with the phone: on, and the file is there" \
    {[file exists [file join $::files boot-start]]
     && [txt rows.boot.state] eq "on" && [txt rows.boot.go] eq "Turn off"
     && [lsearch -glob $::borg_calls activity*] < 0}
.ov_phone.rows.boot.go invoke
update
ok "and off again" \
    {![file exists [file join $::files boot-start]]
     && [txt rows.boot.state] eq "off" && [txt rows.boot.go] eq "Turn on"}

set ::borg_calls {}
.ov_phone.bar.app invoke
ok "the app's own page, from the bar" \
    {[lindex $::borg_calls 0] eq {activity android.settings.APPLICATION_DETAILS_SETTINGS package:chat.janitor.veles {} {} {} {}}}

# ── back from Android's settings: the page redraws by itself ─────────
write_status {notifications=1 background=1 location=precise listener=1 driver=0 \
    maker=Google free=5000000000 total=64000000000} 5
wait_ms 1300
ok "notifications on" \
    {[txt rows.notifications.state] eq "on" && [txt rows.notifications.go] eq "Settings"}
ok "background allowed" {[txt rows.background.state] eq "allowed"}
ok "location precise" {[txt rows.location.state] eq "precise"}
ok "reading notifications on" \
    {[txt rows.reading.state] eq "on" && [txt rows.reading.go] eq "Settings"}
ok "the sideload build's driver row, off, and the button turns it on" \
    {![catch {pack info .ov_phone.rows.driver}]
     && [txt rows.driver.state] eq "off" && [txt rows.driver.go] eq "Turn on"}
set ::borg_calls {}
.ov_phone.rows.driver.go invoke
ok "driver: Android's accessibility switch for veles' driver" \
    {[lindex $::borg_calls 0] eq {activity android.settings.ACCESSIBILITY_DETAILS_SETTINGS {} {} {} {} {android.intent.extra.COMPONENT_NAME chat.janitor.veles/chat.janitor.veles.AccessibilityDriver}}}
set ::borg_calls {}
set ::borg_fail {{activity android.settings.ACCESSIBILITY_DETAILS_SETTINGS*}}
.ov_phone.rows.driver.go invoke
ok "an older Android gets the accessibility list" \
    {[lindex $::borg_calls end] eq {activity android.settings.ACCESSIBILITY_SETTINGS {} {} {} {} {}}}
set ::borg_fail {}
ok "no maker step for a maker that keeps to Android's rules" {[txt maker] eq ""}
set ::borg_calls {}
.ov_phone.rows.location.go invoke
ok "location already precise: the app's page, not another ask" \
    {[lindex $::borg_calls 0] eq {activity android.settings.APPLICATION_DETAILS_SETTINGS package:chat.janitor.veles {} {} {} {}}}

# ── nothing known, nothing running, no network ───────────────────────
file delete [file join $::files phone-status]
set ::health ""
set ::network none
::vphone::render .ov_phone
ok "unknown when DesktopActivity wrote nothing" \
    {[txt rows.notifications.state] eq "unknown" && [txt rows.background.state] eq "unknown"
     && [txt rows.location.state] eq "unknown" && [txt rows.storage.state] eq "unknown"
     && [txt rows.reading.state] eq "unknown"}
ok "the engine not running" {[txt rows.engine.state] eq "not running"}
ok "no network is offline" {[txt rows.network.state] eq "offline"}

# ── closing stops the watch ──────────────────────────────────────────
destroy .ov_phone
update
ok "the poll is cancelled" \
    {$::vphone::poll eq "" && [string first ::vphone::_watch [join [lmap a [after info] {after info $a}]]] < 0}

# ── joining the pool by QR (P2d) ─────────────────────────────────────
set w [::vphone::build .ov_phone]
update
.ov_phone.join invoke
update
ok "the Phone page opens Join a pool" {[winfo exists .ov_pooljoin]}
ok "the camera opens and previews" \
    {[lsearch -exact $::borg_calls {camera open}] >= 0 && [lsearch -exact $::borg_calls {camera start}] >= 0}

dict set ::pool_reply /api/pool/scan {{"error":"no QR code in the photo; hold closer","kind":"no_code"}}
set ::pool_posts {}
.ov_pooljoin.bar.scan invoke
wait_ms 100
ok "Scan sends the camera's JPEG to the gateway" \
    {[lindex $::pool_posts 0] eq [list /api/pool/scan "{\"image_b64\":\"[binary encode base64 JPEGDATA]\"}"]}
ok "no code: said in the window's words" \
    {[string match "No code in the picture*" [.ov_pooljoin.status cget -text]]}
ok "nothing to confirm" {![winfo ismapped .ov_pooljoin.confirm]}

dict set ::pool_reply /api/pool/scan {{"text":"veles-pool:1:{}","arbiter":"vps","arbiter_url":"http://10.0.30.1:9900","name":"phone2","url":"http://10.0.30.4:9900","expires_in":540}}
.ov_pooljoin.bar.scan invoke
wait_ms 100
update
ok "a good code asks first, naming the pool and the address" \
    {[winfo ismapped .ov_pooljoin.confirm]
     && [.ov_pooljoin.confirm.what cget -text] eq "Join the pool of vps as phone2? The pool will reach this phone at http://10.0.30.4:9900."}
ok "nothing is joined before the yes" \
    {[lsearch -index 0 $::pool_posts /api/pool/join] < 0}
dict set ::pool_reply /api/pool/join {{"ok":true,"arbiter":"vps","members":["laptop"],"not_introduced":[],"restart":true}}
.ov_pooljoin.confirm.yes invoke
update
ok "Join sends the scanned invite" \
    {[lindex $::pool_posts end] eq {/api/pool/join {{"text":"veles-pool:1:{}"}}}}
ok "and says who it joined, and to reopen veles" \
    {[.ov_pooljoin.status cget -text] eq "Joined vps and 1 other machine(s). Close veles and open it again so the pool can reach this phone."}
ok "the camera is put away once joined" {[lsearch -exact $::borg_calls {camera close}] >= 0}

# Pasted: the WireGuard code by mistake, then an expired invite.
.ov_pooljoin.bar.paste invoke
update
dict set ::pool_reply /api/pool/scan {{"error":"this is the WireGuard code: …","kind":"wireguard"}}
set ::pool_posts {}
.ov_pooljoin.paste.t insert end {[Interface] "x"}
.ov_pooljoin.paste.read invoke
ok "pasted text goes as text" \
    {[lindex $::pool_posts 0] eq {/api/pool/scan {{"text":"[Interface] \"x\""}}}}
ok "the WireGuard code is named for what it is" \
    {[string match "That is the WireGuard code*" [.ov_pooljoin.status cget -text]]}
dict set ::pool_reply /api/pool/scan {{"text":"veles-pool:1:{}","arbiter":"vps","arbiter_url":"u","name":"p","url":"u","expires_in":0}}
.ov_pooljoin.paste.read invoke
ok "an expired code says so" {[.ov_pooljoin.status cget -text] eq "This code has expired. Ask for a new one."}
dict set ::pool_reply /api/pool/scan {{"error":"something else entirely"}}
.ov_pooljoin.paste.read invoke
ok "anything else is shown as the gateway said it" \
    {[.ov_pooljoin.status cget -text] eq "Could not join: something else entirely"}
destroy .ov_pooljoin
update

# No camera permission yet: Android is asked, and the paste box is there.
set ::camera_perm 0
set ::borg_calls {}
::vphone::join_page
update
ok "without the camera, Android is asked and pasting is offered" \
    {[lsearch -exact $::borg_calls {checkpermission android.permission.CAMERA 1}] >= 0
     && [lsearch -exact $::borg_calls {camera open}] < 0
     && [winfo ismapped .ov_pooljoin.paste]
     && [string match "Allow the camera*" [.ov_pooljoin.status cget -text]]}
destroy .ov_pooljoin
destroy .ov_phone
update
set ::camera_perm 1

# ── reachable by touch, and only on a phone ──────────────────────────
source [file join $_dir palette.tcl]
ok "not in the palette off a phone" \
    {[lsearch -exact -index 1 [::vpalette::actions] {::voverlay::open phone}] < 0}
namespace eval ::vandroid { proc on {} { return 1 } }
set row [lsearch -inline -exact -index 1 [::vpalette::actions] {::voverlay::open phone}]
ok "in the palette on a phone, under settings" {[lindex $row 2] eq "settings"}
set _o [open [file join $_dir overlays.tcl] r]
set _osrc [read $_o]; close $_o
ok "the overlay switch delegates to the module" \
    {[string first {::vphone::build $w} $_osrc] >= 0}

file delete -force $::files
puts "\nphonetest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
