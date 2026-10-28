//! The module host (R3) against the real echo dylib, driven as child
//! processes of the `veles` binary.
//!
//! unix only: the driver (`veles module-host-test`, modulehosttest.rs)
//! reads `/proc/self/maps` and sends signals, and is `cfg(unix)` itself,
//! so on Windows every case here failed on "not available on this
//! platform" (2026-10-13). A Windows module-host test — the same cases
//! over `GetModuleInformation` — is PLAN-windows.md §10.
#![cfg(unix)]
//!
//! A cargo test binary links veles-core STATICALLY, so it has no
//! runtime to give a module (`module::check`'s `HostWithoutRuntime`
//! refusal, pinned in crates/veles-core/src/module.rs's own tests).
//! These tests therefore invoke `veles module-host-test <case>` — the
//! `veles` binary links `libveles_runtime.so` — and assert on its exit
//! and output. Each case's assertions run in the child, in module-host
//! code, against the real dylib.

use assert_cmd::Command;

/// Run one case in a fresh `veles` child; assert it prints OK and exits 0.
fn case(name: &str) {
    let out = Command::cargo_bin("veles")
        .expect("the veles binary")
        .args(["module-host-test", name])
        // The cases assert what the host does with a module's tracing
        // callsites (a module whose `info!` sites registered stays
        // mapped), and a callsite registers only at a level that lets
        // it through: the child's filter is pinned, not inherited from
        // whatever RUST_LOG the gate or the nix sandbox carries.
        .env("RUST_LOG", "info")
        // The irc module's node switch is `IRC_ENABLED` (its manifest's
        // `enabled_env`), and the child INHERITS the ambient env. An
        // operator whose .env sets `IRC_ENABLED=false` — and veles loads
        // .env into its own process env at boot, so this is any agent
        // session on that machine — makes the host stand the module by,
        // and `irc-module` fails on "off on this node" with the module
        // code never having run. Pinned for the same reason RUST_LOG is:
        // a case that asserts a module STARTS must not inherit a switch
        // that says it must not.
        .env("IRC_ENABLED", "true")
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{name} failed\nstatus: {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    assert!(
        stdout.contains(&format!("OK {name}")),
        "{name}: no OK line\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn a_module_loads_runs_and_unloads() {
    case("load-unload");
}

#[test]
fn a_stray_spawn_is_dropped_by_teardown() {
    case("stray-spawn");
}

#[test]
fn a_thread_that_never_yields_keeps_the_module_mapped() {
    case("spin-keeps-mapped");
}

#[test]
fn a_teardown_cut_short_by_the_gateways_stop_keeps_the_module_mapped() {
    case("quit-cut-short");
}

#[test]
fn the_gateways_stop_does_not_wait_on_a_standby_instance() {
    case("standby-quit");
}

#[test]
fn a_call_during_teardown_is_answered_reloading() {
    case("stale-call");
}

#[test]
fn a_module_panic_restarts_the_instance_once() {
    case("fault-restart");
}

#[test]
fn a_key_mismatch_is_refused_before_any_code_runs() {
    case("key-mismatch");
}

#[test]
fn rescan_starts_a_first_install() {
    case("rescan");
}

/// The conformance suite shared with the native tier (R8): the echo
/// module as the fixture.
#[test]
fn installer_module_selection_survives_upgrade_and_rollback() {
    case("installer-module-lifecycle");
}

#[test]
fn the_module_tier_passes_the_conformance_suite() {
    case("conformance");
}

/// R9: the script tier (the lang-js module hosting a TypeScript
/// fixture) under the same suite.
#[test]
fn the_js_script_tier_passes_the_conformance_suite() {
    case("conformance-js");
}

/// E2 (PLAN-extension-api.md): the shipped example override of
/// `failover.decide`, loaded by the real JS host, answers as it says; its
/// host's unload gives the decision back to core.
#[test]
fn the_failover_example_plugin_decides_as_documented() {
    case("failover-example");
}

/// R9: the Racket script tier (the lang-racket module booting Racket CS
/// and hosting a Racket fixture) under the same suite; the host stays
/// mapped after its unload, by design.
#[test]
#[cfg(unix)]
fn the_racket_script_tier_passes_the_conformance_suite() {
    case("conformance-racket");
}

/// R9: the Common Lisp script tier (the lang-lisp module booting ECL in
/// this process) under the same suite, then the module's unload taking
/// the script's registrations away while ECL stays booted.
#[test]
#[cfg(unix)]
fn the_lisp_script_tier_passes_the_conformance_suite() {
    case("conformance-lisp");
}
#[test]
fn the_irc_module_connects_registers_and_unloads() {
    case("irc-module");
}

/// R5: the slopdrop module fills core's seams on start and the host's
/// purge empties them on stop; its data tree moves once with a backup.
#[test]
fn the_slopdrop_module_fills_the_seams_and_leaves_them_empty() {
    case("slopdrop-module");
}

/// The /atproto module registers its direct command on start and the
/// host's purge removes it on stop; a verify-first delete runs through
/// the module against stub repo services.
#[test]
fn the_atproto_cmd_module_registers_and_purges_its_command() {
    case("atproto-cmd-module");
}

/// R6: the Telegram module polls a stub Bot API, claims its route, ends
/// its long poll on stop through the dispatcher's token, and spawns no
/// instance on a node without the token.
#[test]
fn the_telegram_module_polls_claims_its_route_and_stops_its_poll() {
    case("telegram-module");
}

/// The gateway's signal handlers are in before any module's code runs
/// (a module's runtime boot puts back what it found).
#[test]
fn the_gateways_signal_handlers_are_pinned_before_modules_start() {
    case("signal-pin");
}
