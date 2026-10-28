//! Session mail between REAL processes (PLAN-sessions-and-phone.md,
//! round S-a): the real `veles` binary as a gateway holding a desktop
//! session, as a REPL holding its own, and as the command line sending
//! mail into both — over the per-process sockets, against a stub provider.
//!
//! Hermetic: a scratch VELES_HOME (under the per-run TMPDIR), its own
//! desktop port, a provider that is a TcpListener in this process. No
//! network, no key, no tokens.

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const REPLY: &str = "the target answered";

/// A chat.completions provider that answers EVERY request with `REPLY`
/// (a turn may make more than one call: titles, summaries).
fn stub_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub provider");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                // Read the head and the body the head announces.
                let mut raw = Vec::new();
                let mut buf = [0u8; 16384];
                let mut need: Option<usize> = None;
                loop {
                    let n = match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&buf[..n]);
                    if need.is_none() {
                        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                            let len = head
                                .split("content-length:")
                                .nth(1)
                                .and_then(|t| t.split(['\r', '\n']).next())
                                .and_then(|t| t.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            need = Some(end + 4 + len);
                        }
                    }
                    if matches!(need, Some(t) if raw.len() >= t) {
                        break;
                    }
                }
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"role\":\"assistant\",\"content\":\"{REPLY}\"}}}}]}}\n\ndata: [DONE]\n\n"
                );
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body.as_bytes());
                let _ = s.flush();
            });
        }
    });
    format!("http://{addr}/v1")
}

fn scratch_home(base_url: &str, tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "veles-mail-e2e-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("config.toml"),
        format!(
            "[model]\nprovider = \"stub\"\ndefault = \"stub-model\"\n\n\
             [[custom_providers]]\nname = \"stub\"\nbase_url = \"{base_url}\"\n\
             api_mode = \"chat_completions\"\napi_key = \"none\"\nmodel = \"stub-model\"\n"
        ),
    )
    .unwrap();
    d
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills its child and removes the scratch home on the way out, pass or
/// fail.
struct Proc {
    child: Child,
    log: PathBuf,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn log_tail(p: &Path) -> String {
    let s = std::fs::read_to_string(p).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(30)..].join("\n")
}

fn veles(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_veles"));
    c.env("VELES_HOME", home)
        .env("HOME", home)
        .env_remove("VELES_SESSION_PRINCIPAL")
        .stdin(Stdio::null());
    c
}

fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    // `connect_timeout`, not `connect`: on Windows a loopback SYN can sit
    // unanswered for minutes (PLAN-windows.md §3 notes, 2026-10-13); a bounded
    // wait keeps the retry loop alive instead of hanging the test.
    let mut s = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_secs(5),
    )
    .expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = out.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// A window's event stream for a session, held open until dropped —
/// what the desktop does while it shows the session.
fn open_events(port: u16, sid: &str) -> TcpStream {
    let mut s = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_secs(5),
    )
    .expect("connect");
    let req = format!(
        "GET /api/sessions/{sid}/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    // Read until the server says the past is done.
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&got).contains("session-live") {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    assert!(
        String::from_utf8_lossy(&got).contains("session-live"),
        "the event stream opened: {}",
        String::from_utf8_lossy(&got)
    );
    s
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    panic!("timed out waiting for {what}");
}

fn mail_state(home: &Path, id: &str) -> Option<(String, Option<String>)> {
    let store = veles_core::store::Store::open(&home.join("state.db")).ok()?;
    store
        .mail_get(id)
        .ok()
        .flatten()
        .map(|r| (r.state, r.reply))
}

/// The mail id `session send` printed.
fn sent_id(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .skip_while(|w| *w != "mail")
        .nth(1)
        .unwrap_or_else(|| panic!("no mail id in {stdout:?}"))
        .to_string()
}

#[test]
fn the_cli_mails_a_gateway_held_desktop_session_and_gets_its_answer() {
    let url = stub_provider();
    let home = scratch_home(&url, "gw");
    let port = free_port();
    let log = home.join("gw.log");
    let gw = Proc {
        child: veles(&home)
            .args(["gateway", "run"])
            .env("VELES_DESKTOP_PORT", port.to_string())
            .env("VELES_DESKTOP_OPEN", "1")
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(std::fs::File::create(home.join("gw.err")).unwrap())
            .spawn()
            .expect("spawn the gateway"),
        log: log.clone(),
    };
    wait_until("the gateway's health route", 60, || {
        TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            std::time::Duration::from_secs(5),
        )
        .is_ok()
            && http(port, "GET", "/api/health", "").0 == 200
    });

    // A desktop session, mailed BEFORE any window shows it: nobody holds
    // it, so the mail waits in its queue.
    let (code, body) = http(
        port,
        "POST",
        "/api/sessions/new",
        r#"{"title":"mail target"}"#,
    );
    assert_eq!(code, 201, "sessions/new: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let sid = v["session_id"].as_str().expect("session id").to_string();
    let out = veles(&home)
        .args(["session", "send", &sid, "waited for you"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("nobody has it open"), "{stdout}");
    let early = sent_id(&stdout);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        mail_state(&home, &early).map(|s| s.0).as_deref(),
        Some("queued"),
        "nothing runs a session nobody holds"
    );

    // A window opens it (its event stream, held open as the desktop
    // holds it): the waiting mail runs, and the session is held now.
    let _window = open_events(port, &sid);
    wait_until(
        "the waiting mail to run when a window opened it",
        60,
        || mail_state(&home, &early).map(|s| s.0).as_deref() == Some("answered"),
    );
    let out = veles(&home).args(["session", "list"]).output().unwrap();
    let list = String::from_utf8_lossy(&out.stdout);
    assert!(
        list.lines()
            .any(|l| l.starts_with(&sid) && l.contains("desktop")),
        "session list names the desktop session:\n{list}\n--- gateway\n{}",
        log_tail(&gw.log)
    );

    // Mail it and wait for the answer, by title.
    let out = veles(&home)
        .args([
            "session",
            "send",
            "mail target",
            "hello from the cli",
            "--wait",
            "60",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "session send exited {:?}\n{stdout}\n{stderr}\n--- gateway\n{}",
        out.status.code(),
        log_tail(&gw.log)
    );
    assert!(stdout.contains("has it open"), "{stdout}");
    assert!(stdout.contains(REPLY), "the answer is printed: {stdout}");
    let id = sent_id(&stdout);
    assert_eq!(
        mail_state(&home, &id),
        Some(("answered".to_string(), Some(REPLY.to_string())))
    );

    // The transcript: mail provenance, the text labelled as mail.
    let store = veles_core::store::Store::open(&home.join("state.db")).unwrap();
    let hist = store.load_history(&sid).unwrap();
    assert!(
        hist.iter()
            .any(|m| m.injection.as_deref() == Some("origin:mail")
                && m.content.contains("veles command line")),
        "a mail origin row naming the command line"
    );
    assert!(hist
        .iter()
        .any(|m| m.content == "hello from the cli" && m.injection.as_deref() == Some("mail:cli")));
    assert!(
        !hist.iter().any(|m| m.content.contains("[veles mail ")),
        "the header never reaches the transcript"
    );

    // The gateway's socket is in the run dir while it runs.
    let gw_pid = gw.child.id();
    assert!(home.join("run").join(format!("{gw_pid}.sock")).exists());
    drop(gw);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn an_idle_repl_is_told_of_mail_and_an_empty_enter_runs_it() {
    let url = stub_provider();
    let home = scratch_home(&url, "repl");
    let log = home.join("repl.err");
    let mut repl = Proc {
        child: veles(&home)
            .arg("chat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .expect("spawn the REPL"),
        log: log.clone(),
    };
    let repl_pid = repl.child.id();
    // Drain the REPL's stdout so it never blocks on a full pipe.
    let stdout = repl.child.stdout.take().unwrap();
    let printed = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    {
        let printed = printed.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                let mut p = printed.lock().unwrap();
                p.push_str(&line);
                p.push('\n');
            }
        });
    }
    // The REPL holds its session: `session list` shows it.
    let mut sid = String::new();
    wait_until("the REPL in session list", 60, || {
        let out = veles(&home).args(["session", "list"]).output().unwrap();
        let list = String::from_utf8_lossy(&out.stdout).to_string();
        match list.lines().find(|l| l.contains(" repl ")) {
            Some(l) => {
                sid = l.split_whitespace().next().unwrap().to_string();
                true
            }
            None => false,
        }
    });

    // Mail without waiting: delivered to the REPL's process, which is
    // blocked in readline — it says so, and runs nothing yet.
    let out = veles(&home)
        .args(["session", "send", &sid, "ping from the cli"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!("process {repl_pid} has it open")),
        "{stdout}"
    );
    let id = sent_id(&stdout);
    wait_until("the REPL's notice", 20, || {
        std::fs::read_to_string(&repl.log)
            .unwrap_or_default()
            .contains("mail arrived for this session")
    });
    assert_eq!(
        mail_state(&home, &id).map(|s| s.0).as_deref(),
        Some("queued")
    );

    // An empty Enter runs it.
    repl.child.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
    wait_until("the mail's answer", 60, || {
        mail_state(&home, &id).map(|s| s.0).as_deref() == Some("answered")
    });
    let shown = printed.lock().unwrap().clone();
    assert!(shown.contains("mail from the command line"), "{shown}");
    assert!(shown.contains(REPLY), "{shown}");

    // EOF ends the REPL; its socket goes with it.
    drop(repl.child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = repl.child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !home.join("run").join(format!("{repl_pid}.sock")).exists(),
        "the REPL unlinks its socket on the way out"
    );
    drop(repl);
    let _ = std::fs::remove_dir_all(&home);
}

/// A pseudo-terminal pair, sized: the TUI needs a tty on both ends, and
/// ratatui asks it for its size.
#[cfg(unix)]
fn openpty(cols: u16, rows: u16) -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: the out-pointers are valid; a null name and termios are
    // allowed; the winsize is a valid struct for the call.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    assert_eq!(r, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: openpty succeeded, so both fds are open and ours.
    unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(master),
            std::os::fd::OwnedFd::from_raw_fd(slave),
        )
    }
}

#[cfg(unix)]
#[test]
fn an_idle_tui_runs_mail_the_moment_it_arrives() {
    let url = stub_provider();
    let home = scratch_home(&url, "tui");
    let (master, slave) = openpty(120, 40);
    let log = home.join("tui.screen");
    let tui = Proc {
        child: veles(&home)
            .arg("chat")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave))
            .spawn()
            .expect("spawn the TUI"),
        log: log.clone(),
    };
    let tui_pid = tui.child.id();
    // Drain the screen so the TUI never blocks on a full pty.
    {
        let mut f = std::fs::File::from(master);
        let log = log.clone();
        std::thread::spawn(move || {
            let mut out = std::fs::File::create(&log).unwrap();
            let mut buf = [0u8; 8192];
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let _ = out.write_all(&buf[..n]);
            }
        });
    }
    // The TUI holds its session: `session list` shows it.
    let mut sid = String::new();
    wait_until("the TUI in session list", 60, || {
        let out = veles(&home).args(["session", "list"]).output().unwrap();
        let list = String::from_utf8_lossy(&out.stdout).to_string();
        match list.lines().find(|l| l.contains(" tui ")) {
            Some(l) => {
                sid = l.split_whitespace().next().unwrap().to_string();
                true
            }
            None => false,
        }
    });

    // Idle at its prompt, it runs the mail the moment it arrives — no
    // keystroke — and the waiting sender gets the answer.
    let out = veles(&home)
        .args(["session", "send", &sid, "ping from the cli", "--wait", "60"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "session send exited {:?}\n{stdout}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!("process {tui_pid} has it open")),
        "{stdout}"
    );
    assert!(stdout.contains(REPLY), "the answer is printed: {stdout}");
    let screen = String::from_utf8_lossy(&std::fs::read(&log).unwrap_or_default()).to_string();
    assert!(
        screen.contains("mail from cli"),
        "the TUI labels it as mail, not as something its human typed"
    );
    drop(tui);
    let _ = std::fs::remove_dir_all(&home);
}
