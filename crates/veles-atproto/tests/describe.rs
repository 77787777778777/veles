//! The actual process-plugin discovery boundary used by an installer.

#[test]
fn describe_process_exposes_the_config_surface() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_veles-plugin-atproto"))
        .arg("--describe")
        .output()
        .expect("run the real process plugin");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let m: veles_types::plugin::Manifest =
        serde_json::from_slice(&out.stdout).expect("--describe returns the manifest");
    assert_eq!(m.knobs, veles_atproto::config_surface());
    assert!(m
        .knobs
        .iter()
        .any(|k| k.path == "platforms.atproto.bot_handle"));
}
