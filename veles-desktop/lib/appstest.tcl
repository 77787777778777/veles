# veles-desktop/lib/appstest.tcl — the person's apps (lib/apps.tcl,
# PLAN-sessions-and-phone.md A4): a REAL safe interpreter with Tk running
# an app's script, with the gateway and the phone stubbed. What the
# person is asked, what an app can and cannot reach, that a runaway loop
# is stopped, what the agent's device actions do, and the launcher.
#
# Run: scripts/tcl_tests.sh

package require Tk
package require json
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

source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
source [file join $_dir manage.tcl]
namespace eval ::voverlay { proc _header {w title} {} }
namespace eval ::va11y { proc name {w t} {} }

# ── stubs, with the REAL signatures ──────────────────────────────────
namespace eval ::vstate {
    variable blob [dict create]
    proc get {key {default ""}} {
        variable blob
        if {[dict exists $blob $key]} { return [dict get $blob $key] }
        return $default
    }
    proc put {key value} { variable blob ; dict set blob $key $value ; return 1 }
}
# The phone's providers, recorded.
set ::device_calls {}
namespace eval ::vdevice {
    variable caps [dict create]
    variable provided [dict create]
    proc provide {name description schema proc args} {
        variable provided
        dict set provided $name $proc
    }
}
foreach act {notify toast} {
    proc ::fake_$act {a} [list lappend ::device_calls [list $act {$a}]]
    dict set ::vdevice::caps $act [dict create proc ::fake_$act]
}
proc ::fake_notify {a} { lappend ::device_calls [list notify $a] ; return "notification posted" }
proc ::fake_toast {a} { lappend ::device_calls [list toast $a] ; return "shown" }

# The gateway: store and source, recorded.
set ::posts {}
set ::store_reply {{"value":true}}
proc ::vapi::post {path body {ms 0}} {
    lappend ::posts [list $path $body]
    return $::store_reply
}
set ::source_json {}
set ::listing_json {{"apps":[]}}
proc ::vapi::get_json {path {ms 0}} {
    switch -glob -- $path {
        /api/apps/*/source { return [::json::json2dict $::source_json] }
        /api/apps { return [::json::json2dict $::listing_json] }
    }
    error "unexpected GET $path"
}
# The agent's answer to an app's ask: http::geturl, answered later.
set ::asks {}
set ::ask_reply {{"reply":"three eggs"}}
proc ::http::geturl {url args} {
    lappend ::asks [list $url [dict get $args -query]]
    set tok ::fake_tok[llength $::asks]
    set ::fake_data($tok) $::ask_reply
    after 20 [list {*}[dict get $args -command] $tok]
    return $tok
}
proc ::http::status {tok} { return ok }
proc ::http::data {tok} { return $::fake_data($tok) }
proc ::http::cleanup {tok} {}
proc wait_ms {ms} { set ::_t 0 ; after $ms {set ::_t 1} ; vwait ::_t }

source [file join $_dir apps.tcl]

proc payload {caps key main} {
    return [dict create \
        info [dict create name counter title Counter entry main.tcl \
            capabilities $caps grant_key $key unknown {}] \
        files [dict create main.tcl $main]]
}
set main {
    set ::n 0
    ttk::button .b -text 0 -command {
        incr ::n
        .b configure -text $::n
        veles::notify Counter "n=$::n"
    }
    pack .b
}

# ── the person is asked first ────────────────────────────────────────
set why [::vapps::run counter [payload {notify store ask} k1 $main] no]
ok "a no runs nothing" {$why eq "not allowed" && ![dict exists $::vapps::running counter]}
set why [::vapps::run counter [payload {notify store ask} k1 $main] yes]
update
ok "a yes runs it" {$why eq "" && [dict exists $::vapps::running counter]}
ok "and is remembered for this declaration" {[dict get [::vstate::get app_grants] counter] eq "k1"}
set s [dict get $::vapps::running counter interp]
ok "it runs in a safe interpreter" {[interp issafe $s]}
ok "embedded in the window's own toplevel" {[dict get $::vapps::running counter embedded] == 1}

# ── what the app can and cannot reach ────────────────────────────────
$s eval {.b invoke ; .b invoke}
ok "its widgets work" {[$s eval {.b cget -text}] eq "2"}
ok "a declared call reaches the phone" \
    {[lindex $::device_calls end] eq {notify {title Counter text n=2}}}
ok "an undeclared one is refused" \
    {[$s eval {catch {veles::toast hi} e; set e}] eq "counter may not toast: its app.toml does not declare it"}
ok "an undeclared host is refused" \
    {[$s eval {catch {veles::http https://evil.example/x {set ::got}} e; set e}] eq "counter may not http:evil.example: its app.toml does not declare it"}
ok "exec, open and socket are not there" \
    {[$s eval {catch {exec ls}}] && [$s eval {catch {open /etc/hosts}}] && [$s eval {catch {socket localhost 1}}]}
ok "nor is the host's own interpreter" {[$s eval {catch {::vapps::stop counter}}]}

set ::store_reply {{"value":"3"}}
set ::posts {}
$s eval {veles::store set eggs 3}
ok "store set goes to the app's own store" \
    {[lindex $::posts 0] eq {/api/apps/counter/store {{"op":"set","key":"eggs","value":"3"}}}}
ok "store get gives the value back" {[$s eval {veles::store get eggs}] eq "3"}

$s eval {veles::ask "how many eggs?" {set ::answer}}
ok "an ask does not wait in the app" {[$s eval {info exists ::answer}] == 0}
wait_ms 100
ok "the reply comes to the app's callback" {[$s eval {set ::answer}] eq "three eggs"}
ok "asked at the app's own route" {[string match */api/apps/counter/ask [lindex $::asks 0 0]]}

$s eval {veles::log hello there}
ok "veles::log lands in the app's log" {[string match "*hello there" [::vapps::log_text counter]]}

# ── a runaway loop is stopped, the window lives ──────────────────────
set why [::vapps::run counter [payload {notify store ask} k1 {after 50 {while 1 {}}}]]
set t0 [clock milliseconds]
wait_ms 3600
ok "the runaway loop is stopped and logged" {[string match "*limit*" [::vapps::log_text counter]]}
ok "the window kept running" {[clock milliseconds] - $t0 < 6000}

# ── a wider declaration asks again ───────────────────────────────────
set why [::vapps::run counter [payload {notify store ask location} k2 $main] no]
ok "a new capability asks again" {$why eq "not allowed"}

# ── the agent's device actions ───────────────────────────────────────
::vapps::register
ok "the window offers run, stop, reload, logs and screenshot" \
    {[lsort [dict keys $::vdevice::provided]] eq {app.logs app.reload app.run app.screenshot app.stop}}
set ::source_json {{"info":{"name":"counter","title":"Counter","entry":"main.tcl","capabilities":["notify"],"grant_key":"k3","unknown":[]},"files":{"main.tcl":"pack [ttk::label .l -text hi]"}}}
dict set ::vstate::blob app_grants [dict create counter k3]
ok "app.run runs it" {[::vapps::cap_run {name counter}] eq "running counter"}
ok "app.logs reads its log" {[string match "*started*" [::vapps::cap_logs {name counter}]]}
ok "app.stop stops it" {[::vapps::cap_stop {name counter}] eq "stopped counter" && ![dict exists $::vapps::running counter]}
ok "a second stop says so" {[::vapps::cap_stop {name counter}] eq "counter was not running"}
set ::source_json {{"info":{"name":"counter","title":"Counter","entry":"main.tcl","capabilities":[],"grant_key":"k4","unknown":[]},"files":{"main.tcl":"error boom"}}}
ok "a script that fails is said, and logged" \
    {[catch {::vapps::cap_run {name counter}} e] && [string match "*boom*" $e] && [string match "*error: boom*" [::vapps::log_text counter]]}
::vapps::stop counter

# ── off a phone: no screenshot, no pin; on one, Android does both ────
dict set ::vstate::blob app_grants [dict create counter k5]
set ::source_json {{"info":{"name":"counter","title":"Counter","entry":"main.tcl","capabilities":["notify"],"grant_key":"k5","unknown":[]},"files":{"main.tcl":"pack [ttk::label .l -text hi]"}}}
::vapps::cap_run {name counter}
update
ok "off a phone a screenshot says why not" \
    {[catch {::vapps::cap_screenshot {name counter}} e] && [string match "*taken on a phone*" $e]}
ok "off a phone there is no pinning" {[::vapps::pin counter Counter] == 0}
set ::env(INTERNAL_STORAGE) [file join [expr {[info exists ::env(TMPDIR)] ? $::env(TMPDIR) : "/tmp"}] appstest-[pid]]
file mkdir $::env(INTERNAL_STORAGE)
set ::borg_calls {}
proc ::borg {args} {
    lappend ::borg_calls $args
    # DesktopActivity's SNAP receiver: the PNG appears at the path asked.
    if {[lrange $args 0 2] eq {broadcast send chat.janitor.veles.SNAP}} {
        set extras [lindex $args 6]
        after 50 [list apply {{p} { set f [open $p wb] ; puts -nonewline $f "\x89PNGfake" ; close $f }} [dict get $extras path]]
    }
}
set shot [::vapps::cap_screenshot {name counter}]
set snap [lindex $::borg_calls 0]
ok "a screenshot asks Android for the app window's rectangle" \
    {[lrange $snap 0 5] eq {broadcast send chat.janitor.veles.SNAP {} {} {}}
     && [dict get [lindex $snap 6] w] > 1 && [string match "*/snaps/counter-*.png" [dict get [lindex $snap 6] path]]}
ok "and hands back the PNG it wrote" {[binary decode base64 $shot] eq "\x89PNGfake"}
ok "the picture is not left behind" {[llength [glob -nocomplain [file join $::env(INTERNAL_STORAGE) snaps *]]] == 0}
set ::borg_calls {}
::vapps::pin counter Counter ICONB64
ok "pin asks PinActivity, by its component, with the app's name" \
    {[lindex $::borg_calls 0] eq {activity android.intent.action.MAIN {} {} {} {chat.janitor.veles chat.janitor.veles.PinActivity} {name counter label Counter icon ICONB64}}}
::vapps::stop counter
rename ::borg {}
file delete -force $::env(INTERNAL_STORAGE)
unset ::env(INTERNAL_STORAGE)

# Ask the agent to change it: the person's words start in the composer.
namespace eval ::vcmdtree { proc compose {text} { set ::composed $text } }
::vapps::ask_change counter Counter
ok "asking to change an app starts the request in the composer" \
    {$::composed eq "Change my app “Counter” (counter): "}
ok "the launcher's menu has run, stop, change and pin" \
    {[lmap r [::vapps::_menu_rows] {lindex $r 1}] eq {run stop change pin}}

# ── the launcher ─────────────────────────────────────────────────────
::vapps::launcher .ov_apps {}
update
ok "no apps: says how to get one" {[string match "No apps yet*" [.ov_apps.body.c.inner.empty cget -text]]}
destroy .ov_apps
set ::listing_json {{"apps":[{"name":"counter","title":"Counter","capabilities":[],"grant_key":"k4","entry":"main.tcl","unknown":[],"icon_png":null},{"name":"broken","error":"no app.toml"}]}}
::vapps::launcher .ov_apps
update
set kids [winfo children .ov_apps.body.c.inner]
ok "one button per app, broken ones marked" \
    {[llength $kids] == 2 && [.ov_apps.body.c.inner.a1 cget -text] eq "Counter" && [string match "broken*" [.ov_apps.body.c.inner.a2 cget -text]]}
set ::source_json {{"info":{"name":"counter","title":"Counter","entry":"main.tcl","capabilities":[],"grant_key":"k4","unknown":[]},"files":{"main.tcl":"pack [ttk::label .l -text hi]"}}}
.ov_apps.body.c.inner.a1 invoke
update
ok "a tap runs it" {[dict exists $::vapps::running counter]}
destroy [dict get $::vapps::running counter top]
update
ok "closing its window stops it" {![dict exists $::vapps::running counter]}
.ov_apps.body.c.inner.a2 invoke
ok "a broken one says why" {[.ov_apps.status cget -text] eq "broken: no app.toml"}
destroy .ov_apps

# ── reachable by touch ───────────────────────────────────────────────
source [file join $_dir palette.tcl]
ok "the palette offers it" \
    {[lsearch -exact -index 1 [::vpalette::actions] {::voverlay::open apps}] >= 0}

puts "\nappstest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
