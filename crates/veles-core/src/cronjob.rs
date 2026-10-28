//! cronjob tool — durable scheduled jobs, managed by the agent.
//!
//! Actions mirror hermes' cronjob tool: create / list / update / pause /
//! resume / remove / run. Jobs live in sqlite (cron_jobs), the scheduler
//! (crate::cron) ticks them in the gateway process.
//!
//! Two structural improvements over hermes:
//!   - **Origin captured at creation.** The tool instance knows the
//!     (platform, chat) it was built in; deliver='origin' resolves to
//!     that concrete target durably, so a job created from telegram keeps
//!     delivering there after restarts.
//!   - **Recursion guard by construction.** Cron JOB sessions get the base
//!     registry, which lacks this tool — a running job cannot schedule
//!     more jobs (hermes asks nicely in the prompt).

use crate::cron::{describe_schedule, normalize_schedule, Scheduler};
use crate::lockutil::LockOk;
use crate::store::{CronJob, Store};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub struct CronjobTool {
    pub store: Arc<Mutex<Store>>,
    /// 'platform:chat_id' of the session this registry was built for.
    /// None in CLI/TUI sessions (deliver must be explicit there).
    pub origin: Option<String>,
}

pub(crate) fn short_id() -> String {
    uuid::Uuid::new_v4().to_string()[..12].to_string()
}

fn job_summary(j: &CronJob) -> Value {
    json!({
        "job_id": j.id,
        "name": j.name,
        "schedule": j.schedule,
        "schedule_human": describe_schedule(&j.schedule),
        "prompt_preview": crate::session_search::trunc(&j.prompt, 120),
        "deliver": j.deliver,
        "session": j.session,
        "enabled": j.enabled,
        "no_agent": j.no_agent,
        "sandbox": j.sandbox,
        "script": j.script,
        "model": j.model,
        "provider": j.provider,
        "repeat_max": j.repeat_max,
        "run_count": j.run_count,
        "last_run_at": j.last_run_at,
        "last_status": j.last_status,
        "next_run_at": j.next_run_at,
        "node": j.node,
    })
}

impl CronjobTool {
    /// `node` arg → a stored node id. Accepts a full id, a unique id
    /// prefix, or a unique case-insensitive name/name-prefix from
    /// `nodes`; anything else errors with the honest reason (unknown vs
    /// ambiguous) at create/update time, not at 03:00 fire time.
    fn resolve_node(&self, raw: &str) -> Result<String, String> {
        let raw = raw.trim();
        let store = self.store.lock_ok();
        // Exact id or id-prefix first.
        match store
            .node_resolve(raw)
            .map_err(|e| format!("node lookup failed: {e}"))?
        {
            crate::store::NodeMatch::One(row) => return Ok(row.id),
            crate::store::NodeMatch::Ambiguous(c) => {
                return Err(format!("node '{raw}' is ambiguous ({c} matches)"))
            }
            crate::store::NodeMatch::None => {}
        }
        // Name / name-prefix, case-insensitive.
        let all = store
            .nodes_list()
            .map_err(|e| format!("node list failed: {e}"))?;
        let low = raw.to_lowercase();
        let matches: Vec<_> = all
            .iter()
            .filter(|n| n.name.to_lowercase().starts_with(&low))
            .collect();
        match matches.len() {
            1 => Ok(matches[0].id.clone()),
            0 => Err(format!(
                "no node named or prefixed '{raw}' (known: {})",
                all.iter()
                    .map(|n| format!("{}={:.8}", n.name, n.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            n => Err(format!("node '{raw}' is ambiguous ({n} matches)")),
        }
    }

    fn create(&self, args: &Value) -> String {
        let Some(schedule_raw) = args["schedule"].as_str() else {
            return "error: schedule is required for create".into();
        };
        let schedule = match normalize_schedule(schedule_raw) {
            Ok(s) => s,
            Err(e) => return format!("error: {e}"),
        };
        let prompt = args["prompt"].as_str().unwrap_or_default().trim();
        let no_agent = args["no_agent"].as_bool().unwrap_or(false);
        // Session-wakeup job (Stage 5): `session` names an EXISTING
        // session the job resumes instead of minting a fresh one.
        // Resolved at CREATE time, unique-prefix like /resume, so a
        // stale or ambiguous spelling is refused NOW (with the honest
        // reason) rather than failing at 03:00 when the job fires.
        // no_agent scripts never touch it — refusing is better than
        // silently ignoring a field the model believed it set.
        let session = match args["session"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => {
                if no_agent {
                    return "error: session= only applies to agent jobs (a no_agent script has no session to wake)".into();
                }
                match crate::cron::resolve_session(&self.store.lock_ok(), raw) {
                    Ok(sid) => Some(sid),
                    Err(e) => return format!("error: {e}"),
                }
            }
            None => None,
        };
        let script = args["script"]
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if no_agent && script.is_none() {
            return format!(
                "error: no_agent=true requires `script` (path relative to {} or absolute)",
                crate::config::data_dir().join("scripts").display()
            );
        }
        // Sandboxed execution: no_agent .tcl scripts only (see
        // crate::sandbox — a bash script has no interp-level gate, so
        // claiming one for it would be a lie).
        let sandbox = args["sandbox"].as_bool().unwrap_or(false);
        if sandbox && !no_agent {
            return "error: sandbox=true only applies to no_agent script jobs".into();
        }
        if sandbox
            && !script
                .as_deref()
                .map(|p| p.ends_with(".tcl"))
                .unwrap_or(false)
        {
            return "error: sandbox=true requires a .tcl script (bash cannot be interp-gated)"
                .into();
        }
        if !no_agent && prompt.is_empty() {
            return "error: prompt is required for agent jobs".into();
        }
        // Deliver: explicit > home (the platform's home chat, the ops chat —
        // USER 2026-10-21, so the chat a job is made in stays a conversation)
        // > origin > local (CLI sessions have no origin).
        let deliver = args["deliver"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| match self.origin.as_deref() {
                Some(o)
                    if o.split_once(':')
                        .is_some_and(|(p, _)| crate::home::chat(p).is_some()) =>
                {
                    "home".into()
                }
                Some(_) => "origin".into(),
                None => "local".into(),
            });
        if (deliver == "origin" || deliver == "home") && self.origin.is_none() {
            return format!("error: deliver='{deliver}' needs a platform session; this session has no origin — use deliver='local' or 'platform:chat_id'");
        }
        let skills = match args["skills"].as_array() {
            Some(arr) => {
                let names: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                if names.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&names).unwrap_or_default())
                }
            }
            None => None,
        };
        // Node pin (2026-10-22, operator): the lease stops DUPLICATES, it
        // must not force every job onto the lease holder. Default: pin to
        // the CREATING node (the duty lives where it was made — a laptop
        // job runs on the laptop whenever it is up). `node="<id|name>"`
        // pins elsewhere; `node="floating"` restores the legacy
        // lease-holder behavior for a job that genuinely should follow
        // the always-on node.
        let node = match args["node"].as_str().map(str::trim) {
            None | Some("") => Some(
                crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())
                    .map(|i| i.id)
                    .unwrap_or_default(),
            ),
            Some("floating") => None,
            Some(raw) => match self.resolve_node(raw) {
                Ok(n) => Some(n),
                Err(e) => return format!("error: {e}"),
            },
        };
        let job = CronJob {
            id: short_id(),
            name: args["name"].as_str().map(String::from),
            schedule,
            prompt: prompt.to_string(),
            deliver,
            enabled: true,
            script,
            skills,
            no_agent,
            sandbox,
            model: args["model"].as_str().map(String::from),
            provider: args["provider"].as_str().map(String::from),
            reasoning: args["reasoning"].as_str().map(String::from),
            origin: self.origin.clone(),
            session,
            repeat_max: args["repeat"].as_i64(),
            node,
            ..Default::default()
        };
        if let Some(e) = self.store.lock_ok().cron_upsert(&job).err() {
            return format!("error: insert failed: {e}");
        }
        let deliver_note = match job.deliver.as_str() {
            "local" => "output stays local (log only)",
            "home" => "output goes to the platform's home chat (the ops chat) when the gateway adapter is up",
            _ => "output is delivered to the target when the gateway adapter is up",
        };
        let mut shown = job.clone();
        shown.next_run_at = crate::cron::next_fire(&job.schedule, &job, chrono::Utc::now())
            .ok()
            .flatten()
            .map(|t| t.to_rfc3339());
        if let Some(n) = &shown.next_run_at {
            let _ = self.store.lock_ok().cron_set_next(&job.id, Some(n));
        }
        json!({
            "created": job_summary(&shown),
            "note": deliver_note,
        })
        .to_string()
    }

    fn list(&self) -> String {
        let jobs = match self.store.lock_ok().cron_list(false) {
            Ok(j) => j,
            Err(e) => return format!("error: {e}"),
        };
        json!({
            "jobs": jobs.iter().map(job_summary).collect::<Vec<_>>(),
            "count": jobs.len(),
        })
        .to_string()
    }

    fn update(&self, args: &Value) -> String {
        let Some(id) = args["job_id"].as_str() else {
            return "error: job_id required".into();
        };
        let schedule = match args["schedule"].as_str() {
            Some(raw) => match normalize_schedule(raw) {
                Ok(s) => Some(s),
                Err(e) => return format!("error: {e}"),
            },
            None => None,
        };
        let script = args["script"].as_str().map(|s| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        });
        let skills = match args["skills"].as_array() {
            Some(arr) => {
                let names: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                if names.is_empty() {
                    Some(None)
                } else {
                    Some(Some(serde_json::to_string(&names).unwrap_or_default()))
                }
            }
            None => None,
        };
        // `session`: resolvable or refused, same rule as create. An
        // explicit null CLEARS it (back to fresh-session runs); an
        // unresolvable name errors out rather than storing a fire-time
        // landmine.
        let session = match opt_opt_str(args, "session") {
            None => None,
            Some(None) => Some(None),
            Some(Some(raw)) => {
                let raw = raw.trim();
                if raw.is_empty() {
                    Some(None)
                } else {
                    match crate::cron::resolve_session(&self.store.lock_ok(), raw) {
                        Ok(sid) => Some(Some(sid)),
                        Err(e) => return format!("error: {e}"),
                    }
                }
            }
        };
        // `node`: re-pin a job to another node (id or unique name prefix
        // from `nodes`), or explicit null to make it float on the lease
        // again. An unknown node errors NOW rather than storing a pin
        // nobody will ever match.
        let node = match opt_opt_str(args, "node") {
            None => None,
            Some(None) => Some(None),
            Some(Some(raw)) => match self.resolve_node(raw) {
                Ok(n) => Some(Some(n)),
                Err(e) => return format!("error: {e}"),
            },
        };
        let result = self.store.lock_ok().cron_update(
            id,
            args["name"].as_str(),
            schedule.as_deref(),
            args["prompt"].as_str(),
            args["deliver"].as_str(),
            script,
            skills.as_ref().map(|o| o.as_deref()),
            opt_opt_str(args, "model"),
            opt_opt_str(args, "provider"),
            opt_opt_str(args, "reasoning"),
            session.as_ref().map(|o| o.as_deref()),
            args["repeat"].as_i64().map(Some),
            node.as_ref().map(|o| o.as_deref()),
        );
        match result {
            Ok(Some(job)) => json!({"updated": job_summary(&job)}).to_string(),
            Ok(None) => format!("error: no job with id '{id}'"),
            Err(e) => format!("error: {e}"),
        }
    }

    fn set_enabled(&self, args: &Value, enabled: bool) -> String {
        let Some(id) = args["job_id"].as_str() else {
            return "error: job_id required".into();
        };
        match self.store.lock_ok().cron_set_enabled(id, enabled) {
            Ok(true) => format!("job {id} {}", if enabled { "resumed" } else { "paused" }),
            Ok(false) => format!("error: no job with id '{id}'"),
            Err(e) => format!("error: {e}"),
        }
    }

    fn remove(&self, args: &Value) -> String {
        let Some(id) = args["job_id"].as_str() else {
            return "error: job_id required".into();
        };
        match self.store.lock_ok().cron_delete(id) {
            Ok(true) => format!("job {id} removed (run history deleted with it)"),
            Ok(false) => format!("error: no job with id '{id}'"),
            Err(e) => format!("error: {e}"),
        }
    }
}

/// Option<Option<&str>> extraction: key present (even null) = Some, absent = None.
fn opt_opt_str<'a>(args: &'a Value, key: &str) -> Option<Option<&'a str>> {
    match args.get(key) {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => Some(v.as_str()),
    }
}

#[async_trait]
impl crate::tools::Tool for CronjobTool {
    fn name(&self) -> &str {
        "cronjob"
    }
    fn description(&self) -> &str {
        "Manage durable scheduled jobs (sqlite-backed; run by the gateway scheduler). \
         Actions: create (schedule + prompt REQUIRED), list, update, pause, resume, remove, run. \
         Schedule forms: '30m', 'every 2h' (min interval 60s); cron pattern '0 9 * * *' (5 fields); \
         ISO timestamp for one-shots. By default a job runs in a FRESH session with no context — \
         its prompt must be self-contained. session=<id-or-prefix> makes it a WAKEUP instead: the \
         job resumes THAT session with its full history as context (one-off or recurring — 'check \
         in on the deployment we set up yesterday'); if the session is busy the prompt parks on its \
         queue and runs at the turn boundary. deliver: omit for the default — 'home' (the platform's home chat, the ops chat) when \
         this platform has one, else 'origin' (this chat) — or say 'origin', 'home', 'local' (log only), \
         or 'platform:chat_id'. no_agent=true runs `script` \
         with no LLM: non-empty stdout delivered verbatim, EMPTY stdout = silent (watchdog pattern), \
         non-zero exit = error alert. sandbox=true (no_agent .tcl scripts only) runs the script in the \
         safe-interp sandbox (no fs/exec/socket). skills: ordered skill names inlined into the prompt. \
         Jobs cannot schedule more jobs (no cronjob tool in job sessions). Use 'run' to fire once immediately."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "list", "update", "pause", "resume", "remove", "run"]},
                "job_id": {"type": "string"},
                "name": {"type": "string"},
                "schedule": {"type": "string", "description": "'30m' | 'every 2h' | '0 9 * * *' | ISO timestamp"},
                "prompt": {"type": "string"},
                "deliver": {"type": "string"},
                "no_agent": {
                    "type": "boolean"
                },
                "sandbox": {
                    "type": "boolean",
                    "description": "no_agent .tcl scripts only: run in the safe-interp sandbox (no fs/exec/socket)"
                },
                "script": {"type": "string", "description": format!(
                    "Path relative to {} or absolute",
                    crate::config::data_dir().join("scripts").display()
                )},
                "skills": {"type": "array", "items": {"type": "string"}},
                "session": {
                    "type": ["string", "null"],
                    "description": "session id (or unique prefix) to RESUME as a wakeup instead of a fresh session; null clears it (update only)"
                },
                "model": {"type": "string"},
                "provider": {"type": "string"},
                "reasoning": {"type": "string"},
                "repeat": {"type": "integer", "description": "Repeat count cap; omit for forever (recurring) / once (one-shot)"},
                "node": {
                    "type": ["string", "null"],
                    "description": "Node pin: id or unique name prefix from `nodes`. Create defaults to the CREATING node (the duty runs there whenever it is up); 'floating' (create) or null (update) restores the legacy behavior of following the singleton:cron lease holder."
                }
            },
            "required": ["action"]
        })
    }
    async fn call(&self, args: Value) -> String {
        match args["action"].as_str().unwrap_or("") {
            "create" => self.create(&args),
            "list" => self.list(),
            "update" => self.update(&args),
            "pause" => self.set_enabled(&args, false),
            "resume" => self.set_enabled(&args, true),
            "remove" => self.remove(&args),
            "run" => self.run_now(&args).await,
            other => format!("error: unknown action '{other}'"),
        }
    }
}

impl CronjobTool {
    /// Fire a job immediately, outside the tick schedule. Records a run
    /// row but does NOT consume a schedule slot or touch next_run_at.
    async fn run_now(&self, args: &Value) -> String {
        let Some(id) = args["job_id"].as_str() else {
            return "error: job_id required".into();
        };
        let job = match self.store.lock_ok().cron_get(id) {
            Ok(Some(j)) => j,
            Ok(None) => return format!("error: no job with id '{id}'"),
            Err(e) => return format!("error: {e}"),
        };
        let cfg = match crate::config::load() {
            Ok(c) => Arc::new(c),
            Err(e) => return format!("error: config: {e}"),
        };
        let sched = Scheduler::new(
            self.store.clone(),
            cfg,
            crate::hooks::HookBus::new(None),
            None, // standalone: delivery falls back to stdout/log
        );
        let summary = sched.run_job_manual(&job).await;
        format!("manual run finished: {summary}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;

    /// A CronjobTool over a scratch store. `origin` set = the shape a
    /// platform session builds; None = CLI/TUI.
    fn tool(origin: Option<&str>) -> (tempfile::TempDir, CronjobTool) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        (
            dir,
            CronjobTool {
                store,
                origin: origin.map(String::from),
            },
        )
    }

    async fn create_ok(t: &CronjobTool, extra: Value) -> String {
        let mut args = json!({
            "action": "create",
            "schedule": "30m",
            "prompt": "do the thing",
        });
        for (k, v) in extra.as_object().unwrap() {
            args[k.clone()] = v.clone();
        }
        t.call(args).await
    }

    /// USER 2026-10-21 ("Same bot, dedicated ops chat"): a job made in a
    /// chat whose platform has a home chat reports there by default —
    /// the DM stays a working session. `deliver: "origin"` keeps the old
    /// routing; a platform with no home chat is unchanged.
    #[tokio::test]
    async fn a_job_from_a_platform_with_a_home_chat_delivers_home_by_default() {
        crate::home::set("tghome2", "-100888");
        let (_d, t) = tool(Some("tghome2:555"));
        let out = create_ok(&t, json!({})).await;
        let v: Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("json: {out}"));
        assert_eq!(v["created"]["deliver"], json!("home"), "{out}");
        assert!(
            out.contains("home chat"),
            "the note names where it goes: {out}"
        );

        let out = create_ok(&t, json!({ "deliver": "origin" })).await;
        let v: Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("json: {out}"));
        assert_eq!(v["created"]["deliver"], json!("origin"), "{out}");

        let (_d2, t2) = tool(Some("tgnohome2:1"));
        let out = create_ok(&t2, json!({})).await;
        let v: Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("json: {out}"));
        assert_eq!(v["created"]["deliver"], json!("origin"), "{out}");
    }

    #[tokio::test]
    async fn create_requires_schedule_and_prompt() {
        let (_d, t) = tool(None);
        let out = t.call(json!({ "action": "create" })).await;
        assert!(out.contains("schedule is required"), "{out}");

        let out = t
            .call(json!({ "action": "create", "schedule": "30m" }))
            .await;
        assert!(
            out.contains("prompt is required"),
            "agent jobs need a prompt: {out}"
        );
    }

    #[tokio::test]
    async fn no_agent_jobs_need_a_script_and_reject_prompts_only() {
        let (_d, t) = tool(None);
        let out = t
            .call(json!({
                "action": "create", "schedule": "30m",
                "no_agent": true,
            }))
            .await;
        assert!(out.contains("no_agent=true requires `script`"), "{out}");

        // The working form: script, no prompt.
        let out = create_ok(&t, json!({ "no_agent": true, "script": "watch.sh" })).await;
        assert!(out.contains("\"created\""), "{out}");
    }

    #[tokio::test]
    async fn deliver_origin_from_cli_is_refused_not_misrouted() {
        // The recursion-adjacent hazard: a job created in a session with
        // NO origin asking for deliver=origin has no target. It must
        // refuse, never silently fall back to something that looks right.
        let (_d, t) = tool(None);
        let out = t
            .call(json!({
                "action": "create", "schedule": "30m",
                "prompt": "x", "deliver": "origin",
            }))
            .await;
        assert!(out.contains("needs a platform session"), "{out}");
    }

    #[tokio::test]
    async fn create_then_list_round_trips_through_the_store() {
        let (_d, t) = tool(Some("telegram:123"));
        let created = create_ok(&t, json!({})).await;
        assert!(created.contains("\"created\""), "{created}");

        let listed = t.call(json!({ "action": "list" })).await;
        assert!(listed.contains("do the thing"), "{listed}");
        assert!(listed.contains("\"count\":1"), "{listed}");
    }

    #[tokio::test]
    async fn pause_resume_remove_lifecycle() {
        let (_d, t) = tool(Some("telegram:123"));
        let created = create_ok(&t, json!({})).await;
        let id: String = {
            let v: Value = serde_json::from_str(&created).unwrap();
            v["created"]["job_id"].as_str().unwrap().to_string()
        };

        let out = t.call(json!({ "action": "pause", "job_id": id })).await;
        assert!(out.contains("paused"), "{out}");
        let out = t.call(json!({ "action": "resume", "job_id": id })).await;
        assert!(out.contains("resumed"), "{out}");
        let out = t.call(json!({ "action": "remove", "job_id": id })).await;
        assert!(out.contains("removed"), "{out}");
        // And removing again is an ERROR, not a success — a double remove
        // that reported success would hide a real bug.
        let out = t.call(json!({ "action": "remove", "job_id": id })).await;
        assert!(out.starts_with("error:"), "{out}");
    }

    #[tokio::test]
    async fn unknown_action_is_an_error_not_a_panic() {
        let (_d, t) = tool(None);
        let out = t.call(json!({ "action": "explode" })).await;
        assert_eq!(out, "error: unknown action 'explode'");
    }

    /// Stage 5: `session` on create resolves at CREATION (full id or
    /// unique prefix), round-trips through the store, and an
    /// unresolvable or ambiguous reference is refused NOW — a typo
    /// must not become a 03:00 failure.
    #[tokio::test]
    async fn create_with_session_resolves_at_creation_and_refuses_a_bad_one() {
        let (_d, t) = tool(Some("telegram:123"));
        t.store
            .lock()
            .unwrap()
            .create_session("20260918_morning")
            .unwrap();
        t.store
            .lock()
            .unwrap()
            .create_session("20260918_evening")
            .unwrap();

        // Unique prefix resolves to the full id.
        let out = create_ok(&t, json!({ "session": "20260918_morn" })).await;
        assert!(out.contains("\"created\""), "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["created"]["session"], "20260918_morning", "{out}");

        // Ambiguous prefix: refused, not guessed.
        let out = create_ok(&t, json!({ "session": "20260918" })).await;
        assert!(
            out.starts_with("error:") && out.contains("matches 2 sessions"),
            "{out}"
        );

        // Unknown: refused.
        let out = create_ok(&t, json!({ "session": "ghost" })).await;
        assert!(
            out.starts_with("error:") && out.contains("no session matching"),
            "{out}"
        );

        // no_agent + session: refused (a script job has no session to
        // wake; silently ignoring it would lie to the model).
        let out = t
            .call(json!({
                "action": "create", "schedule": "30m", "no_agent": true,
                "script": "watch.sh", "session": "20260918_morning",
            }))
            .await;
        assert!(
            out.starts_with("error:") && out.contains("no_agent"),
            "{out}"
        );
    }

    /// Update can SET, MOVE and CLEAR the wakeup target — a job is not
    /// welded to the session it was created for, and null must go back
    /// to fresh-session runs rather than refusing.
    #[tokio::test]
    async fn update_moves_and_clears_the_session_target() {
        let (_d, t) = tool(Some("telegram:123"));
        t.store.lock().unwrap().create_session("sess-a").unwrap();
        t.store.lock().unwrap().create_session("sess-b").unwrap();
        let created = create_ok(&t, json!({ "session": "sess-a" })).await;
        let id: String = {
            let v: Value = serde_json::from_str(&created).unwrap();
            v["created"]["job_id"].as_str().unwrap().to_string()
        };

        let out = t
            .call(json!({ "action": "update", "job_id": id, "session": "sess-b" }))
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["updated"]["session"], "sess-b", "{out}");

        let out = t
            .call(json!({ "action": "update", "job_id": id, "session": Value::Null }))
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["updated"]["session"].is_null(), "null clears: {out}");

        // And a bad target on update is refused, leaving the row alone.
        let out = t
            .call(json!({ "action": "update", "job_id": id, "session": "ghost" }))
            .await;
        assert!(out.starts_with("error:"), "{out}");
        let j = t.store.lock().unwrap().cron_get(&id).unwrap().unwrap();
        assert!(j.session.is_none(), "the refused update changed nothing");
    }

    #[tokio::test]
    async fn operations_on_unknown_job_ids_error_cleanly() {
        let (_d, t) = tool(Some("telegram:1"));
        for action in ["update", "pause", "resume", "remove"] {
            let out = t
                .call(json!({ "action": action, "job_id": "nosuch" }))
                .await;
            assert!(out.contains("no job with id"), "{action}: {out}");
        }
    }
}
