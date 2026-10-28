//! Named profiles across real process boundaries.
//!
//! Two gateway processes share one XDG root: the ordinary default and an
//! isolated named profile. Nothing reads or writes the operator's home.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tempdir_unique() -> PathBuf {
    std::env::temp_dir().join(format!(
        "veles-profile-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http(port: u16, path: &str, token: &str, body: &str) -> (u16, String) {
    let Ok(mut s) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(3),
    ) else {
        return (0, String::new());
    };
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let req = format!(
        "POST {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(req.as_bytes());
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let code = out
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (code, out.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}

fn health(port: u16) -> Option<serde_json::Value> {
    let mut s = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    )
    .ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    s.write_all(b"GET /api/health HTTP/1.0\r\nHost: x\r\n\r\n")
        .ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    serde_json::from_str(out.split("\r\n\r\n").nth(1)?).ok()
}

fn health_matches(port: u16, profile: &str) -> bool {
    let Some(body) = health(port) else {
        return false;
    };
    match body["profile"].as_str() {
        Some(found) => found == profile,
        // The fixture spawns this exact binary. Legacy compatibility is
        // a launcher concern, not permission to adopt an unlabelled
        // process during a profile-isolation test.
        None => false,
    }
}

fn wait_for(port: u16, profile: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if health_matches(port, profile) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("gateway for {profile} did not come up on :{port}");
}

fn command(root: &Path, profile: Option<&str>) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_veles"));
    c.env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env_remove("VELES_HOME")
        .env_remove("VELES_TOKEN")
        .env_remove("DESKTOP_BEARER_TOKEN")
        .env_remove("VELES_DESKTOP_OPEN")
        .env_remove("VELES_SESSION_PRINCIPAL")
        .env_remove("A2A_HOST")
        .env_remove("A2A_PORT")
        .env_remove("A2A_PEER_TOKENS")
        .env_remove("A2A_BEARER_TOKEN")
        .env_remove("VELES_API")
        .env_remove("VELES_INSTANCE")
        .env_remove("VELES_DESKTOP_PORT")
        .stdin(Stdio::null());
    if let Some(name) = profile {
        c.arg("--profile").arg(name);
    } else {
        c.env_remove("VELES_PROFILE");
    }
    c
}

fn gateway(root: &Path, profile: Option<&str>, port: u16) -> Proc {
    let name = profile.unwrap_or("default");
    let log = root.join(format!("{name}.log"));
    Proc(
        command(root, profile)
            .args(["gateway", "run"])
            .env("VELES_DESKTOP_PORT", port.to_string())
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(std::fs::File::create(root.join(format!("{name}.err"))).unwrap())
            .spawn()
            .expect("spawn isolated gateway"),
    )
}

fn output(root: &Path, profile: Option<&str>, args: &[&str]) -> (bool, String) {
    let out = command(root, profile)
        .args(args)
        .output()
        .expect("run veles");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Pass a test path as Tcl data, and compare Tcl's path representation
/// rather than assuming Unix separators or a particular drive-letter case.
fn tcl_literal(path: &Path) -> String {
    format!("{{{}}}", path.to_string_lossy())
}

fn tcl_path_condition(code: &str, expected: &str) -> String {
    let comparison = if cfg!(windows) {
        "string equal -nocase"
    } else {
        "string equal"
    };
    format!("[{comparison} [string map {{\\\\ /}} [file normalize {code}]] [string map {{\\\\ /}} [file normalize {expected}]]]")
}

fn tcl_path_equals(code: &str, expected: &str) -> String {
    format!("expr {{ {} }}", tcl_path_condition(code, expected))
}

fn final_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|line| !line.is_empty())
}

#[test]
fn named_profiles_isolate_two_real_gateways_and_tcl() {
    let root = tempdir_unique();
    let _cleanup = Cleanup(root.clone());
    std::fs::create_dir_all(root.join("home")).unwrap();
    let default = root.join("data/veles");
    let default_config = root.join("config/veles");
    let named = default.join("profiles/laptop-test");
    put(
        &default_config.join("config.toml"),
        "[model]\ndefault = \"default-model\"\n",
    );
    put(
        &named.join("config.toml"),
        "[model]\ndefault = \"named-model\"\n",
    );
    put(
        &default_config.join(".env"),
        "DESKTOP_BEARER_TOKEN=default-token\n",
    );
    put(&named.join(".env"), "DESKTOP_BEARER_TOKEN=named-token\n");
    put(
        &named.join("plugins/profile-only/plugin.toml"),
        "name = \"profile-only\"\nkind = \"tcl\"\nentry = \"plugin.tcl\"\n",
    );
    put(
        &named.join("plugins/profile-only/plugin.tcl"),
        "proc ::profile_probe {} { list $::veles_config_dir $::veles_data_dir $::veles_cache_dir $::veles_profile [::veles::config_get model.default] [::veles::sessions] }\n",
    );
    put(
        &default_config.join("skills/default-only/SKILL.md"),
        "---\nname: default-only\ndescription: not for the named profile\n---\nDefault only.\n",
    );
    put(
        &named.join("skills/isolated/SKILL.md"),
        "---\nname: isolated\ndescription: only this named profile\n---\nNever inherit the default.\n",
    );

    let default_port = free_port();
    // The named port is genuinely chosen by profiles::default_desktop_port,
    // without a VELES_DESKTOP_PORT environment override. Bind the default
    // beside it so the launch path must not adopt the default gateway.
    let named_port = 10000
        + ("laptop-test"
            .bytes()
            .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619))
            % 50000) as u16;
    let _named_gw = command(&root, Some("laptop-test"))
        .args(["gateway", "run"])
        .stdout(std::fs::File::create(root.join("laptop-test.log")).unwrap())
        .stderr(std::fs::File::create(root.join("laptop-test.err")).unwrap())
        .spawn()
        .map(Proc)
        .expect("spawn named-profile gateway on its default port");
    let _default_gw = gateway(&root, None, default_port);
    wait_for(default_port, "default");
    wait_for(named_port, "laptop-test");
    assert!(default.join("state.db").is_file());
    assert!(named.join("state.db").is_file());
    // Cache creation is lazy. The live Tcl assertion below pins the
    // selected profile's cache path rather than inventing cache work.
    assert!(!root.join("cache/veles/profiles/laptop-test").exists());
    assert!(!default
        .join("profiles/laptop-test/profiles/laptop-test")
        .exists());
    assert_eq!(health(named_port).unwrap()["profile"], "laptop-test");
    let (code, body) = http(
        named_port,
        "/api/sessions/new",
        "named-token",
        r#"{"title":"named-profile-only"}"#,
    );
    assert_eq!(code, 201, "{body}");

    // The wrong profile's credential is not accepted by this process.
    let (code, _) = http(
        named_port,
        "/api/tcl/eval",
        "default-token",
        r#"{"code":"set ok 1"}"#,
    );
    assert_eq!(code, 401);
    let (ok, text) = output(&root, Some("laptop-test"), &["plugins", "list"]);
    assert!(ok && text.contains("profile-only"), "{text}");
    let (ok, text) = output(&root, None, &["plugins", "list"]);
    assert!(ok && !text.contains("profile-only"), "{text}");
    let (ok, text) = output(
        &root,
        Some("laptop-test"),
        &["tool", "--name", "skills_list", "--args", "{}"],
    );
    assert!(
        ok && text.contains("isolated") && !text.contains("default-only"),
        "{text}"
    );
    let (ok, text) = output(
        &root,
        None,
        &["tool", "--name", "skills_list", "--args", "{}"],
    );
    assert!(
        ok && text.contains("default-only") && !text.contains("isolated"),
        "{text}"
    );
    let default_store = veles_core::store::Store::open(&default.join("state.db")).unwrap();
    assert!(!default_store
        .session_meta(50)
        .unwrap()
        .iter()
        .any(|s| s.title == "named-profile-only"));

    // State writes through the CLI and reads through the gateway's real Tcl
    // bridge resolve the same isolated profile, never the default store.
    let memory = root.join("memory.txt");
    put(&memory, "named profile memory\n");
    let out = command(&root, Some("laptop-test"))
        .args(["memory", "write", "reference/isolation.txt", "--file"])
        .arg(&memory)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    let (ok, text) = output(
        &root,
        Some("laptop-test"),
        &["memory", "cat", "reference/isolation.txt"],
    );
    assert!(ok && text.contains("named profile memory"), "{text}");
    let (ok, _) = output(&root, None, &["memory", "cat", "reference/isolation.txt"]);
    assert!(!ok, "default profile unexpectedly read named memory");
    let expected_named = tcl_literal(&named);
    let expected_cache = tcl_literal(&named.join("cache"));
    let (ok, text) = output(
        &root,
        Some("laptop-test"),
        &["tcl", "eval", "::profile_probe"],
    );
    assert!(ok, "{text}");
    for part in ["laptop-test", "named-model", "named-profile-only"] {
        assert!(text.contains(part), "missing {part:?} in {text}");
    }
    for (expression, expected) in [
        ("[lindex [::profile_probe] 0]", expected_named.as_str()),
        ("[lindex [::profile_probe] 1]", expected_named.as_str()),
        ("[lindex [::profile_probe] 2]", expected_cache.as_str()),
    ] {
        let (ok, text) = output(
            &root,
            Some("laptop-test"),
            &["tcl", "eval", &tcl_path_equals(expression, expected)],
        );
        assert!(
            ok && final_line(&text) == Some("1"),
            "wrong Tcl root for {expression}: {text}"
        );
    }

    let roots_check = [
        tcl_path_condition("$::veles_config_dir", &expected_named),
        tcl_path_condition("$::veles_data_dir", &expected_named),
        tcl_path_condition("$::veles_cache_dir", &expected_cache),
    ]
    .join(" && ");
    let roots_check = format!("expr {{ {roots_check} }}");
    let paths = command(&root, Some("laptop-test"))
        .env("VELES_DESKTOP_PORT", named_port.to_string())
        .args(["tcl", "eval", &roots_check])
        .output()
        .unwrap();
    let paths_text = format!(
        "{}{}",
        String::from_utf8_lossy(&paths.stdout),
        String::from_utf8_lossy(&paths.stderr)
    );
    assert!(
        paths.status.success() && final_line(&paths_text) == Some("1"),
        "{paths_text}"
    );

    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let probe = root.join("paths.tcl");
    put(
        &probe,
        &format!(
            "source [file join {{{}}} lib paths.tcl]\nputs [expr {{ {} }}]\n",
            repo.join("veles-desktop").display(),
            [
                tcl_path_condition("[::vpaths::config_dir]", &expected_named),
                tcl_path_condition("[::vpaths::data_dir]", &expected_named),
                tcl_path_condition("[::vpaths::cache_dir]", &expected_cache),
                "[::vpaths::profile] eq {laptop-test}".to_string(),
            ]
            .join(" && ")
        ),
    );
    let out = Command::new(veles_core::executecode::tclsh_path())
        .arg(&probe)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("VELES_PROFILE", "laptop-test")
        .env_remove("VELES_HOME")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let desktop_paths = String::from_utf8_lossy(&out.stdout);
    assert!(desktop_paths.trim() == "1", "{desktop_paths}");

    // With its own gateway already running, GUI startup selects it and
    // hands the selected profile's token to wish (no window is opened).
    let wish = root.join(if cfg!(windows) {
        "wish-probe.bat"
    } else {
        "wish-probe.sh"
    });
    let probe = if cfg!(windows) {
        "@echo off\r\nif not \"%VELES_PROFILE%\"==\"laptop-test\" exit /b 41\r\nif not \"%VELES_TOKEN%\"==\"named-token\" exit /b 42\r\nif not \"%VELES_DESKTOP_PORT%\"==\"%PROBE_PORT%\" exit /b 43\r\nif \"%VELES_API%\"==\"\" exit /b 44\r\n"
    } else {
        "#!/bin/sh\n[ \"$VELES_PROFILE\" = laptop-test ] || exit 41\n[ \"$VELES_TOKEN\" = named-token ] || exit 42\n[ \"$VELES_DESKTOP_PORT\" = \"$PROBE_PORT\" ] || exit 43\n[ -n \"$VELES_API\" ] || exit 44\n"
    };
    put(&wish, probe);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wish, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let (ok, text) = {
        let out = command(&root, Some("laptop-test"))
            .env("VELES_DESKTOP_PORT", named_port.to_string())
            .env("PROBE_PORT", named_port.to_string())
            .env("VELES_DESKTOP_DIR", repo.join("veles-desktop"))
            .env("WISH", &wish)
            .arg("gui")
            .output()
            .unwrap();
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };
    assert!(ok, "{text}");

    // An invalid name is refused before it can become a path.
    let (ok, text) = output(&root, Some("../other"), &["memory", "ls"]);
    assert!(!ok && text.contains("invalid profile"), "{text}");
    assert!(!root.join("other").exists());
    assert!(!default.join("profiles").join("other").exists());
}
