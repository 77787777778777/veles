//! A Tcl eval identifies the gateway that answered, without changing stdout.
//! Hermetic: two isolated homes, two loopback ports, no provider/network.

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

fn veles(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_veles"));
    c.env("VELES_HOME", home)
        .env("HOME", home)
        .env("VELES_DESKTOP_OPEN", "1")
        .env_remove("VELES_SESSION_PRINCIPAL")
        .env_remove("VELES_PROFILE")
        .stdin(Stdio::null());
    c
}

fn health(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    ) else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    if stream
        .write_all(b"GET /api/health HTTP/1.0\r\nHost: x\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut body = String::new();
    let _ = stream.read_to_string(&mut body);
    body.contains("\"status\":\"ok\"")
}

fn wait_ready(port: u16, home: &Path) {
    let until = Instant::now() + Duration::from_secs(60);
    while Instant::now() < until {
        if health(port) {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    panic!(
        "gateway did not become ready\n{}\n{}",
        std::fs::read_to_string(home.join("gw.log")).unwrap_or_default(),
        std::fs::read_to_string(home.join("gw.err")).unwrap_or_default()
    );
}

fn start(home: &Path, name: &str, port: u16) -> Proc {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("config.toml"), "").unwrap();
    std::fs::write(
        home.join("node.toml"),
        format!("id = \"node-{name}\"\nname = \"{name}\"\n"),
    )
    .unwrap();
    Proc(
        veles(home)
            .args(["gateway", "run"])
            .env("VELES_DESKTOP_PORT", port.to_string())
            .stdout(std::fs::File::create(home.join("gw.log")).unwrap())
            .stderr(std::fs::File::create(home.join("gw.err")).unwrap())
            .spawn()
            .expect("start isolated gateway"),
    )
}

fn eval(home: &Path, port: u16) -> std::process::Output {
    veles(home)
        .args(["tcl", "eval", "expr {6 * 7}"])
        .env("VELES_DESKTOP_PORT", port.to_string())
        .output()
        .expect("eval through isolated gateway")
}

#[test]
fn eval_names_the_selected_gateway_on_stderr_only() {
    let root = std::env::temp_dir().join(format!(
        "veles-tcl-identity-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _cleanup = Cleanup(root.clone());
    let a = root.join("a");
    let b = root.join("b");
    let a_port = free_port();
    let b_port = free_port();
    assert_ne!(a_port, b_port);
    let _a = start(&a, "gateway-a", a_port);
    let _b = start(&b, "gateway-b", b_port);
    wait_ready(a_port, &a);
    wait_ready(b_port, &b);

    for (home, name, port) in [(&a, "gateway-a", a_port), (&b, "gateway-b", b_port)] {
        let out = eval(home, port);
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "42\n");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(name), "{stderr}");
        assert!(stderr.contains(&format!("127.0.0.1:{port}")), "{stderr}");
        assert!(stderr.contains(&format!("node-{name}")), "{stderr}");
        assert!(
            !stderr.lines().any(|line| line == "42"),
            "the result must not be a separate stderr line: {stderr}"
        );
    }
}
