# R11 component installer UI. Tk owns choices and display; the gateway
# owns discovery, manifest reads, config writes and artifact recovery.
# All installer requests are asynchronous. No nested event-loop wait.
namespace eval ::vinstall {
    variable request_seq 0
}

proc ::vinstall::signed_request_id {} {
    # A request-scoped cancellation handle, not an authority or a path.
    # Each retry gets a fresh id even when all release fields are unchanged.
    # Unique per desktop process is all it needs (the gateway takes any
    # 1-128 visible ASCII characters). It used tcllib's uuid, which the
    # Android build does not carry: the desktop died at boot on every
    # phone in v0.1.0 (androidtest.tcl checks the requires now).
    variable request_seq
    return [format tk-%d-%d-%d [pid] [clock microseconds] [incr request_seq]]
}

proc ::vinstall::reset {} {
    variable pending_action
    variable generation
    ::vinstall::discard_signed
    if {[info exists generation]} { incr generation }
    catch {grab release .signed_download}
    catch {destroy .signed_download}
    ::vi18n::off_locale_change [list ::vinstall::retext_signed]
    foreach n {install_state install_manifest install_options install_plan install_preview} {
        set ::vsetup::$n {}
    }
    set ::vsetup::install_busy 0
    set ::vsetup::install_apply_pending 0
    set pending_action {}
    variable generation
    if {![info exists generation]} { set generation 0 }
    incr generation
}

proc ::vinstall::note {p key} {
    if {![winfo exists $p.note]} {
        ttk::label $p.note -wraplength 600 -justify left -foreground $::vt::T(fg_dim)
        pack $p.note -anchor w -fill x -pady {3 6}
    }
    $p.note configure -text $key
}

proc ::vinstall::step_profile {p} {
    set f [::vsetup::_step_head $p [::vi18n::mc {Installation profile}] \
        [::vi18n::mc {Choose the installation shape. Local profiles use the reviewed installer. System and container profiles prepare a signed request for the explicit CLI handoff. NixOS only emits a module snippet.}]]
    ttk::radiobutton $f.desktop -variable ::vsetup::install_profile -value desktop \
        -text [::vi18n::mc {Desktop: local core, modules and process plugins}] -command ::vinstall::profile_changed
    pack $f.desktop -anchor w
    ttk::radiobutton $f.server -variable ::vsetup::install_profile -value server \
        -text [::vi18n::mc {Server: gateway configuration and a reviewed user unit}] -command ::vinstall::profile_changed
    pack $f.server -anchor w
    ttk::radiobutton $f.container -variable ::vsetup::install_profile -value container-host \
        -text [::vi18n::mc {Container host: typed host plan and privileged CLI handoff}] -command ::vinstall::profile_changed
    pack $f.container -anchor w
    ttk::radiobutton $f.nixos -variable ::vsetup::install_profile -value nixos \
        -text [::vi18n::mc {NixOS: emit a module snippet; do not edit host declarations}] -command ::vinstall::profile_changed
    pack $f.nixos -anchor w
    ::vinstall::note $f [::vi18n::mc {No profile starts a service or executes a privileged script from this wizard. Review the plan and any handoff before running it.}]
    ttk::checkbutton $f.system -variable ::vsetup::install_system -text [::vi18n::mc {Server: use a system service and prepare a privileged CLI request}]
    ttk::checkbutton $f.user_unit -variable ::vsetup::install_user_unit -text [::vi18n::mc {Server: install a user unit and prepare the separate linger handoff}]
    ttk::frame $f.core
    pack $f.core -fill x -pady 4
    ttk::label $f.core.label -text [::vi18n::mc {Reviewed core bundle in the managed install-sources directory}]
    pack $f.core.label -anchor w
    ttk::entry $f.core.path -textvariable ::vsetup::install_core
    pack $f.core.path -fill x
    ttk::button $f.core.choose -command ::vinstall::choose_profile_core
    ::vi18n::label $f.core.choose {Choose core bundle…}
    pack $f.core.choose -anchor w
    ttk::button $f.container_options -command ::vinstall::choose_container_options
    ::vi18n::label $f.container_options {Choose typed container-host options…}
    pack $f.container_options -anchor w
    ttk::frame $f.container_form
    foreach {field variable label} {
        storage install_container_storage {Storage root}
        gateway install_container_gateway {Gateway account}
        sandbox install_container_sandbox {Sandbox account}
        network install_container_network {Shared container network}
        image install_container_image {Pinned gateway image (repository:tag)}
        digest install_container_digest {Gateway image sha256 digest}
        name install_container_name {Gateway quadlet name}
    } {
        ttk::frame $f.container_form.$field
        pack $f.container_form.$field -fill x -pady 2
        ttk::label $f.container_form.$field.label -text [::vi18n::mc $label] -width 36
        pack $f.container_form.$field.label -side left
        ttk::entry $f.container_form.$field.entry -textvariable ::vsetup::$variable -width 52
        pack $f.container_form.$field.entry -side left -fill x -expand 1
    }
    ttk::frame $f.pds
    ttk::checkbutton $f.pds.enable -variable ::vsetup::install_pds -command ::vinstall::profile_changed \
        -text [::vi18n::mc {Prepare an own vlpds PDS through the signed CLI handoff}]
    pack $f.pds.enable -anchor w
    ttk::label $f.pds.guide -wraplength 720 -justify left -text [::vi18n::mc {The first handoff prepares secrets, the pinned image and runtime files but does not start the PDS. Back up the KEK and PLC rotation key offline, then make a newly reviewed request with backup acknowledgement to set up HTTP-01 TLS and activation. DNS records must point directly to this host; no CDN proxy or DNS changes are made by veles.}]
    pack $f.pds.guide -anchor w -fill x -pady 4
    ttk::frame $f.pds.fields
    pack $f.pds.fields -fill x
    foreach {field variable label} {
        url install_pds_url {Public HTTPS URL}
        domain install_pds_domain {PDS and handle domain}
        email install_pds_email {ACME account email}
        image install_pds_image {Pinned vlpds image (repository:tag)}
        digest install_pds_digest {vlpds image sha256 digest}
        s3_endpoint install_pds_s3_endpoint {S3 HTTPS endpoint}
        s3_region install_pds_s3_region {S3 region}
        s3_bucket install_pds_s3_bucket {S3 bucket}
        s3_prefix install_pds_s3_prefix {S3 prefix}
        network install_pds_network {Existing shared container network}
    } {
        ttk::frame $f.pds.fields.$field
        pack $f.pds.fields.$field -fill x -pady 2
        ttk::label $f.pds.fields.$field.label -text [::vi18n::mc $label] -width 36
        pack $f.pds.fields.$field.label -side left
        ttk::entry $f.pds.fields.$field.entry -textvariable ::vsetup::$variable -width 52
        pack $f.pds.fields.$field.entry -side left -fill x -expand 1
    }
    ttk::checkbutton $f.pds.backup -variable ::vsetup::install_pds_backup -command ::vinstall::profile_changed \
        -text [::vi18n::mc {I backed up the KEK and PLC rotation key offline; allow the reviewed TLS and activation handoff}]
    pack $f.pds.backup -anchor w -pady 3
    ::vinstall::profile_changed
}

proc ::vinstall::profile_controls {} {
    set f $::vsetup::win.content.profile
    if {![winfo exists $f]} { return }
    foreach widget {system user_unit core container_options container_form pds} {
        if {[winfo exists $f.$widget]} { pack forget $f.$widget }
    }
    switch -- $::vsetup::install_profile {
        server {
            pack $f.system $f.user_unit -anchor w
            pack $f.core -fill x -pady 4
        }
        desktop {
            pack $f.core -fill x -pady 4
            pack $f.pds -anchor w -fill x -pady 4
            if {!$::vsetup::install_pds} {
                pack forget $f.pds.fields $f.pds.backup
            } else {
                pack $f.pds.fields -fill x
                pack $f.pds.backup -anchor w -pady 3
            }
        }
        container-host {
            pack $f.container_form -fill x -pady 4
        }
    }
}

proc ::vinstall::choose_profile_core {} {
    set source [tk_getOpenFile -parent .setup_wizard -title [::vi18n::mc {Choose BUNDLE-INFO.json in the managed install-sources directory}] -filetypes [list [list "BUNDLE-INFO.json" BUNDLE-INFO.json]]]
    if {$source ne ""} { set ::vsetup::install_core [file dirname $source]; ::vinstall::profile_changed }
}

proc ::vinstall::choose_container_options {} {
    set source [tk_getOpenFile -parent .setup_wizard -title [::vi18n::mc {Choose typed container-host options JSON}] -filetypes [list [list "JSON" .json]]]
    if {$source eq ""} { return }
    set f [open $source r]
    set text [read $f]
    close $f
    set options [::json::json2dict $text]
    set ::vsetup::install_container_json $options
    set ::vsetup::install_container_file $source
    set ::vsetup::install_container_options $options
    ::vinstall::profile_changed
}

proc ::vinstall::profile_changed {} {
    set ::vsetup::install_preview {}
    set ::vsetup::install_manifest {}
    set ::vsetup::install_apply_pending 0
    set ::vsetup::preview_sig ""
    ::vinstall::profile_controls
}

# The account the desktop runs as. A service shell, a container or the Nix
# build sandbox may have no USER in its environment; Tcl resolves the
# effective uid itself (tcl_platform(user)), which is the same account.
proc ::vinstall::current_user {} {
    if {[info exists ::env(USER)] && $::env(USER) ne ""} { return $::env(USER) }
    return $::tcl_platform(user)
}

proc ::vinstall::profile_body {{confirm 0}} {
    set profile $::vsetup::install_profile
    set setup [::vsetup::answers_body]
    if {$profile in {container-host nixos}} { dict set setup skipped [::vapi::json_strings {provider model telegram irc acl locale}] }
    set answers [dict create profile [::vapi::json_string $profile] \
        setup $setup \
        components [::vapi::json_strings {}] options [::vapi::json_object {}] \
        host [::vapi::json_object {}]]
    if {$profile eq "server" && ![info exists ::vsetup::install_system]} { set ::vsetup::install_system 0 }
    if {$profile eq "server"} {
        if {!$::vsetup::install_system && !$::vsetup::install_user_unit} { error [::vi18n::mc {Choose a user unit or a system service for the server profile. Neither starts a service.}] }
        dict set answers host [::vapi::json_object [dict create \
            user [::vapi::json_string [::vinstall::current_user]] scope [::vapi::json_string [expr {$::vsetup::install_system ? "system" : "user"}]]]]
    }
    if {$profile eq "container-host"} {
        if {$::vsetup::install_container_json ne ""} {
            dict set answers host [::vapi::json_object [dict create container $::vsetup::install_container_json]]
        } else {
            foreach variable {install_container_storage install_container_gateway install_container_sandbox install_container_network install_container_image install_container_digest install_container_name} {
                if {[string trim [set ::vsetup::$variable]] eq ""} { error [::vi18n::mc {Complete every typed container-host field before previewing this profile.}] }
            }
            set gateway $::vsetup::install_container_gateway
            set sandbox $::vsetup::install_container_sandbox
            set root [string trim $::vsetup::install_container_storage]
            set image [dict create source [::vapi::json_string $::vsetup::install_container_image] digest [::vapi::json_string $::vsetup::install_container_digest]]
            set container [dict create \
                gateway [::vapi::json_object [dict create name [::vapi::json_string $gateway] home [::vapi::json_string [file join $root $gateway]] rootless [::vapi::json_bool 1] system [::vapi::json_bool 1] group [::vapi::json_raw null]]] \
                sandbox [::vapi::json_object [dict create name [::vapi::json_string $sandbox] home [::vapi::json_string [file join $root $sandbox]] rootless [::vapi::json_bool 1] system [::vapi::json_bool 1] group [::vapi::json_raw null]]] \
                extra_users [] storage [::vapi::json_object [dict create root [::vapi::json_string $root] source [::vapi::json_raw null] requires_mounts_for [::vapi::json_raw null] bind [::vapi::json_bool 0] mode 493 home_mode 448]] \
                socket [::vapi::json_object [dict create dir [::vapi::json_string "/run/$sandbox"] user [::vapi::json_string $sandbox] group [::vapi::json_string $gateway] dir_mode 488 socket_mode 438]] \
                unit_prefix [::vapi::json_string veles] packages [::vapi::json_object [dict create manager [::vapi::json_string apt] install_missing [::vapi::json_bool 0]]] images [::vapi::json_array [list [::vapi::json_object [dict create user [::vapi::json_string $gateway] image [::vapi::json_object $image]]]]] \
                quadlets [::vapi::json_array [list [::vapi::json_object [dict create user [::vapi::json_string $gateway] name [::vapi::json_string $::vsetup::install_container_name] container [::vapi::json_object [dict create description [::vapi::json_string "veles gateway; configure runtime separately"] image [::vapi::json_object $image] extra []]]]]]] \
                tor [::vapi::json_raw null] firewall [::vapi::json_raw null] network [::vapi::json_string $::vsetup::install_container_network] peer_sync [::vapi::json_raw null]]
            dict set answers host [::vapi::json_object [dict create scope user container [::vapi::json_object $container]]]
        }
    }
    if {$profile eq "desktop" && $::vsetup::install_pds} {
        foreach variable {install_pds_url install_pds_domain install_pds_email install_pds_image install_pds_digest install_pds_s3_endpoint install_pds_s3_region install_pds_s3_bucket install_pds_s3_prefix install_pds_network} {
            if {[string trim [set ::vsetup::$variable]] eq ""} { error [::vi18n::mc {Complete every PDS field, including the pinned image digest, before previewing the handoff.}] }
        }
        set image [::vapi::json_object [dict create source [::vapi::json_string $::vsetup::install_pds_image] digest [::vapi::json_string $::vsetup::install_pds_digest]]]
        set pds [dict create kind pds \
            public_url [::vapi::json_string $::vsetup::install_pds_url] \
            handle_domain [::vapi::json_string $::vsetup::install_pds_domain] \
            acme_email [::vapi::json_string $::vsetup::install_pds_email] \
            backup_confirmed [::vapi::json_bool $::vsetup::install_pds_backup] \
            image $image \
            s3_endpoint [::vapi::json_string $::vsetup::install_pds_s3_endpoint] \
            s3_region [::vapi::json_string $::vsetup::install_pds_s3_region] \
            s3_bucket [::vapi::json_string $::vsetup::install_pds_s3_bucket] \
            s3_prefix [::vapi::json_string $::vsetup::install_pds_s3_prefix] \
            gateway_unit veles-gateway.service gateway_user [::vapi::json_string [::vinstall::current_user]] \
            network [::vapi::json_string $::vsetup::install_pds_network] \
            gateway_quadlet veles-gateway]
        dict set answers host [::vapi::json_object [dict create host_components [::vapi::json_array [list $pds]]]]
    }
    if {$profile in {desktop server} && [dict size $::vsetup::install_manifest]} {
        set name [dict get $::vsetup::install_manifest name]
        dict set answers components [::vapi::json_strings [list $name]]
        set options [dict get $::vsetup::install_preview answers options]
        dict set answers options [::vapi::json_object [dict create $name $options]]
    }
    set body [dict create answers $answers]
    if {$profile ne "nixos" && !($profile eq "desktop" && $::vsetup::install_pds)} {
        if {![info exists ::vsetup::install_core]} { set ::vsetup::install_core {} }
        dict set body core [::vapi::json_string $::vsetup::install_core]
    }
    if {$confirm} {
        dict set body confirm [::vapi::json_bool 1]
        dict set body base_sig [::vapi::json_string $::vsetup::preview_base]
        dict set body plan_sig [::vapi::json_string $::vsetup::preview_plan]
        set vals {}
        foreach {name value} [::vsetup::secret_values] { dict set vals $name [::vapi::json_string $value] }
        if {[dict size $vals]} { dict set body secrets $vals }
    }
    return $body
}

proc ::vinstall::profile_review {} {
    set body [::vinstall::profile_body]
    set rc [catch { set response [::vapi::post_json /api/install/profile $body] } why]
    if {$rc} {
        set ::vsetup::preview_sig ""
        ::vsetup::_show_preview [::vapi::reason $why]
        return 0
    }
    set text ""
    if {[dict exists $response diff]} { append text [dict get $response diff] }
    if {[dict exists $response actions]} { append text "\n" [join [dict get $response actions] "\n"] }
    if {[dict exists $response notes]} { append text "\n" [join [dict get $response notes] "\n"] }
    if {[dict exists $response privileged_script text]} { append text "\n--- reviewed privileged script; not executed ---\n" [dict get $response privileged_script text] }
    if {[dict exists $response host_components]} {
        foreach component [dict get $response host_components] {
            append text "\n--- host component: [dict get $component component]; prepared only by the CLI handoff ---\n"
            if {[dict exists $component host_effects files]} {
                foreach file [dict get $component host_effects files] {
                    append text "\nFile effect ([dict get $file when])\n"
                    if {[dict exists $file path]} { append text "[dict get $file path] (mode [dict get $file mode])\n[dict get $file contents]\n" }
                    if {[dict exists $file link]} { append text "[dict get $file link] -> [dict get $file target]\n" }
                }
            }
            if {[dict exists $component host_effects commands]} {
                foreach command [dict get $component host_effects commands] {
                    append text "\nCommand effect ([dict get $command when])\n[join [dict get $command argv] { }]\n"
                }
            }
            append text "\nExact signed component review (JSON):\n[::vapi::_json_value [::vapi::json_object [dict get $component]]]\n"
        }
    }
    if {[dict exists $response emitted]} {
        foreach file [dict get $response emitted] { append text "\n--- emitted [dict get $file name] ([dict get $file bytes] bytes; reviewed snippet is returned by the emit step) ---\n" }
    }
    ::vsetup::_show_preview $text
    set ::vsetup::preview_base [dict get $response base_sig]
    set ::vsetup::preview_plan [dict get $response plan_sig]
    set ::vsetup::preview_sig [::vapi::_json_value $body]
    return 1
}

proc ::vinstall::step_components {p} {
    variable generation
    variable pending_action
    set pending_action {}
    if {![info exists generation]} { set generation 0 }
    incr generation
    set f [::vsetup::_step_head $p [::vi18n::mc {Components}] \
        [::vi18n::mc {Choose a module or process plugin to inspect. Discovery only reads plugin.toml; selecting a component requests its declared manifest and options. Nothing is installed by opening this step.}]]
    listbox $f.list -height 7 -exportselection 0
    pack $f.list -fill x -pady 4
    ::va11y::name $f.list [::vi18n::mc {available components}]
    bind $f.list <<ListboxSelect>> [list ::vinstall::choose $f.list]
    ttk::frame $f.actions
    pack $f.actions -anchor w -fill x -pady 4
    ttk::button $f.actions.install -command ::vinstall::install_selected -state disabled
    ::vi18n::label $f.actions.install {Install selected component}
    pack $f.actions.install -side left -padx {0 4}
    ttk::button $f.actions.rollback -command ::vinstall::rollback_selected
    ::vi18n::label $f.actions.rollback {Review rollback}
    pack $f.actions.rollback -side left -padx 4
    ttk::button $f.actions.verify -command ::vinstall::verify
    ::vi18n::label $f.actions.verify {Verify installation}
    pack $f.actions.verify -side left -padx 4
    ttk::button $f.actions.bundle -command ::vinstall::choose_bundle
    ::vi18n::label $f.actions.bundle {Review managed local bundle…}
    pack $f.actions.bundle -side left -padx 4
    ttk::button $f.actions.signed -command ::vinstall::choose_signed
    ::vi18n::label $f.actions.signed {Verify signed release…}
    pack $f.actions.signed -side left -padx 4
    set ::vsetup::install_state [dict create _generation $generation]
    ::vinstall::note $f [::vi18n::mc {Loading discovered components…}]
    ::vapi::get_json_async /api/install/state [list ::vinstall::state_done $f $generation]
}

proc ::vinstall::state_done {p epoch ok value} {
    variable generation
    if {![info exists generation] || $generation != $epoch || ![winfo exists $p.list]} { return }
    if {!$ok} {
        ::vsetup::_say [::vapi::reason $value]
        ::vinstall::note $p [::vi18n::mc {Could not discover components. The provider setup steps still work; install through the CLI on the target machine if needed.}]
        return
    }
    set ::vsetup::install_state $value
    $p.list delete 0 end
    if {[dict exists $value available]} {
        foreach component [dict get $value available] {
            $p.list insert end [dict get $component name]
        }
    }
    if {[$p.list size] > 0} {
        $p.list selection set 0
        ::vinstall::choose $p.list
    } else {
        ::vinstall::note $p [::vi18n::mc {No modules or process plugins were discovered. The provider setup steps can still continue.}]
    }
}

proc ::vinstall::choose {listw} {
    variable pending_action
    set pending_action {}
    set selected [$listw curselection]
    if {![llength $selected]} { return }
    set name [$listw get [lindex $selected 0]]
    set ::vsetup::install_manifest {}
    set ::vsetup::install_preview {}
    set ::vsetup::install_apply_pending 1
    set ::vsetup::install_busy 0
    variable generation
    if {![info exists generation]} { set generation 0 }
    incr generation
    set ::vsetup::install_busy 1
    set parent [winfo parent $listw]
    $parent.actions.install state disabled
    ::vapi::get_json_async /api/install/options/$name [list ::vinstall::options_done $name $generation]
}

proc ::vinstall::options_done {name epoch ok value} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    if {![winfo exists .setup_wizard]} { return }
    if {!$ok} {
        set ::vsetup::install_manifest {}
        set ::vsetup::install_apply_pending 0
        ::vsetup::_say [::vapi::reason $value]
        return
    }
    if {![dict exists $value manifest]} {
        set ::vsetup::install_apply_pending 0
        set ::vsetup::install_busy 0
        ::vsetup::_say [::vi18n::mc {The selected component returned no manifest; configuration is disabled.}]
        return
    }
    set manifest [dict get $value manifest]
    if {![dict exists $manifest name] || [dict get $manifest name] ne $name} {
        set ::vsetup::install_apply_pending 0
        set ::vsetup::install_busy 0
        ::vsetup::_say [::vi18n::mc {The manifest belongs to another component; its reply was discarded.}]
        return
    }
    set ::vsetup::install_manifest $manifest
    set ::vsetup::install_preview {}
    set ::vsetup::install_apply_pending 0
    if {$::vsetup::cur == 1 && [winfo exists .setup_wizard.body.c.inner.f.actions.install]} {
        .setup_wizard.body.c.inner.f.actions.install state !disabled
    }
}

proc ::vinstall::step_options {p} {
    set f [::vsetup::_step_head $p [::vi18n::mc {Component options}] \
        [::vi18n::mc {Options come from the selected component manifest. Secret-looking fields stay in the setup/vault flow. Empty fields leave that option unchanged.}]]
    set manifest $::vsetup::install_manifest
    if {![dict size $manifest]} {
        ::vinstall::note $f [::vi18n::mc {Select a component first. No component options will be changed if you skip this step.}]
        return
    }
    ::vinstall::note $f [format [::vi18n::mc {Configuring %s}] [dict get $manifest name]]
    if {![info exists ::vsetup::install_instance]} { set ::vsetup::install_instance "" }
    if {[dict exists $manifest instances kind] && [dict get $manifest instances kind] eq "per_entry"} {
        ttk::label $f.il -text [::vi18n::mc {Instance name (for example an IRC network)}]
        ttk::entry $f.instance -textvariable ::vsetup::install_instance
        pack $f.il -anchor w
        pack $f.instance -fill x
    }
    set knobs {}
    if {[dict exists $manifest knobs]} { set knobs [dict get $manifest knobs] }
    set row 0
    foreach knob $knobs {
        set path [dict get $knob path]
        if {[regexp -nocase {(^|_)(token|secret|password|passwd|api_key|private_key)$} $path]} { continue }
        if {[dict exists $knob shape] && [dict get $knob shape] in {table_array scalar_array}} { continue }
        set var "install_option_$row"
        if {![info exists ::vsetup::$var]} { set ::vsetup::$var "" }
        ttk::label $f.l$row -text $path
        ttk::entry $f.e$row -textvariable ::vsetup::$var
        pack $f.l$row -anchor w -pady {4 0}
        pack $f.e$row -fill x
        if {[dict exists $knob note] && [dict get $knob note] ne "" && [dict get $knob note] ne "null"} {
            ttk::label $f.n$row -text [dict get $knob note] -wraplength 590 -foreground $::vt::T(fg_dim)
            pack $f.n$row -anchor w -fill x
        }
        incr row
    }
}

# Values are parsed by Rust's Knob/type rules, not Tcl's string/list
# inference. A JSON object retains strings even if a path has spaces.
proc ::vinstall::answers {} {
    set manifest $::vsetup::install_manifest
    set options {}
    if {[dict size $manifest]} {
        set row 0
        if {[dict exists $manifest knobs]} {
            foreach knob [dict get $manifest knobs] {
                set path [dict get $knob path]
                if {[regexp -nocase {(^|_)(token|secret|password|passwd|api_key|private_key)$} $path]} { continue }
                if {[dict exists $knob shape] && [dict get $knob shape] in {table_array scalar_array}} { continue }
                set var "install_option_$row"
                if {[info exists ::vsetup::$var] && [string trim [set ::vsetup::$var]] ne ""} {
                    dict set options $path [::vapi::json_string [set ::vsetup::$var]]
                }
                incr row
            }
        }
    }
    set instance ""
    if {[info exists ::vsetup::install_instance]} { set instance $::vsetup::install_instance }
    return [dict create component [::vapi::json_string [dict get $manifest name]] instance [::vapi::json_string $instance] options [::vapi::json_object $options]]
}

proc ::vinstall::apply_profile {} {
    set body [::vinstall::profile_body 1]
    if {$::vsetup::preview_sig ne [::vapi::_json_value [::vinstall::profile_body]]} {
        ::vinstall::profile_review
        ::vsetup::_say [::vi18n::mc {The profile changed. Review the fresh plan before applying it.}]
        return
    }
    set rc [catch { set response [::vapi::post_json /api/install/profile $body] } why]
    unset -nocomplain body
    ::vsetup::_forget_secrets
    if {$rc} {
        ::vsetup::_say [::vapi::reason $why]
        return
    }
    set ::vsetup::applied $response
    if {[dict exists $response command]} {
        ::vsetup::_say [dict get $response command]
    } elseif {[dict exists $response note]} {
        ::vsetup::_say [dict get $response note]
    }
    ::vsetup::_paint
}

proc ::vinstall::paint_result {} {
    set p .setup_wizard.body
    catch {destroy .setup_wizard.nav}
    catch {destroy .setup_wizard.head}
    catch {destroy $p}
    ttk::frame $p
    pack $p -fill both -expand 1 -padx 12 -pady 8
    ttk::label $p.title -text [::vi18n::mc {Profile result}] -font {TkDefaultFont 12 bold}
    pack $p.title -anchor w
    text $p.report -height 20 -wrap word -state disabled -background $::vt::T(bg_input) -foreground $::vt::T(fg) -highlightthickness 0
    pack $p.report -fill both -expand 1 -pady 8
    set lines {}
    foreach key {status applied staged service_running record_path command note} {
        if {[dict exists $::vsetup::applied $key]} { lappend lines "$key: [dict get $::vsetup::applied $key]" }
    }
    if {[dict exists $::vsetup::applied emitted]} {
        foreach file [dict get $::vsetup::applied emitted] { lappend lines "emitted [dict get $file name]:\n[dict get $file contents]" }
    }
    $p.report configure -state normal
    $p.report insert end [join $lines "\n\n"]
    $p.report configure -state disabled
    ttk::button $p.close -command ::vsetup::_close
    ::vi18n::label $p.close {Close}
    pack $p.close -side right
}

proc ::vinstall::refresh_preview {} {
    if {![dict size $::vsetup::install_manifest]} { set ::vsetup::install_preview {}; return }
    set body [::vinstall::answers]
    variable generation
    if {![info exists generation]} { set generation 0 }
    incr generation
    set ::vsetup::install_preview {}
    set ::vsetup::install_busy 1
    set ::vsetup::install_apply_pending 1
    ::vapi::post_json_async /api/install/preview $body \
        [list ::vinstall::preview_failed $generation] [list ::vinstall::preview_done $generation $body]
}

proc ::vinstall::preview_done {epoch body response} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    if {![dict exists $response plan_sig] || ![dict exists $response base_sig]} {
        set ::vsetup::install_preview {}
        set ::vsetup::install_apply_pending 0
        ::vsetup::_say [::vi18n::mc {The installer preview has no verification signatures; apply is disabled.}]
        return
    }
    set ::vsetup::install_preview [dict create answers $body base_sig [dict get $response base_sig] plan_sig [dict get $response plan_sig] review $response]
    set ::vsetup::install_apply_pending 0
    if {$::vsetup::cur == [expr {[::vsetup::step_count] - 1}]} { ::vsetup::preview_refresh }
}

proc ::vinstall::preview_failed {epoch why} {
    variable generation
    variable pending_action
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_preview {}
    set pending_action {}
    set ::vsetup::install_busy 0
    set ::vsetup::install_apply_pending 0
    ::vsetup::_say [::vapi::reason $why]
}

# Async POST with parsed JSON reply and generation/connection checks.
# Installed in api.tcl, not a private reimplementation of the wire.
proc ::vinstall::apply {} {
    variable generation
    if {$::vsetup::install_busy} { return 0 }
    set plan $::vsetup::install_preview
    if {![dict size $plan]} { return 1 }
    if {[dict get $plan answers] ne [::vinstall::answers]} {
        ::vinstall::refresh_preview
        ::vsetup::_say [::vi18n::mc {Component options changed. Review the new preview before applying.}]
        return 0
    }
    set body [dict get $plan answers]
    dict set body base_sig [::vapi::json_string [dict get $plan base_sig]]
    dict set body plan_sig [::vapi::json_string [dict get $plan plan_sig]]
    set ::vsetup::install_busy 1
    ::vapi::post_json_async /api/install/apply $body \
        [list ::vinstall::apply_failed $generation] [list ::vinstall::apply_done $generation]
    return 1
}

proc ::vinstall::apply_failed {epoch why} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    ::vsetup::_say $why
}
proc ::vinstall::apply_done {epoch value} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    if {![dict exists $value ok] || [dict get $value ok] ne "true" || ![dict exists $value action] || [dict get $value action] ne "configure"} {
        ::vsetup::_say [::vi18n::mc {Component configuration returned an invalid result; nothing was accepted as applied.}]
    }
}

proc ::vinstall::install_selected {} {
    variable generation
    variable pending_action
    if {$::vsetup::install_busy} { return }
    if {![info exists generation]} { set generation 0 }
    incr generation
    if {![info exists pending_action]} { set pending_action {} }
    if {[dict size $pending_action]} {
        if {[dict get $pending_action kind] eq "signed"} {
            ::vinstall::confirm_signed [dict get $pending_action review]
            return
        }
        set kind [dict get $pending_action kind]
        if {$kind eq "bundle"} {
            ::vinstall::confirm_bundle [dict get $pending_action source] [dict get $pending_action review]
        } elseif {$kind eq "rollback"} {
            ::vinstall::confirm_rollback [dict get $pending_action name] [dict get $pending_action review]
        }
        return
    }
    set ::vsetup::install_busy 1
    if {![dict size $::vsetup::install_manifest]} {
        set ::vsetup::install_busy 0
        return
    }
    set name [dict get $::vsetup::install_manifest name]
    set ::vsetup::install_busy 1
    ::vapi::post_json_async /api/install/component [dict create name [::vapi::json_string $name] confirm [::vapi::json_bool 0]] \
        [list ::vinstall::preview_failed $generation] [list ::vinstall::install_confirmed $name $generation]
}

proc ::vinstall::install_confirmed {name epoch value} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    if {![dict size $::vsetup::install_manifest] || [dict get $::vsetup::install_manifest name] ne $name} {
        ::vsetup::_say [::vi18n::mc {The component changed before installation; review the selected component again.}]
        return
    }
    if {![dict exists $value confirm_required] || [dict get $value confirm_required] ne "true" || ![dict exists $value plan_sig]} {
        ::vsetup::_say [::vi18n::mc {The component install preview was refused or has no plan signature; nothing was installed.}]
        return
    }
    set ::vsetup::install_busy 1
    set answer [tk_messageBox -type yesno -icon question -title [::vi18n::mc {Install component}] \
        -message [format [::vi18n::mc {Install %s into the managed component store? Its manifest has been shown; this does not start a service.}] $name]]
    if {$answer ne "yes"} { set ::vsetup::install_busy 0; return }
    ::vapi::post_json_async /api/install/component [dict create name [::vapi::json_string $name] confirm [::vapi::json_bool 1] plan_sig [::vapi::json_string [dict get $value plan_sig]]] \
        [list ::vinstall::preview_failed $generation] [list ::vinstall::component_done $generation]
}

proc ::vinstall::component_done {epoch value} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    if {[dict size $::vsetup::install_manifest] && [dict exists $value component name] && [dict get $value component name] ne [dict get $::vsetup::install_manifest name]} {
        ::vsetup::_say [::vi18n::mc {The installed component no longer matches the selected manifest. Review the component list again.}]
        return
    }
    if {![dict exists $value ok] || [dict get $value ok] ne "true" || ![dict exists $value action] || [dict get $value action] ne "install-component"} {
        ::vsetup::_say [::vi18n::mc {Component installation returned an invalid result. Review its state before retrying.}]
        return
    }
    ::vsetup::_say [::vi18n::mc {Component installed. Reload it from Plugins, or restart the gateway if the report requests it.}]
}

# A reviewed bundle or rollback is a pending action, not a generic
# component installation. Install_selected revisits that exact review;
# only its explicit confirmation can produce an apply request.
proc ::vinstall::choose_bundle {} {
    if {$::vsetup::install_busy} { return }
    set source [tk_getOpenFile -parent .setup_wizard -title [::vi18n::mc {Choose BUNDLE-INFO.json in the managed install-sources directory}] \
        -filetypes [list [list "BUNDLE-INFO.json" BUNDLE-INFO.json]]]
    if {$source eq ""} { return }
    ::vinstall::review_bundle [file dirname $source]
}

proc ::vinstall::review_bundle {source} {
    variable generation
    variable pending_action
    if {$::vsetup::install_busy} { return }
    if {![info exists generation]} { set generation 0 }
    incr generation
    set pending_action [dict create kind bundle-review source $source]
    set ::vsetup::install_busy 1
    ::vapi::post_json_async /api/install/bundle \
        [dict create source [::vapi::json_string $source] confirm [::vapi::json_bool 0]] \
        [list ::vinstall::bundle_failed $generation] \
        [list ::vinstall::bundle_preview_done $source $generation]
}

proc ::vinstall::bundle_failed {epoch why} {
    variable generation
    variable pending_action
    if {![info exists generation] || $epoch != $generation} { return }
    set pending_action {}
    set ::vsetup::install_busy 0
    ::vsetup::_say [::vapi::reason $why]
}

proc ::vinstall::bundle_preview_done {source epoch value} {
    variable generation
    variable pending_action
    if {![info exists generation] || $epoch != $generation} { return }
    if {![dict size $pending_action] || [dict get $pending_action kind] ne "bundle-review" || [dict get $pending_action source] ne $source} { return }
    set ::vsetup::install_busy 0
    if {[catch {
        if {[dict get $value preview] ne "true" || [dict get $value confirm_required] ne "true" || [dict get $value action] ne "install-bundle" || [string trim [dict get $value plan_sig]] eq ""} { error "malformed bundle preview" }
        if {![dict exists $value bundle] || ![dict exists $value verification]} { error "malformed bundle preview" }
    } why]} {
        set pending_action {}
        ::vsetup::_say [::vi18n::mc {The bundle preview was malformed; nothing was installed.}]
        return
    }
    set pending_action [dict create kind bundle source $source review $value]
    ::vsetup::_say [::vi18n::mc {Local bundle reviewed. Select Install selected component to confirm installation; nothing has been installed.}]
    if {$::vsetup::cur == 1 && [winfo exists .setup_wizard.body.c.inner.f.actions.install]} {
        .setup_wizard.body.c.inner.f.actions.install state !disabled
    }
    return
}

proc ::vinstall::confirm_bundle {source value} {
    variable generation
    variable pending_action
    set pending_action {}
    set ::vsetup::install_busy 1
    set details ""
    catch {set details [format " %s-%s" [dict get $value bundle version] [dict get $value bundle revision]]}
    set answer [tk_messageBox -parent .setup_wizard -type yesno -icon question \
        -title [::vi18n::mc {Reviewed local bundle}] \
        -message "[::vi18n::mc {Install the reviewed local bundle}]$details? [dict get $value verification]. [::vi18n::mc {This does not start a service or claim signed-download verification.}]"]
    set pending_action {}
    set ::vsetup::install_busy 0
    if {$answer ne "yes"} { return }
    set ::vsetup::install_busy 1
    ::vapi::post_json_async /api/install/bundle \
        [dict create source [::vapi::json_string $source] confirm [::vapi::json_bool 1] plan_sig [::vapi::json_string [dict get $value plan_sig]]] \
        [list ::vinstall::bundle_failed $generation] [list ::vinstall::bundle_done $generation]
}

proc ::vinstall::bundle_done {epoch value} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    if {[catch {
        if {[dict get $value ok] ne "true" || [dict get $value action] ne "install-bundle"} { error "wrong action" }
        if {![dict exists $value component name] || [dict get $value component name] ne "core"} { error "wrong component" }
    } why]} {
        ::vsetup::_say [::vi18n::mc {The bundle operation returned an invalid result. Verify the installation before retrying.}]
        return
    }
    ::vsetup::_say [::vi18n::mc {Reviewed local bundle installed. It was not signed-download verified. Restart is required before using the new core.}]
}

proc ::vinstall::rollback_selected {} {
    variable generation
    variable pending_action
    if {$::vsetup::install_busy || ![dict size $::vsetup::install_manifest]} { return }
    if {![info exists generation]} { set generation 0 }
    incr generation
    set name [dict get $::vsetup::install_manifest name]
    set pending_action [dict create kind rollback-review name $name]
    set ::vsetup::install_busy 1
    ::vapi::post_json_async /api/install/rollback \
        [dict create name [::vapi::json_string $name] confirm [::vapi::json_bool 0]] \
        [list ::vinstall::preview_failed $generation] \
        [list ::vinstall::rollback_preview_done $name $generation]
}

proc ::vinstall::rollback_preview_done {name epoch value} {
    variable generation
    variable pending_action
    if {![info exists generation] || $epoch != $generation} { return }
    if {![dict size $pending_action] || [dict get $pending_action kind] ne "rollback-review" || [dict get $pending_action name] ne $name} { return }
    set ::vsetup::install_busy 0
    if {![dict size $::vsetup::install_manifest] || [dict get $::vsetup::install_manifest name] ne $name} { return }
    if {[catch {
        if {[dict get $value preview] ne "true" || [dict get $value confirm_required] ne "true" || [dict get $value component] ne $name || [string trim [dict get $value plan_sig]] eq ""} { error "malformed rollback preview" }
    } why]} {
        set pending_action {}
        ::vsetup::_say [::vi18n::mc {The rollback preview was malformed or named another component; nothing was restored.}]
        return
    }
    set pending_action [dict create kind rollback name $name review $value]
    ::vsetup::_say [::vi18n::mc {Rollback reviewed. Select Install selected component to confirm restoration; nothing has been restored.}]
    if {$::vsetup::cur == 1 && [winfo exists .setup_wizard.body.c.inner.f.actions.install]} {
        .setup_wizard.body.c.inner.f.actions.install state !disabled
    }
    return
}

proc ::vinstall::confirm_rollback {name value} {
    variable generation
    variable pending_action
    set pending_action {}
    set ::vsetup::install_busy 1
    set from [dict get $value from_version]
    set to ""
    catch {set to [dict get $value to_version]}
    if {$to eq "null"} { set to "" }
    set snapshot [::vi18n::mc {No configuration snapshot will be restored.}]
    if {[dict exists $value restores_config_snapshot] && [dict get $value restores_config_snapshot] eq "true"} {
        set snapshot [::vi18n::mc {The reviewed configuration snapshot will also be restored.}]
    }
    set answer [tk_messageBox -parent .setup_wizard -type yesno -icon warning \
        -title [::vi18n::mc {Review rollback}] \
        -message "[format [::vi18n::mc {Roll back %1$s from %2$s to %3$s?}] $name $from $to] $snapshot"]
    set pending_action {}
    set ::vsetup::install_busy 0
    if {$answer ne "yes"} { return }
    set ::vsetup::install_busy 1
    ::vapi::post_json_async /api/install/rollback \
        [dict create name [::vapi::json_string $name] confirm [::vapi::json_bool 1] plan_sig [::vapi::json_string [dict get $value plan_sig]]] \
        [list ::vinstall::preview_failed $generation] [list ::vinstall::rollback_done $generation]
}

proc ::vinstall::rollback_done {epoch value} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    if {[catch {
        if {[dict get $value ok] ne "true" || [dict get $value action] ne "rollback" || ![dict exists $value component name]} { error "invalid rollback result" }
    } why]} {
        ::vsetup::_say [::vi18n::mc {The rollback returned an invalid result. Verify the installation before retrying.}]
        return
    }
    ::vsetup::_say [::vi18n::mc {Rollback completed. Reload the component or restart as requested by the report.}]
}

# Signed release bootstrap. The server verifies before returning a review;
# the opaque plan id never grants access to an arbitrary local path.
proc ::vinstall::choose_signed {} {
    if {$::vsetup::install_busy} { return }
    set w .signed_download
    if {[winfo exists $w]} { destroy $w }
    toplevel $w
    wm minsize $w 640 500
    ::vtitle::attach $w [::vi18n::mc {Signed release bootstrap}] -close ::vinstall::close_signed
    wm protocol $w WM_DELETE_WINDOW ::vinstall::close_signed
    ttk::frame $w.f -padding 12
    pack $w.f -fill both -expand yes
    ttk::label $w.f.intro -wraplength 650 -justify left
    ::vi18n::label $w.f.intro {Download and verify the release manifest and host archive before installing. All five identity fields are required. The gateway keeps the private staged archive; nothing is installed by this preview.}
    pack $w.f.intro -anchor w -fill x
    set row 0
    foreach {field label} {
        release {GitHub release URL}
        repository {Repository (owner/name)}
        tag {Release tag}
        commit {Full source commit SHA}
        workflow_sha {Full workflow commit SHA}
    } {
        ttk::label $w.f.l$row
        ::vi18n::label $w.f.l$row $label
        pack $w.f.l$row -anchor w -pady {6 0}
        ttk::entry $w.f.e$row -textvariable ::vinstall::signed_$field
        pack $w.f.e$row -fill x
        ::va11y::name $w.f.e$row [::vi18n::mc $label]
        incr row
    }
    if {![info exists ::vinstall::signed_release]} {
        set ::vinstall::signed_release https://github.com/77787777778777/veles/releases/download/v0.1.3/
        set ::vinstall::signed_repository 77787777778777/veles
        set ::vinstall::signed_tag v0.1.3
        set ::vinstall::signed_commit {}
        set ::vinstall::signed_workflow_sha {}
    }
    ttk::frame $w.f.buttons
    pack $w.f.buttons -fill x -pady {10 0}
    ttk::button $w.f.buttons.preview -command ::vinstall::review_signed
    ::vi18n::label $w.f.buttons.preview {Verify and review release…}
    pack $w.f.buttons.preview -side left
    ttk::button $w.f.buttons.install -command ::vinstall::install_selected -state disabled
    ::vi18n::label $w.f.buttons.install {Install verified release}
    pack $w.f.buttons.install -side right -padx {4 0}
    ttk::button $w.f.buttons.cancel -command ::vinstall::close_signed
    ::vi18n::label $w.f.buttons.cancel {Discard review}
    pack $w.f.buttons.cancel -side right
    text $w.f.review -width 85 -height 15 -wrap word -state disabled
    pack $w.f.review -fill both -expand yes -pady {8 0}
    ::va11y::name $w.f.review [::vi18n::mc {verified release review}]
    ::va11y::tab_order [list $w.f.e0 $w.f.e1 $w.f.e2 $w.f.e3 $w.f.e4 $w.f.buttons.preview $w.f.review $w.f.buttons.install $w.f.buttons.cancel]
    focus $w.f.e3
    catch {grab set $w}
    ::vi18n::on_locale_change [list ::vinstall::retext_signed]
}

proc ::vinstall::retext_signed {} {
    if {![winfo exists .signed_download]} { return }
    wm title .signed_download [::vi18n::mc {Verify signed release}]
    ::vtitle::attach .signed_download [::vi18n::mc {Signed release bootstrap}] -close ::vinstall::close_signed
    foreach {row label} {
        0 {GitHub release URL}
        1 {Repository (owner/name)}
        2 {Release tag}
        3 {Full source commit SHA}
        4 {Full workflow commit SHA}
    } {
        ::vi18n::aname .signed_download.f.e$row $label
    }
    ::vi18n::aname .signed_download.f.review {verified release review}
}

proc ::vinstall::close_signed {} {
    ::vinstall::discard_signed
    incr ::vinstall::generation
    set ::vsetup::install_busy 0
    catch {grab release .signed_download}
    catch {destroy .signed_download}
    ::vi18n::off_locale_change [list ::vinstall::retext_signed]
}

proc ::vinstall::review_signed {} {
    variable generation
    variable pending_action
    if {$::vsetup::install_busy || ![winfo exists .signed_download]} { return }
    if {[winfo exists .signed_download.f.buttons.install] && "disabled" ni [.signed_download.f.buttons.install state]} {
        ::vsetup::_say [::vi18n::mc {Confirm or discard the current signed review before starting another download.}]
        return
    }
    if {![info exists generation]} { set generation 0 }
    ::vinstall::discard_signed
    incr generation
    set request_id [::vinstall::signed_request_id]
    dict set ::vinstall::signed_request $generation $request_id
    set body {}
    foreach field {release repository tag commit workflow_sha} {
        dict set body $field [::vapi::json_string [string trim [set ::vinstall::signed_$field]]]
    }
    dict set body confirm [::vapi::json_bool 0]
    dict set body request_id [::vapi::json_string $request_id]
    set ::vsetup::install_busy 1
    set w .signed_download
    if {[winfo exists $w.f.review]} {
        $w.f.review configure -state normal
        $w.f.review delete 1.0 end
        $w.f.review insert end [::vi18n::mc {Downloading and verifying the signed release…}]
        $w.f.review configure -state disabled
    }
    ::vapi::post_json_async /api/install/download $body \
        [list ::vinstall::signed_failed $generation] [list ::vinstall::signed_preview_done $generation]
}

proc ::vinstall::signed_failed {epoch why} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    ::vsetup::_say $why
}

proc ::vinstall::signed_preview_done {epoch value} {
    variable generation
    variable pending_action
    variable signed_request
    if {![info exists generation]} { return }
    if {$epoch != $generation} { return }
    if {![winfo exists .signed_download]} { return }
    set tracked [expr {[info exists signed_request] && [dict exists $signed_request $epoch]}]
    if {!$tracked} { return }
    if {$tracked && (![dict exists $value plan_id] || [dict get $value plan_id] ne [dict get $signed_request $epoch])} {
        dict unset signed_request $epoch
        set ::vsetup::install_busy 0
        if {[dict exists $value plan_id]} { ::vinstall::discard_signed_review $epoch $value }
        ::vsetup::_say [::vi18n::mc {The signed release preview was malformed; nothing was installed.}]
        return
    }
    set ::vsetup::install_busy 0
    set valid 1
    if {[catch {
        foreach key {preview confirm_required plan_id download verification actions} { dict get $value $key }
        if {[dict get $value preview] ne "true" || [dict get $value confirm_required] ne "true" || [dict get $value action] ne "install-signed-bundle" || [string trim [dict get $value plan_id]] eq ""} { error "malformed signed preview" }
        foreach key {archive_name archive_sha256 manifest_sha256 version revision repository tag commit workflow_sha} { dict get $value download $key }
    } why]} {
        set valid 0
        ::vsetup::_say [::vi18n::mc {The signed release preview was malformed; nothing was installed.}]
    }
    if {!$valid} {
        if {$tracked} {
            set id [dict get $signed_request $epoch]
            dict unset signed_request $epoch
            ::vinstall::discard_signed_id $epoch $id $value
        }
        return
    }
    if {$tracked} { dict unset signed_request $epoch }
    set pending_action [dict create kind signed epoch $epoch review $value]
    set d [dict get $value download]
    set text "[::vi18n::mc {Verified signed release — not installed yet}]\n\n"
    foreach {key label} {
        repository {Repository} tag {Tag} version {Version} revision {Revision}
        commit {Source commit} workflow_sha {Workflow SHA} archive_name {Archive}
        archive_sha256 {Archive SHA-256} manifest_sha256 {Manifest SHA-256}
    } {
        # Literal message keys keep the static extractor honest. The
        # catalog's presence alone does not make a dynamic $label visible.
        set translated [switch -- $key {
            repository { ::vi18n::mc {Repository} }
            tag { ::vi18n::mc {Tag} }
            version { ::vi18n::mc {Version} }
            revision { ::vi18n::mc {Revision} }
            commit { ::vi18n::mc {Source commit} }
            workflow_sha { ::vi18n::mc {Workflow SHA} }
            archive_name { ::vi18n::mc {Archive} }
            archive_sha256 { ::vi18n::mc {Archive SHA-256} }
            manifest_sha256 { ::vi18n::mc {Manifest SHA-256} }
            default { error "untranslated signed review label" }
        }]
        append text "$translated: [dict get $d $key]\n"
    }
    append text "\n[dict get $value verification]\n\n"
    foreach action [dict get $value actions] { append text "• $action\n" }
    if {[dict exists $value review_expires_seconds]} { append text "\n[format [::vi18n::mc {This review expires after %s seconds. Closing or cancelling discards its private staging.}] [dict get $value review_expires_seconds]]\n" }
    if {[winfo exists .signed_download.f.review]} {
        .signed_download.f.review configure -state normal
        .signed_download.f.review delete 1.0 end
        .signed_download.f.review insert end $text
        .signed_download.f.review configure -state disabled
    }
    if {[winfo exists .signed_download.f.buttons.install]} {
        .signed_download.f.buttons.install state !disabled
        focus .signed_download.f.buttons.install
    }
    ::vsetup::_say [::vi18n::mc {Signed release verified. Select Install verified release to confirm; nothing has been installed.}]
}

proc ::vinstall::confirm_signed {value} {
    variable generation
    variable pending_action
    if {[info exists signed_request] && [dict exists $signed_request $generation]} {
        dict unset signed_request $generation
    }
    set pending_action {}
    set ::vsetup::install_busy 1
    ::vapi::post_json_async /api/install/download/confirm \
        [dict create plan_id [::vapi::json_string [dict get $value plan_id]] expected_manifest_sha256 [::vapi::json_string [dict get $value download manifest_sha256]] confirm [::vapi::json_bool 1]] \
        [list ::vinstall::signed_failed $generation] [list ::vinstall::signed_done $generation]
}

proc ::vinstall::discard_signed {} {
    variable pending_action
    variable generation
    variable signed_request
    if {![info exists generation]} { return }
    set value {}
    set id ""
    if {[info exists pending_action] && [dict size $pending_action] && [dict get $pending_action kind] eq "signed"} {
        set value [dict get $pending_action review]
        set pending_action {}
    }
    if {[info exists signed_request] && [dict exists $signed_request $generation]} {
        set id [dict get $signed_request $generation]
        dict unset signed_request $generation
    }
    if {$id ne ""} {
        ::vinstall::discard_signed_id $generation $id $value
    } elseif {[dict size $value] && [dict exists $value plan_id]} {
        ::vinstall::discard_signed_review $generation $value
    }
}

proc ::vinstall::discard_signed_id {epoch id value} {
    set digest {}
    catch {set digest [dict get $value download manifest_sha256]}
    ::vapi::post_json_async /api/install/download/confirm \
        [dict create plan_id [::vapi::json_string $id] expected_manifest_sha256 [::vapi::json_string $digest] confirm [::vapi::json_bool 0]] \
        [list ::vinstall::discard_failed $epoch] ::vinstall::discard_done
}

proc ::vinstall::discard_signed_review {epoch value} {
    if {[dict size $value] && [dict exists $value plan_id]} {
        ::vinstall::discard_signed_id $epoch [dict get $value plan_id] $value
    }
}

proc ::vinstall::discard_failed {epoch why} {
    ::vsetup::_say [::vi18n::mc {The signed download could not be discarded explicitly. It expires on the gateway; no installation was requested.}]
}

proc ::vinstall::discard_done {value} {
    if {![dict exists $value ok] || [dict get $value ok] ne "true" || ![dict exists $value cancelled] || [dict get $value cancelled] ne "true"} {
        ::vsetup::_say [::vi18n::mc {The signed download discard returned an invalid response; no installation was requested.}]
    }
}

proc ::vinstall::signed_done {epoch value} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    if {[catch {
        if {[dict get $value ok] ne "true" || [dict get $value action] ne "install-signed-bundle" || [dict get $value component name] ne "core" || [dict get $value restart_required] ne "true"} { error "invalid signed install result" }
    } why]} {
        ::vsetup::_say [::vi18n::mc {The signed installation returned an invalid result. Verify the installation before retrying.}]
        return
    }
    ::vsetup::_say [::vi18n::mc {Verified signed bundle installed. Restart the gateway before using the new core.}]
    catch {destroy .signed_download}
}

proc ::vinstall::verify {} {
    variable generation
    if {$::vsetup::install_busy} { return }
    if {![info exists generation]} { set generation 0 }
    incr generation
    set ::vsetup::install_busy 1
    ::vapi::post_json_async /api/install/verify [::vapi::json_object {}] \
        [list ::vinstall::preview_failed $generation] [list ::vinstall::verify_done $generation]
}

proc ::vinstall::verify_done {epoch value} {
    variable generation
    if {![info exists generation] || $epoch != $generation} { return }
    set ::vsetup::install_busy 0
    if {![dict exists $value ok] || ![dict exists $value problems]} {
        ::vsetup::_say [::vi18n::mc {The verification reply was malformed; no integrity result was accepted.}]
        return
    }
    set ::vsetup::install_verify $value
    if {[dict get $value ok] eq "true" && [llength [dict get $value problems]] == 0} {
        ::vsetup::_say [::vi18n::mc {Installation verification passed.}]
    } elseif {[dict get $value ok] eq "false"} {
        ::vsetup::_say "[::vi18n::mc {Installation verification failed:}] [join [dict get $value problems] {; }]"
    } else {
        ::vsetup::_say [::vi18n::mc {The verification reply was malformed; no integrity result was accepted.}]
    }
}
