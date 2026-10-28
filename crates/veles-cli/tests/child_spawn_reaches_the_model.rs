//! The argv the delegate builds, handed to the binary it actually spawns.
//!
//! Layer 2 of the pin in `main.rs`'s `argv_tests`: that one catches a
//! parser refusal, this one also catches a BOOT refusal — the child is
//! a real `veles` process, not a `Cli::try_parse_from`.
//!
//! WHY IT EXISTS. `delegate_task` spent five weeks dead: `-q` is
//! `--query`, it takes a value, and the builder put a flag there.
//! Nothing noticed, because every spawn test in `delegate.rs` points
//! `VELES_BINARY` at a shell script that records its own argv — which
//! proves what the PARENT intended and nothing at all about the child's
//! fate. Anything this program hands to another program needs one test
//! that hands it to the real program.
//!
//! HERMETIC. `VELES_HOME` is a scratch dir and `--resume` names a
//! session that does not exist, so the run stops immediately after the
//! parse and before any provider is contacted. No tokens, no network,
//! nothing written outside the scratch dir.

use std::process::Command;

fn scratch() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "veles-child-argv-{}-{:x}",
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
fn the_child_argv_gets_past_clap_and_into_the_binary() {
    let home = scratch();
    for child in [None, Some("nosuchsession")] {
        let argv =
            veles_core::delegate::child_argv("say hello", child, Some("some-model"), None, None);
        let out = Command::new(env!("CARGO_BIN_EXE_veles"))
            .args(&argv)
            .env("VELES_HOME", &home)
            // Depth 1 so the child cannot delegate further even if a
            // future change let this reach a provider by accident.
            .env("VELES_CHILD_DEPTH", "1")
            .output()
            .expect("spawn the real binary");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !err.contains("a value is required"),
            "clap refused the spawn argv {argv:?}: {err}"
        );
        assert_ne!(
            out.status.code(),
            Some(2),
            "exit 2 is a clap usage error — the child never ran: {argv:?} {err}"
        );
    }
    let _ = std::fs::remove_dir_all(&home);
}
