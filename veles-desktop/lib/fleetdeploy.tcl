# veles-desktop/lib/fleetdeploy.tcl — reviewed fleet deployment plans.
#
# F1.4: reviewed fleet deployment plans and their explicit staged executor.
# The manifest binds a selected cluster node to its explicitly associated
# saved instance. Credentials stay in that registry and never enter a plan.
# Every external action goes through the transport seam below; tests use a
# fake transport and never touch a real target.
#
# A signed release is staged by the target's authenticated installer API.
# A native-bundle install is not a container-image deployment, and an
# installed current link is not proof that a restarted process is healthy.
package require json
source [file join [file dirname [file normalize [info script]]] fleetjournal.tcl]

namespace eval ::vfleetdeploy {
    variable nodes {}
    variable instances {}
    variable plans [dict create]
    variable transport ::vfleetdeploy::_transport
    variable operations [dict create]
}

proc ::vfleetdeploy::_field {d key {default ""}} {
    if {[dict exists $d $key]} { return [dict get $d $key] }
    return $default
}

proc ::vfleetdeploy::_true {value} {
    return [expr {$value in {1 true yes on}}]
}

proc ::vfleetdeploy::_sha256 {text} {
    # This is a plan identity, not an artifact trust decision. sha256 is
    # already a desktop dependency (used by the Dev contribution review).
    if {[llength [info commands ::sha2::sha256]]} {
        return [::sha2::sha256 -hex $text]
    }
    return ""
}

# GET /api/fleet answers `nodes` as a JSON ARRAY of node objects
# (desktop_api.rs fleet_inventory_json); the manifest looks nodes up by
# id. This is the one conversion between the two, and the suites feed it
# a body captured from a real gateway rather than an id-keyed fixture
# (the F1-02 review finding: the map-shaped fixture hid that no real
# target could ever be found).
proc ::vfleetdeploy::nodes_from_inventory {inventory} {
    if {[catch {dict exists $inventory nodes} has] || !$has} {
        return -code error "fleet inventory lacks its nodes list"
    }
    set keyed [dict create]
    foreach node [dict get $inventory nodes] {
        if {[catch {dict get $node id} id] || [string trim $id] eq "" || $id eq "null"} {
            return -code error "fleet inventory contains a node without an id"
        }
        dict set keyed $id $node
    }
    return $keyed
}

# One parsed health row. /api/health is JSON; reading it as raw text made
# every field empty and refused every real review (F1-01).
proc ::vfleetdeploy::_health_row {health} {
    return [dict create \
        status [_field $health status] version [_field $health version] \
        revision [_field $health revision] stale [_field $health stale 0]]
}

proc ::vfleetdeploy::health_observation {instance} {
    return [_health_row [::vapi::with_instance $instance {::vapi::get_json /api/health 5000}]]
}

proc ::vfleetdeploy::_binding {id} {
    variable instances
    set found {}
    dict for {name spec} $instances {
        if {[_field $spec node_id] eq $id} { lappend found $name }
    }
    if {[llength $found] != 1} {
        return -code error "node $id has no explicitly associated saved instance; add one in Instances first"
    }
    set name [lindex $found 0]
    set spec [dict get $instances $name]
    foreach key {host unit url} {
        if {[_field $spec $key] eq ""} {
            return -code error "saved instance $name lacks required $key for deployment"
        }
    }
    if {![::vinstpane::valid_host [dict get $spec host]] ||
        ![::vinstpane::valid_unit [dict get $spec unit]] ||
        ![::vinst::validate [dict get $spec url]]} {
        return -code error "saved instance $name has an invalid gateway or SSH lifecycle binding"
    }
    return [list $name $spec]
}

# The id of the `self` row in a parsed /api/fleet body, or "".
proc ::vfleetdeploy::_self_id {inventory} {
    if {[catch {dict get $inventory nodes} nodes]} { return "" }
    foreach node $nodes {
        if {[catch {dict get $node self} self] || $self ni {1 true}} { ::continue }
        if {[catch {dict get $node id} id]} { return "" }
        return $id
    }
    return ""
}

# Prove that the saved instance a target is bound to IS the selected
# node, before anything is staged, installed or restarted through it
# (FLEET-2). The binding is operator-typed text: matching the manifest's
# node id against it, and the registry row's version against the
# instance's health, proved nothing about the machine at that URL, so a
# typo'd or stale node id staged a signed bundle on, and restarted, a
# different host. The instance is asked over the link the deployment
# itself uses (its own URL and token) which node it is; anything but a
# `self` row naming the selected id refuses, with both ids named.
proc ::vfleetdeploy::verify_identity {target} {
    set id [dict get $target node_id]
    set instance [dict get $target instance]
    if {[catch {_call identity $target} inventory]} {
        return -code error "node identity unverified: saved instance $instance could not be asked which node it is ($inventory); nothing was staged, installed or restarted"
    }
    set self [_self_id $inventory]
    if {$self eq ""} {
        return -code error "node identity unverified: saved instance $instance reports no cluster node identity, so it cannot be proven to be node $id; nothing was staged, installed or restarted"
    }
    if {$self ne $id} {
        return -code error "node identity mismatch: saved instance $instance is node $self, not $id; nothing was staged, installed or restarted"
    }
    return $self
}

proc ::vfleetdeploy::_validate_release {release} {
    foreach key {repository tag commit workflow_sha} {
        if {[dict get $release $key] eq ""} {
            return -code error "release requires repository, tag, source commit and workflow SHA"
        }
    }
    if {![regexp {^[a-f0-9]{40}$} [dict get $release commit]]} {
        return -code error "source commit must be a full lowercase Git SHA"
    }
    if {![regexp {^[a-f0-9]{40}$} [dict get $release workflow_sha]]} {
        return -code error "workflow SHA must be a full lowercase Git SHA"
    }
    if {![regexp {^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$} [dict get $release repository]]} {
        return -code error "release repository must be owner/repository"
    }
    if {![regexp {^[A-Za-z0-9][A-Za-z0-9._/-]{0,127}$} [dict get $release tag]] ||
        [string match *..* [dict get $release tag]] ||
        [string match -* [dict get $release tag]]} {
        return -code error "invalid release tag"
    }
}

proc ::vfleetdeploy::manifest {selected release service_model} {
    variable nodes
    if {![llength $selected]} {
        return -code error "choose at least one registered node"
    }
    _validate_release $release
    if {$service_model ne "native-bundle"} {
        return -code error "unsupported fleet service model: $service_model; container images need their own reviewed executor"
    }
    set targets {}
    set seen {}
    foreach id $selected {
        if {![dict exists $nodes $id]} {
            return -code error "$id is not a registered node"
        }
        if {[dict exists $seen $id]} {
            return -code error "node $id selected more than once"
        }
        dict set seen $id 1
        set node [dict get $nodes $id]
        lassign [_binding $id] instance spec
        set row [dict create \
            node_id $id name [_field $node name] instance $instance \
            host [dict get $spec host] unit [dict get $spec unit] \
            gateway_url [dict get $spec url] \
            current_version [_field $node version unknown] \
            heartbeat_stale [_field $node stale 1] \
            stage [expr {[llength $targets] == 0 ? "canary" : "remaining"}]]
        lappend targets $row
    }
    set m [dict create \
        version 1 kind fleet-deployment \
        service_model $service_model \
        release [dict create \
            repository [dict get $release repository] \
            tag [dict get $release tag] \
            commit [dict get $release commit] \
            workflow_sha [dict get $release workflow_sha]] \
        targets $targets]
    dict set m plan_sig [_sha256 [::json::dict2json $m]]
    return $m
}

proc ::vfleetdeploy::preflight {m observations} {
    set checked {}
    foreach target [dict get $m targets] {
        set id [dict get $target node_id]
        if {![dict exists $observations $id]} {
            return -code error "node $id has no current health observation"
        }
        set health [dict get $observations $id]
        if {[_field $health status] ne "ok"} {
            return -code error "node $id is not healthy; deployment was not started"
        }
        if {[_field $health stale 0] ni {0 false ""}} {
            return -code error "node $id is stale; refresh its health before deployment"
        }
        set version [_field $health version unknown]
        if {$version ne [dict get $target current_version]} {
            return -code error "node $id version changed since the manifest was reviewed"
        }
        # A development process may honestly report that its source is
        # unknown. That is not proof of a deployed release: verify below
        # still requires the exact reviewed SHA after installation.
        set revision [_field $health revision]
        if {[string trim $revision] eq ""} {
            return -code error "node $id does not report a running source revision"
        }
        if {$revision ni {unreported-development-build unreported-dirty-build unreported} &&
            ![regexp {^[a-f0-9]{40}$} $revision]} {
            return -code error "node $id returned an invalid running source revision"
        }
        lappend checked [dict create node_id $id version $version revision $revision]
    }
    return [dict create \
        version 1 kind fleet-preflight \
        plan_sig [dict get $m plan_sig] \
        ready 1 observations $checked]
}

proc ::vfleetdeploy::_working_preflight {op} {
    if {[dict exists $op preflight observations]} {
        set observations [dict get $op preflight observations]
    } elseif {[dict exists $op preflight]} {
        set observations {}
        dict for {id observation} [dict get $op preflight] {
            lappend observations $observation
        }
    } else {
        return -code error "operation lacks its original preflight observations"
    }
    set mapped [dict create]
    foreach observation $observations {
        if {[dict exists $observation node_id]} {
            dict set mapped [dict get $observation node_id] $observation
        }
    }
    dict set op preflight $mapped
    return $op
}

proc ::vfleetdeploy::apply {m preflight} {
    if {[catch {dict size $preflight} size] || $size == 0} {
        return -code error "no reviewed preflight for this exact manifest; deployment was not started"
    }
    if {[_field $preflight kind] ne "fleet-preflight" ||
        [_field $preflight plan_sig] ne [dict get $m plan_sig] ||
        [_field $preflight ready 0] != 1} {
        return -code error "no reviewed preflight for this exact manifest; deployment was not started"
    }
    set op [::vfleetjournal::begin $m $preflight]
    dict set op manifest $m
    dict set op reviews [dict create]
    foreach observation [dict get $preflight observations] {
        dict set op preflight [dict get $observation node_id] $observation
    }
    return [_save $op]
}

proc ::vfleetdeploy::_journal_manifest {op} {
    if {![dict exists $op manifest] || [catch {dict size [dict get $op manifest]}]} {
        return -code error "journal lacks its reviewed manifest; create a fresh reviewed operation"
    }
    set m [dict get $op manifest]
    foreach key {kind plan_sig release service_model targets} {
        if {![dict exists $m $key]} { return -code error "journal manifest lacks $key" }
    }
    if {[_sha256 [::json::dict2json [dict remove $m plan_sig]]] ne [dict get $m plan_sig]} {
        return -code error "journal manifest identity is invalid; create a fresh reviewed operation"
    }
    if {[dict get $op plan_sig] ne [dict get $m plan_sig]} {
        return -code error "journal operation and manifest identities differ"
    }
    _validate_release [dict get $m release]
    return $m
}

proc ::vfleetdeploy::load {id} {
    set op [_working_preflight [::vfleetjournal::get $id]]
    _journal_manifest $op
    return [_save $op]
}

proc ::vfleetdeploy::resume {id observations} {
    set op [::vfleetjournal::get $id]
    if {[dict exists $op status] && [dict get $op status] in {completed rolled-back abandoned}} {
        return -code error "terminal deployment operation is immutable"
    }
    set op [_working_preflight $op]
    set m [_journal_manifest $op]
    # Resume is a read/reconcile operation. It does NOT repeat an install,
    # restart or rollback command. A pending mutation must be inspected
    # and explicitly rolled back; a staged target must still be running
    # its recorded pre-install source before its retained plan can apply.
    # A target that stopped before any installation attempt is loaded as
    # untouched, so the operation can be abandoned or its rollback
    # finished instead of dead-ending (F1-05).
    set stopped {}
    foreach target [dict get $m targets] {
        set id [dict get $target node_id]
        set state [_state $op $id]
        set recoverable [expr {$state in {failed restarting rollback-preview rollback-staged rollback-restarting} &&
            [dict exists $op targets $id install_started] &&
            [dict get $op targets $id install_started] in {1 true}}]
        if {$state in {staging failed restarting} && !$recoverable} {
            lappend stopped $id
            # `::continue`: inside this namespace a bare `continue` is the
            # operation proc ::vfleetdeploy::continue, not the loop builtin.
            ::continue
        }
        if {$state in {rollback-preview rollback-staged rollback-restarting} && !$recoverable} {
            return -code error "operation stopped at $state on $id without a recoverable installation attempt; inspect live health and create a fresh reviewed operation"
        }
        if {![dict exists $observations $id]} {
            return -code error "recovered target $id has no fresh health observation"
        }
        set health [dict get $observations $id]
        if {[_field $health status] ne "ok" || [_field $health stale 0] ni {0 false ""}} {
            return -code error "recovered target $id is not fresh and healthy"
        }
        if {$state in {reviewed remaining} && [_field $health version] ne [dict get $target current_version]} {
            return -code error "recovered target $id version changed; create a fresh reviewed operation"
        }
        if {$state eq "staged"} {
            if {![dict exists $op reviews $id] || ![dict exists $op targets $id running_revision]} {
                return -code error "staged target $id lacks its durable review or prior running revision"
            }
            if {[_field $health revision] ne [dict get $op targets $id running_revision]} {
                return -code error "staged target $id source changed after interruption; inspect and roll back explicitly"
            }
        } elseif {$state in {healthy rolled-back}} {
            if {[dict get $health revision] ne [dict get $op targets $id running_revision]} {
                return -code error "target $id no longer runs its last verified source revision"
            }
        } elseif {$state ni {reviewed remaining} && !$recoverable} {
            return -code error "recovered target $id has an unsupported state: $state"
        }
    }
    foreach target [dict get $m targets] {
        set id [dict get $target node_id]
        if {$id in $stopped} { ::continue }
        set updated [::vfleetjournal::recovery_observed $op $id [_field [dict get $observations $id] revision]]
        set op [_working_preflight [dict merge $op $updated]]
    }
    return [_save $op]
}

proc ::vfleetdeploy::diff {m} {
    set desired [dict get $m release commit]
    set rows {}
    foreach target [dict get $m targets] {
        lappend rows [dict create \
            node_id [dict get $target node_id] \
            instance [dict get $target instance] \
            stage [dict get $target stage] \
            current_version [dict get $target current_version] \
            desired_commit $desired \
            current_revision [_field $target current_revision unreported] \
            artifact_action "download and verify signed bundle, preview, then confirm exact target plan" \
            lifecycle_action "restart the configured user unit, then verify health and running revision" \
            rollback_action "preview and confirm core rollback, restart, then verify health and prior revision"]
    }
    return $rows
}

# ── Explicit reviewed-apply state machine ─────────────────────────────
# A successful preview is not installation, and a successful installer
# response is not a healthy running process. These are separate steps.
proc ::vfleetdeploy::_transport {action target payload} {
    set instance [dict get $target instance]
    switch -- $action {
        health {
            return [::vapi::with_instance $instance { ::vapi::get_json /api/health 5000 }]
        }
        install-state {
            return [::vapi::with_instance $instance { ::vapi::get_json /api/install/state 15000 }]
        }
        download-preview {
            # The gateway downloads and cosign-verifies the release before
            # it answers; post_json's 10 s default would abandon that.
            return [_post_json $instance /api/install/download $payload 300000]
        }
        download-confirm - download-cancel {
            # The same route: confirm:true applies the pending signed plan,
            # confirm:false cancels it (desktop_install.rs).
            return [_post_json $instance /api/install/download/confirm $payload 300000]
        }
        rollback-preview - rollback-confirm {
            return [_post_json $instance /api/install/rollback $payload 120000]
        }
        identity {
            # The gateway's own registry view: its `self` row names the
            # node it is. The same instance link every installer call uses.
            return [::vapi::with_instance $instance { ::vapi::get_json /api/fleet 5000 }]
        }
        restart {
            set argv [::vinstpane::ssh_argv [dict get $target host] [dict get $target unit] restart]
            # LIST expansion: no shell, no interpolated remote command.
            set status [catch {exec {*}$argv 2>@1} output]
            if {$status != 0} { return -code error "configured unit restart failed: $output" }
            return [dict create restarted 1]
        }
        default { return -code error "unknown deployment transport action: $action" }
    }
}

proc ::vfleetdeploy::_post_json {instance path payload ms} {
    return [::vapi::with_instance $instance {
        ::json::json2dict [::vapi::post $path [::vapi::_json_value $payload] $ms]
    }]
}

proc ::vfleetdeploy::_call {action target {payload {}}} {
    variable transport
    if {![llength [info commands $transport]]} {
        return -code error "deployment transport is not configured"
    }
    return [$transport $action $target $payload]
}

proc ::vfleetdeploy::_target {op id} {
    set m $op
    if {[dict exists $op manifest]} { set m [dict get $op manifest] }
    foreach target [dict get $m targets] {
        if {[dict get $target node_id] eq $id} { return $target }
    }
    return -code error "$id is not a declared deployment target"
}

proc ::vfleetdeploy::_state {op id} {
    return [_field [dict get $op targets $id] status reviewed]
}

proc ::vfleetdeploy::_save {op} {
    variable operations
    dict set operations [dict get $op id] $op
    return $op
}

proc ::vfleetdeploy::_step {op id state detail {running_revision ""}} {
    set updated [::vfleetjournal::transition [dict get $op id] $id $state $detail $running_revision]
    # The durable journal intentionally contains no private review payload.
    # Keep the manifest, preflight revisions and opaque plan ids in the
    # working operation while the journal remains the transition authority.
    set merged [_working_preflight [dict merge $op $updated]]
    if {[dict get $updated status] in {completed rolled-back}} {
        dict set merged status [dict get $updated status]
    }
    return [_save $merged]
}

proc ::vfleetdeploy::_fresh_health {target} {
    set health [_call health $target]
    if {[_field $health status] ne "ok" || [_field $health stale 0] ni {0 false ""}} {
        return -code error "target health is not fresh and healthy"
    }
    if {[string trim [_field $health revision]] eq ""} {
        return -code error "target does not report its running source revision"
    }
    if {[_field $health revision] ni {unreported-development-build unreported-dirty-build unreported} &&
        ![regexp {^[a-f0-9]{40}$} [_field $health revision]]} {
        return -code error "target reports an invalid running source revision"
    }
    return $health
}

proc ::vfleetdeploy::_await_health {target {attempts 30}} {
    for {set attempt 0} {$attempt < $attempts} {incr attempt} {
        if {[catch {_fresh_health $target} health] == 0} { return $health }
        after 1000
    }
    return -code error "target did not become healthy after restart"
}

proc ::vfleetdeploy::_signed_review {target release request_id} {
    set body [dict create \
        request_id [::vapi::json_string $request_id] \
        release [::vapi::json_string "https://github.com/[dict get $release repository]/releases/download/[dict get $release tag]/"] \
        repository [::vapi::json_string [dict get $release repository]] \
        tag [::vapi::json_string [dict get $release tag]] \
        commit [::vapi::json_string [dict get $release commit]] \
        workflow_sha [::vapi::json_string [dict get $release workflow_sha]] \
        confirm [::vapi::json_bool 0]]
    set review [_call download-preview $target $body]
    if {![_true [_field $review preview]] ||
        ![_true [_field $review confirm_required]] ||
        [_field $review action] ne "install-signed-bundle" ||
        [string trim [_field $review plan_id]] eq ""} {
        return -code error "signed release preview is malformed"
    }
    set download [_field $review download]
    foreach key {archive_sha256 manifest_sha256 repository tag commit workflow_sha} {
        if {[_field $download $key] eq ""} {
            return -code error "signed release preview lacks $key"
        }
    }
    foreach key {archive_sha256 manifest_sha256} {
        if {![regexp {^[a-f0-9]{64}$} [_field $download $key]]} {
            return -code error "signed release preview has an invalid $key"
        }
    }
    if {[string trim [_field $review verification]] eq "" ||
        [string trim [_field $review review_expires_seconds]] eq ""} {
        return -code error "signed release preview lacks verification evidence or its review expiry"
    }
    foreach key {repository tag commit workflow_sha} {
        if {[dict get $download $key] ne [dict get $release $key]} {
            return -code error "signed release preview does not match the reviewed $key"
        }
    }
    return $review
}

proc ::vfleetdeploy::_safe_error {error} {
    # Transport diagnostics are not an audit-safe string channel. Keep
    # only bounded, non-secret operational phrases; raw SSH/API output is
    # shown to the operator but never persisted in the journal.
    set text [string tolower $error]
    # The identity refusal (verify_identity) changed nothing. Both ids are
    # in the operator's diagnostic; the journal keeps only this phrase.
    if {[string match "node identity *" $text]} {
        return "the saved instance did not prove it is the selected node; nothing was changed"
    }
    # Exact binding phrases only. A bare "manifest" match used to turn
    # "signed release preview lacks manifest_sha256" into a claimed
    # registry tampering (F1-11).
    if {[string match "*registry binding*" $text] || [string match "*manifest identit*" $text] ||
        [string match "*lacks its reviewed manifest*" $text] ||
        [string match "*operation identity*" $text] || [string match "*reviewed instance*" $text]} {
        return "reviewed registry or operation binding changed; action refused"
    }
    if {[string match "*password*" $text] || [string match "*token*" $text] ||
        [string match "*bearer*" $text] || [string match "*secret*" $text] ||
        [string match "*apikey*" $text] || [string match "*api-key*" $text] ||
        [string match "*api_key*" $text] || [string match "*private key*" $text]} {
        return "transport authentication or credential diagnostic (redacted)"
    }
    if {[string match "*permission denied*" $text]} {
        return "transport permission denied"
    }
    if {[string match "*timed out*" $text] || [string match "*timeout*" $text]} {
        return "transport request timed out"
    }
    if {[string match "*connection refused*" $text] || [string match "*could not resolve*" $text] ||
        [string match "*no route*" $text] || [string match "*network unreachable*" $text]} {
        return "transport connection failed"
    }
    if {[string match "*restart*" $text] && [string match "*failed*" $text]} {
        return "configured unit restart failed (diagnostic redacted)"
    }
    if {[string match "*health*" $text]} {
        return "target health verification failed"
    }
    if {[string match "*not installer-managed*" $text]} {
        return "target core is not installer-managed; rollback would be impossible"
    }
    if {[string match "*signed release*" $text] || [string match "*signed-download*" $text]} {
        return "signed release review was refused or does not match the reviewed release"
    }
    return "transport operation failed (diagnostic redacted)"
}

# The operator's copy of a failure: never journaled, capped, and with
# credential shapes and the known instance tokens scrubbed. The journal
# keeps only _safe_error's bounded phrase (F1-11).
proc ::vfleetdeploy::diagnostic {error {secrets {}}} {
    set text $error
    foreach secret $secrets {
        if {[string length $secret] >= 4} {
            set text [string map [list $secret <redacted>] $text]
        }
    }
    regsub -all -nocase {(bearer[[:space:]]+)[^[:space:]\"]+} $text {\1<redacted>} text
    regsub -all -nocase {((token|password|passwd|secret|api[_-]?key)[\"]?[[:space:]]*[=:][[:space:]]*[\"]?)[^[:space:]\",&]+} $text {\1<redacted>} text
    if {[string length $text] > 600} { set text "[string range $text 0 599]…" }
    return $text
}

proc ::vfleetdeploy::_failed {op id error} {
    set detail [_safe_error $error]
    if {[catch {::vfleetjournal::transition [dict get $op id] $id failed $detail} updated]} {
        # Losing the durable failure is itself an error, not a successful
        # in-memory fallback. The caller must not proceed to another node.
        return -code error "deployment journal could not record failure: $updated (original: [_safe_error $error])"
    }
    set merged [_working_preflight [dict merge $op $updated]]
    dict set merged failure $detail
    return [_save $merged]
}

proc ::vfleetdeploy::_stage {op id} {
    set m [dict get $op manifest]
    set target [_target $op $id]
    if {[_state $op $id] ni {reviewed remaining staged}} {
        return -code error "target $id cannot stage from its current state"
    }
    set first [dict get [lindex [dict get $op manifest targets] 0] node_id]
    if {$id ne $first} {
        if {[_state $op $first] ne "healthy"} {
            return -code error "canary target $first must be healthy before staging $id"
        }
        if {[catch {
            set canary_health [_fresh_health [_target $op $first]]
            if {[_field $canary_health revision] ne [dict get $m release commit]} {
                return -code error "canary target $first no longer runs the reviewed source revision"
            }
        } why options]} {
            set safe [_safe_error $why]
            set saved [::vfleetjournal::canary_blocked $op $id $first $safe]
            dict set operations [dict get $op id] [_save [_working_preflight [dict merge $op $saved]]]
            return -code error -errorcode {VELES CANARY_BLOCKED} $safe
        }
    }
    set health [_fresh_health $target]
    set reviewed [dict get $op preflight $id]
    if {[_field $health version] ne [dict get $reviewed version] ||
        [_field $health revision] ne [dict get $reviewed revision]} {
        return -code error "target $id changed since its reviewed preflight; build a fresh manifest and preflight"
    }
    set op [_step $op $id staging {staging started: installer baseline, then signed release preview}]
    # The rollback baseline is the CURRENT core's digest, read from the
    # installer state. It used to come from a rollback preview, which
    # refuses whenever the current core retains no previous one, so a
    # node's first managed upgrade could never be staged (F1-07). After
    # the signed install the old core is retained as `previous`, and the
    # rollback guard compares that digest with this one.
    set baseline [_core_artifact [_call install-state $target]]
    # The gateway keeps ONE pending signed plan for an hour. A target
    # re-staged after reconciliation still owns the plan of its first
    # preview; cancel it, or the fresh preview is a 409 (F1-05).
    if {[dict exists $op reviews $id plan_id]} {
        _cancel_plan $target [dict get $op reviews $id plan_id]
    }
    # A cancelled request id is tombstoned by the gateway, so every
    # preview attempt carries its own id; it is journaled BEFORE the
    # request so an interrupted stage can still be cancelled.
    set request [_plan_request_id $op]
    set op [_working_preflight [dict merge $op [::vfleetjournal::plan_requested $op $id $request]]]
    set release [dict get $m release]
    if {[catch {
        set review [_signed_review $target $release $request]
        dict set review previous_artifact_sha256 $baseline
        set op [::vfleetjournal::staged $op $id $review [_field $health revision]]
        set op [_working_preflight $op]
        set op [_step $op $id staged {signed release bundle staged; restart and health verification remain explicit}]
    } why options]} {
        # Never leave the target's single signed-download slot occupied by
        # a plan this operation will not confirm.
        catch {_cancel_plan $target $request}
        return -options $options $why
    }
    return [_save $op]
}

proc ::vfleetdeploy::_plan_request_id {op} {
    # Operation ids are at most 80 characters; the gateway allows 128.
    return "[dict get $op id].[clock microseconds]"
}

proc ::vfleetdeploy::_core_artifact {state} {
    set digest ""
    if {[dict exists $state current_artifacts core]} {
        set digest [dict get $state current_artifacts core]
    }
    if {![regexp {^[a-f0-9]{64}$} $digest]} {
        return -code error "target core is not installer-managed, so an installation could not be rolled back; install the core with the Installer first"
    }
    return $digest
}

proc ::vfleetdeploy::_cancel_plan {target plan_id} {
    set reply [_call download-cancel $target [dict create \
        plan_id [::vapi::json_string $plan_id] confirm [::vapi::json_bool 0]]]
    if {![_true [_field $reply cancelled]]} {
        return -code error "target refused to cancel its pending signed plan"
    }
    return $reply
}

# Close an operation in which nothing was installed: every target is
# untouched (no installation confirmation was ever sent). Any signed plan
# it previewed is cancelled first, best effort: an unreachable gateway
# drops it itself after an hour. A target with an installation attempt
# must be rolled back instead, so this can never hide a changed node.
proc ::vfleetdeploy::abandon {op} {
    foreach target [dict get $op manifest targets] {
        set id [dict get $target node_id]
        if {[dict exists $op targets $id install_started] &&
            [dict get $op targets $id install_started] in {1 true}} {
            return -code error "target $id had an installation attempt; roll it back instead of abandoning the operation"
        }
    }
    set uncancelled {}
    foreach target [dict get $op manifest targets] {
        set id [dict get $target node_id]
        set plans {}
        if {[dict exists $op reviews $id plan_id]} { lappend plans [dict get $op reviews $id plan_id] }
        if {[dict exists $op targets $id plan_request]} { lappend plans [dict get $op targets $id plan_request] }
        foreach plan [lsort -unique $plans] {
            if {[catch {_cancel_plan $target $plan}]} { lappend uncancelled $id }
        }
    }
    set detail {operation abandoned before any installation; previewed signed plans cancelled}
    if {[llength $uncancelled]} {
        set detail "operation abandoned before any installation; the pending signed plan on [join [lsort -unique $uncancelled] {, }] could not be cancelled and expires within an hour"
    }
    set final [::vfleetjournal::abandon [dict get $op id] $detail]
    return [_save [_working_preflight [dict merge $op $final]]]
}

proc ::vfleetdeploy::stage {op id} {
    set before [_state $op $id]
    if {$before eq "staged" &&
        (![dict exists $op targets $id recovery_observed] ||
         [dict get $op targets $id recovery_observed] ni {1 true})} {
        return -code error "staged target $id must be reconciled from its durable journal before a fresh preview"
    }
    if {$before in {reviewed remaining staged}} {
        # Refused before the target is touched: nothing is journaled as
        # failed, and the operation can still be abandoned.
        verify_identity [_target $op $id]
    }
    if {[catch {_stage $op $id} result options]} {
        if {[dict get $options -errorcode] eq {VELES CANARY_BLOCKED}} {
            return -code error -errorcode {VELES CANARY_BLOCKED} $result
        }
        if {$before ni {reviewed remaining staged}} {
            return -code error -errorinfo [dict get $options -errorinfo] $result
        }
        _failed $op $id $result
        return -code error -errorinfo [dict get $options -errorinfo] \
            "deployment staging failed for $id: $result"
    }
    return $result
}

proc ::vfleetdeploy::verify {op id} {
    set before [_state $op $id]
    if {$before eq "staged"} {
        # The install confirmation and the restart follow. Refused here,
        # the target stays staged and nothing is journaled as failed.
        verify_identity [_target $op $id]
    }
    if {[catch {_verify $op $id} value options]} {
        if {[dict get $options -errorcode] eq {VELES CANARY_BLOCKED}} {
            return -code error -errorcode {VELES CANARY_BLOCKED} $value
        }
        if {$before ne "staged"} {
            return -code error -errorinfo [dict get $options -errorinfo] $value
        }
        _failed $op $id $value
        return -code error -errorinfo [dict get $options -errorinfo] \
            "deployment verification failed for $id: $value"
    }
    return $value
}

proc ::vfleetdeploy::_verify {op id} {
    set canary [dict get [lindex [dict get $op manifest targets] 0] node_id]
    if {$id ne $canary} {
        if {[catch {
            set canary_health [_fresh_health [_target $op $canary]]
            if {[_field $canary_health revision] ne [dict get $op release commit]} {
                return -code error "canary no longer runs the reviewed source revision"
            }
        } why options]} {
            set safe [_safe_error $why]
            set saved [::vfleetjournal::canary_blocked $op $id $canary $safe]
            dict set operations [dict get $op id] [_save [_working_preflight [dict merge $op $saved]]]
            return -code error -errorcode {VELES CANARY_BLOCKED} $safe
        }
    }
    set target [_target $op $id]
    if {[_state $op $id] ne "staged"} {
        return -code error "target $id must have a staged bundle before restart verification"
    }
    if {![dict exists $op reviews $id]} {
        return -code error "target $id has no retained signed release review"
    }
    set review [dict get $op reviews $id]
    set op [_step $op $id restarting {signed release installation confirmed and configured unit restart requested}]
    set op [::vfleetjournal::mark_install_started $op $id]
    set body [dict create \
        plan_id [::vapi::json_string [dict get $review plan_id]] \
        expected_manifest_sha256 [::vapi::json_string [dict get $review download manifest_sha256]] \
        confirm [::vapi::json_bool 1]]
    set installed [_call download-confirm $target $body]
    if {![_true [_field $installed ok]] ||
        [_field $installed action] ne "install-signed-bundle" ||
        [dict get $installed component name] ne "core" ||
        ![_true [_field $installed restart_required]]} {
        return -code error "signed installer returned an invalid installation result"
    }
    _call restart $target
    set health [_await_health $target]
    if {[dict get $health revision] ne [dict get $op release commit]} {
        return -code error "restarted target does not run the reviewed source revision"
    }
    return [_step $op $id healthy {restarted target is healthy and runs the reviewed source revision} [dict get $health revision]]
}

proc ::vfleetdeploy::continue {op} {
    set canary [dict get [lindex [dict get $op manifest targets] 0] node_id]
    if {[_state $op $canary] ne "healthy"} {
        return -code error "canary must be healthy before continuing"
    }
    # A previously healthy canary can have failed since verification;
    # never use its old result to admit another target.
    set canary_health [_fresh_health [_target $op $canary]]
    if {[_field $canary_health revision] ne [dict get $op release commit]} {
        return -code error "canary no longer runs the reviewed source revision"
    }
    set next ""
    foreach target [dict get $op manifest targets] {
        set id [dict get $target node_id]
        set state [_state $op $id]
        if {$state ni {healthy reviewed remaining completed}} {
            return -code error "target $id is not ready for continuation: $state"
        }
        if {$state in {reviewed remaining}} {
            _fresh_health $target
            if {$next eq ""} { set next $id }
        }
    }
    if {$next eq ""} {
        return [complete $op completed]
    }
    # The journal's legal transition for a not-yet-staged target is
    # reviewed -> staging. Health was revalidated above; staging performs
    # its own fresh check before asking for the signed preview.
    return [stage $op $next]
}

proc ::vfleetdeploy::rollback {op id} {
    set before [_state $op $id]
    if {$before in {reviewed remaining staging staged}} {
        return -code error "target $id was not installed; its rollback is a no-op"
    }
    if {![dict exists $op targets $id install_started] ||
        [dict get $op targets $id install_started] ni {1 true}} {
        return -code error "target $id has no recorded installation attempt; refusing to restart or restore it"
    }
    # A restore and a restart follow: the same proof as before an install.
    if {$before ni {rolled-back completed}} {
        verify_identity [_target $op $id]
    }
    if {[catch {_rollback $op $id} value options]} {
        if {$before ni {reviewed remaining staging staged restarting healthy failed rollback-preview rollback-staged rollback-restarting}} {
            return -code error -errorinfo [dict get $options -errorinfo] $value
        }
        _failed $op $id $value
        return -code error -errorinfo [dict get $options -errorinfo] \
            "deployment rollback failed for $id: $value"
    }
    return $value
}

proc ::vfleetdeploy::_verify_terminal_target {op id} {
    set target [_target $op $id]
    set health [_fresh_health $target]
    set running [_field $health revision]
    if {[dict exists $op targets $id running_revision] &&
        $running ne [dict get $op targets $id running_revision]} {
        return -code error "running revision changed since target verification"
    }
    if {[_state $op $id] eq "healthy" && $running ne [dict get $op release commit]} {
        return -code error "healthy target is not running the reviewed release"
    }
    if {[_state $op $id] eq "rolled-back"} {
        set expected [dict get $op preflight $id revision]
        if {$running ne $expected} {
            return -code error "rollback target is not running its recorded prior revision"
        }
    }
    set updated [::vfleetjournal::verify_target $op $id $running]
    return [_save [_working_preflight [dict merge $op $updated]]]
}

proc ::vfleetdeploy::complete {op {status completed}} {
    if {$status ni {completed rolled-back}} {
        return -code error "invalid terminal deployment status"
    }
    foreach target [dict get $op manifest targets] {
        set id [dict get $target node_id]
        set state [_state $op $id]
        if {$status eq "completed" && $state ne "healthy"} {
            return -code error "target $id is not healthy"
        }
        if {$status eq "rolled-back" && $state ni {healthy rolled-back reviewed remaining failed}} {
            return -code error "target $id is not accounted for in rollback"
        }
        if {$status eq "rolled-back" && [_untouched $op $id]} {
            # No install was attempted on this target; leave it unchanged.
        } elseif {$status eq "rolled-back" && $state eq "failed"} {
            return -code error "target $id is not accounted for in rollback"
        } else {
            set op [_verify_terminal_target $op $id]
        }
    }
    set final [::vfleetjournal::complete [dict get $op id] $status ::vfleetdeploy::_verify_terminal_target]
    return [_save [_working_preflight [dict merge $op $final]]]
}

# Untouched: no installation confirmation was ever sent to the target,
# and it is not holding a staged plan (a staged target must be explicitly
# finished or abandoned, never silently counted as rolled back).
proc ::vfleetdeploy::_untouched {op id} {
    if {[dict exists $op targets $id install_started] &&
        [dict get $op targets $id install_started] in {1 true}} {
        return 0
    }
    return [expr {[_state $op $id] in {reviewed remaining failed}}]
}

proc ::vfleetdeploy::finish_rollback {op} {
    foreach target [dict get $op manifest targets] {
        set id [dict get $target node_id]
        set state [_state $op $id]
        if {$state in {healthy rolled-back}} {
            # Already accounted for; no further action is needed.
        } elseif {[_untouched $op $id]} {
            # This target was never installed and is intentionally untouched
            # (including one whose staging failed before any installation).
        } else {
            return -code error "target $id is not accounted for in rollback: $state"
        }
    }
    return [complete $op rolled-back]
}

proc ::vfleetdeploy::_rollback {op id} {
    set target [_target $op $id]
    if {[_state $op $id] in {rolled-back completed}} {
        return -code error "target $id is already terminal"
    }
    if {[_state $op $id] eq "rollback-restarting" &&
        [dict exists $op targets $id recovery_observed] &&
        [dict get $op targets $id recovery_observed] in {1 true}} {
        set health [_fresh_health $target]
        set running [_field $health revision]
        set expected [dict get $op preflight $id revision]
        if {$running ne $expected} {
            return -code error "interrupted rollback target does not run its recorded prior source revision"
        }
        set op [::vfleetjournal::rollback_verified $op $id $running [regexp {^[a-f0-9]{40}$} $expected]]
        return [_step $op $id rolled-back {interrupted rollback reconciled by fresh health; no restore or restart replayed} $running]
    }
    if {[_state $op $id] in {reviewed remaining staging staged}} {
        return -code error "target $id was not installed; its rollback is a no-op"
    }
    if {![dict exists $op targets $id install_started] ||
        [dict get $op targets $id install_started] ni {1 true}} {
        return -code error "target $id has no recorded installation attempt; refusing to restart or restore it"
    }
    set before [dict get $op preflight $id revision]
    set exact [regexp {^[a-f0-9]{40}$} $before]
    set unknown [expr {$before in {unreported-development-build unreported-dirty-build unreported}}]
    if {!$exact && !$unknown} {
        return -code error "rollback has an invalid recorded prior source revision"
    }
    set op [_step $op $id rollback-preview {review retained prior core artifact}]
    set preview [_call rollback-preview $target [dict create name [::vapi::json_string core] confirm [::vapi::json_bool 0]]]
    if {![_true [_field $preview preview]] ||
        ![_true [_field $preview confirm_required]] ||
        [_field $preview component] ne "core" ||
        [string trim [_field $preview plan_sig]] eq "" ||
        ![regexp {^[a-f0-9]{64}$} [_field $preview previous_artifact_sha256]]} {
        return -code error "rollback preview is malformed or lacks its retained artifact digest"
    }
    if {![dict exists $op reviews $id previous_artifact_sha256]} {
        return -code error "this operation lacks its original core artifact digest; refusing rollback"
    }
    if {[_field $preview previous_artifact_sha256] ne [dict get $op reviews $id previous_artifact_sha256]} {
        return -code error "retained rollback artifact changed after this operation; refusing confirmation and restart"
    }
    set op [_step $op $id rollback-staged {retained prior core artifact selected}]
    set applied [_call rollback-confirm $target [dict create \
        name [::vapi::json_string core] confirm [::vapi::json_bool 1] \
        plan_sig [::vapi::json_string [dict get $preview plan_sig]]]]
    if {![_true [_field $applied ok]] || [_field $applied action] ne "rollback" ||
        [dict get $applied component name] ne "core"} {
        return -code error "rollback returned an invalid result"
    }
    set op [_step $op $id rollback-restarting {prior core restored; configured unit restart requested}]
    _call restart $target
    set health [_await_health $target]
    set running [_field $health revision unreported]
    if {$exact} {
        if {$running ne $before} {
            return -code error "rollback target is not running its recorded prior source revision"
        }
        set detail {prior core is healthy and runs the recorded prior source revision}
    } else {
        if {$running ni {unreported-development-build unreported-dirty-build unreported}} {
            return -code error "rollback target reports a source revision different from the recorded development state"
        }
        set detail {prior core restored and healthy; prior source revision was unreported and cannot be proven}
    }
    set op [::vfleetjournal::rollback_verified $op $id $running $exact]
    set op [_step $op $id rolled-back $detail $running]
    return [_save $op]
}
