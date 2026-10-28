//! `config` — the agent reads and changes veles' own settings for the
//! owner (PLAN-sessions-and-phone.md A2, "by talking").
//!
//! On a phone, editing `config.toml` by hand means typing TOML on a
//! touch keyboard, or adb. Here the person says what they want and the
//! agent does it — but a settings change is the owner's to make, so
//! every write is put to them first: the key, its value now, the value
//! it would get, and when the change takes effect. Nothing is written
//! until they say yes.
//!
//! What it does not do:
//! - credentials: a key named like one, or a value that looks like one,
//!   is refused with a pointer to `secret_request` (the vault, asked
//!   privately) — a token must never pass through a turn's transcript;
//! - free-form roots and arrays of tables: `get` shows them, `set` takes
//!   a value at a dotted path, which is what `configedit` writes safely.
//!
//! The write goes through the running gateway's `/api/config/set` when
//! there is one, so the live config handle reloads with it; without one
//! it is written in place (`configedit::edit`: comments and order kept,
//! the old file backed up, a result that would not parse refused).

use crate::ask::{AskKind, AskRequest};
use crate::tools::Tool;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct ConfigTool {
    pub session_id: String,
    pub timeout_secs: u64,
}

/// A path whose last segment names a credential.
pub fn names_a_secret(path: &str) -> bool {
    let last = path.rsplit('.').next().unwrap_or(path).to_ascii_lowercase();
    [
        "token", "secret", "password", "passwd", "api_key", "apikey", "key", "private",
    ]
    .iter()
    .any(|w| last == *w || last.ends_with(&format!("_{w}")) || last.starts_with(&format!("{w}_")))
}

/// Sections that decide who reaches veles, what runs without asking, or
/// which programs it starts. The agent reads them and never writes them:
/// one approval card is too thin a gate here. A page the agent read can
/// talk it into asking, and one tap on "allow" would open the agent to
/// strangers or start a stranger's program. The owner changes these by
/// hand, in Settings or in config.toml.
const GUARDED: &[(&str, &str)] = &[
    ("acl", "who may talk to veles and what they may do"),
    ("tool_caps", "what each tool asks of the person using it"),
    ("approvals", "what runs without asking"),
    ("sandbox", "where commands run"),
    ("cluster", "which machines the pool trusts"),
    ("node", "this machine's identity in the pool"),
    ("pool", "how new machines join the pool"),
    ("vault", "where credentials are kept"),
    ("hooks", "the commands run on events"),
    ("mcp_servers", "the programs veles starts"),
    ("platforms.a2a", "which other agents can reach this one"),
    (
        "platforms.desktop",
        "the local API every window and tool uses",
    ),
];

/// Why the agent may not write `path`, when it may not.
fn guarded(path: &str) -> Option<&'static str> {
    GUARDED
        .iter()
        .find(|(p, _)| path == *p || path.starts_with(&format!("{p}.")))
        .map(|(_, why)| *why)
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('.').all(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

const HIDDEN: &str = "(set — a credential, not shown)";

/// `v` with every credential in it replaced by [`HIDDEN`]: a value under
/// a key named like one, or a string shaped like one. `get` on a whole
/// table shows the rest of it.
fn hide_credentials(key: &str, v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), hide_credentials(k, x)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|x| hide_credentials(key, x)).collect()),
        _ if names_a_secret(key) || crate::a2a::secret_shaped(&v.to_string()) => {
            Value::String(HIDDEN.to_string())
        }
        _ => v.clone(),
    }
}

fn render(v: &Option<Value>) -> String {
    match v {
        None => "(unset)".to_string(),
        Some(Value::String(s)) => format!("{s:?}"),
        Some(v) => v.to_string(),
    }
}

#[async_trait]
impl Tool for ConfigTool {
    fn name(&self) -> &str {
        "config"
    }

    fn description(&self) -> &str {
        "Read or change veles' own settings (config.toml) for the owner. get: a key or a whole \
         table, with its default and when a change takes effect. set / unset: change one key — the \
         owner is asked first and shown the value now and the new one. Paths are dotted: \
         model.default, platforms.telegram.enabled, agent.compaction.keep_recent. Credentials are \
         refused here: ask for them with secret_request."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["get", "set", "unset"]},
                "path": {"type": "string", "description": "a dotted key, e.g. model.default"},
                "value": {"description": "set: the new value (string, number, boolean or list)"}
            },
            "required": ["action", "path"]
        })
    }

    async fn call(&self, args: Value) -> String {
        let path = args["path"].as_str().unwrap_or("").trim().to_string();
        if !valid_path(&path) {
            return "error: path must be a dotted key such as model.default".to_string();
        }
        match args["action"].as_str() {
            Some("get") => get(&path),
            Some("set") => match args.get("value") {
                None | Some(Value::Null) => "error: set needs a value (unset removes a key)".into(),
                Some(v) => self.change(&path, Some(v.clone())).await,
            },
            Some("unset") => self.change(&path, None).await,
            _ => "error: action must be get, set or unset".to_string(),
        }
    }
}

fn get(path: &str) -> String {
    let segs: Vec<&str> = path.split('.').collect();
    let current = match crate::configedit::read_doc() {
        Ok(doc) => crate::configedit::get_json(&doc, &segs),
        Err(e) => return format!("error: config.toml could not be read: {e:#}"),
    };
    let default = crate::config_surface::knobs()
        .into_iter()
        .find(|k| k.path == path)
        .map(|k| k.default.unwrap_or_else(|| "(unset)".to_string()));
    let effect = crate::configedit::effect_for(path);
    let last = path.rsplit('.').next().unwrap_or(path);
    let shown = match current.map(|v| hide_credentials(last, &v)) {
        None => "(unset)".to_string(),
        Some(Value::String(s)) if s == HIDDEN => s,
        Some(v @ (Value::Object(_) | Value::Array(_))) => {
            let mut s = serde_json::to_string_pretty(&v).unwrap_or_default();
            if s.len() > 4000 {
                let mut end = 4000;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
                s.push_str("\n…");
            }
            s
        }
        v => render(&v),
    };
    let mut out = format!("{path} = {shown}");
    if let Some(d) = default {
        out.push_str(&format!("\ndefault: {d}"));
    }
    out.push_str(&format!("\nchanging it: {}", effect.note()));
    out
}

impl ConfigTool {
    async fn change(&self, path: &str, value: Option<Value>) -> String {
        if let Some(why) = guarded(path) {
            return format!(
                "error: {path} decides {why}, so the agent does not change it. The owner can, \
                 in the desktop's Settings or in config.toml."
            );
        }
        if names_a_secret(path)
            || value
                .as_ref()
                .is_some_and(|v| crate::a2a::secret_shaped(&v.to_string()))
        {
            return format!(
                "error: {path} is a credential, and credentials never go through a turn. Ask for \
                 it with secret_request: the person types it privately and it goes to the vault."
            );
        }
        if let Some(v) = &value {
            if let Err(e) = crate::configedit::json_to_toml(v) {
                return format!("error: that value cannot be written to config.toml: {e:#}");
            }
        }
        let segs: Vec<&str> = path.split('.').collect();
        let now = crate::configedit::read_doc()
            .ok()
            .and_then(|doc| crate::configedit::get_json(&doc, &segs));
        if now == value {
            return format!("{path} is already {}: nothing to change", render(&now));
        }
        let effect = crate::configedit::effect_for(path);
        let detail = format!(
            "{path}\n  now: {}\n  new: {}\n{}",
            render(&now),
            render(&value),
            effect.note()
        );
        let req = AskRequest::new(
            &self.session_id,
            AskKind::Approval,
            &format!("change the setting {path}?"),
            crate::ask::approval_choices(false),
        )
        .with_detail(detail)
        .with_timeout(self.timeout_secs);
        let Some(answer) = crate::ask::ask_session(&self.session_id, req).await else {
            return format!(
                "nobody is here to approve a settings change. Tell the user what to set: \
                 {path} = {}",
                render(&value)
            );
        };
        if answer.timed_out {
            return format!("the owner did not answer in time; {path} is unchanged");
        }
        if answer.first() != Some(crate::ask::CHOICE_ONCE) {
            return format!("the owner said no; {path} is unchanged");
        }
        self.apply(path, value, effect).await
    }

    async fn apply(
        &self,
        path: &str,
        value: Option<Value>,
        effect: crate::configedit::Effect,
    ) -> String {
        let body = json!({ "path": path, "value": value.clone().unwrap_or(Value::Null) });
        // The running gateway first: it writes AND reloads its live
        // handle. Without one, the file is written in place.
        match crate::desktop_tools::desktop_post(
            "/api/config/set",
            body,
            std::time::Duration::from_secs(15),
        )
        .await
        {
            Ok(v) if v.get("error").is_none() => format!(
                "{path} is now {} ({})",
                render(&value),
                v["note"].as_str().unwrap_or(effect.note())
            ),
            Ok(v) => format!(
                "error: {}",
                v["error"].as_str().unwrap_or("the gateway refused it")
            ),
            Err(_) => {
                let segs: Vec<&str> = path.split('.').collect();
                let r = crate::configedit::edit(path, |doc| match &value {
                    None => crate::configedit::unset(doc, &segs),
                    Some(v) => {
                        let tv = crate::configedit::json_to_toml(v)?;
                        crate::configedit::set_value(doc, &segs, tv)
                    }
                });
                match r {
                    Ok(_) => format!(
                        "{path} is now {} (no gateway is running here: it takes effect when \
                         veles next starts)",
                        render(&value)
                    ),
                    Err(e) => format!("error: {e:#}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::{AskAnswer, Asker};
    use std::sync::Arc;

    struct Answer(&'static str);
    #[async_trait]
    impl Asker for Answer {
        async fn ask(&self, req: AskRequest) -> AskAnswer {
            assert_eq!(req.kind, AskKind::Approval);
            assert!(req.detail.as_deref().unwrap_or("").contains("now:"));
            AskAnswer::choice(self.0)
        }
    }

    /// The owner who never answers.
    struct Away;
    #[async_trait]
    impl Asker for Away {
        async fn ask(&self, _req: AskRequest) -> AskAnswer {
            AskAnswer::timeout()
        }
    }

    /// Fails the test if anyone is asked at all.
    struct NotAsked;
    #[async_trait]
    impl Asker for NotAsked {
        async fn ask(&self, req: AskRequest) -> AskAnswer {
            panic!("nobody should have been asked: {}", req.prompt);
        }
    }

    fn tool(sid: &str) -> ConfigTool {
        ConfigTool {
            session_id: sid.into(),
            timeout_secs: 5,
        }
    }

    #[test]
    fn credentials_are_named_and_paths_checked() {
        for p in [
            "platforms.telegram.token",
            "a2a_agents.vps.auth.token",
            "custom_providers.api_key",
            "x.secret",
            "x.password",
        ] {
            assert!(names_a_secret(p), "{p}");
        }
        for p in [
            "model.default",
            "agent.compaction.keep_recent",
            "tui.keymap",
            "platforms.irc.enabled",
        ] {
            assert!(!names_a_secret(p), "{p}");
        }
        for p in [
            "acl",
            "acl.owner",
            "tool_caps.terminal",
            "approvals.mode",
            "sandbox.backend",
            "cluster.task_peers",
            "pool.wireguard.subnet",
            "hooks.on_start",
            "mcp_servers.x.command",
            "platforms.a2a.enabled",
            "platforms.desktop.extra.port",
        ] {
            assert!(guarded(p).is_some(), "{p}");
        }
        for p in [
            "model.default",
            "platforms.telegram.enabled",
            "aclx",
            "nodes",
            "tui.theme",
        ] {
            assert!(guarded(p).is_none(), "{p}");
        }
        assert!(valid_path("model.default"));
        assert!(!valid_path("model..default"));
        assert!(!valid_path("model.default; rm"));
    }

    #[tokio::test]
    async fn a_change_is_asked_first_and_only_a_yes_writes_it() {
        // No gateway answers on the test's port: the write lands in place.
        let _home = crate::testenv::scratch_home();
        let prev_port = std::env::var("VELES_DESKTOP_PORT").ok();
        std::env::set_var("VELES_DESKTOP_PORT", "9");
        std::fs::write(
            crate::configedit::config_path(),
            "# mine\n[model]\ndefault = \"a\"\n",
        )
        .unwrap();

        crate::ask::set_session_asker("cfg-no", Arc::new(Answer(crate::ask::CHOICE_DENY)));
        let out = tool("cfg-no")
            .call(json!({"action": "set", "path": "model.default", "value": "b"}))
            .await;
        assert!(out.contains("said no"), "{out}");
        assert!(std::fs::read_to_string(crate::configedit::config_path())
            .unwrap()
            .contains("default = \"a\""));
        crate::ask::clear_session_asker("cfg-no");

        crate::ask::set_session_asker("cfg-yes", Arc::new(Answer(crate::ask::CHOICE_ONCE)));
        let out = tool("cfg-yes")
            .call(json!({"action": "set", "path": "model.default", "value": "b"}))
            .await;
        assert!(out.contains("model.default is now \"b\""), "{out}");
        let text = std::fs::read_to_string(crate::configedit::config_path()).unwrap();
        assert!(text.contains("default = \"b\""), "{text}");
        assert!(text.contains("# mine"), "comments survive: {text}");
        crate::ask::clear_session_asker("cfg-yes");

        let got = tool("cfg-get")
            .call(json!({"action": "get", "path": "model.default"}))
            .await;
        assert!(got.starts_with("model.default = \"b\""), "{got}");
        assert!(got.contains("changing it:"), "{got}");
        match prev_port {
            Some(p) => std::env::set_var("VELES_DESKTOP_PORT", p),
            None => std::env::remove_var("VELES_DESKTOP_PORT"),
        }
    }

    #[tokio::test]
    async fn no_answer_no_change_and_credentials_stay_hidden() {
        let _home = crate::testenv::scratch_home();
        let prev_port = std::env::var("VELES_DESKTOP_PORT").ok();
        std::env::set_var("VELES_DESKTOP_PORT", "9");
        std::fs::write(
            crate::configedit::config_path(),
            "[model]\ndefault = \"a\"\n[platforms.telegram]\nenabled = true\ntoken = \"123456:AAbbCCddEEffGGhhIIjjKKllMMnnOOppQQr\"\n",
        )
        .unwrap();

        crate::ask::set_session_asker("cfg-away", Arc::new(Away));
        let out = tool("cfg-away")
            .call(json!({"action": "set", "path": "model.default", "value": "b"}))
            .await;
        assert!(out.contains("did not answer"), "{out}");
        crate::ask::clear_session_asker("cfg-away");

        // Already so: said at once, nobody is asked.
        crate::ask::set_session_asker("cfg-same", Arc::new(NotAsked));
        let out = tool("cfg-same")
            .call(json!({"action": "set", "path": "model.default", "value": "a"}))
            .await;
        assert!(out.contains("already"), "{out}");
        crate::ask::clear_session_asker("cfg-same");

        // A credential is reported as set, never shown, whole table or key.
        for path in ["platforms.telegram.token", "platforms.telegram"] {
            let got = tool("cfg-get")
                .call(json!({"action": "get", "path": path}))
                .await;
            assert!(!got.contains("AAbbCC"), "{path}: {got}");
        }
        let got = tool("cfg-get")
            .call(json!({"action": "get", "path": "platforms.telegram.token"}))
            .await;
        assert!(got.contains("not shown"), "{got}");
        let got = tool("cfg-get")
            .call(json!({"action": "get", "path": "platforms.telegram"}))
            .await;
        assert!(
            got.contains("\"enabled\": true"),
            "the rest is shown: {got}"
        );
        assert!(std::fs::read_to_string(crate::configedit::config_path())
            .unwrap()
            .contains("default = \"a\""));
        match prev_port {
            Some(p) => std::env::set_var("VELES_DESKTOP_PORT", p),
            None => std::env::remove_var("VELES_DESKTOP_PORT"),
        }
    }

    #[tokio::test]
    async fn credentials_and_absent_owners_change_nothing() {
        let _home = crate::testenv::scratch_home();
        let out = tool("cfg-secret")
            .call(json!({"action": "set", "path": "platforms.telegram.token", "value": "123:abc"}))
            .await;
        assert!(out.contains("secret_request"), "{out}");
        // Who may reach the agent is the owner's to change by hand: refused
        // before anyone is asked (no asker is attached to this session).
        let out = tool("cfg-acl")
            .call(json!({"action": "set", "path": "acl.lockdown", "value": false}))
            .await;
        assert!(out.contains("does not change it"), "{out}");
        // Nobody attached to the session: no write, and the agent is told
        // what to say instead.
        let out = tool("cfg-nobody")
            .call(json!({"action": "set", "path": "model.default", "value": "z"}))
            .await;
        assert!(out.contains("nobody is here"), "{out}");
        assert!(
            !crate::configedit::config_path().exists()
                || !std::fs::read_to_string(crate::configedit::config_path())
                    .unwrap()
                    .contains("\"z\"")
        );
    }
}
