//! The device bridge end to end (PLAN-sessions-and-phone.md, round P1): a
//! REAL gateway whose desktop API holds a token, the REAL window-side
//! client (veles-desktop's api.tcl + device.tcl, in a tclsh), and the REAL
//! `device` tool through `veles tool` — the path an agent's call takes.
//!
//! The window offers two actions; the tool lists them, runs one, reads a
//! provider's refusal, and is told plainly when the window is gone. It
//! also proves the loopback tools now send the API's token: `open_preview`
//! was refused 401 from every gateway session until this round.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "tok-device-bridge-test";

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tclsh() -> String {
    veles_core::executecode::tclsh_path()
}

/// The window-side client needs Tcl's http and tcllib's json, as the
/// desktop does. A test environment without them says so out loud.
fn tcl_has_the_desktops_packages() -> bool {
    let out = Command::new(tclsh())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut c| {
            c.stdin
                .take()
                .unwrap()
                .write_all(b"package require http; package require json; puts ok\n")?;
            c.wait_with_output()
        });
    matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).contains("ok"))
}

fn veles(home: &Path, port: u16) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_veles"));
    c.env("VELES_HOME", home)
        .env("HOME", home)
        .env("VELES_DESKTOP_PORT", port.to_string())
        .env_remove("VELES_DESKTOP_OPEN")
        .env_remove("DESKTOP_BEARER_TOKEN")
        .env_remove("VELES_SESSION_PRINCIPAL")
        .stdin(Stdio::null());
    c
}

fn tool(home: &Path, port: u16, name: &str, args: &str) -> String {
    let out = veles(home, port)
        .args(["tool", "--name", name, "--args", args])
        .output()
        .unwrap();
    // `veles tool` logs to stdout too: read the whole of it.
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

fn health(port: u16) -> bool {
    // `connect_timeout`, not `connect`: on Windows a loopback SYN can sit
    // unanswered for minutes (PLAN-windows.md §3 notes, 2026-10-13); a bounded
    // wait keeps the retry loop alive instead of hanging the test.
    let Ok(mut s) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_secs(5),
    ) else {
        return false;
    };
    let _ = s.write_all(b"GET /api/health HTTP/1.0\r\nHost: x\r\n\r\n");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.starts_with("HTTP/1.1 200") || out.starts_with("HTTP/1.0 200")
}

#[test]
fn the_agent_lists_and_calls_what_a_window_offers() {
    if !tcl_has_the_desktops_packages() {
        eprintln!(
            "SKIP device_bridge: `{}` cannot load http + json (tcllib), which the \
             window-side client needs",
            tclsh()
        );
        return;
    }
    let home = std::env::temp_dir().join(format!(
        "veles-device-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    // A token-protected desktop API, as every real one is by default.
    std::fs::write(home.join(".env"), format!("DESKTOP_BEARER_TOKEN={TOKEN}\n")).unwrap();
    let port = free_port();
    let _gw = Proc(
        veles(&home, port)
            .args(["gateway", "run"])
            .stdout(std::fs::File::create(home.join("gw.log")).unwrap())
            .stderr(std::fs::File::create(home.join("gw.err")).unwrap())
            .spawn()
            .expect("spawn the gateway"),
    );
    wait_until("the gateway", 60, || health(port));

    // Without a window, the tool says nothing is connected.
    let out = tool(&home, port, "device", r#"{"action":"list"}"#);
    assert!(out.contains("No devices are connected"), "{out}");

    // The window: the desktop's own client code, two actions.
    let lib = repo().join("veles-desktop/lib");
    let script = format!(
        r#"
set lib {{{lib}}}
source [file join $lib jsonfield.tcl]
source [file join $lib api.tcl]
source [file join $lib device.tcl]
set ::vapi::base http://127.0.0.1:{port}
set ::vapi::token {TOKEN}
proc ::vapi::unattached {{}} {{ return 0 }}
namespace eval ::vstatus {{ proc set_msg {{m}} {{}} }}
proc ::vdevice::device_name {{}} {{ return testbox }}
::vdevice::provide echo "Say it back." \
    {{{{"type":"object","properties":{{"text":{{"type":"string"}}}}}}}} \
    [list apply {{{{a}} {{ return "echo: [dict get $a text]" }}}}]
::vdevice::provide boom "Always fails." {{}} [list apply {{{{a}} {{ error "it broke" }}}}]
::vdevice::start
vwait forever
"#,
        lib = lib.display()
    );
    let script_path = home.join("window.tcl");
    std::fs::write(&script_path, script).unwrap();
    let window = Proc(
        Command::new(tclsh())
            .arg(&script_path)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(home.join("window.log")).unwrap())
            .stderr(std::fs::File::create(home.join("window.err")).unwrap())
            .spawn()
            .expect("spawn the window client"),
    );

    let mut listed = String::new();
    wait_until("the window's catalog", 60, || {
        listed = tool(&home, port, "device", r#"{"action":"list"}"#);
        listed.contains("testbox") && listed.contains("echo(text)")
    });
    assert!(listed.contains("- boom: Always fails."), "{listed}");

    let out = tool(
        &home,
        port,
        "device",
        r#"{"action":"echo","device":"testbox","args":{"text":"hi there"}}"#,
    );
    assert!(out.contains("echo: hi there"), "the window ran it: {out}");
    let out = tool(&home, port, "device", r#"{"action":"boom","args":{}}"#);
    assert!(
        out.contains("error: it broke"),
        "a provider's refusal reads as one: {out}"
    );
    let out = tool(&home, port, "device", r#"{"action":"fly"}"#);
    assert!(out.contains("no connected device can fly"), "{out}");

    // The loopback tools carry the API's token now.
    let out = tool(
        &home,
        port,
        "open_preview",
        r#"{"url":"https://example.org/"}"#,
    );
    assert!(
        out.contains("preview opened"),
        "open_preview reaches a token-protected API: {out}"
    );

    // The window goes away: the catalog says so, and a call is refused
    // at once instead of waiting out a timeout.
    drop(window);
    wait_until("the window to be seen gone", 30, || {
        tool(&home, port, "device", r#"{"action":"list"}"#).contains("not connected now")
    });
    let t0 = Instant::now();
    let out = tool(
        &home,
        port,
        "device",
        r#"{"action":"echo","args":{"text":"x"}}"#,
    );
    assert!(out.contains("not connected"), "{out}");
    assert!(
        t0.elapsed() < Duration::from_secs(15),
        "refused at once, not after a timeout"
    );
    let _ = std::fs::remove_dir_all(&home);
}
