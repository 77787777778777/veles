//! The zai-MCP incident, pinned (2026-10-02/03).
//!
//! WHAT HAPPENED. `~/.hermes/config.yaml` carried three Z.AI MCP
//! servers with hermes' own `enabled: false` on them. veles'
//! `config::load()` falls back to that file when it finds no
//! config.toml of its own — and `hermes_config_yaml()` resolved
//! through the REAL `$HOME`, ignoring `VELES_HOME`. The e2e tests in
//! this directory boot a real child with a scratch `VELES_HOME` that
//! has no config.toml, so the child loaded the developer's live hermes
//! config; veles' `McpServerConfig` parser had no `enabled` key, so
//! "disabled" read as "on", and every test run spawned
//! `npx -y @z_ai/mcp-server@latest` — ten times on 2026-10-02 alone,
//! against a vendor the user does not trust ("I don't want it even
//! installed").
//!
//! THIS PIN. A child shaped exactly like the incident: scratch
//! `VELES_HOME` (no config.toml), a FAKE `$HOME` holding a hermes
//! config.yaml with a stdio MCP server named `npx`, and a stub `npx`
//! first on PATH that logs any spawn. The child must boot WITHOUT
//! touching the stub. Two independent fixes make that true. This pin
//! covers the first — a forced `VELES_HOME` never reads the ambient
//! `~/.hermes` (also pinned at the source,
//! `config::a_forced_home_never_falls_back…`); the second, hermes'
//! `enabled: false` honoured, is pinned at the parser
//! (`mcp::a_disabled_entry_never_connects`) because with the first in
//! place no hermes entry reaches the parser at all.
//!
//! HERMETIC. No network is needed: the stub npx exits 3 immediately,
//! and the fake config's model route fails before any provider call
//! (no key, no reachable base_url). Nothing is written outside the
//! scratch dirs.

use std::process::Command;

fn scratch(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "veles-zai-{}-{}-{:x}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_scratch_child_never_boots_the_hermes_compat_mcp_servers() {
    // The fake developer install: a hermes config.yaml whose MCP
    // entries are DISABLED hermes-side — the exact shape that leaked
    // (zai-vision was `enabled: false`, `command: npx`), plus a
    // `leak-canary` with no `enabled` key that WOULD spawn if the file
    // were read. The model + provider block makes the file a complete
    // config: were it read, the child would set its provider from it,
    // boot MCP (the canary spawns) and die at the dead route
    // (127.0.0.1:0 — instant refusal, no network, no tokens).
    let home = scratch("home");
    std::fs::create_dir_all(home.join(".hermes")).unwrap();
    std::fs::write(
        home.join(".hermes").join("config.yaml"),
        "model:\n\
         \x20 provider: custom:fake\n\
         \x20 default: fake-model\n\
         custom_providers:\n\
         \x20 - name: fake\n\
         \x20   base_url: http://127.0.0.1:0/v1\n\
         \x20   key_env: NO_SUCH_KEY_ANYWHERE\n\
         \x20   model: fake-model\n\
         mcp_servers:\n\
         \x20 zai-vision:\n\
         \x20   enabled: false\n\
         \x20   command: npx\n\
         \x20   args: [\"-y\", \"@z_ai/mcp-server@latest\"]\n\
         \x20 leak-canary:\n\
         \x20   command: npx\n\
         \x20   args: [\"-y\", \"canary\"]\n\
         \x20   connect_timeout: 3\n",
    )
    .unwrap();

    // The stub npx: logs any spawn attempt, then fails like a server
    // that should never have been started.
    let bin = scratch("bin");
    let hits = scratch("hits").join("hits.log");
    let stub = bin.join("npx");
    std::fs::write(
        &stub,
        format!(
            "#!/usr/bin/env bash\n\
             {{ echo \"=== $(date -Is) STUB SPAWNED\"; printf 'args:'; printf ' %q' \"$@\"; echo; }} >> {}\n\
             exit 3\n",
            hits.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // The child's world: VELES_HOME with NO config.toml (the trigger
    // for the compat fallback), the fake HOME, and the stub first on
    // PATH.
    let vhome = scratch("vhome");
    let path = std::env::var("PATH").unwrap_or_default();
    let argv = veles_core::delegate::child_argv("say hello", None, Some("some-model"), None, None);
    let out = Command::new(env!("CARGO_BIN_EXE_veles"))
        .args(&argv)
        .env("VELES_HOME", &vhome)
        .env("HOME", &home)
        .env("PATH", format!("{}:{}", bin.display(), path))
        .env("VELES_CHILD_DEPTH", "1")
        .output()
        .expect("spawn the real binary");

    // The run is allowed to fail; what is not allowed is a spawn. The
    // failure itself is the proof the ambient file was never read: the
    // child has `-m` but no provider of its own, so it dies with "no
    // model.provider in config" — had it read the fake hermes config,
    // its provider would be `custom:fake` and it would die at the dead
    // route instead. (The old guard accepted any stderr naming
    // "model", which that very message satisfies — it could not tell
    // the two apart.)
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no model.provider in config"),
        "the child took its provider from somewhere — the ambient hermes \
         config was read. stderr: {err}"
    );
    let spawned = std::fs::read_to_string(&hits).unwrap_or_default();
    assert!(
        spawned.is_empty(),
        "the child booted MCP servers out of the ambient hermes config:\n{spawned}\n\
         child stderr: {err}"
    );

    for d in [&home, &bin, &vhome] {
        let _ = std::fs::remove_dir_all(d);
    }
    let _ = std::fs::remove_dir_all(hits.parent().unwrap());
}
