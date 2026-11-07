//! The browser tab bridge's native host, launched the way a browser
//! launches it (BX-1, BX-8, BX-9, BX-20, BX-17d).
//!
//! `veles browser-extension install` writes native-host registrations.
//! On Linux Chrome then runs `<path> chrome-extension://<id>/` and Firefox
//! runs `<path> <manifest.json> <extension id>`, with the browser's own
//! environment and nothing else. Until 0.2.0 `path` was the bare veles
//! binary, which clap rejected, so the host never ran in any browser while
//! the in-process host tests stayed green. These tests run what the
//! manifest names, with the browser's argv, from a different HOME and with
//! no VELES_HOME, and speak the native-messaging protocol on its stdio.
//!
//! On Windows the manifest names a `.cmd` and each browser runs it through
//! cmd.exe with a command line of its own; the Windows test below builds
//! those two command lines exactly, reads the registrations from HKCU under
//! a test name, and uninstalls them.
//!
//! Hermetic on Linux: every path is under a scratch directory. On Windows
//! the registry is the real HKCU, under `veles_browser_test_<pid>_<tag>`
//! names that the test removes whatever happens.
#![cfg(any(target_os = "linux", windows))]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHROME_ORIGIN: &str = "chrome-extension://ndalephnnclghcacbljmhngnpcagpgdh/";
const FIREFOX_ID: &str = "veles@7778777.online";

struct Proc(Child);

impl Proc {
    /// The child and everything it started. On Windows the child is
    /// cmd.exe and the host is its child: killing cmd.exe alone leaves
    /// veles.exe running with the test's pipes open, and a failing launch
    /// then hung the test on stderr instead of failing it (HANDOFF
    /// pitfall 108).
    fn kill_tree(&mut self) {
        #[cfg(windows)]
        let _ = Command::new("taskkill.exe")
            .args(["/F", "/T", "/PID", &self.0.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.0.kill();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill_tree();
        let _ = self.0.wait();
    }
}

fn veles(home: &Path, veles_home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_veles"))
        .args(["browser-extension"])
        .args(args)
        .env("HOME", home)
        .env("VELES_HOME", veles_home)
        .env_remove("VELES_PROFILE")
        .env_remove("APPIMAGE")
        .env_remove("VELES_BUNDLE_ROOT")
        .env_remove("VELES_BROWSER_BRIDGE_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn install(home: &Path, veles_home: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["install"];
    args.extend_from_slice(extra);
    veles(home, veles_home, &args)
}

fn uninstall(home: &Path, veles_home: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["uninstall"];
    args.extend_from_slice(extra);
    veles(home, veles_home, &args)
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    serde_json::from_str(&text).unwrap()
}

#[cfg(target_os = "linux")]
fn manifest(home: &Path, relative: &str) -> Value {
    read_json(&home.join(relative))
}

fn send(out: &mut impl Write, value: &Value) {
    let body = serde_json::to_vec(value).unwrap();
    out.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
    out.write_all(&body).unwrap();
    out.flush().unwrap();
}

#[cfg(target_os = "linux")]
fn recv(input: &mut impl Read) -> Value {
    let mut length = [0u8; 4];
    input
        .read_exact(&mut length)
        .expect("the native host answered with a frame");
    let mut body = vec![0u8; u32::from_le_bytes(length) as usize];
    input.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// Launch `program argv…` as a browser would: a foreign HOME, no veles
/// variables, stdio piped. Then authenticate and connect a tab.
#[cfg(target_os = "linux")]
fn browser_launch(program: &Path, argv: &[&str], browser_home: &Path, token: &str) -> Value {
    let mut command = Command::new(program);
    command.args(argv);
    launch(command, browser_home, token)
}

/// Run `command` with a browser's environment and stdio, say hello with
/// `token`, connect a tab and return the host's first frame. After a
/// `hello-ok` the tab is closed again and the host must exit.
fn launch(mut command: Command, browser_home: &Path, token: &str) -> Value {
    let what = format!("{command:?}");
    let mut child = Proc(
        command
            .env("HOME", browser_home)
            .env_remove("VELES_HOME")
            .env_remove("VELES_PROFILE")
            .env_remove("VELES_BROWSER_BRIDGE_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = child.0.stdout.take().unwrap();
    send(
        &mut stdin,
        &json!({"type":"hello","id":"c-1","token":token}),
    );
    send(&mut stdin, &json!({"type":"connect","id":"c-1"}));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut length = [0u8; 4];
        let got = stdout.read_exact(&mut length).map(|()| {
            let mut body = vec![0u8; u32::from_le_bytes(length) as usize];
            stdout.read_exact(&mut body).map(|()| (length, body))
        });
        let _ = tx.send(got);
    });
    let reply: Value = match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(Ok((_, body)))) => serde_json::from_slice(&body).unwrap_or_else(|e| {
            panic!(
                "{what}: the first frame is not JSON ({e}); something else wrote to stdout: {:?}",
                String::from_utf8_lossy(&body)
            )
        }),
        other => {
            child.kill_tree();
            // Whatever reached stderr, but never a wait on a pipe that
            // something still holds open.
            let mut stderr = child.0.stderr.take().unwrap();
            let (err_tx, err_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut err = String::new();
                stderr.read_to_string(&mut err).ok();
                let _ = err_tx.send(err);
            });
            let err = err_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| "(still open after the kill)".into());
            panic!("no framed reply from {what}: {other:?}\nstderr: {err}");
        }
    };
    if reply["type"] == "hello-ok" {
        send(&mut stdin, &json!({"type":"close","id":"c-1"}));
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if child.0.try_wait().unwrap().is_some() {
                return reply;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{what}: the host did not exit after close");
    }
    reply
}

fn token(veles_home: &Path) -> String {
    std::fs::read_to_string(veles_home.join("browser-extension-token"))
        .unwrap()
        .trim()
        .to_owned()
}

fn assert_ok(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_installed_host_answers_chrome_and_firefox_launches() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let veles_home = dir.path().join("veles");
    let browser_home = dir.path().join("browser-home");
    for d in [&home, &veles_home, &browser_home] {
        std::fs::create_dir_all(d).unwrap();
    }
    let out = install(&home, &veles_home, &[]);
    assert_ok(&out, "install");
    let token = token(&veles_home);
    let chrome = manifest(
        &home,
        ".config/google-chrome/NativeMessagingHosts/veles_browser.json",
    );
    let chromium = manifest(
        &home,
        ".config/chromium/NativeMessagingHosts/veles_browser.json",
    );
    let firefox = manifest(&home, ".mozilla/native-messaging-hosts/veles_browser.json");
    assert_eq!(chrome["allowed_origins"], json!([CHROME_ORIGIN]));
    assert_eq!(chromium["allowed_origins"], json!([CHROME_ORIGIN]));
    assert_eq!(firefox["allowed_extensions"], json!([FIREFOX_ID]));
    for m in [&chrome, &chromium, &firefox] {
        assert_eq!(m["name"], "veles_browser");
        assert_eq!(m["type"], "stdio");
    }
    let host = PathBuf::from(chrome["path"].as_str().unwrap());
    assert!(host.is_absolute(), "browsers need an absolute host path");
    assert_eq!(firefox["path"], chrome["path"]);
    // Chrome: one argument, the caller's origin.
    let reply = browser_launch(&host, &[CHROME_ORIGIN], &browser_home, &token);
    assert_eq!(reply["type"], "hello-ok", "{reply}");
    // Chrome on Windows adds `--parent-window=<hwnd>`: the host's argument
    // parser takes that too (it is the same parser on every platform).
    let reply = browser_launch(
        &host,
        &[CHROME_ORIGIN, "--parent-window=0"],
        &browser_home,
        &token,
    );
    assert_eq!(reply["type"], "hello-ok", "{reply}");
    // Firefox: the manifest path and the extension id.
    let ff_manifest = home.join(".mozilla/native-messaging-hosts/veles_browser.json");
    let reply = browser_launch(
        &host,
        &[ff_manifest.to_str().unwrap(), FIREFOX_ID],
        &browser_home,
        &token,
    );
    assert_eq!(reply["type"], "hello-ok", "{reply}");
    // A wrong token is refused in a frame the extension can match.
    let reply = browser_launch(&host, &[CHROME_ORIGIN], &browser_home, "wrong");
    assert_eq!(reply["type"], "error");
    assert_eq!(reply["id"], "c-1");
    // The install printed a user-owned copy of the extension.
    let printed = String::from_utf8_lossy(&out.stdout);
    let copy = veles_home.join("browser-extension/extension");
    assert!(
        copy.join("manifest.json").is_file() && printed.contains(&copy.display().to_string()),
        "the extension directory must be a stable user-owned copy: {printed}"
    );
}

/// A manifest written before the launcher existed names the bare binary.
/// The binary itself recognises a browser's argv.
#[cfg(target_os = "linux")]
#[test]
fn the_bare_binary_also_accepts_a_browser_launch() {
    let dir = tempfile::tempdir().unwrap();
    let veles_home = dir.path().join("veles");
    std::fs::create_dir_all(&veles_home).unwrap();
    let out = install(&dir.path().join("home"), &veles_home, &[]);
    assert!(out.status.success());
    let token = token(&veles_home);
    let mut child = Proc(
        Command::new(env!("CARGO_BIN_EXE_veles"))
            .arg(CHROME_ORIGIN)
            .env("HOME", dir.path().join("home"))
            .env("VELES_HOME", &veles_home)
            .env_remove("VELES_PROFILE")
            .env_remove("VELES_BROWSER_BRIDGE_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = child.0.stdout.take().unwrap();
    send(
        &mut stdin,
        &json!({"type":"hello","id":"c-2","token":token}),
    );
    send(&mut stdin, &json!({"type":"connect","id":"c-2"}));
    assert_eq!(recv(&mut stdout)["type"], "hello-ok");
    send(&mut stdin, &json!({"type":"close","id":"c-2"}));
}

/// BX-17(d): refusing to overwrite must not leave half an install.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_install_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let veles_home = dir.path().join("veles");
    let existing = home.join(".mozilla/native-messaging-hosts/veles_browser.json");
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::fs::write(&existing, "{}").unwrap();
    let out = install(&home, &veles_home, &[]);
    assert!(
        !out.status.success(),
        "an existing manifest needs --overwrite"
    );
    assert!(
        !home
            .join(".config/google-chrome/NativeMessagingHosts/veles_browser.json")
            .exists(),
        "no browser may be half-installed"
    );
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "{}");
    let out = install(&home, &veles_home, &["--overwrite"]);
    assert!(out.status.success());
    assert_eq!(
        manifest(&home, ".mozilla/native-messaging-hosts/veles_browser.json")["allowed_extensions"],
        json!([FIREFOX_ID])
    );
}

/// BX-9: `uninstall` takes back everything `install` wrote, and a second
/// one says there is nothing left.
#[cfg(target_os = "linux")]
#[test]
fn uninstall_removes_what_install_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let veles_home = dir.path().join("veles");
    assert_ok(&install(&home, &veles_home, &[]), "install");
    let written = [
        home.join(".config/google-chrome/NativeMessagingHosts/veles_browser.json"),
        home.join(".config/chromium/NativeMessagingHosts/veles_browser.json"),
        home.join(".mozilla/native-messaging-hosts/veles_browser.json"),
        veles_home.join("browser-extension/native-host"),
        veles_home.join("browser-extension/extension/manifest.json"),
        veles_home.join("browser-extension-token"),
    ];
    for path in &written {
        assert!(path.exists(), "{path:?}");
    }
    let out = uninstall(&home, &veles_home, &[]);
    assert_ok(&out, "uninstall");
    for path in &written {
        assert!(!path.exists(), "uninstall left {path:?}");
    }
    assert!(!veles_home.join("browser-extension").exists());
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(printed.starts_with("Removed:"), "{printed}");
    let again = uninstall(&home, &veles_home, &[]);
    assert_ok(&again, "a second uninstall");
    assert!(String::from_utf8_lossy(&again.stdout).starts_with("Nothing to remove"));
}

/// BX-9 on Windows: the registrations in HKCU, the `.cmd` launcher run
/// through cmd.exe exactly as Chrome and Firefox run it, and uninstall.
#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::process::CommandExt as _;
    use veles_core::platform::{
        user_registry_default, user_registry_delete, user_registry_delete_if_empty,
        user_registry_key_exists,
    };

    const VENDORS: [&str; 4] = [r"Google\Chrome", "Chromium", r"Microsoft\Edge", "Mozilla"];

    fn key(vendor: &str, name: &str) -> String {
        format!(r"Software\{vendor}\NativeMessagingHosts\{name}")
    }

    /// The test name's keys go, and the keys above them that were not
    /// there before, whatever the test did.
    struct Registered {
        name: String,
        absent: Vec<String>,
    }

    impl Drop for Registered {
        fn drop(&mut self) {
            for vendor in VENDORS {
                let _ = user_registry_delete(&key(vendor, &self.name));
            }
            let mut absent = self.absent.clone();
            absent.sort_by_key(|k| std::cmp::Reverse(k.len()));
            for key in absent {
                let _ = user_registry_delete_if_empty(&key);
            }
        }
    }

    fn comspec() -> String {
        std::env::var("COMSPEC").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into())
    }

    /// Chrome (native_process_launcher_win.cc): `%COMSPEC% /d /c <host
    /// command line> < \\.\pipe\… > \\.\pipe\…`, the program quoted by
    /// base::CommandLine when it holds a space. The child's inherited
    /// stdio stands in for the two named pipes.
    fn chrome_command(launcher: &Path) -> Command {
        let mut command = Command::new(comspec());
        command.raw_arg(format!(
            "/d /c {} {CHROME_ORIGIN} --parent-window=0",
            quote(launcher.to_str().unwrap())
        ));
        command
    }

    /// Firefox (subprocess_win.worker.js) for a `.bat` or `.cmd`:
    /// `cmd.exe /s/c "<argv joined by spaces>"`, each argument quoted only
    /// when it holds whitespace or a quote.
    fn firefox_command(launcher: &Path, manifest: &Path) -> Command {
        let mut command = Command::new(comspec());
        command.raw_arg(format!(
            "/s/c \"{} {} {FIREFOX_ID}\"",
            quote(launcher.to_str().unwrap()),
            quote(manifest.to_str().unwrap())
        ));
        command
    }

    /// Both browsers' quoting: as it is, unless it holds whitespace or a
    /// quote; then quoted, with the backslashes before a quote or the end
    /// doubled and the quote escaped.
    fn quote(arg: &str) -> String {
        if !arg.contains([' ', '\t', '"']) {
            return arg.to_owned();
        }
        let mut out = String::from("\"");
        let mut slashes = 0;
        for c in arg.chars() {
            if c == '\\' {
                slashes += 1;
                continue;
            }
            let doubled = if c == '"' { slashes * 2 + 1 } else { slashes };
            out.push_str(&"\\".repeat(doubled));
            out.push(c);
            slashes = 0;
        }
        out.push_str(&"\\".repeat(slashes * 2));
        out.push('"');
        out
    }

    /// A plain path and one with a space, a `%` and Cyrillic: the `.cmd`
    /// quotes the first, doubles the second and needs `chcp 65001` for
    /// the third.
    #[test]
    fn the_installed_cmd_launcher_answers_chrome_and_firefox_launches() {
        for (tag, dir_name) in [("plain", "veles"), ("odd", "veles 100% Ђурђевдан")] {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let veles_home = dir.path().join(dir_name);
            let browser_home = dir.path().join("browser-home");
            for d in [&home, &veles_home, &browser_home] {
                std::fs::create_dir_all(d).unwrap();
            }
            let name = format!("veles_browser_test_{}_{tag}", std::process::id());
            // The keys above each registration that are not there yet: the
            // install creates them and the uninstall must take them back.
            let absent: Vec<String> = VENDORS
                .iter()
                .flat_map(|vendor| {
                    let parts: Vec<&str> = vendor.split('\\').collect();
                    (1..=parts.len())
                        .map(move |n| format!(r"Software\{}", parts[..n].join(r"\")))
                        .chain([format!(r"Software\{vendor}\NativeMessagingHosts")])
                })
                .filter(|key| !user_registry_key_exists(key))
                .collect();
            let registered = Registered {
                name: name.clone(),
                absent: absent.clone(),
            };
            let out = install(&home, &veles_home, &["--native-name", &name]);
            assert_ok(&out, "install");
            let token = token(&veles_home);

            let named = |vendor: &str| {
                PathBuf::from(
                    user_registry_default(&key(vendor, &name))
                        .unwrap()
                        .unwrap_or_else(|| panic!(r"no HKCU\{}", key(vendor, &name))),
                )
            };
            let chromium = named(r"Google\Chrome");
            assert_eq!(named("Chromium"), chromium);
            assert_eq!(named(r"Microsoft\Edge"), chromium);
            let firefox = named("Mozilla");
            assert!(chromium.is_absolute() && firefox.is_absolute());
            let chromium_manifest = read_json(&chromium);
            let firefox_manifest = read_json(&firefox);
            assert_eq!(chromium_manifest["name"], name.as_str());
            assert_eq!(firefox_manifest["name"], name.as_str());
            assert_eq!(chromium_manifest["allowed_origins"], json!([CHROME_ORIGIN]));
            assert_eq!(firefox_manifest["allowed_extensions"], json!([FIREFOX_ID]));
            assert_eq!(chromium_manifest["path"], firefox_manifest["path"]);
            let launcher = PathBuf::from(chromium_manifest["path"].as_str().unwrap());
            assert!(launcher.is_absolute(), "{launcher:?}");
            assert_eq!(launcher.extension().and_then(|e| e.to_str()), Some("cmd"));

            let reply = launch(chrome_command(&launcher), &browser_home, &token);
            assert_eq!(
                reply["type"], "hello-ok",
                "Chrome's launch ({tag}): {reply}"
            );
            let reply = launch(firefox_command(&launcher, &firefox), &browser_home, &token);
            assert_eq!(
                reply["type"], "hello-ok",
                "Firefox's launch ({tag}): {reply}"
            );
            let reply = launch(chrome_command(&launcher), &browser_home, "wrong");
            assert_eq!(reply["type"], "error", "{reply}");
            assert_eq!(reply["id"], "c-1");

            let out = uninstall(&home, &veles_home, &["--native-name", &name]);
            assert_ok(&out, "uninstall");
            for vendor in VENDORS {
                assert!(
                    !user_registry_key_exists(&key(vendor, &name)),
                    r"uninstall left HKCU\{}",
                    key(vendor, &name)
                );
            }
            for key in &absent {
                assert!(
                    !user_registry_key_exists(key),
                    r"the install created HKCU\{key} and the uninstall left it"
                );
            }
            assert!(!chromium.exists() && !firefox.exists() && !launcher.exists());
            drop(registered);
        }
    }
}
