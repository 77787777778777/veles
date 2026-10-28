//! Where a web call leaves from, for a session that is not the owner's
//! (PLAN-challenge-relay.md D5, D6).
//!
//! The owner's searches, fetches and browser leave from wherever the
//! owner's tools are homed: on this pool, the laptop and its home
//! address, which is what the USER wants. Anyone else's must not wear
//! that address down (USER, 2026-10-11). `[web] others` says, per node,
//! how this node carries them:
//!
//! - `direct` (the default): like the owner's. A single-node install
//!   has nowhere else to send them.
//! - `proxy`: through `[web] others_proxy` (fetches) and
//!   `others_searxng_url` (searches), and the call stays on this node
//!   whatever `[tools.home]` says. The pool's VPS: both are its Tor.
//! - `refuse`: not from this node at all. The laptop. With `[web]
//!   others_via` set, the calls go to that node instead (the VPS).
//!
//! The owner's own session can go the same way on purpose (`/egress
//! tor`, [`chosen`]): "I should still be able to run through sandbox/tor
//! to debug other users' requests" (USER, 2026-10-13). By default it
//! does not: "my own requests should bypass sandboxing/tor".
//!
//! A call learns whose it is from [`scope`], which the tool registry
//! enters around every call it runs for a session (local or from a
//! peer); the web and browser tools read it with [`others`].

use std::future::Future;
use std::pin::Pin;

/// How this node carries web traffic for a session that is not the
/// owner's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Others {
    /// This node's own connection, as for the owner.
    Direct,
    /// Through these, and the call stays on this node.
    Proxy {
        proxy: Option<String>,
        searxng: Option<String>,
    },
    /// Not from this node.
    Refuse,
}

impl Others {
    /// `[web] others` and its two addresses. A word this node does not
    /// know refuses: a typo must not send a stranger's traffic out of
    /// the owner's address.
    pub fn from_web(web: &crate::config::WebSection) -> Self {
        let set = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        match web.others.trim().to_ascii_lowercase().as_str() {
            "" | "direct" => Others::Direct,
            "proxy" => Others::Proxy {
                proxy: set(&web.others_proxy),
                searxng: set(&web.others_searxng_url),
            },
            _ => Others::Refuse,
        }
    }
}

/// This node's `[web] others`, as configured now.
pub fn mode() -> Others {
    Others::from_web(&crate::config::live().web)
}

/// The calls whose traffic `[web] others` governs: the searches and
/// fetches, and every browser call (the guest browser, and since D3 the
/// `browser_*` tools of anyone but the owner).
pub fn governs(tool: &str) -> bool {
    matches!(tool, "web_search" | "web_extract") || tool.starts_with("browser_")
}

tokio::task_local! {
    static OTHERS: String;
}

/// Run `fut` as a call for `session`. A session that is not the owner's
/// puts its id in scope, for the web tools to read ([`others`]); so does
/// the owner's when `as_others` says it goes their way ([`chosen`]).
///
/// Boxed and type-erased, and not an `async fn`: the tool dispatch runs
/// inside it, and one more layer of nested future types took a caller
/// (a cron job's turn) past the compiler's query depth limit.
pub fn scope<'a, T: 'a>(
    session: &str,
    as_others: bool,
    fut: impl Future<Output = T> + Send + 'a,
) -> Pin<Box<dyn Future<Output = T> + Send + 'a>> {
    if !session.is_empty() && (as_others || !crate::ask::owner_session(session)) {
        Box::pin(OTHERS.scope(session.to_string(), fut))
    } else {
        Box::pin(fut)
    }
}

/// The session this call runs for, when it goes the way other people's
/// calls go: not the owner's, or the owner's sent that way.
pub fn others() -> Option<String> {
    OTHERS.try_with(Clone::clone).ok()
}

/// Where a call to `tool` goes, made in the current [`scope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Wherever its home says, as the owner's would.
    Home,
    /// This node, whatever its home says: someone else's call on a node
    /// that sends their traffic through its proxy. Followed home, a
    /// search of theirs went to the laptop and left from the USER's
    /// address.
    Here,
    /// That node (`[web] others_via`), from a node that carries none of
    /// it itself.
    Via(String),
}

/// [`Route`] for `tool`, by this node's `[web] others`.
pub fn route(tool: &str) -> Route {
    if others().is_none() || !governs(tool) {
        return Route::Home;
    }
    match mode() {
        Others::Proxy { .. } => Route::Here,
        Others::Refuse => via().map_or(Route::Home, Route::Via),
        Others::Direct => Route::Home,
    }
}

/// `[web] others_via`: the node this one sends other people's web calls
/// to, when it carries none of them itself.
pub fn via() -> Option<String> {
    crate::config::live()
        .web
        .others_via
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ── the owner's own choice: `/egress` ────────────────────────────────

/// The meta row that holds a session's `/egress` choice. Absent: home.
fn meta_key(session: &str) -> String {
    format!("egress:{session}")
}

/// Whether `session` chose to go the way other people's sessions go
/// (`/egress tor`). Only the owner's choice changes anything: everyone
/// else goes that way regardless.
pub fn chosen(store: &crate::store::Store, session: &str) -> bool {
    !session.is_empty() && store.meta_get(&meta_key(session)).as_deref() == Some("tor")
}

/// `/egress [tor|home]` for `session`: bare reports, a word sets it.
pub fn command(
    store: &crate::store::Store,
    session: &str,
    word: Option<&str>,
) -> anyhow::Result<String> {
    if session.is_empty() {
        anyhow::bail!("/egress needs a session");
    }
    let owner = crate::ask::owner_session(session);
    let tor = match word.map(|w| w.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") => return Ok(describe(owner, chosen(store, session))),
        Some("tor" | "others" | "on") => true,
        Some("home" | "owner" | "off") => false,
        Some(other) => anyhow::bail!("unknown /egress choice '{other}' (tor|home)"),
    };
    if !owner {
        return Ok(describe(false, false));
    }
    if tor {
        store.meta_set(&meta_key(session), "tor")?;
    } else {
        store.meta_delete(&meta_key(session))?;
    }
    Ok(describe(true, tor))
}

/// Where this session's web calls leave, in words.
fn describe(owner: bool, tor: bool) -> String {
    let theirs = format!(
        "searches, fetches and browser go the way other people's do: {}",
        theirs_here()
    );
    match (owner, tor) {
        (false, _) => format!(
            "This session is not the owner's: its {theirs}. Only the owner's sessions \
             can choose."
        ),
        (true, true) => format!("/egress tor: this session's {theirs}. `/egress home` to go back."),
        (true, false) => "/egress home: this session's searches, fetches and browser leave \
             the way the owner's do, from the owner's own address and with no sandbox. \
             `/egress tor` sends them the way other people's go."
            .into(),
    }
}

/// Where other people's web calls go from this node, in words.
fn theirs_here() -> String {
    match mode() {
        Others::Proxy { .. } => "through this node's proxy (Tor, on the pool's VPS), and the \
             browser in a sandbox box of the session's own"
            .into(),
        Others::Refuse => match via() {
            Some(node) => format!(
                "to node {node} ([web] others_via), which sends them through its proxy, \
                 and the browser in a sandbox box there"
            ),
            None => "nowhere: this node refuses them ([web] others = \"refuse\")".into(),
        },
        Others::Direct => "from this node's own address, like the owner's ([web] others = \
             \"direct\"), and no browser"
            .into(),
    }
}

/// What a refused call answers.
pub fn refusal(tool: &str) -> String {
    format!(
        "{tool} error: this node does not carry web traffic for anyone but its owner \
         ([web] others = \"refuse\"). Other people's searches and fetches leave from a \
         node that sends them through its proxy."
    )
}

/// What a `browser_*` call answers where it may not run for this
/// session: the browser runs for other people only in their own box, on
/// a node that sends their traffic through its proxy (D3).
pub fn browser_refusal() -> String {
    match mode() {
        Others::Refuse => format!("error: {}", refusal("the browser")),
        _ => "error: the browser runs for anyone but the owner only in their own sandbox \
              box, on a node whose [web] others is \"proxy\"; this node's is not"
            .into(),
    }
}

/// What a `proxy` node without the address a call needs answers.
pub fn unset(tool: &str, key: &str) -> String {
    format!(
        "{tool} error: [web] others = \"proxy\" on this node, but [web] {key} is not set, \
         so there is no way out for this call that keeps it off this node's own address"
    )
}

/// The proxy a fetch made in the current [`scope`] goes through: `None`
/// for the owner's (and on a `direct` node), the session's own name on
/// `[web] others_proxy` on a `proxy` node. `Err`: the call's answer, a
/// refusal by `tool`'s name.
pub fn fetch_proxy(tool: &str) -> Result<Option<String>, String> {
    let Some(session) = others() else {
        return Ok(None);
    };
    match mode() {
        Others::Direct => Ok(None),
        Others::Refuse => Err(refusal(tool)),
        Others::Proxy { proxy: None, .. } => Err(unset(tool, "others_proxy")),
        Others::Proxy { proxy: Some(p), .. } => proxy_for(&p, &session)
            .map(Some)
            .map_err(|e| format!("{tool} error: {e}")),
    }
}

/// `proxy` with a user name of `session`'s own, so that a Tor SOCKS port
/// with `IsolateSOCKSAuth` puts each session on its own circuit: two
/// people's fetches never share an exit by construction. The name is a
/// hash, so it fits SOCKS5's 255 bytes and needs no URL escaping.
pub fn proxy_for(proxy: &str, session: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(proxy.trim())
        .map_err(|e| format!("[web] others_proxy is not a URL: {e}"))?;
    let tag = blake3::hash(session.as_bytes()).to_hex();
    url.set_username(&format!("veles-{}", &tag.as_str()[..16]))
        .map_err(|()| "[web] others_proxy cannot carry a user name".to_string())?;
    url.set_password(Some("x"))
        .map_err(|()| "[web] others_proxy cannot carry a password".to_string())?;
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::{Principal, Role};

    fn web(others: &str, proxy: Option<&str>, searxng: Option<&str>) -> crate::config::WebSection {
        crate::config::WebSection {
            others: others.into(),
            others_proxy: proxy.map(str::to_string),
            others_searxng_url: searxng.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn the_modes_parse_and_an_unknown_word_refuses() {
        assert_eq!(Others::from_web(&web("direct", None, None)), Others::Direct);
        assert_eq!(Others::from_web(&web("", None, None)), Others::Direct);
        assert_eq!(Others::from_web(&web("Refuse", None, None)), Others::Refuse);
        assert_eq!(
            Others::from_web(&web("proxy", Some(" socks5h://10.0.30.1:9050 "), Some(""))),
            Others::Proxy {
                proxy: Some("socks5h://10.0.30.1:9050".into()),
                searxng: None,
            }
        );
        assert_eq!(
            Others::from_web(&web("tor please", None, None)),
            Others::Refuse,
            "a typo must not fall back to the owner's address"
        );
    }

    #[test]
    fn the_default_is_direct() {
        let cfg: crate::config::VelesConfig = toml::from_str("").unwrap();
        assert_eq!(Others::from_web(&cfg.web), Others::Direct);
    }

    #[tokio::test]
    async fn only_a_session_that_is_not_the_owners_is_in_scope() {
        crate::acl::set_session_principal(
            "egress-guest",
            Principal::known("irc:guest", Role::User),
        );
        crate::acl::set_session_principal(
            "egress-owner",
            Principal::known("irc:owner", Role::Owner),
        );
        assert_eq!(
            scope("egress-guest", false, async { others() })
                .await
                .as_deref(),
            Some("egress-guest")
        );
        assert_eq!(scope("egress-owner", false, async { others() }).await, None);
        assert_eq!(scope("", false, async { others() }).await, None);
        assert_eq!(
            others(),
            None,
            "outside a scope a call is nobody's in particular"
        );
    }

    // ── the web tools, under each mode ───────────────────────────────

    use crate::tools::Tool as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// A scratch home whose config.toml says `body`, read live at once.
    fn config(home: &tempfile::TempDir, body: &str) {
        std::fs::write(home.path().join("config.toml"), body).expect("config");
        crate::config::invalidate_live();
    }

    /// An HTTP server answering every request with `body` as JSON.
    async fn fake_searxng(body: &'static str) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        crate::rt::spawn(async move {
            while let Ok((mut sock, _)) = l.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        format!("http://{addr}")
    }

    /// A SOCKS5 proxy that serves one page itself, and records the user
    /// name and the destination of every connection it is asked for.
    async fn fake_socks() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let log = seen.clone();
        crate::rt::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let log = log.clone();
                crate::rt::spawn(async move {
                    let mut b = [0u8; 2];
                    s.read_exact(&mut b).await?;
                    let mut methods = vec![0u8; b[1] as usize];
                    s.read_exact(&mut methods).await?;
                    let mut user = String::new();
                    if methods.contains(&2) {
                        s.write_all(&[5, 2]).await?;
                        s.read_exact(&mut b).await?;
                        let mut u = vec![0u8; b[1] as usize];
                        s.read_exact(&mut u).await?;
                        let mut pl = [0u8; 1];
                        s.read_exact(&mut pl).await?;
                        let mut pw = vec![0u8; pl[0] as usize];
                        s.read_exact(&mut pw).await?;
                        user = String::from_utf8_lossy(&u).into_owned();
                        s.write_all(&[1, 0]).await?;
                    } else {
                        s.write_all(&[5, 0]).await?;
                    }
                    let mut head = [0u8; 4];
                    s.read_exact(&mut head).await?;
                    let host = match head[3] {
                        1 => {
                            let mut a = [0u8; 4];
                            s.read_exact(&mut a).await?;
                            std::net::Ipv4Addr::from(a).to_string()
                        }
                        3 => {
                            let mut n = [0u8; 1];
                            s.read_exact(&mut n).await?;
                            let mut name = vec![0u8; n[0] as usize];
                            s.read_exact(&mut name).await?;
                            String::from_utf8_lossy(&name).into_owned()
                        }
                        _ => return Ok::<(), std::io::Error>(()),
                    };
                    let mut port = [0u8; 2];
                    s.read_exact(&mut port).await?;
                    let dest = format!("{host}:{}", u16::from_be_bytes(port));
                    log.lock().unwrap().push((user, dest));
                    s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                    let mut req = [0u8; 4096];
                    let _ = s.read(&mut req).await?;
                    let page = "<html><body><p>fetched through the proxy</p></body></html>";
                    s.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
                            page.len()
                        )
                        .as_bytes(),
                    )
                    .await?;
                    Ok(())
                });
            }
        });
        (format!("socks5h://{addr}"), seen)
    }

    fn as_user(sid: &str) {
        crate::acl::set_session_principal(sid, Principal::known(&format!("irc:{sid}"), Role::User));
    }

    /// The laptop: someone else's search and fetch are refused, and say
    /// why; nothing is asked of any backend.
    #[tokio::test]
    async fn a_refusing_node_carries_nothing_for_others() {
        let (home, _guard) = crate::testenv::scratch_home();
        config(&home, "[web]\nothers = \"refuse\"\nbackends = [\"searxng\"]\nsearxng_url = \"http://127.0.0.1:0\"\n");
        as_user("egress-refused");
        let search = scope(
            "egress-refused",
            false,
            crate::webtools::WebSearchTool.call(serde_json::json!({"query": "x"})),
        )
        .await;
        assert!(search.contains("others = \"refuse\""), "{search}");
        let fetch = scope(
            "egress-refused",
            false,
            crate::webtools::WebExtractTool
                .call(serde_json::json!({"url": "http://93.184.216.34/"})),
        )
        .await;
        assert!(fetch.contains("others = \"refuse\""), "{fetch}");
        crate::acl::clear_session_principal("egress-refused");
    }

    /// The VPS: someone else's search goes to the proxy's searxng and
    /// nowhere else — not to the direct backends, which would leave from
    /// this node's own address, even when that searxng finds nothing.
    #[tokio::test]
    async fn a_proxying_node_searches_for_others_only_through_its_searxng() {
        let (home, _guard) = crate::testenv::scratch_home();
        let found = fake_searxng(
            r#"{"results":[{"title":"over tor","url":"https://example.org/t","content":"from the tor searxng"}]}"#,
        )
        .await;
        let empty = fake_searxng(r#"{"results":[]}"#).await;
        as_user("egress-search");
        for (url, expect) in [
            (found.as_str(), "over tor"),
            (empty.as_str(), "searxng: no results"),
        ] {
            config(
                &home,
                &format!(
                    "[web]\nothers = \"proxy\"\nothers_searxng_url = \"{url}\"\n\
                     backends = [\"brave\", \"ddg\"]\n"
                ),
            );
            let out = scope(
                "egress-search",
                false,
                crate::webtools::WebSearchTool.call(serde_json::json!({"query": "x"})),
            )
            .await;
            assert!(out.contains(expect), "{out}");
            assert!(
                !out.contains("brave") && !out.contains("ddg"),
                "a direct backend was asked: {out}"
            );
        }
        config(&home, "[web]\nothers = \"proxy\"\n");
        let out = scope(
            "egress-search",
            false,
            crate::webtools::WebSearchTool.call(serde_json::json!({"query": "x"})),
        )
        .await;
        assert!(out.contains("others_searxng_url is not set"), "{out}");
        crate::acl::clear_session_principal("egress-search");
    }

    /// The VPS: someone else's fetch goes through the proxy, under a user
    /// name of its session's own (its own Tor circuit), to the address
    /// the URL names; and without a proxy configured, not at all.
    #[tokio::test]
    async fn a_proxying_node_fetches_for_others_through_its_proxy() {
        let (home, _guard) = crate::testenv::scratch_home();
        let (proxy, seen) = fake_socks().await;
        config(
            &home,
            &format!("[web]\nothers = \"proxy\"\nothers_proxy = \"{proxy}\"\n"),
        );
        as_user("egress-fetch");
        let out = scope(
            "egress-fetch",
            false,
            crate::webtools::WebExtractTool
                .call(serde_json::json!({"url": "http://93.184.216.34/page"})),
        )
        .await;
        assert!(out.contains("fetched through the proxy"), "{out}");
        let seen = seen.lock().unwrap().clone();
        let want_user = proxy_for(&proxy, "egress-fetch").unwrap();
        let want_user = want_user
            .split("//")
            .nth(1)
            .unwrap()
            .split(':')
            .next()
            .unwrap();
        assert_eq!(
            seen,
            vec![(want_user.to_string(), "93.184.216.34:80".to_string())]
        );
        // A private address is still refused here, before the proxy.
        let out = scope(
            "egress-fetch",
            false,
            crate::webtools::WebExtractTool
                .call(serde_json::json!({"url": "http://10.0.30.1:9911/"})),
        )
        .await;
        assert!(out.contains("private"), "{out}");
        config(&home, "[web]\nothers = \"proxy\"\n");
        let out = scope(
            "egress-fetch",
            false,
            crate::webtools::WebExtractTool
                .call(serde_json::json!({"url": "http://93.184.216.34/"})),
        )
        .await;
        assert!(out.contains("others_proxy is not set"), "{out}");
        crate::acl::clear_session_principal("egress-fetch");
    }

    /// `vision_analyze` with a URL is a fetch like web_extract's: for
    /// someone else, through the proxy under their own name, or refused.
    #[tokio::test]
    async fn someone_elses_image_url_is_fetched_like_their_pages() {
        let (home, _guard) = crate::testenv::scratch_home();
        let (proxy, seen) = fake_socks().await;
        as_user("egress-vision");
        let tool = crate::vision::VisionAnalyzeTool {
            session_id: "egress-vision".into(),
        };
        let args =
            serde_json::json!({"image_url": "http://93.184.216.34/cat.png", "question": "?"});
        config(&home, "[web]\nothers = \"refuse\"\n");
        let out = scope("egress-vision", false, tool.call(args.clone())).await;
        assert!(out.contains("others = \"refuse\""), "{out}");
        assert!(
            !out.contains("vision_analyze error: vision_analyze error"),
            "said once: {out}"
        );
        config(
            &home,
            &format!("[web]\nothers = \"proxy\"\nothers_proxy = \"{proxy}\"\n"),
        );
        let _ = scope("egress-vision", false, tool.call(args)).await;
        let seen = seen.lock().unwrap().clone();
        let user = proxy_for(&proxy, "egress-vision").unwrap();
        let user = user.split("//").nth(1).unwrap().split(':').next().unwrap();
        assert_eq!(
            seen,
            vec![(user.to_string(), "93.184.216.34:80".to_string())]
        );
        crate::acl::clear_session_principal("egress-vision");
    }

    /// Through the registry: on a proxying node someone else's search
    /// stays here, while the owner's still follows `[tools.home]` (to a
    /// node this test does not have, which its answer names).
    #[tokio::test]
    async fn someone_elses_search_is_not_sent_home() {
        let (home, _guard) = crate::testenv::scratch_home();
        let found = fake_searxng(
            r#"{"results":[{"title":"over tor","url":"https://example.org/t","content":"c"}]}"#,
        )
        .await;
        config(
            &home,
            &format!(
                "[tools.home]\n\"web_*\" = \"node-elsewhere\"\n\n[web]\nothers = \"proxy\"\n\
                 others_searxng_url = \"{found}\"\n"
            ),
        );
        let dir = crate::testenv::kept_dir();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.join("state.db")).expect("store"),
        ));
        let (user, owner) = ("egress-reg-user", "egress-reg-owner");
        as_user(user);
        crate::acl::set_session_principal(owner, Principal::known("irc:owner", Role::Owner));
        let search = serde_json::json!({"name": "web_search", "arguments": {"query": "x"}});
        let out = crate::tools::builtin_registry(store.clone(), None, user)
            .dispatch("tool_call", search.clone())
            .await;
        assert!(out.contains("over tor"), "{out}");
        assert!(!out.contains("node-elsewhere"), "{out}");
        let out = crate::tools::builtin_registry(store, None, owner)
            .dispatch("tool_call", search)
            .await;
        assert!(
            out.contains("node-elsewhere"),
            "the owner's goes home: {out}"
        );
        crate::acl::clear_session_principal(user);
        crate::acl::clear_session_principal(owner);
    }

    /// `/egress`: the owner's session reports, chooses Tor, and comes
    /// back; nobody else's can choose, and a word it does not know is an
    /// error, never a silent change.
    #[test]
    fn the_owner_chooses_where_their_session_leaves() {
        let (home, _guard) = crate::testenv::scratch_home();
        config(&home, "[web]\nothers = \"proxy\"\n");
        let store = crate::store::Store::open(&home.path().join("t.db")).unwrap();
        let (owner, user) = ("egress-cmd-owner", "egress-cmd-user");
        as_user(user);
        let said = command(&store, owner, None).unwrap();
        assert!(said.starts_with("/egress home"), "{said}");
        assert!(!chosen(&store, owner));
        let said = command(&store, owner, Some("tor")).unwrap();
        assert!(
            said.starts_with("/egress tor") && said.contains("Tor"),
            "{said}"
        );
        assert!(chosen(&store, owner));
        assert!(command(&store, owner, None)
            .unwrap()
            .starts_with("/egress tor"));
        assert!(command(&store, owner, Some("onion")).is_err());
        assert!(chosen(&store, owner), "a bad word changed nothing");
        command(&store, owner, Some("home")).unwrap();
        assert!(!chosen(&store, owner));
        let said = command(&store, user, Some("home")).unwrap();
        assert!(said.contains("not the owner's"), "{said}");
        assert!(!chosen(&store, user));
        command(&store, user, Some("tor")).unwrap();
        assert!(!chosen(&store, user), "only the owner's choice is kept");
        crate::acl::clear_session_principal(user);
    }

    /// Where a call goes, by the scope and this node's mode.
    #[tokio::test]
    async fn a_call_is_routed_by_its_scope_and_the_nodes_mode() {
        let (home, _guard) = crate::testenv::scratch_home();
        as_user("egress-route");
        let routed = |tool: &'static str| scope("egress-route", false, async move { route(tool) });
        config(
            &home,
            "[web]\nothers = \"proxy\"\nothers_via = \"node-vps\"\n",
        );
        assert_eq!(routed("web_search").await, Route::Here);
        assert_eq!(routed("browser_navigate").await, Route::Here);
        assert_eq!(routed(crate::browsertools::GUEST_OP).await, Route::Here);
        assert_eq!(routed("terminal").await, Route::Home, "not a web call");
        assert_eq!(route("web_search"), Route::Home, "the owner's");
        config(
            &home,
            "[web]\nothers = \"refuse\"\nothers_via = \"node-vps\"\n",
        );
        assert_eq!(routed("web_extract").await, Route::Via("node-vps".into()));
        assert_eq!(routed("browser_click").await, Route::Via("node-vps".into()));
        assert_eq!(route("web_extract"), Route::Home, "the owner's");
        config(&home, "[web]\nothers = \"refuse\"\nothers_via = \" \"\n");
        assert_eq!(
            routed("web_extract").await,
            Route::Home,
            "no via: refused at the tool"
        );
        config(
            &home,
            "[web]\nothers = \"direct\"\nothers_via = \"node-vps\"\n",
        );
        assert_eq!(routed("web_extract").await, Route::Home);
        crate::acl::clear_session_principal("egress-route");
    }

    /// Through the registry, the owner's session with `/egress tor`: on a
    /// proxying node its search stays here and goes over the proxy's
    /// searxng; on a refusing node with `others_via` it is sent to that
    /// node, as someone else's is. Without the choice it goes home.
    #[tokio::test]
    async fn the_owners_tor_choice_goes_the_way_others_calls_go() {
        let (home, _guard) = crate::testenv::scratch_home();
        let found = fake_searxng(
            r#"{"results":[{"title":"over tor","url":"https://example.org/t","content":"c"}]}"#,
        )
        .await;
        let dir = crate::testenv::kept_dir();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.join("state.db")).expect("store"),
        ));
        let (owner, user) = ("egress-tor-owner", "egress-tor-user");
        as_user(user);
        let search = serde_json::json!({"name": "web_search", "arguments": {"query": "x"}});
        let ask = |sid: &'static str| {
            let reg = crate::tools::builtin_registry(store.clone(), None, sid);
            let search = search.clone();
            async move { reg.dispatch("tool_call", search).await }
        };
        config(
            &home,
            &format!(
                "[tools.home]\n\"web_*\" = \"node-home\"\n\n[web]\nothers = \"proxy\"\n\
                 others_searxng_url = \"{found}\"\n"
            ),
        );
        assert!(
            ask(owner).await.contains("node-home"),
            "the owner's goes home"
        );
        command(&store.lock().unwrap(), owner, Some("tor")).unwrap();
        let out = ask(owner).await;
        assert!(
            out.contains("over tor") && !out.contains("node-home"),
            "{out}"
        );
        config(
            &home,
            "[tools.home]\n\"web_*\" = \"node-home\"\n\n[web]\nothers = \"refuse\"\n\
             others_via = \"node-via\"\n",
        );
        for sid in [owner, user] {
            let out = ask(sid).await;
            assert!(
                out.contains("node-via") && !out.contains("node-home"),
                "{sid}: {out}"
            );
        }
        command(&store.lock().unwrap(), owner, Some("home")).unwrap();
        assert!(ask(owner).await.contains("node-home"), "back home");
        crate::acl::clear_session_principal(user);
    }

    /// A call from another node runs as the caller's session too, and
    /// whose it is comes from the principal it CARRIES. On a refusing
    /// node (the laptop) a peer's call for someone other than the owner
    /// is refused; the owner's runs, whether it came with an owner ticket
    /// or unasked and capped at user level, which is how the owner's own
    /// searches from IRC arrive. The D5 cut read the capped level and
    /// refused those (live 2026-10-13). The owner's call sent the way
    /// other people's go (`/egress tor`) is refused here like theirs.
    #[tokio::test]
    async fn a_peers_call_is_the_owners_only_when_it_carries_the_owner() {
        use crate::remotetool::{capped, execute, Caller, Grant, RemoteToolAsk};
        let (home, _guard) = crate::testenv::scratch_home();
        let found = fake_searxng(
            r#"{"results":[{"title":"from home","url":"https://example.org/h","content":"c"}]}"#,
        )
        .await;
        config(
            &home,
            &format!(
                "[web]\nothers = \"refuse\"\nbackends = [\"searxng\"]\nsearxng_url = \"{found}\"\n\
                 searxng_wake_cmd = \"\"\n"
            ),
        );
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&home.path().join("t.db")).unwrap(),
        ));
        let caller = Caller {
            node: Some("node-vps"),
            name: "vps",
        };
        let ask = |principal: Option<Principal>, others: bool| RemoteToolAsk {
            from_node: "node-vps".into(),
            session: "irc-session".into(),
            tool: "web_search".into(),
            args: serde_json::json!({"query": "x"}),
            principal: principal.map(|p| serde_json::to_string(&p).unwrap()),
            ticket: None,
            wire_task_id: None,
            others,
        };
        let run = |a: RemoteToolAsk, g: Grant| {
            let store = store.clone();
            let caller = &caller;
            async move {
                let v = execute(store, caller, &a, &g, crate::providers::CancelToken::new()).await;
                v["output"].as_str().unwrap_or_default().to_string()
            }
        };
        let unasked = |p: Option<Principal>| {
            Grant::Unasked(capped(&p.unwrap_or_else(Principal::owner), &caller))
        };
        let owner_ticket = || Grant::Ticket {
            role: Role::Owner,
            signer: "s".into(),
            id: "t".into(),
        };
        let irc_owner = Principal::known("irc:owner", Role::Owner);
        let irc_user = Principal::known("irc:someone", Role::User);
        for (what, a, g, runs) in [
            (
                "an owner surface, unasked",
                ask(None, false),
                unasked(None),
                true,
            ),
            (
                "the owner's IRC session, unasked",
                ask(Some(irc_owner.clone()), false),
                unasked(Some(irc_owner.clone())),
                true,
            ),
            ("an owner ticket", ask(None, false), owner_ticket(), true),
            (
                "someone else's, unasked",
                ask(Some(irc_user.clone()), false),
                unasked(Some(irc_user)),
                false,
            ),
            (
                "the owner's /egress tor, unasked",
                ask(None, true),
                unasked(None),
                false,
            ),
            (
                "the owner's /egress tor, ticketed",
                ask(None, true),
                owner_ticket(),
                false,
            ),
        ] {
            let out = run(a, g).await;
            if runs {
                assert!(out.contains("from home"), "{what} runs: {out}");
            } else {
                assert!(
                    out.contains("others = \"refuse\""),
                    "{what} is refused: {out}"
                );
            }
        }
    }

    #[test]
    fn each_session_gets_its_own_socks_user() {
        let a = proxy_for("socks5h://10.0.30.1:9050", "session-a").unwrap();
        let b = proxy_for("socks5h://10.0.30.1:9050", "session-b").unwrap();
        assert!(a.starts_with("socks5h://veles-"), "{a}");
        assert!(a.ends_with(":x@10.0.30.1:9050"), "{a}");
        assert_ne!(a, b, "two sessions share no circuit");
        assert_eq!(
            a,
            proxy_for("socks5h://10.0.30.1:9050", "session-a").unwrap()
        );
        assert!(proxy_for("not a url", "s").is_err());
    }
}
