# The desktop frontend on Android: /assets/app/main.tcl in the APK.
#
# AndroWish's Tcl mounts the APK itself at /assets and runs this file
# instead of its console (DesktopActivity). It does for veles-desktop
# what `veles gui` does on a laptop, then sources the same tree:
#
#   - VELES_HOME is the gateway's home, files/home (Engine.java). This
#     process is the same app, so the desktop reads config.toml there
#     as it does on a laptop.
#   - The desktop API's bearer token is DESKTOP_BEARER_TOKEN in that
#     home's .env. The gateway mints it at its first start, so wait for
#     the gateway before reading it; DesktopActivity has just started it.
#   - VELES_EXE is the engine (lib/<abi>/libveles.so), for the ticket
#     signer the desktop runs as a child.
#
# AndroWish sets INTERNAL_STORAGE (the app's files directory), HOME,
# LD_LIBRARY_PATH (the native library directory) and LANG (Android's
# language, which the desktop's i18n reads) before this runs.

package require http

set _files $env(INTERNAL_STORAGE)
set env(VELES_HOME) [file join $_files home]

foreach _dir [split $env(LD_LIBRARY_PATH) :] {
    if {[file executable [file join $_dir libveles.so]]} {
        set env(VELES_EXE) [file join $_dir libveles.so]
        break
    }
}

# The value of KEY in a dotenv file, or "".
proc _dotenv {path key} {
    if {[catch { open $path r } f]} { return "" }
    set v ""
    while {[gets $f line] >= 0} {
        set line [string trim $line]
        if {[string first "$key=" $line] == 0} {
            set v [string trim [string range $line [string length "$key="] end] {"'}]
        }
    }
    close $f
    return $v
}

# Up to 30 s for the gateway to answer, the event loop running, so
# Android does not take the window for a hung app. A first start
# unpacks the runtime before the engine starts.
proc _gateway_up {} {
    if {[catch {
        set t [http::geturl http://127.0.0.1:8642/api/health -timeout 1000]
    }]} { return 0 }
    set ok [expr {[http::status $t] eq "ok" && [http::ncode $t] == 200}]
    http::cleanup $t
    return $ok
}
set _deadline [expr {[clock milliseconds] + 30000}]
while {![_gateway_up] && [clock milliseconds] < $_deadline} {
    after 500 { set ::_tick 1 }
    vwait ::_tick
}

set _token [_dotenv [file join $env(VELES_HOME) .env] DESKTOP_BEARER_TOKEN]
if {$_token ne ""} { set env(VELES_TOKEN) $_token }

source /assets/veles-desktop/main.tcl
