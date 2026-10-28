//! The PDS XRPC client: session lifecycle, posting, blob upload,
//! notifications, identity resolution, and the operator's social graph.
//!
//! One client per adapter, holding the bot's session tokens behind a lock
//! and refreshing the access JWT before it expires. Every mutating call
//! goes to our OWN PDS (`pds_url`) — the bot's repo lives there. Read-only
//! AppView/graph calls (resolveHandle, getFollows/getFollowers) go to the
//! public AppView, because the operator's account does NOT live on our
//! PDS — its graph is an AppView question, not a PDS question.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::sync::Mutex;

/// A live atproto session (the createSession/refreshSession shape).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    #[serde(rename = "accessJwt")]
    access_jwt: String,
    #[serde(rename = "refreshJwt")]
    refresh_jwt: String,
    #[serde(default)]
    did: String,
    #[serde(default)]
    handle: String,
}

/// The XRPC client. Cheap to clone (shares the session + http handle).
#[derive(Clone)]
pub struct PdsClient {
    base: String,
    appview: String,
    http: reqwest::Client,
    session: std::sync::Arc<Mutex<Session>>,
    /// The bot account's DID (the session's, authoritative).
    did: String,
}

impl PdsClient {
    /// Open a session with the app password. `base` is the PDS URL
    /// (`http://127.0.0.1:2583` co-located, or the public URL).
    pub async fn login(
        base: &str,
        appview: &str,
        identifier: &str,
        password: &str,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            // The notification poll calls the PDS every 30 s. A pooled
            // keep-alive connection the PDS closed in the meantime fails
            // the NEXT send ("error sending request") — live, every
            // second poll since at least 2026-10-09 19:38 — and the retry
            // 30 s later opens a fresh one and works. Idle connections
            // are dropped well inside the poll period instead.
            .pool_idle_timeout(std::time::Duration::from_secs(10))
            .build()?;
        let url = format!(
            "{}/xrpc/com.atproto.server.createSession",
            base.trim_end_matches('/')
        );
        let resp = http
            .post(&url)
            .json(&serde_json::json!({ "identifier": identifier, "password": password }))
            .send()
            .await
            .context("createSession send")?;
        if !resp.status().is_success() {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("createSession {code}: {body}"));
        }
        let session: Session = resp.json().await.context("createSession parse")?;
        let did = session.did.clone();
        tracing::info!("atproto: session as {} ({did})", session.handle);
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            appview: appview.trim_end_matches('/').to_string(),
            http,
            session: std::sync::Arc::new(Mutex::new(session)),
            did,
        })
    }

    /// A client with no session, pointed at a port nothing listens on:
    /// every call fails at once, which is what a test of the paths that
    /// must decide WITHOUT the network wants.
    #[cfg(test)]
    pub fn offline() -> Self {
        Self {
            base: "http://127.0.0.1:0".into(),
            appview: "http://127.0.0.1:0".into(),
            http: reqwest::Client::new(),
            session: std::sync::Arc::new(Mutex::new(Session {
                access_jwt: String::new(),
                refresh_jwt: String::new(),
                did: String::new(),
                handle: String::new(),
            })),
            did: "did:plc:bot".into(),
        }
    }

    /// The bot account's DID.
    pub fn did(&self) -> &str {
        &self.did
    }

    /// The public URL at which a blob CID is served from this PDS.
    ///
    /// `com.atproto.sync.getBlob` reads by (did, cid) with NO session —
    /// a blob is content-addressed public bytes, so the link the Bluesky
    /// surface posts for a withheld/elided `/tcl` result is just this
    /// endpoint. The PDS base here is the LOOPBACK the adapter posts
    /// through, so the public hostname must come from config; the caller
    /// (the atproto adapter) substitutes it.
    pub fn blob_url(&self, public_base: &str, cid: &str) -> String {
        format!(
            "{}/xrpc/com.atproto.sync.getBlob?did={}&cid={}",
            public_base.trim_end_matches('/'),
            self.did,
            cid
        )
    }

    fn access_token(&self) -> String {
        self.session.lock().unwrap().access_jwt.clone()
    }

    /// Refresh the access JWT from the refresh JWT. Called on a 401/400
    /// ExpiredToken; the caller retries once after this.
    async fn refresh(&self) -> Result<()> {
        let refresh = self.session.lock().unwrap().refresh_jwt.clone();
        let url = format!("{}/xrpc/com.atproto.server.refreshSession", self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&refresh)
            .send()
            .await
            .context("refreshSession send")?;
        if !resp.status().is_success() {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("refreshSession {code}: {body}"));
        }
        let fresh: Session = resp.json().await.context("refreshSession parse")?;
        *self.session.lock().unwrap() = fresh;
        Ok(())
    }

    /// POST an authed XRPC procedure, refreshing once on an expired token.
    async fn post(&self, method: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.send_authed(method, |http, url| http.post(url).json(body))
            .await
    }

    /// GET an authed XRPC query on our own PDS, refreshing once.
    async fn get(&self, method: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        self.send_authed(method, |http, url| http.get(url).query(params))
            .await
    }

    /// One authed XRPC call on our own PDS. An `app.bsky.*` method is the
    /// AppView's, which the PDS only proxies when told where to: without
    /// `atproto-proxy` (and with no default AppView configured) vlpds
    /// answered every notification poll "400 No service configured" —
    /// 1,362 times on the live node, so the backstop never ran.
    ///
    /// An expired access token is refreshed once and the call retried.
    /// atproto says EXPIRED with `400 {"error":"ExpiredToken"}`, not 401;
    /// checking only 401 left a session dead after its token's lifetime.
    async fn send_authed(
        &self,
        method: &str,
        build: impl Fn(&reqwest::Client, String) -> reqwest::RequestBuilder,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/xrpc/{method}", self.base);
        let mut refreshed = false;
        loop {
            let mut req = build(&self.http, url.clone()).bearer_auth(self.access_token());
            if let Some(proxy) = proxy_for(method) {
                req = req.header("atproto-proxy", proxy);
            }
            let resp = req.send().await.with_context(|| format!("{method} send"))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                // A procedure may answer 200 with NO body
                // (`app.bsky.notification.updateSeen` does): that is
                // success, not a parse error — parsing it failed every
                // poll's last step once the AppView was reachable.
                if text.trim().is_empty() {
                    return Ok(serde_json::Value::Null);
                }
                return serde_json::from_str(&text).with_context(|| format!("{method} parse"));
            }
            if !refreshed && token_expired(status, &text) {
                self.refresh().await?;
                refreshed = true;
                continue;
            }
            return Err(anyhow!("{method} {status}: {text}"));
        }
    }

    // ── posting ────────────────────────────────────────────────────────

    /// Create an `app.bsky.feed.post`. `record` is the full post body the
    /// caller assembled (text, createdAt, facets, reply, embed). Returns
    /// the created `{uri, cid}`.
    pub async fn create_post(&self, record: serde_json::Value) -> Result<(String, String)> {
        let body = serde_json::json!({
            "repo": self.did,
            "collection": "app.bsky.feed.post",
            "record": record,
        });
        let out = self.post("com.atproto.repo.createRecord", &body).await?;
        let uri = out
            .get("uri")
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string();
        let cid = out
            .get("cid")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        Ok((uri, cid))
    }

    /// Like a post: `app.bsky.feed.like` with the subject's strong ref.
    /// Used as a cheap liveness ack — a mention from an authorised user
    /// gets a like the moment it's admitted, long before the (slow,
    /// image-generating) turn replies. A like failure must never block the
    /// turn: the caller logs and continues.
    pub async fn create_like(&self, subject_uri: &str, subject_cid: &str) -> Result<()> {
        let now = crate::post::format_epoch_pub(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
        let body = serde_json::json!({
            "repo": self.did,
            "collection": "app.bsky.feed.like",
            "record": {
                "$type": "app.bsky.feed.like",
                "subject": { "uri": subject_uri, "cid": subject_cid },
                "createdAt": now,
            },
        });
        self.post("com.atproto.repo.createRecord", &body).await?;
        Ok(())
    }

    /// Upload a blob for an image embed. Returns the blob metadata object.
    /// Caller enforces the ≤1MB size cap and strips EXIF BEFORE this.
    pub async fn upload_blob(&self, bytes: &[u8], mime: &str) -> Result<serde_json::Value> {
        let url = format!("{}/xrpc/com.atproto.repo.uploadBlob", self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.access_token())
            .header("Content-Type", mime)
            .body(bytes.to_vec())
            .send()
            .await
            .context("uploadBlob send")?;
        if !resp.status().is_success() {
            let code = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("uploadBlob {code}: {text}"));
        }
        let out: serde_json::Value = resp.json().await.context("uploadBlob parse")?;
        Ok(out.get("blob").cloned().unwrap_or(out))
    }

    /// Create an arbitrary record in the bot's own repo. `validate: false`:
    /// the caller's collection is not a known lexicon (the `/tcl` output
    /// pointer below), so schema validation would refuse it. Returns the
    /// created `{uri, cid}`; the rkey is the uri's last segment.
    pub async fn create_record_any(
        &self,
        collection: &str,
        record: serde_json::Value,
    ) -> Result<(String, String)> {
        let body = serde_json::json!({
            "repo": self.did,
            "collection": collection,
            "record": record,
            "validate": false,
        });
        let out = self.post("com.atproto.repo.createRecord", &body).await?;
        let uri = out
            .get("uri")
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string();
        let cid = out
            .get("cid")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        Ok((uri, cid))
    }

    /// Delete a record from the bot's own repo by collection + rkey.
    pub async fn delete_record(&self, collection: &str, rkey: &str) -> Result<()> {
        let body = serde_json::json!({
            "repo": self.did,
            "collection": collection,
            "rkey": rkey,
        });
        self.post("com.atproto.repo.deleteRecord", &body).await?;
        Ok(())
    }

    /// Fetch a record (a post, to resolve a reply's root/parent refs).
    pub async fn get_record(
        &self,
        repo: &str,
        collection: &str,
        rkey: &str,
    ) -> Result<serde_json::Value> {
        self.get(
            "com.atproto.repo.getRecord",
            &[("repo", repo), ("collection", collection), ("rkey", rkey)],
        )
        .await
    }

    /// Fetch a record via the AppView — for repos we do NOT host (the
    /// common case: a stranger's or the operator's post on bsky.social).
    /// Our own PDS's `getRecord` only serves repos it hosts, so resolving
    /// a reply target that lives elsewhere MUST go through the AppView.
    pub async fn get_record_appview(
        &self,
        repo: &str,
        collection: &str,
        rkey: &str,
    ) -> Result<serde_json::Value> {
        self.appview_get(
            "com.atproto.repo.getRecord",
            &[("repo", repo), ("collection", collection), ("rkey", rkey)],
        )
        .await
    }

    /// One record by AT URI from the public AppView's record store, raw:
    /// `Ok(None)` when it is not there (deleted). No viewer is sent, so
    /// no block hides it — what a thread view reports as blocked reads
    /// here (the `repo.get_record` service).
    pub async fn get_record_public(
        &self,
        uri: &str,
    ) -> Result<Option<(Option<String>, serde_json::Value)>> {
        let (repo, collection, rkey) = veles_plugin_sdk::proto::at_uri_parts(uri)
            .ok_or_else(|| anyhow!("not one record's AT URI: {uri}"))?;
        let url = format!("{}/xrpc/com.atproto.repo.getRecord", self.appview);
        let resp = self
            .http
            .get(&url)
            .query(&[("repo", repo), ("collection", collection), ("rkey", rkey)])
            .send()
            .await
            .context("getRecord send")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if record_not_found(status, &body) {
                return Ok(None);
            }
            return Err(anyhow!("getRecord {status}: {body}"));
        }
        let mut out: serde_json::Value = resp.json().await?;
        let cid = out["cid"].as_str().map(str::to_string);
        Ok(Some((cid, out["value"].take())))
    }

    // ── notifications (the authoritative backstop lane) ────────────────

    /// Notifications for `reasons`, newest first. Served by our own PDS,
    /// which proxies the AppView (`APPVIEW_PROXY`). `reasons` is an array
    /// parameter: repeated, not one comma-joined value. The AppView
    /// answers only the reasons asked for, so a reason left out here is
    /// one the poll never sees.
    pub async fn list_notifications(
        &self,
        reasons: &[&str],
        limit: u32,
    ) -> Result<serde_json::Value> {
        let lim = limit.to_string();
        let mut q: Vec<(&str, &str)> = reasons.iter().map(|r| ("reasons", *r)).collect();
        q.push(("limit", &lim));
        self.get("app.bsky.notification.listNotifications", &q)
            .await
    }

    /// How `actor` and `other` relate, both directions, in one call
    /// (`app.bsky.graph.getRelationships`; public AppView, no auth).
    pub async fn get_relationship(&self, actor: &str, other: &str) -> Result<serde_json::Value> {
        self.appview_get(
            "app.bsky.graph.getRelationships",
            &[("actor", actor), ("others", other)],
        )
        .await
    }

    /// One actor's profile (`app.bsky.actor.getProfile`; public AppView,
    /// no auth) — the verified handle a Jetstream event does not carry.
    pub async fn get_profile(&self, actor: &str) -> Result<serde_json::Value> {
        self.appview_get("app.bsky.actor.getProfile", &[("actor", actor)])
            .await
    }

    /// Move the seen mark. Take `seen_at` BEFORE listing, not after, so
    /// anything indexed mid-loop stays unread for the next pass.
    pub async fn update_seen(&self, seen_at: &str) -> Result<()> {
        self.post(
            "app.bsky.notification.updateSeen",
            &serde_json::json!({ "seenAt": seen_at }),
        )
        .await?;
        Ok(())
    }

    // ── identity / graph (AppView, read-only) ──────────────────────────

    /// Resolve a handle to a DID. Public AppView; no auth needed.
    pub async fn resolve_handle(&self, handle: &str) -> Result<Option<String>> {
        let url = format!("{}/xrpc/com.atproto.identity.resolveHandle", self.appview);
        let resp = self
            .http
            .get(&url)
            .query(&[("handle", handle)])
            .send()
            .await
            .context("resolveHandle send")?;
        if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            return Ok(None); // unknown handle renders as text, never a link
        }
        if !resp.status().is_success() {
            return Err(anyhow!(
                "resolveHandle {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }
        let out: serde_json::Value = resp.json().await?;
        Ok(out.get("did").and_then(|d| d.as_str()).map(str::to_string))
    }

    /// One page of the operator's follows (DIDs). AppView, read-only.
    pub async fn get_follows(
        &self,
        actor: &str,
        cursor: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut params = vec![("actor", actor), ("limit", "100")];
        if let Some(c) = cursor {
            params.push(("cursor", c));
        }
        self.appview_get("app.bsky.graph.getFollows", &params).await
    }

    /// A post thread, via the AppView. Used to build the conversation
    /// context for a re-mention: the posts others made in the thread that
    /// never mentioned the bot (so never became turns) but the model
    /// should see to follow the conversation.
    pub async fn get_post_thread(&self, uri: &str, depth: u32) -> Result<serde_json::Value> {
        let d = depth.to_string();
        self.appview_get(
            "app.bsky.feed.getPostThread",
            &[("uri", uri), ("depth", &d), ("parentHeight", "80")],
        )
        .await
    }

    /// One AppView query AS THE BOT: through our own PDS with the bot's
    /// session, which proxies it (`atproto-proxy`) and signs it for the
    /// AppView. Unlike the public AppView calls here, the AppView answers
    /// a signed-in viewer: posts limited to signed-in users read, and the
    /// bot's blocks apply. The caller checks the method is a read.
    pub async fn appview_query_as_bot(
        &self,
        method: &str,
        params: &[(String, String)],
    ) -> Result<serde_json::Value> {
        let borrowed: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        self.get(method, &borrowed).await
    }

    /// One page of an account's followers (DIDs). AppView, read-only.
    pub async fn get_followers(
        &self,
        actor: &str,
        cursor: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut params = vec![("actor", actor), ("limit", "100")];
        if let Some(c) = cursor {
            params.push(("cursor", c));
        }
        self.appview_get("app.bsky.graph.getFollowers", &params)
            .await
    }

    async fn appview_get(
        &self,
        method: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value> {
        let url = format!("{}/xrpc/{method}", self.appview);
        let resp = self
            .http
            .get(&url)
            .query(params)
            .send()
            .await
            .with_context(|| format!("{method} send"))?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "{method} {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }
        Ok(resp.json().await?)
    }
}

/// The AppView every `app.bsky.*` call is proxied to by our PDS.
pub const APPVIEW_PROXY: &str = "did:web:api.bsky.app#bsky_appview";

/// The `atproto-proxy` target for an XRPC method, when the PDS must
/// forward it: the `app.bsky.*` lexicons live on the AppView.
fn proxy_for(method: &str) -> Option<&'static str> {
    method.starts_with("app.bsky.").then_some(APPVIEW_PROXY)
}

/// Does this answer mean "no such record" — the XRPC error a deleted
/// record (or a repo that is gone) gets — rather than a failure?
fn record_not_found(status: reqwest::StatusCode, body: &str) -> bool {
    (status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::NOT_FOUND)
        && serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .is_some_and(|e| e == "RecordNotFound" || e == "RepoNotFound")
}

/// Does this answer mean "your access token expired — refresh"?
fn token_expired(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED
        || (status == reqwest::StatusCode::BAD_REQUEST && body.contains("ExpiredToken"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::HeaderMap, routing::get, routing::post, Json, Router};
    use std::sync::{Arc, Mutex};

    /// What the stub PDS saw: the bearer of each listNotifications call
    /// and its `atproto-proxy` header.
    #[derive(Default)]
    struct Seen {
        calls: Vec<(String, Option<String>)>,
        refreshed: usize,
    }

    /// A PDS whose first access token ("old") has expired the way atproto
    /// says it: `400 {"error":"ExpiredToken"}`, not a 401.
    async fn stub() -> (String, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (s1, s2) = (seen.clone(), seen.clone());
        let app = Router::new()
            .route(
                "/xrpc/com.atproto.server.createSession",
                post(|| async {
                    Json(serde_json::json!({"accessJwt":"old","refreshJwt":"r",
                        "did":"did:plc:bot","handle":"bot.example"}))
                }),
            )
            .route(
                "/xrpc/com.atproto.server.refreshSession",
                post(move || {
                    s1.lock().unwrap().refreshed += 1;
                    async {
                        Json(serde_json::json!({"accessJwt":"new","refreshJwt":"r2",
                            "did":"did:plc:bot","handle":"bot.example"}))
                    }
                }),
            )
            .route(
                "/xrpc/app.bsky.notification.updateSeen",
                post(|| async { axum::http::StatusCode::OK }),
            )
            .route(
                "/xrpc/app.bsky.notification.listNotifications",
                get(move |h: HeaderMap| {
                    let bearer = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let proxy = h
                        .get("atproto-proxy")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    s2.lock().unwrap().calls.push((bearer.clone(), proxy));
                    async move {
                        if bearer == "Bearer old" {
                            (
                                axum::http::StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({"error":"ExpiredToken",
                                    "message":"Token has expired"})),
                            )
                        } else {
                            (
                                axum::http::StatusCode::OK,
                                Json(serde_json::json!({"notifications":[]})),
                            )
                        }
                    }
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, seen)
    }

    #[tokio::test]
    async fn an_expired_token_answered_with_400_is_refreshed_and_retried() {
        let (url, seen) = stub().await;
        let c = PdsClient::login(&url, &url, "bot.example", "pw")
            .await
            .unwrap();
        let out = c
            .list_notifications(crate::notify::TURN_REASONS, 5)
            .await
            .expect("refreshed and retried");
        assert!(out["notifications"].is_array());
        let s = seen.lock().unwrap();
        assert_eq!(s.refreshed, 1, "one refresh");
        assert_eq!(
            s.calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            ["Bearer old", "Bearer new"]
        );
    }

    /// A raw record reads from the AppView with no viewer; a deleted one
    /// is `None`, not an error, and a real failure stays an error.
    #[tokio::test]
    async fn a_public_record_reads_raw_and_a_deleted_one_is_none() {
        let (pds, _seen) = stub().await;
        let auth: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let a2 = auth.clone();
        let appview = Router::new().route(
            "/xrpc/com.atproto.repo.getRecord",
            get(
                move |h: HeaderMap, q: axum::extract::Query<std::collections::HashMap<String, String>>| {
                    a2.lock().unwrap().push(
                        h.get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                    );
                    let rkey = q.get("rkey").cloned().unwrap_or_default();
                    async move {
                        match rkey.as_str() {
                            "live" => (
                                axum::http::StatusCode::OK,
                                Json(serde_json::json!({"uri": "at://did:plc:a/app.bsky.feed.post/live",
                                    "cid": "bafyx", "value": {"text": "hidden by a block"}})),
                            ),
                            "gone" => (
                                axum::http::StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({"error": "RecordNotFound",
                                    "message": "Could not locate record"})),
                            ),
                            _ => (
                                axum::http::StatusCode::BAD_GATEWAY,
                                Json(serde_json::json!({"error": "UpstreamFailure"})),
                            ),
                        }
                    }
                },
            ),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let av = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, appview).await.unwrap() });
        let c = PdsClient::login(&pds, &av, "bot.example", "pw")
            .await
            .unwrap();
        let (cid, value) = c
            .get_record_public("at://did:plc:a/app.bsky.feed.post/live")
            .await
            .unwrap()
            .expect("found");
        assert_eq!(cid.as_deref(), Some("bafyx"));
        assert_eq!(value["text"], "hidden by a block");
        assert!(c
            .get_record_public("at://did:plc:a/app.bsky.feed.post/gone")
            .await
            .unwrap()
            .is_none());
        assert!(c
            .get_record_public("at://did:plc:a/app.bsky.feed.post/boom")
            .await
            .is_err());
        assert!(c.get_record_public("https://bsky.app/x").await.is_err());
        assert_eq!(
            *auth.lock().unwrap(),
            vec![None, None, None],
            "no viewer is sent"
        );
    }

    /// updateSeen answers 200 with an empty body; that is success.
    #[tokio::test]
    async fn an_empty_success_body_is_not_a_parse_error() {
        let (url, _seen) = stub().await;
        let c = PdsClient::login(&url, &url, "bot.example", "pw")
            .await
            .unwrap();
        c.update_seen("2026-10-06T00:00:00Z")
            .await
            .expect("an empty 200 is success");
    }

    #[tokio::test]
    async fn an_appview_method_names_the_appview_to_proxy_to() {
        let (url, seen) = stub().await;
        let c = PdsClient::login(&url, &url, "bot.example", "pw")
            .await
            .unwrap();
        let _ = c.list_notifications(crate::notify::TURN_REASONS, 5).await;
        let s = seen.lock().unwrap();
        assert!(
            s.calls
                .iter()
                .all(|c| c.1.as_deref() == Some(APPVIEW_PROXY)),
            "every app.bsky call carries atproto-proxy: {:?}",
            s.calls
        );
        assert_eq!(proxy_for("com.atproto.repo.createRecord"), None);
    }
}
