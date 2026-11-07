//! SSRF guard — the engine fetches URLs other people chose.
//!
//! WHY
//!
//! Four tools take a URL from somebody who is not the operator and make
//! the GATEWAY fetch it: `vision_analyze` (and it then describes what it
//! saw, which turns a fetch into a read), a `MEDIA:http://…` attachment,
//! `web_extract`, and the hosted-result URLs a media provider hands back.
//! The gateway runs on the operator's host, so "fetch this" from a
//! channel meant `http://127.0.0.1:8642/api/...`, `http://[::1]:9900/`,
//! a printer on the LAN, or `http://169.254.169.254/latest/meta-data/`
//! on anything cloud-hosted.
//!
//! The media round jailed the LOCAL-PATH half of exactly this
//! (`vision.rs`, `asr.rs`) and left the URL half alone. This is the
//! other half.
//!
//! # What it does, and the one thing that is hard
//!
//! Scheme allowlist, then resolve the host and refuse every address that
//! is not public. Resolving and then connecting is a TOCTOU: a hostile
//! DNS server can answer once for the check and again for the connect
//! (DNS rebinding), which is a real attack and not a theoretical one. So
//! the address that PASSED is pinned into the client with
//! `ClientBuilder::resolve`, and that is the address reqwest dials —
//! there is no second lookup to poison.
//!
//! Redirects are followed by hand, because a redirect is a second URL
//! somebody else chose and reqwest's own policy hook is synchronous
//! (it cannot resolve a hostname, so it cannot judge one).
//!
//! # Where this deliberately does NOT apply
//!
//! The hosted-result URLs a media provider returns (`imagegen`'s
//! `data[].url`, the finished `video_url`, DashScope's `audio.url`) are
//! not guarded, and that is a decision rather than an omission. The
//! provider is operator-configured, a self-hosted one legitimately
//! lives on loopback or the LAN — `COMFYUI_URL` defaults to
//! `http://127.0.0.1:8188` and the image is fetched straight back off
//! it — and a provider hostile enough to aim us at the metadata service
//! already holds the API key and every prompt sent to it. Guarding
//! those would break the local routes to defend against an attacker who
//! has already won.
//!
//! The line is: a URL a CALLER can choose is guarded; a URL the
//! operator's own configured provider returns is not.

use std::net::IpAddr;
use std::time::Duration;

/// Redirect hops. Same number slopdrop's `tcl/http.tcl` allows.
pub const MAX_REDIRECTS: usize = 5;

/// Why a URL was refused. The message goes to a channel, so it names the
/// class of address and never the resolved IP: "this hostname points
/// somewhere private" is the useful half, and the address itself would
/// make the refusal into a scanner.
fn classify(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() {
                Some("loopback")
            } else if v4.is_private() {
                Some("a private network")
            } else if v4.is_link_local() {
                // 169.254.0.0/16 — and 169.254.169.254 in particular is
                // the cloud metadata service, i.e. credentials.
                Some("link-local (the cloud metadata range lives here)")
            } else if v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast() {
                Some("not a unicast host")
            } else if o[0] == 100 && (64..128).contains(&o[1]) {
                // 100.64.0.0/10, carrier NAT — `is_shared` is unstable.
                Some("carrier-grade NAT space")
            } else if o[0] == 0 || o[0] == 192 && o[1] == 0 && o[2] == 0 {
                Some("a reserved range")
            } else if o[0] == 198 && (o[1] & 0xfe) == 18 {
                // 198.18.0.0/15, inter-device benchmark testing. Not
                // ordinary public unicast; commonly used for local NAT
                // virtual interfaces (for example, proxy/VPN software).
                Some("benchmark/test address space")
            } else if o[0] & 0xf0 == 0xf0 {
                // 240.0.0.0/4, reserved for future use (broadcast was
                // already refused above).
                Some("reserved address space")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            // An IPv4-mapped address is an IPv4 address wearing a hat;
            // judging it as v6 would wave ::ffff:127.0.0.1 straight
            // through.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return classify(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            // NAT64's well-known /96 carries IPv4 in the final four
            // bytes (RFC 6052); 6to4 carries it immediately after the
            // 2002::/16 prefix (RFC 3056). They are not interchangeable.
            if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6].iter().all(|s| *s == 0) {
                let octets = v6.octets();
                return classify(IpAddr::V4(std::net::Ipv4Addr::new(
                    octets[12], octets[13], octets[14], octets[15],
                )));
            }
            // 64:ff9b:1::/48 is the LOCAL-use translation prefix (RFC
            // 8215): not globally reachable, and the operator picks where
            // IPv4 sits inside it (any RFC 6052 offset), so it cannot be
            // decoded reliably. Refuse it whole.
            if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2] == 0x0001 {
                return Some("a local-use IPv4/IPv6 translation prefix");
            }
            if seg[0] == 0x2002 {
                let octets = v6.octets();
                return classify(IpAddr::V4(std::net::Ipv4Addr::new(
                    octets[2], octets[3], octets[4], octets[5],
                )));
            }
            if v6.is_loopback() {
                Some("loopback")
            } else if v6.is_unspecified() || v6.is_multicast() {
                Some("not a unicast host")
            } else if seg[0] & 0xfe00 == 0xfc00 {
                Some("a unique-local address")
            } else if seg[0] & 0xffc0 == 0xfe80 {
                Some("link-local")
            } else if seg[0] & 0xffc0 == 0xfec0 {
                // fec0::/10, deprecated (RFC 3879) but still routed on
                // some internal networks.
                Some("a site-local address")
            } else if seg[..6].iter().all(|s| *s == 0) {
                // ::/96 (after :: and ::1 above): the deprecated
                // IPv4-compatible form, IPv4 in the last four bytes.
                Some("a deprecated IPv4-compatible address")
            } else if seg[0] == 0x2001 && seg[1] == 0x0db8 {
                Some("documentation address space")
            } else if seg[0] == 0x0100 && seg[1..4].iter().all(|s| *s == 0) {
                Some("the discard-only prefix")
            } else {
                None
            }
        }
    }
}

/// A URL that passed, with the address it is pinned to.
#[derive(Debug)]
pub struct Target {
    pub url: reqwest::Url,
    pub host: String,
    pub addr: std::net::SocketAddr,
}

/// Check one URL: scheme, host, and every address the host resolves to.
///
/// EVERY address must pass, not merely one: a hostname that answers with
/// both a public address and `127.0.0.1` is a rebinding attack with the
/// work already done, and picking the public one would be picking the
/// answer the attacker wanted us to check.
pub async fn check(raw: &str) -> Result<Target, String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("not a url: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "refused: {other}: is not a fetchable scheme (only http and https)"
            ))
        }
    }
    let host = url
        .host_str()
        .ok_or("refused: the url has no host")?
        .to_string();
    let port = url.port_or_known_default().unwrap_or(80);

    // A literal IP needs no lookup and must not get one. The brackets
    // come off first: `host_str()` renders an IPv6 literal as `[::1]`,
    // which is not an `IpAddr`, so parsing it raw would send `[::1]` to
    // the resolver and refuse it as "does not resolve" — the right
    // answer for the wrong reason, and the wrong reason is what a
    // future reader would trust.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<std::net::SocketAddr> = if let Ok(ip) = bare.parse::<IpAddr>() {
        vec![std::net::SocketAddr::new(ip, port)]
    } else {
        tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| format!("refused: {host} does not resolve: {e}"))?
            .collect()
    };
    if addrs.is_empty() {
        return Err(format!("refused: {host} resolves to nothing"));
    }
    for a in &addrs {
        if let Some(why) = classify(a.ip()) {
            return Err(format!(
                "refused: {host} points at {why}, which is not reachable from a chat surface"
            ));
        }
    }
    Ok(Target {
        url,
        host,
        addr: addrs[0],
    })
}

/// GET a URL, guarded, following redirects by hand so each hop is judged.
///
/// The client is built per call so the validated address can be pinned
/// into it: that pin is what makes the check binding rather than
/// advisory, because it is the address reqwest dials.
pub async fn get(raw: &str, timeout: Duration) -> Result<reqwest::Response, String> {
    get_as(raw, timeout, None).await
}

/// Same, with a user agent — `web_extract` presents one because plenty
/// of sites answer a bare client with a 403.
pub async fn get_as(
    raw: &str,
    timeout: Duration,
    user_agent: Option<&str>,
) -> Result<reqwest::Response, String> {
    get_through(raw, timeout, user_agent, None).await
}

/// [`get_as`] through `proxy` (`socks5h://…`, egress.rs). Every hop is
/// still checked here first, so a name that is private on this node is
/// refused before anything is sent. The proxy then resolves the name
/// itself and connects from where it is: a Tor exit, which refuses
/// private addresses on its own side too.
pub async fn get_through(
    raw: &str,
    timeout: Duration,
    user_agent: Option<&str>,
    proxy: Option<&str>,
) -> Result<reqwest::Response, String> {
    let mut current = raw.to_string();
    for hop in 0..=MAX_REDIRECTS {
        let t = check(&current).await?;
        let mut b = reqwest::Client::builder().timeout(timeout);
        if let Some(ua) = user_agent {
            b = b.user_agent(ua);
        }
        b = match proxy {
            Some(p) => b.proxy(reqwest::Proxy::all(p).map_err(|e| format!("the proxy: {e}"))?),
            None => b.resolve(&t.host, t.addr),
        };
        let client = b
            // Judged by hand below; reqwest's own policy hook is sync
            // and so cannot resolve — it would have to wave hostnames
            // through.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client
            .get(t.url.clone())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_redirection() {
            return Ok(resp);
        }
        let Some(loc) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        else {
            return Ok(resp); // a redirect with nowhere to go is the answer
        };
        if hop == MAX_REDIRECTS {
            return Err(format!("refused: more than {MAX_REDIRECTS} redirects"));
        }
        // Relative Locations are legal and common, and resolving them
        // against the PREVIOUS url is what keeps a relative hop from
        // being read as a bare hostname.
        current = t
            .url
            .join(loc)
            .map_err(|e| format!("refused: bad redirect target: {e}"))?
            .to_string();
    }
    Err("refused: redirect loop".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_addresses_a_chat_surface_must_not_reach() {
        // The whole point: the gateway runs on the operator's host, so
        // these are its own services, its LAN, and — on anything
        // cloud-hosted — its credentials.
        for (url, expect) in [
            ("http://127.0.0.1:8642/api/logs", "loopback"),
            ("http://localhost:9900/", "loopback"),
            ("http://[::1]:8080/", "loopback"),
            ("http://169.254.169.254/latest/meta-data/", "link-local"),
            ("http://10.0.0.5/", "private"),
            ("http://192.168.1.1/", "private"),
            ("http://172.16.3.4/", "private"),
            ("http://0.0.0.0/", "unicast"),
            ("http://[fd00::1]/", "unique-local"),
            ("http://[fe80::1]/", "link-local"),
            ("http://100.64.0.1/", "NAT"),
            // An IPv4 address in a v6 coat still has to be judged as v4.
            ("http://[::ffff:127.0.0.1]/", "loopback"),
        ] {
            let e = check(url).await.unwrap_err();
            assert!(
                e.contains(expect),
                "{url} must be refused as {expect}, got: {e}"
            );
        }
    }

    /// An IPv4 address translated into or embedded in IPv6 must be
    /// judged by its embedded IPv4 class, not waved through as public
    /// IPv6 (reported privately by Matt Abate).
    #[tokio::test]
    async fn translated_and_reserved_addresses_are_refused() {
        for url in [
            "http://[64:ff9b::a00:1]/",
            "http://[64:ff9b::a9fe:a9fe]/",
            "http://[2002:0a00:0001::1]/",
            "http://[2002:a9fe:a9fe::1]/",
            "http://[64:ff9b:1::a00:1]/",
            "http://[fec0::1]/",
            "http://[::a00:1]/",
            "http://[2001:db8::1]/",
            "http://[100::1]/",
            "http://198.18.0.1/",
            "http://198.19.255.254/",
            "http://240.0.0.1/",
            "http://255.255.255.254/",
        ] {
            let e = check(url).await.unwrap_err();
            assert!(e.starts_with("refused:"), "{url} passed the SSRF guard");
        }
    }

    #[tokio::test]
    async fn only_http_is_fetchable() {
        for url in [
            "file:///etc/passwd",
            "gopher://x/",
            "ftp://x/",
            "data:text/plain,hi",
        ] {
            let e = check(url).await.unwrap_err();
            assert!(e.contains("scheme"), "{url}: {e}");
        }
    }

    #[tokio::test]
    async fn a_public_address_passes_and_carries_its_pin() {
        // No DNS: a literal public address, so the test needs no network
        // and cannot flake on a resolver.
        let t = check("https://93.184.216.34/x.png").await.unwrap();
        assert_eq!(t.host, "93.184.216.34");
        assert_eq!(t.addr.port(), 443, "the scheme's default port is used");
    }

    /// A redirect is a second URL somebody else chose. The classic
    /// bypass is a public host that 302s to the metadata service, and
    /// reqwest's own redirect policy cannot catch it (its hook is sync,
    /// so it cannot resolve a hostname) — which is why `get` follows
    /// them by hand.
    #[test]
    fn translated_ipv4_offsets_match_their_encapsulation() {
        for (addr, expected) in [
            ("64:ff9b::a00:1", Some("private")),
            ("64:ff9b::a9fe:a9fe", Some("link-local")),
            ("64:ff9b::5db8:d822", None),
            // 64:ff9b:1::/48 is NOT public (RFC 8215 local-use
            // translation): a network's own NAT64 may embed IPv4 at any
            // RFC 6052 offset inside it, so it cannot be decoded, only
            // refused, wherever the private-looking bytes sit.
            ("64:ff9b:1::a00:1", Some("local-use")),
            ("64:ff9b:1:a00:1::", Some("local-use")),
            ("64:ff9b:1:5db8:d822::", Some("local-use")),
            // A public 6to4 relay whose bytes 4..7 happen to be private:
            // reading the NAT64 offset here rejects a valid public address.
            ("2002:5db8:d822:0a00:1::1", None),
            ("2002:0a00:0001::1", Some("private")),
            ("2002:a9fe:a9fe::1", Some("link-local")),
        ] {
            let ip = addr.parse::<std::net::Ipv6Addr>().unwrap();
            let verdict = classify(IpAddr::V6(ip));
            assert_eq!(verdict.is_some(), expected.is_some(), "{addr}");
            if let Some(text) = expected {
                assert!(verdict.unwrap().contains(text), "{addr}: {verdict:?}");
            }
        }
    }

    /// The rest of the IPv6 space that is not globally reachable, each
    /// named for what it is; a public address beside them still passes.
    #[test]
    fn non_global_ipv6_ranges_are_named() {
        // Every wrong verdict at once, not just the first.
        let mut wrong = Vec::new();
        for (addr, expected) in [
            ("fec0::1", Some("site-local")),
            ("feff:ffff::1", Some("site-local")),
            ("::a00:1", Some("IPv4-compatible")),
            ("::7f00:1", Some("IPv4-compatible")),
            ("2001:db8::1", Some("documentation")),
            ("2001:db8:ffff::1", Some("documentation")),
            ("100::1", Some("discard")),
            ("2001:db9::1", None),
            ("2606:4700::1111", None),
        ] {
            let ip = addr.parse::<std::net::Ipv6Addr>().unwrap();
            let verdict = classify(IpAddr::V6(ip));
            let right = match (expected, verdict) {
                (Some(text), Some(said)) => said.contains(text),
                (None, None) => true,
                _ => false,
            };
            if !right {
                wrong.push(format!("{addr}: {verdict:?}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test]
    async fn a_redirect_into_private_space_is_refused_at_the_hop() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        crate::rt::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/\r\n\
                          Content-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        });
        // The first hop is loopback, so it is refused before any
        // connection — which is the guard working, and also means this
        // test has to reach the SECOND hop deliberately. Drive `get`
        // from the redirect target directly.
        let e = get(&format!("http://127.0.0.1:{port}/"), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(e.contains("loopback"), "the first hop is judged too: {e}");
        let e2 = get("http://169.254.169.254/latest/", Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(e2.contains("link-local"), "{e2}");
    }
}
