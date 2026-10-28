#!/usr/bin/env wish
# R11 component installer. Real Tk flow and api.tcl; only the HTTP wire
# is replaced. Replies can arrive out of order to exercise generations.
set _dir [file dirname [file normalize [info script]]]
if {![info exists ::env(XVFB_RUNNING)]} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}
source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir i18n.tcl]
::vi18n::init
package require json
package require uuid
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
# Keep production callback code intact, but make the errors it normally
# reports through _say visible in this hermetic wire test.
if {[info exists ::env(VELES_INSTALLTEST_TRACE)]} {
    rename ::vapi::_get_json_async_done ::vapi::_get_json_async_done_original
    proc ::vapi::_get_json_async_done {tok} {
        puts "ASYNC CALLBACK $tok status=[::http::status $tok] code=[::http::ncode $tok] body=[::http::data $tok] registration=[array get ::vapi::_async_done $tok]"
        set rc [catch {::vapi::_get_json_async_done_original $tok} error options]
        puts "ASYNC RESULT $tok rc=$rc error=$error"
        if {$rc} { puts [dict get $options -errorinfo] }
    }
}
source [file join $_dir manage.tcl]

namespace eval ::vstatus {
    variable msgs {}
    proc set_msg {m} { variable msgs; lappend msgs $m }
}
namespace eval ::vstate {
    variable blob {}
    variable error ""
    proc get {key {default ""}} {
        variable blob
        expr {[dict exists $blob $key] ? [dict get $blob $key] : $default}
    }
    proc put {key value} { variable blob; dict set blob $key $value; return 1 }
}
namespace eval ::va11y {
    proc name {w label} {}
    proc tab_order {widgets} {}
}
namespace eval ::vplace { proc centered {w width height args} { wm geometry $w ${width}x${height} } }
namespace eval ::vtitle { proc attach {w title args} {} }
set ::mb_answer yes
set ::mb_calls {}
rename tk_messageBox tk_messageBox_real
proc tk_messageBox {args} { lappend ::mb_calls $args; return $::mb_answer }

namespace eval ::wire {
    variable seq 0
    variable requests {}
    variable callbacks
    array set callbacks {}
    variable replies
    array set replies {}
    variable pending
    array set pending {}
    variable local_state {{"first_run":true,"config_exists":false,"config_path":"/cfg/config.toml","env_path":"/cfg/.env","env_exists":false,"current":null,"current_error":null,"stored":[],"accepts_secrets":true}}
}
# The existing setup preview is synchronous. The new installer component
# calls are asynchronous. Stub the same transport boundary for both.
proc ::vapi::get_json {path {ms 0}} {
    if {$path ne "/api/setup/state"} { error "unexpected synchronous GET $path" }
    return [::json::json2dict $::wire::local_state]
}
proc ::vapi::post {path body {ms 0}} {
    if {$path ni {/api/setup/preview /api/setup/apply /api/install/profile}} { error "unexpected synchronous POST $path" }
    lappend ::wire::requests [dict create token sync path $path body $body]
    # Never reset: the end-of-suite wire check reads every request.
    lappend ::wire::all [dict create token sync path $path body $body]
    if {![info exists ::wire::replies($path)] || ![llength $::wire::replies($path)]} {
        error "no queued synchronous preview for $path"
    }
    set reply [lindex $::wire::replies($path) 0]
    set ::wire::replies($path) [lrange $::wire::replies($path) 1 end]
    if {[dict get $reply code] >= 400} { error "HTTP [dict get $reply code]: [dict get $reply body]" }
    return [dict get $reply body]
}
proc ::http::geturl {args} {
    incr ::wire::seq
    set tok "wire-$::wire::seq"
    set url [lindex $args 0]
    set path $url
    regexp {^https?://[^/]+(/.*)$} $url -> path
    set body ""
    set command ""
    set i [lsearch -exact $args -query]
    if {$i >= 0} { set body [lindex $args $i+1] }
    set i [lsearch -exact $args -command]
    if {$i >= 0} { set command [lindex $args $i+1] }
    set ::wire::callbacks($tok) $command
    lappend ::wire::requests [dict create token $tok path $path body $body]
    # Never reset: the end-of-suite wire check reads every request.
    lappend ::wire::all [dict create token $tok path $path body $body]
    if {[info exists ::env(VELES_INSTALLTEST_TRACE)]} {
        puts "REQUEST $tok path=$path body=$body command=$command"
    }
    if {[info exists ::wire::replies($path)] && [llength $::wire::replies($path)]} {
        set reply [lindex $::wire::replies($path) 0]
        set ::wire::replies($path) [lrange $::wire::replies($path) 1 end]
    } else {
        set reply [dict create delay 0 status ok code 500 body {{"ok":false,"error":"unexpected installer request"}}]
    }
    if {$path eq "/api/install/download"} {
        # The real server publishes the same opaque id the attempt sent.
        # Do not teach the client to accept a fixture's unrelated id.
        set sent [::json::json2dict $body]
        if {[dict exists $sent request_id]} {
            set response [dict get $reply body]
            if {[regexp {"plan_id":"([^"]+)"} $response -> old_id]} {
                dict set reply body [string map [list "\"plan_id\":\"$old_id\"" "\"plan_id\":\"[dict get $sent request_id]\""] $response]
            }
        }
    }
    set ::wire::pending($tok) [after [dict get $reply delay] \
        [list ::wire::complete $tok [dict get $reply status] [dict get $reply code] [dict get $reply body]]]
    return $tok
}
proc ::wire::complete {tok status code text} {
    if {![info exists ::wire::callbacks($tok)]} { return }
    set command $::wire::callbacks($tok)
    unset ::wire::callbacks($tok)
    unset -nocomplain ::wire::pending($tok)
    set ::http::bodies($tok) $text
    set ::http::codes($tok) $code
    set ::http::statuses($tok) $status
    if {$command ne ""} {
        if {[catch {uplevel #0 [linsert $command end $tok]} error options]} {
            puts "CALLBACK ERROR $tok path=<see request> command=$command\n$error"
            puts [dict get $options -errorinfo]
            incr ::failed
        }
    }
}
namespace eval ::http {
    variable bodies
    variable codes
    variable statuses
    proc data {tok} { variable bodies; return $bodies($tok) }
    proc ncode {tok} { variable codes; return $codes($tok) }
    proc status {tok} { variable statuses; return $statuses($tok) }
    proc cleanup {tok} {}
    proc error {tok} { return "fixture transport error" }
    proc reset {tok} {}
}
source [file join $_dir install.tcl]
source [file join $_dir setup.tcl]
if {[info exists ::env(VELES_INSTALLTEST_TRACE)]} {
    rename ::vinstall::signed_preview_done ::vinstall::signed_preview_done_original
    proc ::vinstall::signed_preview_done {epoch value} {
        puts "SIGNED CALLBACK epoch=$epoch generation=$::vinstall::generation tracked=[expr {[info exists ::vinstall::signed_request] ? $::vinstall::signed_request : {}}] value=$value"
        set rc [catch {::vinstall::signed_preview_done_original $epoch $value} why options]
        puts "SIGNED CALLBACK RESULT rc=$rc why=$why requests=$::wire::requests pending=[expr {[info exists ::vinstall::pending_action] ? $::vinstall::pending_action : {}}]"
        if {$rc} { return -options $options $why }
    }
}
if {[info exists ::env(VELES_INSTALLTEST_TRACE)]} {
    rename ::vsetup::_show_preview ::vsetup::_show_preview_original
    proc ::vsetup::_show_preview {text} {
        puts "SUMMARY TEXT <<$text>>"
        ::vsetup::_show_preview_original $text
    }
}

set passed 0
set failed 0
proc ok {name cond} {
    if {[catch {uplevel 1 [list expr $cond]} r]} {
        puts "  ✗ $name (threw: $r)"; incr ::failed; return
    }
    if {$r} { puts "  ✓ $name"; incr ::passed } else { puts "  ✗ $name"; incr ::failed }
}
proc pump {} { for {set i 0} {$i < 60} {incr i} { after 5; update } }
proc queue {path body {delay 0} {code 200}} {
    lappend ::wire::replies($path) [dict create delay $delay status ok code $code body $body]
}
proc paths {} { return [lmap r $::wire::requests {dict get $r path}] }
proc any_message {pattern} {
    foreach message $::vstatus::msgs { if {[string match $pattern $message]} { return 1 } }
    return 0
}
proc last_body {path} {
    set found {}
    foreach r $::wire::requests { if {[dict get $r path] eq $path} { set found $r } }
    if {$found eq {}} { return {} }
    return [::json::json2dict [dict get $found body]]
}
proc manifest_reply {name {delay 0}} {
    # Names in this fixture are deliberately restricted ASCII. Explicit
    # arrays and nulls preserve the real Manifest shape.
    set text [format {{"manifest":{"name":"%1$s","version":"1","protocol":1,"config_sections":["platforms.%1$s"],"instances":{"kind":"single"},"knobs":[{"path":"platforms.%1$s.server","default":"\"one\"","shape":"value","note":"server URL","env":null},{"path":"platforms.%1$s.token","default":null,"shape":"unset","note":null,"env":null}]}}} $name]
    return [dict create delay $delay status ok code 200 body $text]
}
proc queue_manifest {name {delay 0}} {
    lappend ::wire::replies(/api/install/options/$name) [manifest_reply $name $delay]
}
proc queue_preview {{plan plan-component} {base base-component} {delay 0}} {
    set text [format {{"ok":true,"base_sig":"%s","plan_sig":"%s","action":"configure","changes":["platforms.fixture.server"],"actions":["write declared component options"],"note":"configuration only","config_text":null}} $base $plan]
    queue /api/install/preview $text $delay
}
proc queue_state {{names {fixture}}} {
    set components {}
    foreach name $names { lappend components "{\"name\":\"$name\"}" }
    queue /api/install/state "{\"available\":\[[join $components ,]\],\"installed\":\[\]}"
}
proc queue_combined_preview {} {
    queue /api/setup/preview {{"ok":true,"base_sig":"base-combined","plan_sig":"plan-combined","mode":"edit","diff":"+server = \"two\"","config_toml":null,"env_text":"","secrets":[],"notes":[]}}
}
proc reset {} {
    set ::vsetup::install_busy 0
    catch {::vsetup::_close}
    catch {destroy .setup_wizard}
    set ::vsetup::win ""
    set ::vsetup::cur 0
    set ::vsetup::state {}
    set ::vsetup::api_err ""
    set ::vsetup::preview_sig ""
    set ::vsetup::preview_base ""
    set ::vsetup::preview_plan ""
    set ::vsetup::install_mode 0
    set ::vsetup::profile_mode 0
    set ::vsetup::install_profile desktop
    set ::vsetup::install_core ""
    set ::vsetup::install_container_json ""
    set ::vsetup::install_system 0
    set ::vsetup::install_user_unit 0
    set ::vsetup::install_container_storage ""
    set ::vsetup::install_container_gateway veles
    set ::vsetup::install_container_sandbox veles-sandbox
    set ::vsetup::install_container_network velesnet
    set ::vsetup::install_container_image ""
    set ::vsetup::install_container_digest ""
    set ::vsetup::install_container_name gateway
    set ::vsetup::install_pds 0
    set ::vsetup::install_pds_backup 0
    set ::vsetup::install_pds_url https://pds.example
    set ::vsetup::install_pds_domain pds.example
    set ::vsetup::install_pds_email acme@example
    set ::vsetup::install_pds_image ghcr.io/jazware/vlpds:1.2.2
    set ::vsetup::install_pds_digest sha256:[string repeat a 64]
    set ::vsetup::install_pds_s3_endpoint https://s3.example
    set ::vsetup::install_pds_s3_region auto
    set ::vsetup::install_pds_s3_bucket pds-bucket
    set ::vsetup::install_pds_s3_prefix pds
    set ::vsetup::install_pds_network veles-net
    ::vinstall::reset
    set ::vsetup::install_manifest {}
    set ::vsetup::install_instance ""
    set ::vsetup::skipped {}
    set ::vsetup::applied {}
    set ::vstatus::msgs {}
    set ::mb_answer yes
    set ::mb_calls {}
    set ::wire::requests {}
    array unset ::wire::replies
    array unset ::wire::callbacks
    foreach {tok event} [array get ::wire::pending] { after cancel $event }
    array unset ::wire::pending
    foreach n {install_option_0 install_option_1 install_option_2} { catch {unset ::vsetup::$n} }
}

# Named profile flow: typed answers, reviewed core path and explicit
# handoff results. Tk never invokes the privileged operation itself.
reset
set ::vsetup::install_core /managed/source
set ::vsetup::install_profile desktop
queue /api/install/profile {{"preview":true,"confirm_required":true,"base_sig":"base-profile","plan_sig":"plan-profile","diff":"[model]","actions":["install reviewed core"],"notes":[],"secrets":[]}}
queue /api/install/profile {{"ok":true,"status":"applied","applied":true,"service_running":false}}
::vsetup::open_profiles
pump
::vsetup::next
pump
::vsetup::next
pump
::vsetup::next
pump
set profile [last_body /api/install/profile]
ok "profile preview uses typed desktop answers" {[dict get $profile answers profile] eq "desktop" && [dict get $profile core] eq "/managed/source" && ![dict exists $profile confirm]}
ok "profile preview carries setup answers" {[dict exists $profile answers setup provider]}
::vsetup::next
pump
set profile [last_body /api/install/profile]
ok "profile apply returns both review signatures" {[dict get $profile confirm] eq "true" && [dict get $profile base_sig] eq "base-profile" && [dict get $profile plan_sig] eq "plan-profile"}

reset
set ::vsetup::install_profile desktop
set ::vsetup::install_core /managed/source
set ::vsetup::install_pds 1
set body [::json::json2dict [::vapi::_json_value [::vinstall::profile_body]]]
set pds [lindex [dict get $body answers host host_components] 0]
ok "PDS form produces a typed reviewed host component" {[dict get $pds kind] eq "pds" && [dict get $pds handle_domain] eq "pds.example" && [dict get $pds acme_email] eq "acme@example" && [dict get $pds image source] eq "ghcr.io/jazware/vlpds:1.2.2" && ![dict exists $body core]}
ok "PDS first handoff requires a separate backup acknowledgement" {[dict get $pds backup_confirmed] eq "false" && ![dict exists $pds s3_access_key] && ![dict exists $pds s3_secret_key]}
set ::vsetup::install_pds_backup 1
set pds [lindex [dict get [::json::json2dict [::vapi::_json_value [::vinstall::profile_body]]] answers host host_components] 0]
ok "PDS backup acknowledgement changes the signed request not a host flag" {[dict get $pds backup_confirmed] eq "true"}

# A service shell, a container or the Nix sandbox has no USER: the wizard
# must still name the account it runs as (tcl_platform(user)), not abort.
set saved_user [expr {[info exists ::env(USER)] ? $::env(USER) : ""}]
catch {unset ::env(USER)}
set rc [catch {set body [::json::json2dict [::vapi::_json_value [::vinstall::profile_body]]]} why]
ok "PDS request builds without a USER variable" {$rc == 0}
ok "PDS gateway account falls back to the process account" {$rc == 0 && [dict get [lindex [dict get $body answers host host_components] 0] gateway_user] eq $::tcl_platform(user)}
reset
set ::vsetup::install_profile server
set ::vsetup::install_system 1
set ::vsetup::install_user_unit 0
set ::vsetup::install_core /managed/source
set rc [catch {set body [::json::json2dict [::vapi::_json_value [::vinstall::profile_body]]]} why]
ok "server request builds without a USER variable" {$rc == 0 && [dict get $body answers host user] eq $::tcl_platform(user)}
if {$saved_user ne ""} { set ::env(USER) $saved_user }

reset
set ::vsetup::install_profile desktop
set ::vsetup::install_pds 1
queue /api/install/profile {{"preview":true,"base_sig":"base-pds","plan_sig":"plan-pds","diff":"","actions":[],"notes":[],"secrets":[],"host_components":[{"component":"own-pds","host_effects":{"files":[{"path":"/home/gw/.config/containers/systemd/vlpds.container","mode":"0644","contents":"Image=fixture@sha256:aaa\\nPull=never\\n","when":"always"}],"commands":[{"argv":["runuser","-u","gw","--","podman","pull","-q","fixture@sha256:aaa"],"when":"always"}]}}]}}
::vsetup::open_profiles
pump
::vsetup::next
pump
::vsetup::next
pump
::vsetup::next
pump
set preview_text [$::vsetup::win.body.c.inner.f.pv.preview get 1.0 end]
ok "Tk displays the exact PDS file and command effects before handoff" {[string match *Pull=never* $preview_text] && [string match *podman*pull* $preview_text] && [string match *Exact*signed*component*review* $preview_text]}

reset
set ::vsetup::install_profile nixos
queue /api/install/profile {{"preview":true,"confirm_required":true,"base_sig":"","plan_sig":"nix-plan","files":[{"name":"veles.nix","contents":"{ services.veles.enable = true; }","sha256":"nix"}],"actions":[],"notes":[],"secrets":[]}}
queue /api/install/profile {{"ok":true,"status":"emitted","applied":false,"emitted":[{"name":"veles.nix","contents":"{ services.veles.enable = true; }","sha256":"nix"}]}}
::vsetup::open_profiles
pump
::vsetup::next
pump
::vsetup::next
pump
::vsetup::next
pump
ok "nixos preview omits the core path" {![dict exists [last_body /api/install/profile] core]}
::vsetup::next
pump
ok "nixos result is emitted not applied" {[wm title .setup_wizard] eq "Setup — profile result" && [dict get $::vsetup::applied status] eq "emitted"}

reset
set ::vsetup::install_profile container-host
set ::vsetup::install_container_json {gateway {name gw home /srv/gw} sandbox {name sbx home /srv/sbx}}
queue /api/install/profile {{"preview":true,"confirm_required":true,"base_sig":"","plan_sig":"container-plan","actions":[],"notes":[],"secrets":[],"privileged_script":{"text":"set -eu\n# reviewed host plan\n","executed":false}}}
queue /api/install/profile {{"ok":true,"status":"handoff-required","applied":false,"staged":true,"command":"veles install profile-handoff --request /managed/request.json --yes","note":"pending CLI handoff"}}
::vsetup::open_profiles
pump
::vsetup::next
pump
::vsetup::next
pump
::vsetup::next
pump
ok "container profile forwards typed host options" {[dict get [last_body /api/install/profile] answers host container gateway name] eq "gw"}
::vsetup::next
pump
ok "container result stays a pending handoff" {[dict get $::vsetup::applied status] eq "handoff-required" && ![dict get $::vsetup::applied applied]}

reset
set ::vsetup::install_profile container-host
set ::vsetup::install_container_storage /srv/containers
set ::vsetup::install_container_gateway gw
set ::vsetup::install_container_sandbox sbx
set ::vsetup::install_container_image docker.io/library/alpine:3.20
set ::vsetup::install_container_digest sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
set ::vsetup::install_container_network velesnet
set body [::vinstall::profile_body]
set typed [::vapi::_json_value $body]
# The wire text, not json2dict's reading of it: a bare true/null in the
# builder encodes as the strings "true"/"null" (a group named "null", a
# tor section the engine cannot parse), and a multi-word description as
# an array of its words. json2dict reads every one of those back fine.
set raw $typed
ok "container form sends real booleans, nulls and a one-string description" {
    [regexp {"rootless":\s*true\M} $raw] && [regexp {"group":\s*null\M} $raw]
    && [regexp {"bind":\s*false\M} $raw] && [regexp {"install_missing":\s*false\M} $raw]
    && [regexp {"tor":\s*null\M} $raw] && [regexp {"source":\s*null\M} $raw]
    && [string match {*"description": "veles gateway; configure runtime separately"*} $raw]
    && ![regexp {":\s*"(true|false|null)"} $raw]}
set typed [::json::json2dict $typed]
ok "container form constructs typed storage and account inputs" {[dict get $typed answers host container storage root] eq "/srv/containers" && [dict get $typed answers host container gateway home] eq "/srv/containers/gw"}
ok "container form pins its gateway image and network" {[dict get [lindex [dict get $typed answers host container quadlets] 0] container image digest] eq "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" && [dict get $typed answers host container network] eq "velesnet"}
ok "container preparation has no native component or core" {[dict get $typed answers components] eq {} && [dict get $typed core] eq ""}

reset
set ::vsetup::install_profile server
set ::vsetup::install_user_unit 1
set ::vsetup::install_core /managed/source
set body [::vapi::_json_value [::vinstall::profile_body]]
set server [::json::json2dict $body]
ok "server form can explicitly choose a user unit" {[dict get $server answers host scope] eq "user" && [dict get $server core] eq "/managed/source"}

# Ordinary component request/apply wire, with typed dotted options.
reset
queue_state
queue_manifest fixture
queue_preview
::vsetup::open_install
pump
ok "installer starts at the workstation profile" {[wm title .setup_wizard] eq "Setup — installation profile"}
::vsetup::next
pump
ok "discovery describes only the selected component" {[dict get $::vsetup::install_manifest name] eq "fixture" && [lsearch -exact [paths] /api/install/options/other] < 0}
ok "manifest arrays retain their shape" {[dict get $::vsetup::install_manifest config_sections] eq {platforms.fixture} && [llength [dict get $::vsetup::install_manifest knobs]] == 2}
set ::vsetup::install_option_0 "https://two.example"
::vinstall::refresh_preview
pump
ok "component preview is requested" {[lsearch -exact [paths] /api/install/preview] >= 0}
set body [last_body /api/install/preview]
ok "dotted option paths and JSON strings survive the wire" {[dict get $body component] eq "fixture" && [dict get $body options platforms.fixture.server] eq "https://two.example"}
ok "secret knobs are not component options" {![dict exists $body options platforms.fixture.token]}
ok "component preview carries both signatures" {[dict get $::vsetup::install_preview base_sig] eq "base-component" && [dict get $::vsetup::install_preview plan_sig] eq "plan-component"}
queue /api/install/apply {{"ok":true,"action":"configure","reloaded":true}}
::vinstall::apply
pump
set applied [last_body /api/install/apply]
ok "component apply returns reviewed answers and signatures" {[dict get $applied base_sig] eq "base-component" && [dict get $applied plan_sig] eq "plan-component" && [dict get $applied options platforms.fixture.server] eq "https://two.example"}
ok "configuration apply never installs an artifact" {[lsearch -exact [paths] /api/install/component] < 0}

# Artifact review is distinct from configuration: cancel never applies,
# and confirmation must return the exact plan_sig from the preview.
reset
queue_state
queue_manifest fixture
::vsetup::open_install
pump
::vsetup::next
pump
queue /api/install/component {{"preview":true,"confirm_required":true,"artifact_sha256":"reviewed-bytes","plan_sig":"artifact-plan"}} 80
::vinstall::install_selected
update
after 100
set ::mb_answer no
pump
ok "cancelled artifact review never sends an apply" {[llength [lsearch -all -inline [paths] /api/install/component]] == 1 && !$::vsetup::install_busy}
incr ::vinstall::generation
queue /api/install/component {{"preview":true,"confirm_required":true,"artifact_sha256":"reviewed-bytes","plan_sig":"artifact-plan"}}
::vinstall::install_selected
pump
queue /api/install/component {{"ok":true,"action":"install-component","component":{"name":"fixture"}}}
set ::mb_answer yes
::vinstall::install_confirmed fixture $::vinstall::generation {preview true confirm_required true artifact_sha256 reviewed-bytes plan_sig artifact-plan}
pump
set artifact [last_body /api/install/component]
ok "artifact apply returns the reviewed plan signature" {[dict get $artifact confirm] eq "true" && [dict get $artifact plan_sig] eq "artifact-plan"}
ok "artifact confirmation never writes component configuration" {[lsearch -exact [paths] /api/install/apply] < 0}

# A slow reply for the first component must not overwrite the second.
reset
queue_state {fixture other}
lappend ::wire::replies(/api/install/options/fixture) [manifest_reply fixture 80]
queue_manifest other
::vsetup::open_install
pump
::vsetup::next
update
set list .setup_wizard.body.c.inner.f.list
$list selection clear 0 end
$list selection set 1
::vinstall::choose $list
pump
ok "an older component reply is discarded after a newer selection" {[dict get $::vsetup::install_manifest name] eq "other"}
ok "the late reply cannot restore the first component" {$::vsetup::install_manifest ne {} && [dict get $::vsetup::install_manifest name] ne "fixture"}

# Two previews deliberately complete in reverse order.
reset
queue_state
queue_manifest fixture
queue_preview old-plan old-base 80
queue_preview new-plan new-base
::vsetup::open_install
pump
::vsetup::next
update
set ::vsetup::install_option_0 old
::vinstall::refresh_preview
set ::vsetup::install_option_0 new
::vinstall::refresh_preview
pump
ok "the newest preview wins when replies are reversed" {[dict get $::vsetup::install_preview plan_sig] eq "new-plan" && [dict get $::vsetup::install_preview base_sig] eq "new-base"}
ok "the winning preview matches the newest options" {[dict get [last_body /api/install/preview] options platforms.fixture.server] eq "new" && [dict get $::vsetup::install_preview answers] eq [::vinstall::answers]}

# Full wizard: preview the component, then one combined setup apply.
if {[info exists ::env(VELES_INSTALLTEST_TRACE)]} {
    rename ::vsetup::next ::vsetup::next_original
    proc ::vsetup::next {{skip 0}} {
        puts "BEFORE NEXT step=$::vsetup::cur pending=$::vsetup::install_apply_pending busy=$::vsetup::install_busy preview=$::vsetup::install_preview"
        set rc [catch {::vsetup::next_original $skip} error options]
        puts "AFTER NEXT step=$::vsetup::cur rc=$rc error=$error pending=$::vsetup::install_apply_pending busy=$::vsetup::install_busy messages=$::vstatus::msgs"
        if {$rc} { puts [dict get $options -errorinfo] }
        return -code $rc $error
    }
}
reset
queue_state
queue_manifest fixture
queue_preview
queue_combined_preview
queue /api/setup/apply {{"ok":true,"action":"setup-and-components","config_path":"/cfg/config.toml","config_backup":null,"env_path":"/cfg/.env","env_written":false,"secrets_needed":[],"secrets_stored":[],"notes":[],"reloaded":true}}
::vsetup::open_install
pump
::vsetup::next
pump
::vsetup::next
pump
ok "component review precedes the options step" {$::vsetup::cur == 2 && [dict size $::vsetup::install_preview] > 0}
set ::vsetup::install_option_0 two
queue_preview plan-component-two base-component-two
::vsetup::next
pump
ok "changed options require a fresh component preview" {$::vsetup::cur == 2 && [dict get $::vsetup::install_preview plan_sig] eq "plan-component-two"}
::vsetup::next
pump
ok "the summary shows both declared actions and config diff" {[set text [.setup_wizard.body.c.inner.f.pv.preview get 1.0 end]; string match "*write declared component options*" $text] && [string match {*server = "two"*} $text]}
::vsetup::next
pump
set combined [last_body /api/setup/apply]
ok "combined apply returns exact base and plan signatures" {[dict get $combined expect_base] eq "base-combined" && [dict get $combined plan_sig] eq "plan-combined"}
ok "combined apply confirms and carries component options" {[dict get $combined confirm] eq "true" && [dict get $combined component_name] eq "fixture" && [dict get $combined options platforms.fixture.server] eq "two"}
ok "provider sections stay skipped in component mode" {[lsort [dict get $combined skipped]] eq {acl irc locale model provider telegram}}
ok "the combined flow finishes only after apply succeeds" {[wm title .setup_wizard] eq "Setup — done"}
ok "the wizard does not use the lower-level artifact apply" {[lsearch -exact [paths] /api/install/apply] < 0 && [lsearch -exact [paths] /api/install/component] < 0}

::vsetup::_close
update idletasks

# Managed local bundles are reviewed before confirmation. They never
# claim signature verification or write config.toml through setup/apply.
reset
queue_state
queue_manifest fixture
::vsetup::open_install
pump
::vsetup::next
pump
rename tk_getOpenFile tk_getOpenFile_real
proc tk_getOpenFile {args} { return /managed/source/BUNDLE-INFO.json }
queue /api/install/bundle {{"preview":true,"confirm_required":true,"action":"install-bundle","bundle":{"version":"0.1.0","revision":"abc1234","arch":"x86_64-linux"},"plan_sig":"bundle-plan","actions":["install reviewed local files"],"unsupported":[],"verification":"reviewed local directory; no signed-download or runtime-health claim"}}
set ::mb_answer no
::vinstall::choose_bundle
pump
set bundle_review [last_body /api/install/bundle]
ok "bundle review uses the managed source directory, not an arbitrary archive" {[dict get $bundle_review source] eq "/managed/source" && [dict get $bundle_review confirm] eq "false"}
ok "bundle review waits for a separate confirmation action" {[llength [lsearch -all -inline [paths] /api/install/bundle]] == 1 && [llength $::mb_calls] == 0 && [dict get $::vinstall::pending_action kind] eq "bundle"}
::vinstall::bundle_preview_done /managed/source $::vinstall::generation {preview true confirm_required true action install-bundle plan_sig bundle-plan bundle {version 0.1.0 revision abc1234} actions {install-reviewed} verification reviewed-local}
ok "a repeated preview callback cannot replace the reviewed plan" {[dict get $::vinstall::pending_action review bundle revision] eq "abc1234" && [llength $::mb_calls] == 0}
::vinstall::install_selected
pump
ok "cancelled bundle review never installs or changes configuration" {[llength [lsearch -all -inline [paths] /api/install/bundle]] == 1 && [lsearch -exact [paths] /api/setup/apply] < 0 && !$::vsetup::install_busy}
queue /api/install/bundle {{"preview":true,"confirm_required":true,"action":"install-bundle","bundle":{"version":"0.1.0","revision":"abc1234","arch":"x86_64-linux"},"plan_sig":"bundle-plan","actions":["install reviewed local files"],"unsupported":[],"verification":"reviewed local directory; no signed-download or runtime-health claim"}}
set ::mb_answer yes
::vinstall::choose_bundle
pump
queue /api/install/bundle {{"ok":true,"action":"install-bundle","component":{"name":"core"},"restart_required":true,"verification":"operator-reviewed local directory; not signed-download evidence"}}
::vinstall::install_selected
pump
ok "bundle confirmation returns the exact reviewed plan" {[dict get [last_body /api/install/bundle] confirm] eq "true" && [dict get [last_body /api/install/bundle] plan_sig] eq "bundle-plan"}
ok "a successful bundle response is not reported as signed" {![any_message {*signature verified*}] && [any_message {*Reviewed local bundle installed*}] && ![any_message {*invalid result*}]}

# Wrong action/component, missing signatures, transport errors and stale
# replies must never lead to a confirmation POST.
foreach bad {
    {preview true confirm_required true action install-bundle}
    {preview true confirm_required true action other plan_sig bundle-plan}
} {
    reset
    set ::vinstall::pending_action {kind bundle-review source /managed/source}
    set before [llength $::wire::requests]
    ::vinstall::bundle_preview_done /managed/source $::vinstall::generation $bad
    ok "malformed bundle preview cannot confirm an install: $bad" {!$::vsetup::install_busy && [llength $::wire::requests] == $before && [llength $::mb_calls] == 0 && [dict size $::vinstall::pending_action] == 0}
}
reset
::vinstall::review_bundle /managed/source
::vinstall::bundle_failed $::vinstall::generation refused
ok "bundle transport failure releases the busy state" {!$::vsetup::install_busy && [lindex $::vstatus::msgs end] eq "refused"}
reset
::vinstall::review_bundle /managed/source
incr ::vinstall::generation
::vinstall::bundle_preview_done /managed/source [expr {$::vinstall::generation - 1}] {preview true confirm_required true action install-bundle plan_sig stale}
ok "stale bundle replies cannot open a confirmation" {[llength $::mb_calls] == 0}

reset
set ::vsetup::install_manifest {name fixture}
set ::mb_answer no
queue /api/install/rollback {{"preview":true,"confirm_required":true,"component":"fixture","from_version":"2","to_version":"1","restores_config_snapshot":true,"plan_sig":"rollback-plan"}}
::vinstall::rollback_selected
pump
ok "rollback review waits for a separate confirmation action" {[llength [lsearch -all -inline [paths] /api/install/rollback]] == 1 && [llength $::mb_calls] == 0 && [dict get $::vinstall::pending_action kind] eq "rollback"}
::vinstall::rollback_preview_done fixture $::vinstall::generation {preview true confirm_required true component fixture from_version 2 to_version 1 restores_config_snapshot true plan_sig rollback-plan}
ok "repeated rollback preview cannot replace the reviewed plan" {[dict get $::vinstall::pending_action review from_version] eq "2" && [llength $::mb_calls] == 0}
::vinstall::install_selected
pump
ok "cancelled rollback never sends apply or setup config" {[llength [lsearch -all -inline [paths] /api/install/rollback]] == 1 && [lsearch -exact [paths] /api/setup/apply] < 0}
queue /api/install/rollback {{"preview":true,"confirm_required":true,"component":"fixture","from_version":"2","to_version":"1","restores_config_snapshot":true,"plan_sig":"rollback-plan"}}
set ::mb_answer yes
::vinstall::rollback_selected
pump
queue /api/install/rollback {{"ok":true,"action":"rollback","component":{"name":"fixture"},"reload_required":true}}
::vinstall::install_selected
pump
ok "confirmed rollback returns its reviewed signature" {[dict get [last_body /api/install/rollback] confirm] eq "true" && [dict get [last_body /api/install/rollback] plan_sig] eq "rollback-plan"}
reset
set ::vsetup::install_manifest {name fixture}
set ::vinstall::pending_action {kind rollback-review name fixture}
::vinstall::rollback_preview_done fixture $::vinstall::generation {preview true confirm_required true component other plan_sig wrong}
ok "rollback cannot be confirmed for a different component" {[lsearch -exact [paths] /api/install/rollback] < 0 && [llength $::mb_calls] == 0 && [dict size $::vinstall::pending_action] == 0}
set ::vinstall::pending_action {kind rollback-review name fixture}
::vinstall::rollback_preview_done fixture $::vinstall::generation {preview true confirm_required true component fixture}
ok "rollback without a plan signature stays unapplied" {[lsearch -exact [paths] /api/install/rollback] < 0 && !$::vsetup::install_busy}

reset
queue /api/install/verify {{"ok":false,"problems":["fixture: SHA-256 differs"]}}
::vinstall::verify
pump
ok "verification failure is visible and not reported as success" {[any_message *SHA-256*] && ![any_message *passed*]}
queue /api/install/verify {{"ok":true,"problems":[]}}
::vinstall::verify
pump
ok "verification success is explicit" {[any_message *passed*]}
queue /api/install/verify {{"ok":true}}
::vinstall::verify
pump
ok "malformed verification reply is not accepted as success" {[lindex $::vstatus::msgs end] ne [lindex $::vstatus::msgs end-1] && [lsearch -glob [lindex $::vstatus::msgs end] *malformed*] >= 0}
queue /api/install/verify {{"error":"verification unavailable"}} 0 503
::vinstall::verify
pump
ok "verification HTTP failure releases busy state" {!$::vsetup::install_busy && [lsearch -glob [lindex $::vstatus::msgs end] *unavailable*] >= 0}

# The signed release dialog uses the same verify/review/confirm contract.
# No archive path is accepted from the desktop client.
reset
queue /api/install/download {{"preview":true,"confirm_required":true,"action":"install-signed-bundle","plan_id":"signed-one","review_expires_seconds":3600,"download":{"archive_name":"veles-1.0.0-abcdef1-linux-x86_64.tar.zst","archive_sha256":"archive-hash","manifest_sha256":"manifest-hash","version":"1.0.0","revision":"abcdef1","repository":"77787777778777/veles","tag":"v1.0.0","commit":"abcdef1234567890abcdef1234567890abcdef12","workflow_sha":"1234567890abcdef1234567890abcdef12345678"},"verification":"cosign verified manifest and archive","actions":["install verified archive"],"unsupported":["restart is separate"]}}
::vinstall::choose_signed
set ::vinstall::signed_tag v1.0.0
set ::vinstall::signed_commit abcdef1234567890abcdef1234567890abcdef12
set ::vinstall::signed_workflow_sha 1234567890abcdef1234567890abcdef12345678
::vinstall::review_signed
pump
set signed_request [last_body /api/install/download]
ok "signed form submits explicit release identities without a path" {[dict get $signed_request repository] eq "77787777778777/veles" && [dict get $signed_request tag] eq "v1.0.0" && [dict get $signed_request confirm] eq "false" && ![dict exists $signed_request source]}
set review_text [.signed_download.f.review get 1.0 end]
ok "signed review names archive and manifest digests" {[string first archive-hash $review_text] >= 0 && [string first manifest-hash $review_text] >= 0 && [string first "not installed yet" $review_text] >= 0}
::vinstall::close_signed
pump
set cancel_body [last_body /api/install/download/confirm]
ok "declining signed review discards its exact server-owned plan" {[dict get $cancel_body plan_id] eq [dict get $signed_request request_id] && [dict get $cancel_body expected_manifest_sha256] eq "manifest-hash" && [dict get $cancel_body confirm] eq "false"}
ok "declining signed review never sends an apply" {[dict size [last_body /api/install/download/confirm]] && [llength [lsearch -all -inline [paths] /api/install/download/confirm]] == 1 && !$::vsetup::install_busy}

reset
queue /api/install/download {{"preview":true,"confirm_required":true,"action":"install-signed-bundle","plan_id":"signed-two","review_expires_seconds":3600,"download":{"archive_name":"verified.tar.zst","archive_sha256":"archive-hash","manifest_sha256":"manifest-hash","version":"1.0.0","revision":"abcdef1","repository":"77787777778777/veles","tag":"v1.0.0","commit":"abcdef1234567890abcdef1234567890abcdef12","workflow_sha":"1234567890abcdef1234567890abcdef12345678"},"verification":"cosign verified","actions":["install verified archive"],"unsupported":[]}}
::vinstall::choose_signed
::vinstall::review_signed
set ::mb_answer yes
pump
ok "signed review requires the operator's separate confirmation" {[llength [lsearch -all -inline [paths] /api/install/download/confirm]] == 0 && [dict get $::vinstall::pending_action kind] eq "signed" && !$::vsetup::install_busy}
queue /api/install/download/confirm {{"ok":true,"action":"install-signed-bundle","component":{"name":"core"},"restart_required":true}}
.signed_download.f.buttons.install invoke
pump
set confirmed [last_body /api/install/download/confirm]
ok "signed confirmation returns opaque id and reviewed manifest digest" {[dict get $confirmed plan_id] eq [dict get [last_body /api/install/download] request_id] && [dict get $confirmed expected_manifest_sha256] eq "manifest-hash" && [dict get $confirmed confirm] eq "true" && [any_message {*Verified signed bundle installed*}]}

reset
::vinstall::choose_signed
set ::vinstall::pending_action {kind signed epoch 0 review {plan_id stale-plan download {manifest_sha256 stale-hash}}}
::vinstall::review_signed
set first_attempt [last_body /api/install/download]
pump
ok "a fresh review explicitly discards the previous signed plan" {[dict get [last_body /api/install/download/confirm] plan_id] eq "stale-plan" && [dict get [last_body /api/install/download/confirm] confirm] eq "false"}
# The replacement is now in flight. Starting another review cancels it,
# not the old plan a second time.
::vinstall::review_signed
ok "Tk supplies the same opaque id for an in-flight request and cancellation" {[dict get [last_body /api/install/download/confirm] plan_id] eq [dict get $first_attempt request_id] && [dict get [last_body /api/install/download] request_id] ne [dict get $first_attempt request_id]}
reset
# No preview reply is queued: close while the gateway is still verifying.
::vinstall::choose_signed
::vinstall::review_signed
set first_download [last_body /api/install/download]
::vinstall::close_signed
set first_cancel [last_body /api/install/download/confirm]
set before [llength $::wire::requests]
set closed_epoch $::vinstall::generation
# Simulate a response arriving after close, rather than after cancel's timer.
::vinstall::signed_preview_done $closed_epoch [dict create preview true confirm_required true action install-signed-bundle plan_id [dict get $first_download request_id] download {manifest_sha256 late-hash}]
ok "closing an in-flight signed request cancels its matching id" {[dict exists $first_download request_id] && [string length [dict get $first_download request_id]] >= 16 && [dict get $first_cancel plan_id] eq [dict get $first_download request_id] && [dict get $first_cancel confirm] eq "false"}
ok "late preview cannot recreate a closed dialog or leave a cancellable plan" {![winfo exists .signed_download] && !$::vsetup::install_busy && [dict get [last_body /api/install/download/confirm] plan_id] eq [dict get $first_cancel plan_id] && [llength $::wire::requests] == $before}
reset
::vinstall::choose_signed
::vinstall::review_signed
::vinstall::close_signed
::vinstall::choose_signed
::vinstall::review_signed
ok "a new attempt never reuses a cancelled request id" {[dict get [last_body /api/install/download] request_id] ne [dict get $first_download request_id]}
reset
::vinstall::choose_signed
set ::vinstall::signed_commit abcdef1234567890abcdef1234567890abcdef12
set before [llength $::wire::requests]
::vi18n::use de
set translated [.signed_download.f.l3 cget -text]
set german_expected [::vi18n::mc {Full source commit SHA}]
set registered [::vi18n::widget_key .signed_download.f.l3]
::vi18n::use en
ok "signed fields relabel without losing answers or cancelling a request" {$translated eq $german_expected && [lindex $registered 0] eq "Full source commit SHA" && [wm title .signed_download] eq "Verify signed release" && [.signed_download.f.l3 cget -text] eq "Full source commit SHA" && $::vinstall::signed_commit eq "abcdef1234567890abcdef1234567890abcdef12" && [llength $::wire::requests] == $before}
::vinstall::review_signed
::vsetup::_close
ok "closing the wizard destroys the signed dialog and cancels its request" {![winfo exists .signed_download] && [dict get [last_body /api/install/download/confirm] confirm] eq "false" && [dict get [last_body /api/install/download/confirm] plan_id] eq [dict get [last_body /api/install/download] request_id]}
reset
set before [llength $::wire::requests]
queue /api/install/download {{"preview":true,"confirm_required":true,"action":"install-signed-bundle","plan_id":"bad-plan","download":{}}}
::vinstall::choose_signed
::vinstall::review_signed
set malformed_request [last_body /api/install/download]
pump
ok "malformed signed preview discards its plan and never confirms" {[dict get [last_body /api/install/download/confirm] plan_id] eq [dict get $malformed_request request_id] && [llength $::mb_calls] == 0 && !$::vsetup::install_busy}
reset
set before [llength $::wire::requests]
::vinstall::signed_preview_done [expr {$::vinstall::generation - 1}] {preview true confirm_required true action install-signed-bundle plan_id stale-plan download {manifest_sha256 stale-hash}}
ok "stale signed callback cannot create a review or request installation" {[llength $::wire::requests] == $before && [llength $::mb_calls] == 0}

# Every flag went out as a JSON boolean. The engine's request types take
# `confirm: bool`, and a bare `true` in a body dict encodes as the
# STRING "true" (api.tcl, _json_value), which serde refuses with a 422:
# every reviewed apply and every cancel from the Tk installer failed
# that way. json2dict reads both back as `true`, so the wire text is
# what has to be checked.
set _stringly {}
foreach r $::wire::all {
    if {[regexp {"[A-Za-z_]+":\s*"(true|false)"} [dict get $r body] m]} {
        lappend _stringly "[dict get $r path] $m"
    }
}
ok "every flag on the wire is a JSON boolean, not a string" {[llength $_stringly] == 0}
if {[llength $_stringly]} { puts "      stringly: [lsort -unique $_stringly]" }

rename tk_getOpenFile {}
rename tk_getOpenFile_real tk_getOpenFile
::vsetup::_close
update idletasks
puts "\ninstalltest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
