//! The guest browser's only way out (PLAN-challenge-relay.md, step 2).
//!
//! A page somebody other than the owner chose — the URL an IRC user's
//! turn fetched, opened so that a person can solve the challenge in front
//! of it — loads in agent-browser's GUEST session, and every request that
//! session makes goes through this proxy. It is urlguard's rule applied
//! to the network instead of to one URL: resolve the host, refuse every
//! address that is not public, and dial the address that passed, so a
//! rebinding DNS answer has no second lookup to win. Without it a public
//! page could put the laptop's LAN in an iframe, and the screenshot the
//! solver is shown would carry it out.
//!
//! It speaks HTTP proxy as Chrome does: CONNECT for https and wss,
//! absolute-form requests for plain http. It listens on loopback only.
//! Plain http goes upstream with `Connection: close`, so a proxy
//! connection Chrome reuses can never carry a second request to the
//! first request's server.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The most a request head may take before it is refused.
const HEAD_MAX: usize = 16 * 1024;

/// The ports a guest page may reach. A browser needs the web's ports and
/// nothing else; anything more would make the node a relay for mail and
/// whatever else listens on a public host.
const PORTS: &[u16] = &[80, 443, 8080, 8443];

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

static PORT: tokio::sync::OnceCell<u16> = tokio::sync::OnceCell::const_new();

/// The proxy's port on this node, started on first use.
pub async fn port() -> Result<u16, String> {
    PORT.get_or_try_init(|| async {
        let l = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| format!("the guest browser's proxy could not listen: {e}"))?;
        let port = l
            .local_addr()
            .map_err(|e| format!("the guest browser's proxy: {e}"))?
            .port();
        crate::rt::spawn(serve(l));
        tracing::info!("guest browser proxy on 127.0.0.1:{port}");
        Ok(port)
    })
    .await
    .copied()
}

/// Accept and serve until the listener fails.
pub(crate) async fn serve(l: TcpListener) {
    loop {
        match l.accept().await {
            Ok((sock, _)) => {
                crate::rt::spawn(async move {
                    if let Err(e) = handle(sock).await {
                        tracing::debug!("guest proxy connection: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("guest proxy accept: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle(mut client: TcpStream) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let head_len = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client, &mut buf)).await
    {
        Ok(Ok(Some(n))) => n,
        Ok(Ok(None)) => return refuse(&mut client, 400, "a request this proxy cannot read").await,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Ok(()),
    };
    let mut headers = [httparse::EMPTY_HEADER; 96];
    let mut req = httparse::Request::new(&mut headers);
    if !matches!(
        req.parse(&buf[..head_len]),
        Ok(httparse::Status::Complete(_))
    ) {
        return refuse(&mut client, 400, "a request this proxy cannot read").await;
    }
    let method = req.method.unwrap_or_default().to_string();
    let target = req.path.unwrap_or_default().to_string();
    let rest = buf[head_len..].to_vec();

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some(port) = authority_port(&target) else {
            return refuse(&mut client, 400, "CONNECT needs host:port").await;
        };
        if !PORTS.contains(&port) {
            return refuse(&mut client, 403, &format!("port {port} is not a web port")).await;
        }
        let t = match crate::urlguard::check(&format!("https://{target}/")).await {
            Ok(t) => t,
            Err(why) => return refuse(&mut client, 403, &why).await,
        };
        let Some(mut upstream) = dial(t.addr).await else {
            return refuse(&mut client, 502, "the host did not answer").await;
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        if !rest.is_empty() {
            upstream.write_all(&rest).await?;
        }
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        return Ok(());
    }

    // Plain http, absolute-form: `GET http://host/path HTTP/1.1`.
    let Ok(url) = reqwest::Url::parse(&target) else {
        return refuse(&mut client, 400, "not a proxy request").await;
    };
    if url.scheme() != "http" {
        return refuse(&mut client, 403, "only http is proxied in the clear").await;
    }
    let port = url.port_or_known_default().unwrap_or(80);
    if !PORTS.contains(&port) {
        return refuse(&mut client, 403, &format!("port {port} is not a web port")).await;
    }
    let t = match crate::urlguard::check(&target).await {
        Ok(t) => t,
        Err(why) => return refuse(&mut client, 403, &why).await,
    };
    let Some(mut upstream) = dial(t.addr).await else {
        return refuse(&mut client, 502, "the host did not answer").await;
    };
    let mut path = url.path().to_string();
    if let Some(q) = url.query() {
        path.push('?');
        path.push_str(q);
    }
    let mut head = format!("{method} {path} HTTP/1.1\r\n").into_bytes();
    for h in req.headers.iter() {
        let hop = [
            "connection",
            "keep-alive",
            "proxy-connection",
            "proxy-authorization",
        ]
        .iter()
        .any(|n| h.name.eq_ignore_ascii_case(n));
        if !hop {
            head.extend_from_slice(h.name.as_bytes());
            head.extend_from_slice(b": ");
            head.extend_from_slice(h.value);
            head.extend_from_slice(b"\r\n");
        }
    }
    head.extend_from_slice(b"Connection: close\r\n\r\n");
    upstream.write_all(&head).await?;
    if !rest.is_empty() {
        upstream.write_all(&rest).await?;
    }
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Read until the request head is complete: its length, or `None` for a
/// head this proxy will not read (malformed, or over [`HEAD_MAX`]).
async fn read_head(client: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<Option<usize>> {
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 96];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(buf) {
            Ok(httparse::Status::Complete(len)) => return Ok(Some(len)),
            Ok(httparse::Status::Partial) if buf.len() < HEAD_MAX => continue,
            _ => return Ok(None),
        }
    }
}

/// `host:port` or `[v6]:port` → the port.
fn authority_port(authority: &str) -> Option<u16> {
    let (_, port) = authority.rsplit_once(':')?;
    port.parse().ok()
}

async fn dial(addr: std::net::SocketAddr) -> Option<TcpStream> {
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .ok()?
        .ok()
}

async fn refuse(client: &mut TcpStream, status: u16, why: &str) -> std::io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        502 => "Bad Gateway",
        _ => "Forbidden",
    };
    let body = format!("veles guest browser: {why}\n");
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(resp.as_bytes()).await?;
    client.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A proxy on its own listener, for this test's runtime.
    async fn proxy() -> u16 {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        crate::rt::spawn(serve(l));
        port
    }

    async fn send(port: u16, raw: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(raw.as_bytes()).await.unwrap();
        let mut out = String::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            s.read_to_string(&mut out),
        )
        .await;
        out
    }

    /// A server on loopback that says whether anything reached it.
    async fn witness() -> (u16, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = hit.clone();
        crate::rt::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                h.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nsecret")
                    .await;
            }
        });
        (port, hit)
    }

    /// The point of the proxy: loopback, the LAN, link-local and the
    /// node's WireGuard peers are refused, by CONNECT and in the clear,
    /// and the server behind them is never reached.
    #[tokio::test]
    async fn private_destinations_are_refused_and_never_dialed() {
        let p = proxy().await;
        let (wport, hit) = witness().await;
        // Loopback on a port the witness listens on, in the clear: the
        // port is not a web port, so it is refused before any lookup...
        let out = send(
            p,
            &format!("GET http://127.0.0.1:{wport}/ HTTP/1.1\r\nHost: x\r\n\r\n"),
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        // ...and on web ports, refused for the address itself.
        for raw in [
            "CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\n\r\n",
            "CONNECT [::1]:443 HTTP/1.1\r\n\r\n",
            "CONNECT 10.0.30.1:443 HTTP/1.1\r\n\r\n",
            "CONNECT 192.168.1.1:80 HTTP/1.1\r\n\r\n",
            "CONNECT 169.254.169.254:80 HTTP/1.1\r\n\r\n",
            "CONNECT localhost:8080 HTTP/1.1\r\n\r\n",
            "GET http://127.0.0.1/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            "GET http://192.168.1.1:8080/admin HTTP/1.1\r\nHost: 192.168.1.1\r\n\r\n",
        ] {
            let out = send(p, raw).await;
            assert!(out.starts_with("HTTP/1.1 403"), "{raw:?} → {out}");
            assert!(!out.contains("secret"), "{out}");
        }
        assert!(
            !hit.load(std::sync::atomic::Ordering::SeqCst),
            "nothing reached the loopback server"
        );
    }

    #[tokio::test]
    async fn non_web_ports_schemes_and_garbage_are_refused() {
        let p = proxy().await;
        for (raw, want) in [
            ("CONNECT example.com:25 HTTP/1.1\r\n\r\n", "HTTP/1.1 403"),
            ("CONNECT example.com:22 HTTP/1.1\r\n\r\n", "HTTP/1.1 403"),
            ("GET https://example.com/ HTTP/1.1\r\n\r\n", "HTTP/1.1 403"),
            ("GET ftp://example.com/ HTTP/1.1\r\n\r\n", "HTTP/1.1 403"),
            (
                "GET /relative HTTP/1.1\r\nHost: example.com\r\n\r\n",
                "HTTP/1.1 400",
            ),
            ("NOT HTTP AT ALL\r\n\r\n", "HTTP/1.1 400"),
        ] {
            let out = send(p, raw).await;
            assert!(out.starts_with(want), "{raw:?} → {out}");
        }
    }

    #[test]
    fn authority_ports_parse() {
        assert_eq!(authority_port("example.com:443"), Some(443));
        assert_eq!(authority_port("[::1]:8443"), Some(8443));
        assert_eq!(authority_port("example.com"), None);
    }
}
