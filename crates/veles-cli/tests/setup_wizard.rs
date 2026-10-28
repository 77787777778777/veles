//! `veles setup` end to end, as a first-run user drives it: the provider
//! menu, the API key, the model picked from the provider's own `/models`
//! list, then the written config. Answers are piped on stdin (the hidden
//! key prompt reads a plain line when stdin is not a terminal).
//!
//! Hermetic: a scratch VELES_HOME and a loopback "provider" in this
//! process that serves `/v1/models` and records the bearer it was given.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// Serve `/v1/models` until the test ends; remember each Authorization.
fn fake_provider() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut auth = String::new();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("authorization:") {
                    auth = v.trim().to_string();
                }
                line.clear();
            }
            log.lock().unwrap().push(auth);
            let body = r#"{"data":[{"id":"zeta-model"},{"id":"alpha-model"}]}"#;
            let mut s = stream;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, seen)
}

#[test]
fn a_first_run_picks_a_preset_and_a_listed_model() {
    let (port, seen) = fake_provider();
    let home = tempfile::tempdir().unwrap();
    // The menu's "Other OpenAI-compatible server" row, by its number.
    let other = veles_core::setup::PROVIDER_PRESETS
        .iter()
        .position(|p| p.choice == veles_core::setup::ProviderChoice::OpenAiCompatible)
        .unwrap()
        + 1;
    let answers = [
        other.to_string(),                     // provider
        format!("http://127.0.0.1:{port}/v1"), // base URL
        "MYSERVER_API_KEY".into(),             // the key's NAME
        "sk-wizard-test-0001".into(),          // the key (hidden prompt)
        "n".into(),                            // advanced options
        "1".into(),                            // model #1 of the sorted list
        "n".into(),                            // telegram
        String::new(),                         // IRC: skip
        "a".into(),                            // ACL: legacy open
        "y".into(),                            // write it
    ]
    .join("\n")
        + "\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_veles"))
        .arg("setup")
        .env("VELES_HOME", home.path())
        .env("HOME", home.path())
        .env_remove("MYSERVER_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("veles setup starts");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "setup failed:\n{out}");

    // The menu a stranger sees names real providers, not the owner's route.
    assert!(
        out.contains("OpenRouter") && out.contains("Anthropic"),
        "{out}"
    );
    assert!(!out.contains("recommended for this user"), "{out}");
    assert!(!out.contains("ali-token-plan-personal"), "{out}");

    // The list was fetched with the key the user just typed …
    assert_eq!(
        seen.lock().unwrap().first().map(String::as_str),
        Some("bearer sk-wizard-test-0001")
    );
    // … and model #1 is the SORTED first, so the pick used the list.
    let cfg = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(cfg.contains("default = \"alpha-model\""), "{cfg}");
    assert!(
        cfg.contains(&format!("base_url = \"http://127.0.0.1:{port}/v1\"")),
        "{cfg}"
    );
    assert!(cfg.contains("key_env = \"MYSERVER_API_KEY\""), "{cfg}");
    assert!(
        !cfg.contains("sk-wizard-test-0001"),
        "the key never lands in config.toml"
    );
}
