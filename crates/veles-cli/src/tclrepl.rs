//! `veles tcl` — live-interpreter surfaces against the running gateway.
//!
//! Subcommands:
//!   eval   <code>     evaluate Tcl in the gateway's plugin interpreter
//!   repl              interactive readline loop (history, multi-line)
//!   reload            re-source all plugins (manual hot reload)
//!   file   <path>     source a file through the live interpreter
//!
//! This is the emacs-doctrine "scratch buffer for the runtime": the
//! agent's own lisp, inspectable and patchable at runtime. The eval
//! runs in the SAME interpreter that hosts plugins, hooks, and the
//! veles::call_llm bridge — so a REPL session can probe everything.
//!
//! Transport: loopback HTTP against the desktop API (POST /api/tcl/eval
//! and /api/tcl/reload), with the API's bearer token — the gateway mints
//! one into `<config_dir>/.env` on first boot, and these routes are the
//! live interpreter, which is why they are behind it. No new deps: a
//! minimal HTTP/1.0 client over TcpStream (the crate already avoids
//! pulling reqwest into the CLI).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

// Core comes through the runtime dylib (lib.rs says why).
use veles_runtime::veles_core;

fn api_base() -> String {
    let port = veles_core::config::desktop_port(&veles_core::config::live());
    format!("127.0.0.1:{port}")
}

/// Minimal blocking HTTP request (HTTP/1.0, connection-close).
pub(crate) fn http_request(
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<(u16, String), String> {
    let base = api_base();
    let mut sock = TcpStream::connect_timeout(
        &base
            .parse()
            .map_err(|e| format!("bad address {base}: {e}"))?,
        Duration::from_secs(2),
    )
    .map_err(|e| {
        format!("gateway desktop API not reachable on {base} (is the gateway running?): {e}")
    })?;
    sock.set_read_timeout(Some(Duration::from_secs(30))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(5))).ok();

    // The desktop API authenticates every route but /api/health, and the
    // gateway mints the token into `<config_dir>/.env` on first boot — so
    // read it through the same helper the server resolves, and this client
    // cannot disagree with the gateway about where the credential lives.
    let token = veles_core::authn::desktop_bearer_token();
    let auth = if token.is_empty() {
        String::new()
    } else {
        format!("Authorization: Bearer {token}\r\n")
    };
    let req = match body {
        Some(b) => format!(
            "{method} {path} HTTP/1.0\r\nHost: {base}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
            b.len()
        ),
        None => format!("{method} {path} HTTP/1.0\r\nHost: {base}\r\n{auth}\r\n"),
    };
    sock.write_all(req.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;

    let mut buf = Vec::new();
    sock.read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = match text.find("\r\n\r\n") {
        Some(i) => (&text[..i], &text[i + 4..]),
        None => return Err("malformed HTTP response".into()),
    };
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    Ok((status, body.to_string()))
}

fn json_str(v: &serde_json::Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

/// What a non-200 from the desktop API means, in the one case where the
/// body does not say: a 401 comes back as `{"error":"unauthorized"}`,
/// which tells an operator nothing about the token they did not know
/// existed. The gateway mints one on first boot, so the usual causes are
/// a client pointed at a different config dir than the gateway's, or a
/// token that appeared after this process resolved it.
pub(crate) fn describe(status: u16, resp: &str) -> String {
    if status == 401 {
        return format!(
            "HTTP 401 from the desktop API — it wants a bearer token and this client sent \
             none it recognized. The gateway mints one into <config_dir>/.env as {} on first \
             boot and `veles tcl` reads it from there, so either this process is pointed at a \
             different config dir than the gateway (VELES_HOME), or the token was minted after \
             it started. {}=1 makes the gateway serve open instead. (body: {resp})",
            veles_core::authn::DESKTOP_BEARER_KEY,
            veles_core::authn::DESKTOP_OPEN_KEY
        );
    }
    format!("HTTP {status}: {resp}")
}

/// POST /api/tcl/eval — returns the result string; Err on transport or
/// Tcl error (the error text is the message).
pub fn eval(code: &str) -> Result<String, String> {
    let body = serde_json::json!({"code": code}).to_string();
    let (status, resp) = http_request("POST", "/api/tcl/eval", Some(&body))?;
    if status == 401 {
        return Err(describe(status, &resp));
    }
    let v: serde_json::Value =
        serde_json::from_str(&resp).map_err(|e| format!("bad JSON from API: {e}: {resp}"))?;
    if status != 200 {
        return Err(describe(status, &resp));
    }
    let result = json_str(&v, "result");
    if v["ok"].as_bool().unwrap_or(false) {
        Ok(result)
    } else {
        Err(result)
    }
}

pub fn reload() -> Result<String, String> {
    let (status, resp) = http_request("POST", "/api/tcl/reload", Some("{}"))?;
    if status != 200 {
        // Read the STATUS first: a 401 body is `{"error":"unauthorized"}`,
        // which has no `result` key, so pulling the field out first
        // answered with an empty string and left the operator nothing to
        // act on. `describe` carries the body, so a 500's own message is
        // not lost either.
        return Err(describe(status, &resp));
    }
    let v: serde_json::Value =
        serde_json::from_str(&resp).map_err(|e| format!("bad JSON from API: {e}:"))?;
    if v["ok"].as_bool().unwrap_or(false) {
        Ok(json_str(&v, "result"))
    } else {
        Err(json_str(&v, "result"))
    }
}

/// One-shot eval (veles tcl eval '<code>').
pub fn run_eval(code: &str) -> anyhow::Result<()> {
    match eval(code) {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Source a file through the live interpreter.
pub fn run_file(path: &str) -> anyhow::Result<()> {
    let abs = if path.starts_with('/') || path.starts_with('~') {
        path.to_string()
    } else {
        std::env::current_dir()?
            .join(path)
            .to_string_lossy()
            .to_string()
    };
    // Read locally, send as a script (avoids needing the gateway to
    // have the same cwd).
    let content =
        std::fs::read_to_string(&abs).map_err(|e| anyhow::anyhow!("cannot read {abs}: {e}"))?;
    match eval(&content) {
        Ok(out) => {
            println!("sourced {abs}");
            if !out.is_empty() {
                println!("{out}");
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("error in {abs}: {e}");
            std::process::exit(1);
        }
    }
}

pub fn run_reload() -> anyhow::Result<()> {
    match reload() {
        Ok(msg) => {
            println!("{msg}");
            Ok(())
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Interactive REPL with readline (history + multi-line accumulation).
pub fn run_repl() -> anyhow::Result<()> {
    println!(
        "veles tcl — live interpreter REPL (gateway @ {})",
        api_base()
    );
    println!("  :reload   re-source plugins     :file <path>   source a file");
    println!("  :info     interpreter state     :help          this help");
    println!("  Ctrl-D to exit. Errors are Tcl errors, not exits.\n");

    let mut rl = rustyline::DefaultEditor::new()?;
    let hist_dir = veles_core::config::veles_home().join("history");
    let _ = std::fs::create_dir_all(&hist_dir);
    let hist_file = hist_dir.join("tcl-repl.txt");
    let _ = rl.load_history(&hist_file);

    loop {
        match rl.readline("tcl> ") {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.starts_with(':') {
                    match trimmed.split_whitespace().next() {
                        Some(":reload") => match reload() {
                            Ok(m) => println!("{m}"),
                            Err(e) => println!("error: {e}"),
                        },
                        Some(":file") => {
                            if let Some(p) = trimmed.split_whitespace().nth(1) {
                                match std::fs::read_to_string(p) {
                                    Ok(content) => match eval(&content) {
                                        Ok(out) => {
                                            if !out.is_empty() {
                                                println!("{out}");
                                            }
                                        }
                                        Err(e) => println!("error: {e}"),
                                    },
                                    Err(e) => println!("cannot read {p}: {e}"),
                                }
                            } else {
                                println!("usage: :file <path>");
                            }
                        }
                        Some(":info") => match eval(
                            "list interpreter=[info patchlevel] procs=[llength [info procs]] \
                             commands=[llength [info commands]] hooks=[info exists ::veles::hooks]",
                        ) {
                            Ok(out) => println!("{out}"),
                            Err(e) => println!("error: {e}"),
                        },
                        Some(":help") | Some(":h") => {
                            println!(
                                "  :reload   re-source plugins     :file <path>   source a file"
                            );
                            println!("  :info     interpreter state     :help          this help");
                        }
                        Some(other) => println!("unknown command {other} (:help)"),
                        None => {}
                    }
                    let _ = rl.add_history_entry(trimmed);
                    continue;
                }
                let _ = rl.add_history_entry(trimmed);
                match eval(trimmed) {
                    Ok(out) => {
                        if !out.is_empty() {
                            println!("{out}");
                        }
                    }
                    Err(e) => println!("error: {e}"),
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                println!("(Ctrl-C — :help or Ctrl-D to exit)");
                continue;
            }
            Err(rustyline::error::ReadlineError::Eof) => {
                println!("bye");
                break;
            }
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        }
    }
    let _ = rl.save_history(&hist_file);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The credential goes out on the wire, in the header the server
    /// parses — asserted against a real socket rather than by reading the
    /// format string, because "looks right" is exactly what a request with
    /// a mangled `\r\n` join also does, and the failure it produces is a
    /// 401 the operator cannot diagnose from this side.
    #[test]
    fn the_bearer_header_goes_out_on_the_wire() {
        use std::io::{Read, Write};
        // Serialized with the launcher hand-off test, which resolves the
        // same environment variable this one sets. See `crate::env_lock`.
        let _guard = crate::env_lock();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // The port and the token are both read per request, so this needs
        // no server of its own — and no gateway, which is why it can live
        // in the unit suite.
        let prev_port = std::env::var("VELES_DESKTOP_PORT").ok();
        let prev_token = std::env::var(veles_core::authn::DESKTOP_BEARER_KEY).ok();
        std::env::set_var("VELES_DESKTOP_PORT", port.to_string());
        std::env::set_var(veles_core::authn::DESKTOP_BEARER_KEY, "cli-token");

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap_or(0);
            let got = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = "{\"ok\":true,\"result\":\"fine\"}";
            let resp = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            // Closing is what ends the client's read_to_end.
            got
        });

        let out = http_request(
            "POST",
            "/api/tcl/eval",
            Some("{\"code\":\"info patchlevel\"}"),
        );
        let got = server.join().expect("server join");

        match prev_port {
            Some(v) => std::env::set_var("VELES_DESKTOP_PORT", v),
            None => std::env::remove_var("VELES_DESKTOP_PORT"),
        }
        match prev_token {
            Some(v) => std::env::set_var(veles_core::authn::DESKTOP_BEARER_KEY, v),
            None => std::env::remove_var(veles_core::authn::DESKTOP_BEARER_KEY),
        }

        assert!(
            got.starts_with(&format!(
                "POST /api/tcl/eval HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n"
            )),
            "the request line/host are wrong: {got:?}"
        );
        assert!(
            got.contains("\r\nAuthorization: Bearer cli-token\r\n"),
            "the request carried no usable credential: {got:?}"
        );
        assert!(
            got.ends_with("{\"code\":\"info patchlevel\"}"),
            "the body must still arrive intact after the header: {got:?}"
        );
        let (status, body) = out.expect("the round trip");
        assert_eq!(status, 200);
        assert!(body.contains("fine"), "{body}");
    }

    /// A 401 says what to do about it. The body is
    /// `{"error":"unauthorized"}`, which is true and useless: the gateway
    /// mints the token, so the usual cause is a client pointed at a
    /// different config dir than the gateway's.
    #[test]
    fn a_refused_token_explains_itself() {
        let said = describe(401, "{\"error\":\"unauthorized\"}");
        assert!(
            said.contains(veles_core::authn::DESKTOP_BEARER_KEY)
                && said.contains(".env")
                && said.contains(veles_core::authn::DESKTOP_OPEN_KEY),
            "the 401 must name the key, the file and the opt-out: {said}"
        );
        assert!(
            said.contains("{\"error\":\"unauthorized\"}"),
            "and still carry the server's own body: {said}"
        );
        assert_eq!(
            describe(500, "boom"),
            "HTTP 500: boom",
            "every other status is passed through unchanged"
        );
    }
}
