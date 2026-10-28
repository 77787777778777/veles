# veles-desktop/lib/device_android.tcl — the phone's capabilities, through
# AndroWish's `borg` (PLAN-sessions-and-phone.md A1, USER decision D3).
#
# These run in the desktop's process, so they answer while veles is open
# on the phone; a phone that froze the app in the background answers
# nothing until it is opened again (the agent is told so). Capabilities
# that must work in the background belong to the gateway's service
# process, in Java — P4.
#
# Each one is small and says what it did: the agent reads the answer.

# Providers are named cap_<action>: a proc called `open`, `info` or
# `clipboard` inside this namespace would shadow the Tcl/Tk command of
# that name for every caller in it, itself included.
namespace eval ::vdevice::android {
    variable notify_id 1000
}

proc ::vdevice::android::_arg {a key {default ""}} {
    if {[dict exists $a $key]} { return [dict get $a $key] }
    return $default
}

proc ::vdevice::android::cap_notify {a} {
    variable notify_id
    set title [_arg $a title veles]
    set text [_arg $a text]
    incr notify_id
    borg notification add $notify_id $title $text
    return "notification $notify_id posted"
}

proc ::vdevice::android::cap_toast {a} {
    borg toast [_arg $a text] [expr {[_arg $a long 0] ? 1 : 0}]
    return "shown"
}

proc ::vdevice::android::cap_vibrate {a} {
    set ms [_arg $a ms 400]
    if {![string is integer -strict $ms] || $ms < 1 || $ms > 5000} {
        error "ms must be 1..5000"
    }
    borg vibrate $ms
    return "vibrated ${ms} ms"
}

proc ::vdevice::android::cap_speak {a} {
    set text [_arg $a text]
    if {$text eq ""} { error "nothing to say" }
    set lang [_arg $a lang]
    if {$lang eq ""} {
        borg speak $text
    } else {
        borg speak $text $lang
    }
    return "speaking"
}

# A link, a map, a phone number to dial, a web search: anything Android
# opens with an intent the person then sees.
proc ::vdevice::android::cap_open {a} {
    set uri [_arg $a uri]
    if {$uri eq ""} { error "open needs a uri (https:…, geo:…, tel:…, mailto:…)" }
    if {![regexp {^(https?|geo|tel|mailto|sms|market):} $uri]} {
        error "only https, geo, tel, mailto, sms and market links open from here"
    }
    set action android.intent.action.VIEW
    if {[string match tel:* $uri]} { set action android.intent.action.DIAL }
    borg activity $action $uri {}
    return "opened $uri"
}

proc ::vdevice::android::cap_share {a} {
    set text [_arg $a text]
    # action uri type ?cats comp args?: the extras are the SEVENTH word.
    # In the sixth, the component's, Android was asked for an activity
    # named after the extra's key, and refused (the real phone,
    # 2026-10-11, the Phone page's twin of this call).
    borg activity android.intent.action.SEND {} text/plain {} {} \
        [list android.intent.extra.TEXT $text]
    return "the share sheet is open"
}

proc ::vdevice::android::cap_info {a} {
    set out [dict create]
    catch { dict set out device [borg osbuildinfo] }
    catch { dict set out network [borg networkinfo] }
    catch { dict set out display [borg displaymetrics] }
    return $out
}

# Where the phone is: the newest fix any source has, waiting up to
# `wait` seconds for a first one.
proc ::vdevice::android::cap_location {a} {
    # Asked the first time the agent wants it, and only then: the
    # manifest declares it, Android asks the person.
    set perms {android.permission.ACCESS_FINE_LOCATION android.permission.ACCESS_COARSE_LOCATION}
    if {1 ni [borg checkpermission {*}$perms 0]} {
        borg checkpermission {*}$perms 1
        error "the phone is asking the person to allow location for veles — call again once they have answered"
    }
    set wait [_arg $a wait 20]
    borg location start 1000 0
    set fix ""
    set deadline [expr {[clock milliseconds] + 1000 * $wait}]
    while {[clock milliseconds] < $deadline} {
        catch { set fix [borg location get] }
        if {[llength $fix]} { break }
        set ::vdevice::android::_tick 0
        after 500 [list set ::vdevice::android::_tick 1]
        vwait ::vdevice::android::_tick
    }
    catch { borg location stop }
    if {![llength $fix]} {
        error "no location fix within ${wait}s (is location on, and permitted for veles?)"
    }
    return $fix
}

proc ::vdevice::android::register {} {
    if {![::vdevice::on_android]} { return }
    ::vdevice::provide notify \
        "Post a notification on the phone." \
        {{"type":"object","properties":{"title":{"type":"string"},"text":{"type":"string"}},"required":["text"]}} \
        ::vdevice::android::cap_notify
    ::vdevice::provide toast \
        "Show a short message on the phone's screen." \
        {{"type":"object","properties":{"text":{"type":"string"},"long":{"type":"boolean"}},"required":["text"]}} \
        ::vdevice::android::cap_toast -foreground 1
    ::vdevice::provide vibrate \
        "Vibrate the phone (ms, 1..5000)." \
        {{"type":"object","properties":{"ms":{"type":"integer"}}}} \
        ::vdevice::android::cap_vibrate
    ::vdevice::provide speak \
        "Read text aloud on the phone (lang: a language code, optional)." \
        {{"type":"object","properties":{"text":{"type":"string"},"lang":{"type":"string"}},"required":["text"]}} \
        ::vdevice::android::cap_speak
    ::vdevice::provide open \
        "Open a link on the phone: https:, geo: (a map), tel: (the dialer, not a call), mailto:, sms:, market:." \
        {{"type":"object","properties":{"uri":{"type":"string"}},"required":["uri"]}} \
        ::vdevice::android::cap_open -foreground 1
    ::vdevice::provide share \
        "Open the phone's share sheet with text for the person to send on." \
        {{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}} \
        ::vdevice::android::cap_share -foreground 1
    ::vdevice::provide info \
        "The phone's model, Android version, network and screen." \
        {} ::vdevice::android::cap_info
    ::vdevice::provide location \
        "Where the phone is now (waits up to `wait` seconds for a fix)." \
        {{"type":"object","properties":{"wait":{"type":"integer"}}}} \
        ::vdevice::android::cap_location -foreground 1 -timeout 60
}
