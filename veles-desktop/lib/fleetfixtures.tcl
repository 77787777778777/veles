# veles-desktop/lib/fleetfixtures.tcl — gateway response bodies for the
# fleet deployment suites, as the WIRE carries them.
#
# Captured 2026-11-06 from a throwaway gateway (debug build of e4301c08
# plus this change's installer-state `current_artifacts`, fresh
# VELES_HOME, VELES_DESKTOP_OPEN, loopback port 19167, no cluster config;
# every key path and value type re-compared with jq afterwards): GET
# /api/health, GET /api/fleet, GET /api/install/state and the signed
# download cancel reply. Only identifying values were replaced (the
# machine's hostname, its GPU name, its config paths); every key, every
# null, every boolean and every nesting level is the gateway's own.
#
# WHY. The F1 review (F1-01, F1-02, F1-08, FLEET-1) found that every
# suite passed while the real dialog could not deploy anything: the
# stubs returned /api/health as a Tcl dict (the real call returns JSON
# text) and /api/fleet `nodes` as an id-keyed map (the gateway sends an
# ARRAY). A fixture that cannot produce the wire's shape tests the
# fixture (jsonfield.tcl says the same about nulls). These are text; a
# suite parses them with the same ::json::json2dict the client uses.

namespace eval ::fleetfix {}

proc ::fleetfix::health_json {{revision e4301c085dabea639c44bf0258770e50662539cf} {version 0.1.6} {status ok}} {
    return [string map [list @REV@ $revision @VER@ $version @STATUS@ $status] \
        {{"profile":"default","revision":"@REV@","status":"@STATUS@","uptime_s":4,"version":"@VER@"}}]
}

# One captured node object; `nodes` lists {id name version stale self}.
proc ::fleetfix::node_json {id name version stale self} {
    return [string map [list @ID@ $id @NAME@ $name @VER@ $version @STALE@ $stale @SELF@ $self] \
        {{"capabilities":{"os":"linux","tools":["bash","sh","git","rg","jq","curl","ssh","sqlite3","tclsh"]},"endpoint":null,"gpu":{"memory_total_mb":16384,"name":"fixture GPU"},"heartbeat_at":"2026-11-06 13:37:38","host":"@NAME@-host","id":"@ID@","load1":null,"load_at":null,"name":"@NAME@","registered_at":"2026-11-06 13:37:38","replica":null,"self":@SELF@,"sessions_served":0,"stale":@STALE@,"static_ip":null,"static_ip_source":null,"sync_peer":null,"uptime_class":"ephemeral","version":"@VER@"}}]
}

proc ::fleetfix::fleet_json {nodes} {
    set rows {}
    foreach node $nodes {
        lassign $node id name version stale self
        if {$stale eq ""} { set stale false }
        if {$self eq ""} { set self false }
        lappend rows [node_json $id $name $version $stale $self]
    }
    return "\{\"nodes\":\[[join $rows ,]\],\"stale_after_secs\":300,\"sync\":\{\"config_auto\":true,\"config_waiting\":0,\"gossip_peers\":\[\],\"memory_auto\":false,\"memory_peers\":\[\],\"store_auto\":false,\"store_peers\":\[\]\}\}"
}

# /api/install/state of a node whose core is NOT installer-managed (the
# captured body). `managed_digest` adds the `current_artifacts` map the
# installer state reports for an installed core (service.rs state();
# its digest equals the post-install previous_artifact_sha256, which the
# veles-install service test asserts).
proc ::fleetfix::install_state_json {{managed_digest ""}} {
    set artifacts "\{\}"
    if {$managed_digest ne ""} { set artifacts "\{\"core\":\"$managed_digest\"\}" }
    return [string map [list @ART@ $artifacts] \
        {{"available":[],"capabilities":["reviewed-local-bundle","component-config","component-install","verify","rollback","named-profiles","cli-reviewed-system-handoff","cli-reviewed-container-host","signed-download-bootstrap"],"config_path":"/fixture/config.toml","configured_only":[],"current_artifacts":@ART@,"history":[],"install_root":"/fixture/install","installed":[],"profile":"workstation","record_path":"/fixture/install.toml","schema":1,"unsupported":["http-privileged-execution","nixos-apply","service-activation","container-application-components"]}}]
}

proc ::fleetfix::cancel_json {} {
    return {{"cancelled":true,"discarded":false,"ok":true}}
}

# ── The command center's observation bodies (lib/fleetobs.tcl) ─────
# Captured 2026-11-07 from a throwaway pair of gateways (debug build of
# this change, scratch VELES_HOME each, loopback ports 19341-19344, each
# the other's gossip and memsync peer; `[notify] fleet_alert_debounce_secs
# = 0`, `[cluster] stale_after_secs = 30`). Node a's desktop API answered:
# GET /api/fleet, POST /api/fleet/metrics|logs|drift|gossip for node b.
# Three registry rows were seeded into both stores before the capture: a
# "phone" on 0.1.6 (fresh, no [a2a_agents] entry, so drift cannot ask it),
# a "ghost" that went stale during the run (its alert), and a "retired"
# one, forgotten through /api/fleet/forget (its tombstone). Replaced: the
# machine's hostname, the GPU name and the data-home path; the log answer
# keeps six of its lines (b's boot line carries a declared secret, which
# b redacted). Every key, null, boolean, number type and nesting level is
# the gateway's own.
namespace eval ::fleetfix { variable obs }
array set ::fleetfix::obs {}
set ::fleetfix::obs(fleet) {{"alerts":{"debounce_secs":0,"enabled":false,"recent":[{"at":"2026-11-07T00:37:23.716484999+00:00","detail":"no heartbeat for 1 min","from":"fresh","name":"ghost","node":"ghost-node-0002","note":"not sent: [notify] fleet_alerts is off","sent":false,"text":"veles fleet: node ghost is stale — no heartbeat for 1 min","to":"stale"}]},"forgotten":[{"forgotten_at":"2026-11-07 00:37:42","id":"retired-node-0003"}],"nodes":[{"capabilities":null,"endpoint":null,"gpu":null,"health":{"failing":[],"heartbeat_age_secs":58,"probe":null,"state":"stale"},"heartbeat_at":"2026-11-07 00:36:51","host":"ghost-host","id":"ghost-node-0002","load1":null,"load_at":null,"name":"ghost","registered_at":"2026-10-01 00:00:00","replica":null,"self":false,"sessions_served":0,"stale":true,"static_ip":null,"static_ip_source":null,"sync_kinds":[],"sync_peer":null,"sync_status":{},"uptime_class":"ephemeral","version":"0.2.0"},{"capabilities":null,"endpoint":null,"gpu":null,"health":{"failing":[],"heartbeat_age_secs":0,"probe":null,"state":"fresh"},"heartbeat_at":"2026-11-07 01:37:11","host":"phone-host","id":"phone-node-0001","load1":null,"load_at":null,"name":"phone","registered_at":"2026-10-01 00:00:00","replica":null,"self":false,"sessions_served":0,"stale":false,"static_ip":null,"static_ip_source":null,"sync_kinds":[],"sync_peer":null,"sync_status":{},"uptime_class":"ephemeral","version":"0.1.6"},{"capabilities":{"os":"linux","tools":["bash","sh","git","rg","jq","curl","ssh","sqlite3","tclsh","podman","systemd-nspawn","firecracker","agent-browser","xclip","wl-paste","python3","node"]},"endpoint":null,"gpu":{"memory_total_mb":16384,"name":"fixture GPU"},"health":{"failing":[],"heartbeat_age_secs":5,"probe":null,"state":"fresh"},"heartbeat_at":"2026-11-07 00:37:44","host":"fixture-host","id":"90f004e1-ecc7-425d-a170-f7502f09b0ee","load1":31,"load_at":"2026-11-07 00:37:44","name":"b","registered_at":"2026-11-07 00:37:07","replica":null,"self":false,"sessions_served":0,"stale":false,"static_ip":null,"static_ip_source":null,"sync_kinds":["config","memory","vault"],"sync_peer":null,"sync_status":{"config":{"auto":true,"backoff":null,"behind":6,"cursor":5,"dialable":true,"last":{"at":"2026-11-07T00:37:11.047071205+00:00","line":"stored 5 row(s) across 5 key(s), since seq 0 as 01794011829870204966-task-ac3a13a66a2640e38ca25402f702a8c1.json — applied (1 offer(s))","ok":true},"local":11,"peer":"b"},"memory":{"auto":false,"backoff":null,"behind":0,"cursor":0,"dialable":true,"last":null,"local":0,"peer":"b"},"vault":{"auto":true,"backoff":null,"current":false,"dialable":true,"has_vault":false,"last":null,"peer":"b"}},"uptime_class":"ephemeral","version":"0.2.0"},{"capabilities":{"os":"linux","tools":["bash","sh","git","rg","jq","curl","ssh","sqlite3","tclsh","podman","systemd-nspawn","firecracker","agent-browser","xclip","wl-paste","python3","node"]},"endpoint":null,"gpu":{"memory_total_mb":16384,"name":"fixture GPU"},"health":{"failing":[],"heartbeat_age_secs":5,"probe":null,"state":"self"},"heartbeat_at":"2026-11-07 00:37:44","host":"fixture-host","id":"dc70a853-e8ff-472d-8c17-27bf504a8f18","load1":32,"load_at":"2026-11-07 00:37:44","name":"a","registered_at":"2026-11-07 00:37:07","replica":null,"self":true,"sessions_served":0,"stale":false,"static_ip":null,"static_ip_source":null,"sync_kinds":[],"sync_peer":null,"sync_status":{},"uptime_class":"ephemeral","version":"0.2.0"}],"stale_after_secs":30,"sync":{"config_auto":true,"config_waiting":0,"gossip":[{"dialable":true,"last":{"at":"2026-11-07T00:37:44.112439401+00:00","line":"b: merged 4 row(s), sent 1 tombstone(s); we appear as 127.0.0.1","ok":true},"peer":"b"}],"gossip_peers":["b"],"inbox":{"config":0,"memory":0,"store":0,"vault":0},"memory_auto":false,"memory_peers":["b"],"store_auto":false,"store_peers":[]},"versions":{"drift":true,"groups":[{"nodes":[{"id":"ghost-node-0002","name":"ghost","stale":true},{"id":"90f004e1-ecc7-425d-a170-f7502f09b0ee","name":"b","stale":false},{"id":"dc70a853-e8ff-472d-8c17-27bf504a8f18","name":"a","stale":false}],"version":"0.2.0"},{"nodes":[{"id":"phone-node-0001","name":"phone","stale":false}],"version":"0.1.6"}]}}}
set ::fleetfix::obs(metrics) {{"arch":"x86_64","cpu":{"cores":16,"load1":5.19,"load15":6.32,"load1_pct":32,"load5":5.41},"disk":{"free_bytes":153097949184,"path":"/fixture/veles-home","total_bytes":1006708895744,"used_pct":84.8},"gateway_uptime_secs":33,"host_uptime_secs":139064,"in_flight":{"tasks":{},"tasks_total":0,"turns":{},"turns_total":0},"io_pressure":{"full_avg10":37.27,"full_avg300":46.51,"full_avg60":41.35,"some_avg10":38.09,"some_avg300":47.83,"some_avg60":42.46},"memory":{"available_bytes":26569830400,"swap_free_bytes":24661950464,"swap_total_bytes":26429145088,"total_bytes":33036902400,"used_pct":19.6},"node":{"id":"90f004e1-ecc7-425d-a170-f7502f09b0ee","name":"b"},"op":"metrics","os":"linux","profile":"default","revision":"unreported-dirty-build","sampled_at":"2026-11-07T00:37:42.421619445+00:00","unavailable":[],"version":"0.2.0","via":"b"}}
set ::fleetfix::obs(logs) {{"capacity":500,"held":23,"level":"all","lines":[{"level":"INFO","msg":"cluster: registered node 90f004e1-ecc7-425d-a170-f7502f09b0ee (b) [pre-fence]","target":"veles_cli","time":"01:37:08.759"},{"level":"INFO","msg":"home chat: e2esecret:FLEET_CAPTURE_SECRET=<REDACTED>","target":"veles_frontends::gateway","time":"01:37:08.761"},{"level":"WARN","msg":"cron scheduler on STANDBY: the fence cannot arm yet — [cluster] arbiter is not set — no node may run the singletons (cron, telegram, IRC) until one is named. This is the fail-closed default; set it on the always-on node's id. (retrying every 30s; a new node learns the arbiter by gossip)","target":"veles_frontends::gateway","time":"01:37:08.761"},{"level":"INFO","msg":"cluster: registered node 90f004e1-ecc7-425d-a170-f7502f09b0ee (b)","target":"veles_frontends::gateway","time":"01:37:08.973"},{"level":"WARN","msg":"store lock: a2a::gossip held it 194ms","target":"veles_core::store","time":"01:37:14.610"},{"level":"WARN","msg":"store lock: gossip::exchange_reply held it 115ms","target":"veles_core::store","time":"01:37:14.727"}],"node":{"id":"90f004e1-ecc7-425d-a170-f7502f09b0ee","name":"b"},"op":"logs","redacted":true,"via":"b"}}
set ::fleetfix::obs(drift) {{"checked_at":"2026-11-07T00:37:42.507807540+00:00","differing":0,"nodes":[{"asked":true,"config":{"comparable":true,"keys":{"differ":[],"only_here":[],"only_there":[]},"keys_error":null,"log":{"here_ahead":[],"there_ahead":[]},"log_same":true,"same":true},"error":null,"id":"90f004e1-ecc7-425d-a170-f7502f09b0ee","name":"b","registry_version":"0.2.0","revision":"unreported-dirty-build","revision_differs":false,"syncs_config":true,"version":"0.2.0","version_differs":false},{"asked":false,"error":"stale: not asked","id":"ghost-node-0002","name":"ghost","registry_version":"0.2.0","syncs_config":false},{"asked":true,"error":"no [a2a_agents] entry names phone; its registry endpoint is not dialed","id":"phone-node-0001","name":"phone","registry_version":"0.1.6","syncs_config":false}],"self":{"config_keys":6,"config_rows":11,"id":"dc70a853-e8ff-472d-8c17-27bf504a8f18","revision":"unreported-dirty-build","version":"0.2.0"},"versions":{"drift":true,"groups":[{"nodes":[{"id":"ghost-node-0002","name":"ghost","stale":true},{"id":"90f004e1-ecc7-425d-a170-f7502f09b0ee","name":"b","stale":false},{"id":"dc70a853-e8ff-472d-8c17-27bf504a8f18","name":"a","stale":false}],"version":"0.2.0"},{"nodes":[{"id":"phone-node-0001","name":"phone","stale":false}],"version":"0.1.6"}]}}}
set ::fleetfix::obs(gossip) {{"ok":1,"result":"gossip: 1 of 1 peer(s) answered","results":[{"line":"b: merged 4 row(s), sent 1 tombstone(s); we appear as 127.0.0.1","ok":true,"peer":"b"}]}}

proc ::fleetfix::obs_fleet_json {} { return $::fleetfix::obs(fleet) }
proc ::fleetfix::obs_metrics_json {} { return $::fleetfix::obs(metrics) }
proc ::fleetfix::obs_logs_json {} { return $::fleetfix::obs(logs) }
proc ::fleetfix::obs_drift_json {} { return $::fleetfix::obs(drift) }
proc ::fleetfix::obs_gossip_json {} { return $::fleetfix::obs(gossip) }

# ── A loopback fake gateway for the end-to-end worker suite ───────────
# Serves the captured bodies above over real HTTP so the worker's real
# ::vapi transport, JSON parsing and ssh restart are what is exercised.
# State lives in ::fleetfix::gw: `revision` (what /api/health reports),
# `restarted_marker` (a file the PATH-shimmed ssh creates; once it
# exists, health reports `after_revision`), `digest` (current core),
# `pending` (the single pending signed plan id, as desktop_install.rs
# keeps it), `log` (method path pairs), `self_id` (when set, GET
# /api/fleet answers as THAT node: one self row, the shape the deploy's
# identity proof reads; unset, the captured observation body). The
# signed preview and confirm bodies carry the fields desktop_install.rs
# adds to the installer's review; they cannot be captured without a
# published signed release.
namespace eval ::fleetfix {
    variable gw
    array set gw {self_id "" revision "" after_revision "" restarted_marker "" digest "" pending "" log {} release {} bodies {}}
}

proc ::fleetfix::serve {} {
    set sock [socket -server ::fleetfix::_accept -myaddr 127.0.0.1 0]
    return [list $sock [lindex [fconfigure $sock -sockname] 2]]
}

# Non-blocking: the client may be this same process (a synchronous
# ::vapi call waits in the event loop), so the server must never block
# reading a request that has not been written yet.
proc ::fleetfix::_accept {chan addr port} {
    variable inbuf
    set inbuf($chan) ""
    fconfigure $chan -blocking 0 -translation binary
    fileevent $chan readable [list ::fleetfix::_read $chan]
}

proc ::fleetfix::_read {chan} {
    variable inbuf
    append inbuf($chan) [read $chan]
    set raw $inbuf($chan)
    set end [string first "\r\n\r\n" $raw]
    if {$end < 0} {
        if {[eof $chan]} { unset inbuf($chan); close $chan }
        return
    }
    set head [string range $raw 0 [expr {$end - 1}]]
    set length 0
    regexp -nocase {content-length:\s*(\d+)} $head -> length
    set body [string range $raw [expr {$end + 4}] end]
    if {[string length $body] < $length && ![eof $chan]} { return }
    fileevent $chan readable {}
    unset inbuf($chan)
    lassign [split [lindex [split $head \n] 0] " "] method path
    _respond $chan $method $path [encoding convertfrom utf-8 $body]
}

proc ::fleetfix::_respond {chan method path body} {
    fconfigure $chan -blocking 1
    if {[catch {_route $method $path $body} reply]} {
        set reply [list 500 "{\"ok\":false,\"error\":\"fake gateway failure\"}"]
    }
    lassign $reply code text
    set bytes [encoding convertto utf-8 $text]
    puts -nonewline $chan "HTTP/1.1 $code X\r\nContent-Type: application/json\r\nContent-Length: [string length $bytes]\r\nConnection: close\r\n\r\n"
    puts -nonewline $chan $bytes
    close $chan
}

proc ::fleetfix::_route {method path body} {
    variable gw
    lappend gw(log) $method $path
    set request {}
    if {$body ne ""} { set request [::json::json2dict $body] }
    switch -- "$method $path" {
        "GET /api/fleet" {
            if {$gw(self_id) ne ""} {
                return [list 200 [fleet_json [list [list $gw(self_id) self 0.1.6 false true]]]]
            }
            return [list 200 [obs_fleet_json]]
        }
        "POST /api/fleet/metrics" { lappend gw(bodies) $request ; return [list 200 [obs_metrics_json]] }
        "POST /api/fleet/logs" { lappend gw(bodies) $request ; return [list 200 [obs_logs_json]] }
        "POST /api/fleet/drift" { lappend gw(bodies) $request ; return [list 200 [obs_drift_json]] }
        "POST /api/fleet/gossip" { lappend gw(bodies) $request ; return [list 200 [obs_gossip_json]] }
        "GET /api/health" {
            set revision $gw(revision)
            if {$gw(restarted_marker) ne "" && [file exists $gw(restarted_marker)]} {
                set revision $gw(after_revision)
            }
            return [list 200 [health_json $revision]]
        }
        "GET /api/install/state" {
            return [list 200 [install_state_json $gw(digest)]]
        }
        "POST /api/install/download" {
            if {$gw(pending) ne ""} {
                return [list 409 {{"ok":false,"error":"a signed-download review is still awaiting confirmation or cancellation"}}]
            }
            set gw(pending) [dict get $request request_id]
            set r $gw(release)
            return [list 200 [format {{"action":"install-signed-bundle","confirm_required":true,"download":{"archive_sha256":"%s","manifest_sha256":"%s","repository":"%s","tag":"%s","commit":"%s","workflow_sha":"%s"},"plan_id":"%s","preview":true,"review_expires_seconds":3600,"verification":"fixture cosign verification"}} \
                [string repeat e 64] [string repeat f 64] [dict get $r repository] [dict get $r tag] \
                [dict get $r commit] [dict get $r workflow_sha] $gw(pending)]]
        }
        "POST /api/install/download/confirm" {
            if {[dict get $request confirm] in {true 1}} {
                if {$gw(pending) ne [dict get $request plan_id]} {
                    return [list 409 {{"ok":false,"error":"signed-download preview is missing, cancelled, expired or already used; review a fresh download"}}]
                }
                set gw(pending) ""
                return [list 200 {{"action":"install-signed-bundle","component":{"name":"core","version":"0.1.7"},"ok":true,"restart_required":true}}]
            }
            if {$gw(pending) eq [dict get $request plan_id]} { set gw(pending) "" }
            return [list 200 [cancel_json]]
        }
    }
    # A suite may answer more routes (fleetmanagetest: the node-management
    # routes, with bodies captured from a real gateway).
    if {[llength [info commands ::fleetfix::manage_route]]} {
        set reply [manage_route $method $path $request]
        if {$reply ne ""} { return $reply }
    }
    return [list 404 {{"ok":false,"error":"not found"}}]
}
