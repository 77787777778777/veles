//! The `sessions` tool — list the live sessions on this machine and send
//! one of them mail (PLAN-sessions-and-phone.md S3; the mechanism is
//! `crate::mail`).
//!
//! Deferred: the core schema has no headroom, and a model that wants
//! another session finds this through tool_search. Unclassified in the
//! ACL, so owner-only unless `[tool_caps] sessions = …` says otherwise.

use crate::lockutil::LockOk;
use crate::mail::{self, Delivery, Resolved, Sender, Waited};
use crate::mailnet;
use crate::store::Store;
use crate::tools::Tool;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The longest wait one send may ask for.
pub const MAX_WAIT_SECS: u64 = 600;

/// An answer longer than this is cut (the full text stays in the
/// target's transcript and in `session_mail`).
const MAX_REPLY_CHARS: usize = 16_000;

pub struct SessionsTool {
    pub store: Arc<Mutex<Store>>,
    pub session_id: String,
}

#[async_trait]
impl Tool for SessionsTool {
    fn name(&self) -> &str {
        "sessions"
    }

    fn description(&self) -> &str {
        "Talk to the user's other veles sessions on this machine — a phone session, a long-running \
         worker, another chat. list: the sessions some process holds open or is running a turn in. \
         send: put text into another session; it runs there as that session's next turn, marked as \
         mail from you (not from the user), with the lower of your rights and its own. wait_secs \
         (up to 600) waits for that turn's final answer; otherwise check later with action=check \
         and the mail id. Mail to a session nobody has open waits until it is opened. A session on \
         another pool node (the phone, the server) is addressed node/session; list with \
         remote=true shows them."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "send", "check"]},
                "to": {"type": "string", "description": "send: a session id, a unique id prefix, or an exact title"},
                "text": {"type": "string", "description": "send: the message"},
                "wait_secs": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECS, "description": "send: seconds to wait for the answer (0 = return at once)"},
                "mail": {"type": "string", "description": "check: the mail id send returned"},
                "remote": {"type": "boolean", "description": "list: also the live sessions on the pool nodes this one reaches"}
            },
            "required": ["action"]
        })
    }

    async fn call(&self, args: Value) -> String {
        match args["action"].as_str() {
            Some("list") => self.list(args["remote"].as_bool().unwrap_or(false)).await,
            Some("send") => self.send(&args).await,
            Some("check") => self.check(&args).await,
            _ => "error: action must be list, send or check".to_string(),
        }
    }
}

impl SessionsTool {
    /// Who is sending: this session, with the rights it holds now.
    fn sender(&self) -> Sender {
        let st = self.store.lock_ok();
        Sender {
            session: Some(self.session_id.clone()),
            title: st.session_title(&self.session_id).ok().flatten(),
            principal: crate::acl::session_principal(&self.session_id).map(|p| (*p).clone()),
        }
    }

    async fn list(&self, remote: bool) -> String {
        let mut out = self.list_here();
        if remote {
            let cfg = crate::config::load().unwrap_or_default();
            let mut names: Vec<&String> = cfg.a2a_agents.keys().collect();
            names.sort();
            if names.is_empty() {
                out.push_str("\nNo pool nodes are configured ([a2a_agents]).");
            }
            for name in names {
                let Some(r) = mailnet::remote(&self.store.lock_ok(), &cfg, name) else {
                    continue;
                };
                match mailnet::list(&r).await {
                    Ok(ls) if ls.is_empty() => {
                        out.push_str(&format!("\nNode {name}: no live sessions."))
                    }
                    Ok(ls) => {
                        out.push_str(&format!("\nNode {name} ({}):", ls.len()));
                        for l in ls {
                            let title = if l.title.is_empty() {
                                String::new()
                            } else {
                                format!(" \u{201c}{}\u{201d}", l.title)
                            };
                            let state = if l.running { "running a turn" } else { "idle" };
                            out.push_str(&format!(
                                "\n- {name}/{}{title} — {}, {state}",
                                l.session_id, l.surface
                            ));
                        }
                    }
                    Err(e) => out.push_str(&format!("\nNode {name}: {e:#}")),
                }
            }
        }
        out
    }

    fn list_here(&self) -> String {
        let live = mail::live(&self.store);
        if live.is_empty() {
            return "No live sessions on this machine.".to_string();
        }
        let mut out = format!("Live sessions on this machine ({}):\n", live.len());
        for l in &live {
            let title = if l.title.is_empty() {
                String::new()
            } else {
                format!(" \u{201c}{}\u{201d}", l.title)
            };
            let state = match (l.running, l.held) {
                (true, true) => "running a turn",
                (true, false) => "running a one-shot turn (mail waits until it is opened again)",
                (false, _) => "idle",
            };
            let pid = l.pid.map(|p| format!(", pid {p}")).unwrap_or_default();
            let you = if l.session_id == self.session_id {
                " ← you"
            } else {
                ""
            };
            out.push_str(&format!(
                "- {}{title} — {}{pid}, {state}{you}\n",
                l.session_id, l.surface
            ));
        }
        out.push_str("Send with action=send to=<id, prefix or title>.");
        out
    }

    async fn send(&self, args: &Value) -> String {
        let Some(to_ref) = args["to"].as_str() else {
            return "error: send needs `to`".to_string();
        };
        let Some(text) = args["text"].as_str() else {
            return "error: send needs `text`".to_string();
        };
        let wait_secs = args["wait_secs"].as_u64().unwrap_or(0).min(MAX_WAIT_SECS);
        if let Some((node, sid)) = mailnet::split_address(to_ref) {
            return self.send_remote(node, sid, text, wait_secs).await;
        }
        let open: Vec<String> = mail::live(&self.store)
            .into_iter()
            .filter(|l| l.held)
            .map(|l| l.session_id)
            .collect();
        let resolved = match mail::resolve(&self.store.lock_ok(), to_ref, &open) {
            Ok(r) => r,
            Err(e) => return format!("error: {e:#}"),
        };
        let to = match resolved {
            Resolved::One(s) => s,
            Resolved::None => {
                // Not here: a pool node may own it (its replica says so).
                let owner = crate::storesync::session_owner(&self.store.lock_ok(), to_ref)
                    .ok()
                    .flatten();
                if let Some(node) = owner {
                    return self.send_remote(&node, to_ref, text, wait_secs).await;
                }
                return format!("error: no session matches {to_ref:?} — list shows the live ones");
            }
            Resolved::Many(ids) => {
                return format!(
                    "error: {to_ref:?} matches several sessions ({}); use a full id",
                    ids.join(", ")
                )
            }
        };
        if wait_secs > 0 {
            if let Some(cur) = mail::current(&self.session_id) {
                let waiting = self
                    .store
                    .lock_ok()
                    .mail_waiting_in_chain(&to, &cur.chain_id)
                    .unwrap_or(false);
                if waiting {
                    return format!(
                        "error: {to} is waiting for YOUR answer in this exchange. Finish your turn: \
                         your final message is its reply. A waited send back to it would \
                         deadlock — send without wait_secs if you must add something."
                    );
                }
            }
        }
        let from = self.sender();
        let sent = match mail::send(&self.store, &from, &to, text) {
            Ok(s) => s,
            Err(e) => return format!("error: {e:#}"),
        };
        let held_by = match sent.delivery {
            Delivery::Here => "a surface in this process has it open".to_string(),
            Delivery::Process(pid) => format!("process {pid} has it open"),
            Delivery::Queued => {
                return format!(
                    "Sent mail {} to {to}. Nobody has that session open, so it runs when the \
                     session is next opened; check for the answer later with action=check mail={}.",
                    sent.id, sent.id
                )
            }
        };
        if wait_secs == 0 {
            return format!(
                "Sent mail {} to {to} ({held_by}). Check for the answer with action=check mail={}.",
                sent.id, sent.id
            );
        }
        match mail::wait(&self.store, &sent.id, Duration::from_secs(wait_secs)).await {
            Ok(Waited::Answered(reply)) => {
                format!("Mail {} to {to} answered:\n{}", sent.id, cut(&reply))
            }
            Ok(Waited::Failed(e)) => format!("Mail {} to {to} failed: {e}", sent.id),
            Ok(Waited::Pending(state)) => format!(
                "No answer from {to} within {wait_secs}s (the mail is {state}). Check later with \
                 action=check mail={}.",
                sent.id
            ),
            Err(e) => format!("error: waiting for mail {}: {e:#}", sent.id),
        }
    }

    /// Mail to a session on another pool node, over the A2A link.
    async fn send_remote(&self, node: &str, sid: &str, text: &str, wait_secs: u64) -> String {
        let cfg = crate::config::load().unwrap_or_default();
        let Some(remote) = mailnet::remote(&self.store.lock_ok(), &cfg, node) else {
            return format!(
                "error: no pool node {node:?} — [a2a_agents] names the nodes this one reaches"
            );
        };
        let to = format!("{}/{sid}", remote.name);
        match mailnet::send(&remote, &self.sender(), sid, text, wait_secs).await {
            Err(e) => format!("error: {e:#}"),
            Ok(r) => {
                let id = format!("{}/{}", remote.name, r.id);
                match (r.state.as_str(), r.held) {
                    ("answered", _) => format!(
                        "Mail {id} to {to} answered:\n{}",
                        cut(r.reply.as_deref().unwrap_or(""))
                    ),
                    ("failed", _) => {
                        format!("Mail {id} to {to} failed: {}", r.reply.unwrap_or_default())
                    }
                    (_, false) => format!(
                        "Sent mail {id} to {to}. Nobody has that session open on {}, so it runs \
                         when it is next opened; check later with action=check mail={id}.",
                        remote.name
                    ),
                    (state, true) if wait_secs > 0 => format!(
                        "No answer from {to} within {wait_secs}s (the mail is {state}). Check later \
                         with action=check mail={id}."
                    ),
                    _ => format!(
                        "Sent mail {id} to {to} ({} has it open). Check for the answer with \
                         action=check mail={id}.",
                        remote.name
                    ),
                }
            }
        }
    }

    async fn check(&self, args: &Value) -> String {
        let Some(id) = args["mail"].as_str() else {
            return "error: check needs `mail`".to_string();
        };
        if let Some((node, mid)) = mailnet::split_address(id) {
            let cfg = crate::config::load().unwrap_or_default();
            let Some(remote) = mailnet::remote(&self.store.lock_ok(), &cfg, node) else {
                return format!("error: no pool node {node:?}");
            };
            return match mailnet::check(&remote, mid).await {
                Ok((state, reply)) => match state.as_str() {
                    "answered" => format!(
                        "Mail {id} answered:\n{}",
                        cut(reply.as_deref().unwrap_or(""))
                    ),
                    "failed" => format!("Mail {id} failed: {}", reply.unwrap_or_default()),
                    other => format!("Mail {id}: {other} on {node}."),
                },
                Err(e) => format!("error: {e:#}"),
            };
        }
        let row = match self.store.lock_ok().mail_get(id) {
            Ok(Some(r)) => r,
            Ok(None) => return format!("error: no mail {id}"),
            Err(e) => return format!("error: {e:#}"),
        };
        let mine = row.from_session.as_deref() == Some(self.session_id.as_str())
            || row.to_session == self.session_id;
        if !mine {
            return format!("error: mail {id} was neither sent by nor to this session");
        }
        match row.state.as_str() {
            "answered" => format!(
                "Mail {id} to {} answered:\n{}",
                row.to_session,
                cut(row.reply.as_deref().unwrap_or(""))
            ),
            "failed" => format!(
                "Mail {id} to {} failed: {}",
                row.to_session,
                row.reply.as_deref().unwrap_or("")
            ),
            "running" => format!("Mail {id}: {} is running its turn now.", row.to_session),
            _ => format!(
                "Mail {id}: still queued in {} (it runs when that session's current turn ends, or \
                 when the session is opened).",
                row.to_session
            ),
        }
    }
}

fn cut(s: &str) -> String {
    if s.chars().count() <= MAX_REPLY_CHARS {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_REPLY_CHARS).collect();
    format!("{head}\n[… cut; the whole answer is in that session's transcript]")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tool over a scratch home: `list` and `send` read the run dir
    /// of whatever data dir is current, and a test must never knock on a
    /// real one.
    fn tool() -> (SessionsTool, (tempfile::TempDir, crate::testenv::EnvGuard)) {
        let home = crate::testenv::scratch_home();
        let st = Store::open(&home.0.path().join("state.db")).unwrap();
        st.create_session("tool-me").unwrap();
        (
            SessionsTool {
                store: Arc::new(Mutex::new(st)),
                session_id: "tool-me".into(),
            },
            home,
        )
    }

    #[tokio::test]
    async fn bad_arguments_are_named_not_guessed() {
        let (t, _d) = tool();
        assert!(t.call(json!({})).await.starts_with("error: action"));
        assert!(t
            .call(json!({"action": "send"}))
            .await
            .contains("needs `to`"));
        assert!(t
            .call(json!({"action": "send", "to": "x"}))
            .await
            .contains("needs `text`"));
        assert!(t
            .call(json!({"action": "send", "to": "nobody", "text": "hi"}))
            .await
            .contains("no session matches"));
        assert!(t
            .call(json!({"action": "check"}))
            .await
            .contains("needs `mail`"));
    }

    #[tokio::test]
    async fn check_reads_only_this_sessions_mail() {
        let (t, _d) = tool();
        {
            let st = t.store.lock_ok();
            st.create_session("tool-a").unwrap();
            st.create_session("tool-b").unwrap();
        }
        // a → b: not ours.
        let id = mail::new_id();
        let body = mail::render_body(&id, "x");
        t.store
            .lock_ok()
            .mail_enqueue(&crate::store::NewMail {
                id: &id,
                to_session: "tool-b",
                from_session: Some("tool-a"),
                from_title: None,
                from_node: None,
                from_pid: None,
                from_principal: None,
                via_peer: None,
                chain_id: &id,
                hop: 0,
                body: &body,
                body_sha: &mail::body_sha(&body),
                kind: "mail",
            })
            .unwrap();
        let out = t.call(json!({"action": "check", "mail": id})).await;
        assert!(out.contains("neither sent by nor to"), "{out}");
    }
}
