//! A loopback HTTPS proxy for the phone's static tools
//! (PLAN-sessions-and-phone.md A3, round P5).
//!
//! git ships on the phone as a static musl binary (flake.nix,
//! velesGitAndroid). musl resolves names from /etc/resolv.conf, which
//! Android has not got, so `git ls-remote https://…` said "Could not
//! resolve host: github.com" on the real phone (2026-10-12). The engine
//! is linked against Android's own libc, which asks Android's resolver.
//! So the engine listens on loopback and takes `CONNECT host:port`: it
//! resolves the name, connects, and copies bytes both ways. TLS stays
//! end to end between git and the server; the proxy sees only a host
//! name and a port.
//!
//! It runs only when the app asks (`VELES_CONNECT_PROXY`, set by
//! android/java/…/Engine.java with a token it minted for this start),
//! and answers only a request carrying that token as proxy credentials —
//! another app on the phone gets 407. git finds it through
//! `GIT_CONFIG_*` in the engine's environment (`http.proxy`), which every
//! git the engine or the agent runs inherits.

use base64::Engine as _;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The longest request head accepted.
const MAX_HEAD: usize = 8 * 1024;

/// `VELES_CONNECT_PROXY` = `<addr> <token>`, e.g. `127.0.0.1:9877 3f…`.
pub fn spec_from_env() -> Option<(SocketAddr, String)> {
    parse_spec(&std::env::var("VELES_CONNECT_PROXY").ok()?)
}

fn parse_spec(s: &str) -> Option<(SocketAddr, String)> {
    let mut it = s.split_whitespace();
    let addr: SocketAddr = it.next()?.parse().ok()?;
    let token = it.next()?.to_string();
    if !addr.ip().is_loopback() || token.len() < 16 {
        return None;
    }
    Some((addr, token))
}

/// Listen and serve until the listener fails.
pub async fn serve(addr: SocketAddr, token: String) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("connect proxy for the phone's tools on {addr}");
    serve_on(listener, token).await
}

/// [`serve`] on a listener the caller bound (tests bind port 0).
pub async fn serve_on(listener: TcpListener, token: String) -> anyhow::Result<()> {
    let token = std::sync::Arc::new(token);
    loop {
        let (sock, _) = listener.accept().await?;
        let token = token.clone();
        crate::rt::spawn(async move {
            if let Err(e) = handle(sock, &token).await {
                tracing::debug!("connect proxy: {e:#}");
            }
        });
    }
}

/// The request's target, if it is a CONNECT this proxy answers.
fn decide(head: &str, token: &str) -> Result<String, (u16, &'static str)> {
    let mut lines = head.split("\r\n");
    let request = lines.next().unwrap_or("");
    let mut parts = request.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let want = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("veles:{token}"))
    );
    let authorized = lines.any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("proxy-authorization") && v.trim() == want
        })
    });
    if !authorized {
        return Err((407, "Proxy Authentication Required"));
    }
    if method != "CONNECT" {
        return Err((405, "Method Not Allowed"));
    }
    match target.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => {
            Ok(target.to_string())
        }
        _ => Err((400, "Bad Request")),
    }
}

/// One request head off the connection, and whatever came after it.
/// `None` when the client hung up or sent too much.
async fn read_head(sock: &mut TcpStream, carry: &mut Vec<u8>) -> anyhow::Result<Option<String>> {
    let mut buf = [0u8; 1024];
    loop {
        if let Some(end) = carry.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&carry[..end + 4]).to_string();
            carry.drain(..end + 4);
            return Ok(Some(head));
        }
        if carry.len() > MAX_HEAD {
            sock.write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\n\r\n")
                .await?;
            return Ok(None);
        }
        let n = sock.read(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        carry.extend_from_slice(&buf[..n]);
    }
}

async fn handle(mut sock: TcpStream, token: &str) -> anyhow::Result<()> {
    let mut carry = Vec::new();
    // curl (git's) asks first without credentials to learn the scheme,
    // then again on the same connection with them: a 407 keeps the
    // connection open for that second request. A proxy that closed after
    // its 407 left git with "Connection reset by peer" (the real phone,
    // 2026-10-12).
    let mut target = None;
    for _ in 0..3 {
        let Some(head) = read_head(&mut sock, &mut carry).await? else {
            return Ok(());
        };
        match decide(&head, token) {
            Ok(t) => {
                target = Some(t);
                break;
            }
            Err((407, why)) => {
                sock.write_all(
                    format!(
                        "HTTP/1.1 407 {why}\r\nProxy-Authenticate: Basic realm=\"veles\"\r\n\
                         Content-Length: 0\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await?;
            }
            Err((code, why)) => {
                sock.write_all(
                    format!(
                        "HTTP/1.1 {code} {why}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await?;
                return Ok(());
            }
        }
    }
    let Some(target) = target else {
        return Ok(());
    };
    let mut up = match TcpStream::connect(&target).await {
        Ok(s) => s,
        Err(e) => {
            sock.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await?;
            return Err(anyhow::anyhow!("{target}: {e}"));
        }
    };
    sock.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    // Bytes that came after the head (a client that did not wait).
    if !carry.is_empty() {
        up.write_all(&carry).await?;
    }
    tokio::io::copy_bidirectional(&mut sock, &mut up).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123";

    fn auth() -> String {
        format!(
            "Proxy-Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("veles:{TOKEN}"))
        )
    }

    #[test]
    fn the_spec_is_loopback_with_a_token() {
        assert!(parse_spec(&format!("127.0.0.1:9877 {TOKEN}")).is_some());
        assert!(
            parse_spec(&format!("0.0.0.0:9877 {TOKEN}")).is_none(),
            "loopback only"
        );
        assert!(parse_spec("127.0.0.1:9877 short").is_none());
        assert!(parse_spec("127.0.0.1:9877").is_none());
    }

    #[test]
    fn only_an_authorized_connect_is_answered() {
        let ok = format!(
            "CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\n{}\r\n\r\n",
            auth()
        );
        assert_eq!(decide(&ok, TOKEN), Ok("github.com:443".into()));
        let none = "CONNECT github.com:443 HTTP/1.1\r\n\r\n";
        assert_eq!(decide(none, TOKEN).unwrap_err().0, 407);
        let wrong = "CONNECT github.com:443 HTTP/1.1\r\nProxy-Authorization: Basic dmVsZXM6bm9wZQ==\r\n\r\n";
        assert_eq!(decide(wrong, TOKEN).unwrap_err().0, 407);
        let get = format!("GET http://example.org/ HTTP/1.1\r\n{}\r\n\r\n", auth());
        assert_eq!(decide(&get, TOKEN).unwrap_err().0, 405);
        let bad = format!("CONNECT nohost HTTP/1.1\r\n{}\r\n\r\n", auth());
        assert_eq!(decide(&bad, TOKEN).unwrap_err().0, 400);
    }

    /// End to end on loopback: a CONNECT with the token reaches an echo
    /// server through the proxy; without it, nothing is connected.
    #[tokio::test]
    async fn a_tunnel_carries_bytes_both_ways() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        crate::rt::spawn(async move {
            while let Ok((mut s, _)) = echo.accept().await {
                crate::rt::spawn(async move {
                    let mut b = [0u8; 64];
                    while let Ok(n) = s.read(&mut b).await {
                        if n == 0 || s.write_all(&b[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        crate::rt::spawn(serve_on(proxy, TOKEN.into()));

        let mut c = TcpStream::connect(proxy_addr).await.unwrap();
        c.write_all(format!("CONNECT {echo_addr} HTTP/1.1\r\n{}\r\n\r\n", auth()).as_bytes())
            .await
            .unwrap();
        let mut b = [0u8; 128];
        let n = c.read(&mut b).await.unwrap();
        assert!(String::from_utf8_lossy(&b[..n]).starts_with("HTTP/1.1 200"));
        c.write_all(b"through the tunnel").await.unwrap();
        let n = c.read(&mut b).await.unwrap();
        assert_eq!(&b[..n], b"through the tunnel");

        // curl's way: first without credentials, then with them on the
        // same connection.
        let mut d = TcpStream::connect(proxy_addr).await.unwrap();
        d.write_all(format!("CONNECT {echo_addr} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let n = d.read(&mut b).await.unwrap();
        assert!(String::from_utf8_lossy(&b[..n]).starts_with("HTTP/1.1 407"));
        d.write_all(format!("CONNECT {echo_addr} HTTP/1.1\r\n{}\r\n\r\n", auth()).as_bytes())
            .await
            .unwrap();
        let n = d.read(&mut b).await.unwrap();
        assert!(
            String::from_utf8_lossy(&b[..n]).starts_with("HTTP/1.1 200"),
            "the second ask on the same connection is answered"
        );
        d.write_all(b"again").await.unwrap();
        let n = d.read(&mut b).await.unwrap();
        assert_eq!(&b[..n], b"again");
    }
}
