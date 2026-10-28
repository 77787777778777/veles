# Desktop half of live-join-rehearsal.sh. The application has already
# sourced its production main.tcl under Xvfb; no HTTP or join proc is stubbed.
set phase [lindex $argv 0]
set participant [lindex $argv 1]
set root $::env(VELES_JOIN_LIVE_ROOT)
set status [file join $root "desktop-$phase.ok"]
file delete -force $status

proc fail {text} {
    puts stderr "LIVE-JOIN $::phase: $text"
    exit 1
}
proc await {predicate {tries 160}} {
    for {set i 0} {$i < $tries} {incr i} {
        if {[uplevel #0 $predicate]} { return }
        after 100 {set ::tick 1}
        vwait ::tick
    }
    fail "timed out: $predicate"
}
proc assert {condition description} {
    if {![uplevel #0 $condition]} { fail $description }
    puts "PASS desktop-$::phase: $description"
}

after idle {
    if {[catch {
        if {$phase eq "join"} {
            await {expr {[llength [::vinst::names]] >= 2}}
            ::vsidebar::new_session
            set origin $::vchat::session
            assert {expr {$origin ne ""}} "the origin desktop created a session"
            ::vchat::_join_command $origin /join "vps/$participant"
            assert {expr {[::vapi::attached] eq "vps"}} "/join attaches to the named target gateway"
            assert {expr {$::vchat::session eq $participant}} "the target participant is open"
            set spec [::vinst::remote_join]
            assert {expr {[dict get $spec from_session] eq $origin && [dict get $spec from_instance] eq "laptop"}} "the desktop saved only its local return view"
            set jid [dict get $spec join_id]
            set f [open [file join $root join-id] w]
            puts -nonewline $f $jid
            close $f
            .mid.chatwrap.chat.input delete 1.0 end
            .mid.chatwrap.chat.input insert 1.0 "owner line before restart"
            ::vchat::send_current
            await {expr {!$::vchat::busy}}
            assert {expr {[dict size [::vinst::remote_join]] > 0}} "the joined view remained active after composing"
        } else {
            await {expr {[llength [::vinst::names]] >= 2}}
            await {expr {[::vapi::attached] eq "vps" && $::vchat::session eq $participant && [dict size [::vinst::remote_join]] > 0}}
            set spec [::vinst::remote_join]
            assert {expr {[dict get $spec instance] eq "vps"}} "a fresh desktop restored the target attachment"
            assert {expr {[dict get $spec target_session] eq $participant}} "a fresh desktop restored the exact joined participant"
            .mid.chatwrap.chat.input delete 1.0 end
            .mid.chatwrap.chat.input insert 1.0 "owner line after both restarts"
            ::vchat::send_current
            await {expr {!$::vchat::busy}}
            await {expr {![file exists [file join $root may-leave]]}}
            ::vchat::_join_command $participant /leave {}
            assert {expr {[::vapi::attached] eq "laptop" && [dict size [::vinst::remote_join]] == 0}} "/leave returned to the origin and cleared the saved join"
        }
        set f [open $status w]
        puts $f ok
        close $f
        after 100 {destroy .}
    } err opts]} {
        puts stderr "LIVE-JOIN $::phase: $err"
        puts stderr [dict get $opts -errorinfo]
        exit 1
    }
}
