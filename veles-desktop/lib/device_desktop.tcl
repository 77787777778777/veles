# veles-desktop/lib/device_desktop.tcl — what a desktop window can do for
# the agent (PLAN-sessions-and-phone.md A1): the laptop's half of the
# device bridge. The phone's half is device_android.tcl; a window
# registers one or the other.

# Providers are named cap_<action>: a proc called `open`, `info` or
# `clipboard` inside this namespace would shadow the Tcl/Tk command of
# that name for every caller in it, itself included.
namespace eval ::vdevice::desktop {}

proc ::vdevice::desktop::_arg {a key {default ""}} {
    if {[dict exists $a $key]} { return [dict get $a $key] }
    return $default
}

proc ::vdevice::desktop::cap_notify {a} {
    set title [_arg $a title veles]
    set text [_arg $a text]
    if {$::tcl_platform(platform) eq "windows"} {
        return [_toast $title $text]
    }
    set cmd [auto_execok notify-send]
    if {$cmd eq ""} { error "this desktop has no notify-send" }
    if {![::vsafe::spawn [lindex $cmd 0] -- $title $text]} {
        error "notify-send did not start"
    }
    return "notification posted"
}

# A Windows toast through Windows PowerShell's WinRT projection (pwsh 7
# has none without an SDK package). The title and the text travel in the
# environment, never in the script text, so nothing a sender wrote is
# parsed as PowerShell.
proc ::vdevice::desktop::_toast {title text} {
    set ps [auto_execok powershell]
    if {$ps eq ""} { error "this desktop has no powershell for toasts" }
    set ::env(VELES_TOAST_TITLE) $title
    set ::env(VELES_TOAST_TEXT) $text
    # The toast is posted under the AppUserModelId `veles`. An id Windows
    # has not been told about is a toast it drops without a word: Show()
    # returns, nothing appears, nothing is stored (measured 2026-10-14 on
    # winbox — a ticket-authorized notify answered "notification
    # posted" and Windows' own store, wpndatabase.db, had no row for it;
    # the same toast under a registered id showed). What an unpackaged
    # app registers is the per-user key below, DisplayName and all; no
    # shortcut, no elevation. Idempotent, so every toast may run it.
    set script {
        [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
        [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null
        $k = 'HKCU:\Software\Classes\AppUserModelId\veles'
        if (-not (Test-Path $k)) { New-Item -Path $k -Force | Out-Null }
        if ((Get-ItemProperty -Path $k -Name DisplayName -ErrorAction SilentlyContinue).DisplayName -ne 'veles') {
            Set-ItemProperty -Path $k -Name DisplayName -Value 'veles'
        }
        $x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
        $t = $x.GetElementsByTagName('text')
        $t.Item(0).AppendChild($x.CreateTextNode($env:VELES_TOAST_TITLE)) | Out-Null
        $t.Item(1).AppendChild($x.CreateTextNode($env:VELES_TOAST_TEXT)) | Out-Null
        [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('veles').Show([Windows.UI.Notifications.ToastNotification]::new($x))
    }
    # Windows PowerShell's console host needs a console, and Tcl's exec
    # creates a console child of a GUI parent (wish) DETACHED — the
    # child then exits 0 at once having run nothing, whatever the
    # launch form (measured 2026-10-14 on winbox: -Command,
    # -EncodedCommand and -File from wish all silent, the same lines
    # from tclsh all ran). `cmd /c start` gives it a console: minimized,
    # and -WindowStyle Hidden hides it as PowerShell starts (a taskbar
    # button for a moment). The script travels base64-encoded, so none
    # of its characters meets cmd's parser on the way.
    set start [auto_execok start]
    if {$start eq ""} { error "this desktop has no cmd.exe to start powershell with" }
    set enc [binary encode base64 [encoding convertto unicode $script]]
    set exe [file nativename [lindex $ps 0]]
    if {[catch { exec {*}$start "" /min [::vsafe::arg $exe] -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand [::vsafe::arg $enc] & } e]} {
        error "powershell did not start: $e"
    }
    return "notification posted"
}

proc ::vdevice::desktop::cap_open {a} {
    set uri [_arg $a uri]
    if {$uri eq ""} { error "open needs a uri" }
    if {![::vmd::open_url $uri]} {
        error "could not open $uri (only http, https and mailto links open from here)"
    }
    return "opened $uri"
}

proc ::vdevice::desktop::cap_clipboard {a} {
    if {[dict exists $a text]} {
        clipboard clear
        clipboard append [dict get $a text]
        return "copied to the clipboard"
    }
    if {[catch { clipboard get } text]} { return "" }
    return $text
}

proc ::vdevice::desktop::cap_info {a} {
    return [::vapi::json_raw [format {{"host": %s, "os": %s, "screen": "%dx%d"}} \
        "\"[::vapi::json_escape [info hostname]]\"" \
        "\"[::vapi::json_escape "$::tcl_platform(os) $::tcl_platform(osVersion)"]\"" \
        [winfo screenwidth .] [winfo screenheight .]]]
}

proc ::vdevice::desktop::register {} {
    if {[::vdevice::on_android]} { return }
    ::vdevice::provide notify \
        "Post a desktop notification." \
        {{"type":"object","properties":{"title":{"type":"string"},"text":{"type":"string"}},"required":["text"]}} \
        ::vdevice::desktop::cap_notify
    ::vdevice::provide open \
        "Open an http, https or mailto link in this computer's browser." \
        {{"type":"object","properties":{"uri":{"type":"string"}},"required":["uri"]}} \
        ::vdevice::desktop::cap_open
    ::vdevice::provide clipboard \
        "Read this computer's clipboard, or set it with text." \
        {{"type":"object","properties":{"text":{"type":"string"}}}} \
        ::vdevice::desktop::cap_clipboard
    ::vdevice::provide info \
        "This computer's host name, operating system and screen." \
        {} ::vdevice::desktop::cap_info
}
