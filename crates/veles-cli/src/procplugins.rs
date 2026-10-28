//! `veles plugins status|reload|stop|start|logs` — the running gateway's
//! process plugins, over its desktop API (the same loopback client and
//! bearer token as `veles tcl`).
//!
//! These act on the GATEWAY's host, not on anything in this process: a
//! process plugin is a child of the gateway, and reloading one leaves the
//! gateway — and every other plugin — running.

use serde_json::Value;

use crate::tclrepl::{describe, http_request};

/// `target` is `name` or `name:instance`; it goes in a path segment.
fn segment(target: &str) -> Result<&str, String> {
    let ok = !target.is_empty()
        && target != "."
        && target != ".."
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-:".contains(c));
    if ok {
        Ok(target)
    } else {
        Err(format!(
            "`{target}` is not a plugin target (name or name:instance)"
        ))
    }
}

fn call(method: &str, path: &str) -> Result<Value, String> {
    let (status, resp) = http_request(method, path, None)?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::Null);
    if status != 200 {
        return Err(v["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| describe(status, &resp)));
    }
    Ok(v)
}

/// One line per instance, for a terminal. `tier` marks the row when the
/// generic runtime surface answered both kinds.
pub fn render_status(v: &Value) -> String {
    if v["host"] != Value::Bool(true) {
        return "the gateway runs no plugin host".into();
    }
    let rows = v["instances"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no process plugins (installed under $VELES_PLUGIN_PATH, \
                <data_dir>/process-plugins or <prefix>/lib/veles/plugins)"
            .into();
    }
    let mut out = Vec::new();
    for r in rows {
        let s = |k: &str| r[k].as_str().unwrap_or_default().to_string();
        let label = if s("instance").is_empty() {
            s("plugin")
        } else {
            format!("{}:{}", s("plugin"), s("instance"))
        };
        let tier = match s("tier").as_str() {
            "" | "process" => String::new(),
            t => format!("[{t}] "),
        };
        let mut line = format!("{tier}{label:<28} {:<8}", s("state"));
        if let Some(pid) = r["pid"].as_u64() {
            line.push_str(&format!(" pid {pid}"));
        }
        if !s("version").is_empty() {
            line.push_str(&format!(" v{}", s("version")));
        }
        if let Some(n) = r["restarts"].as_u64().filter(|n| *n > 0) {
            line.push_str(&format!(" ({n} restarts)"));
        }
        if !s("lease").is_empty() {
            line.push_str(&format!(" lease {}", s("lease")));
        }
        out.push(line);
        if !s("last_error").is_empty() {
            out.push(format!("    last error: {}", s("last_error")));
        }
    }
    out.join("\n")
}

/// The generic runtime surface (R3): both tiers, one status call.
pub fn status() -> anyhow::Result<()> {
    // Prefer the generic surface; fall back to the process-only one for
    // a gateway that predates it (the two are the same data for the
    // process tier).
    let v = call("GET", "/api/plugins/runtime")
        .or_else(|_| call("GET", "/api/process-plugins"))
        .map_err(anyhow::Error::msg)?;
    println!("{}", render_status(&v));
    Ok(())
}

/// `reload`, `stop`, `start` or `unload`, on whichever tier hosts the
/// target (a module, a process plugin, or a Tcl plugin by name).
pub fn control(action: &str, target: &str) -> anyhow::Result<()> {
    let target = segment(target).map_err(anyhow::Error::msg)?;
    // The generic surface resolves the tier; a gateway that predates it
    // takes the process-only path (module targets 404 there, with the
    // same message the operator would get).
    let v = call("POST", &format!("/api/plugins/runtime/{target}/{action}"))
        .or_else(|_| call("POST", &format!("/api/process-plugins/{target}/{action}")))
        .map_err(anyhow::Error::msg)?;
    for line in v["results"].as_array().cloned().unwrap_or_default() {
        println!("{}", line.as_str().unwrap_or_default());
    }
    Ok(())
}

/// Re-discover under the gateway's roots and start anything new.
pub fn rescan() -> anyhow::Result<()> {
    let v = call("POST", "/api/plugins/runtime/rescan").map_err(anyhow::Error::msg)?;
    let started = v["started"].as_array().cloned().unwrap_or_default();
    if started.is_empty() {
        println!("nothing new");
    } else {
        for s in started {
            println!("started {}", s.as_str().unwrap_or_default());
        }
    }
    Ok(())
}

/// The running gateway's service registry: who provides each service,
/// who declares and uses it, what is missing (`/plugins services`).
pub fn services() -> anyhow::Result<()> {
    let v = call("GET", "/api/plugins/services").map_err(anyhow::Error::msg)?;
    print!("{}", render_services(&v));
    Ok(())
}

/// `veles plugins overrides`: the running gateway's override points
/// (PLAN-extension-api.md D2), each with who answers it, top first.
pub fn overrides() -> anyhow::Result<()> {
    let v = call("GET", "/api/plugins/overrides").map_err(anyhow::Error::msg)?;
    print!("{}", render_overrides(&v));
    Ok(())
}

/// The `/api/plugins/overrides` rows as text, one point per paragraph.
pub fn render_overrides(v: &serde_json::Value) -> String {
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no override points declared\n".into();
    }
    let mut out = String::new();
    for r in rows {
        let name = r["name"].as_str().unwrap_or("?");
        let stack: Vec<&str> = r["stack"]
            .as_array()
            .map(|a| a.iter().filter_map(|s| s.as_str()).collect())
            .unwrap_or_default();
        let state = match stack.as_slice() {
            [] => "nothing answers".to_string(),
            [only] => format!("← {only} (the default; no override)"),
            [top, rest @ ..] => format!("← {top}, over {}", rest.join(", ")),
        };
        out.push_str(&format!("{name} {state}\n"));
        if let Some(s) = r["summary"].as_str().filter(|s| !s.is_empty()) {
            out.push_str(&format!("    {s}\n"));
        }
    }
    out
}

/// The `/api/plugins/services` rows as text, one service per paragraph.
pub fn render_services(v: &serde_json::Value) -> String {
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no services: no plugin provides or uses one\n".into();
    }
    let mut out = String::new();
    for r in rows {
        let name = r["name"].as_str().unwrap_or("?");
        let state = match r["provider"].as_str() {
            Some(p) => format!("← {p} ({})", r["access"].as_str().unwrap_or("?")),
            None if r["missing_required"].as_bool().unwrap_or(false) => {
                "MISSING (a required use has no provider here)".into()
            }
            None => "not provided here".into(),
        };
        out.push_str(&format!("{name} {state}\n"));
        let list = |key: &str| -> Vec<String> {
            r[key]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|x| match x {
                    serde_json::Value::String(s) => s.clone(),
                    o => {
                        let p = o["plugin"].as_str().unwrap_or("?");
                        if o["optional"].as_bool().unwrap_or(false) {
                            format!("{p} (optional)")
                        } else {
                            p.to_string()
                        }
                    }
                })
                .collect()
        };
        let declared = list("declared_by");
        if !declared.is_empty() {
            out.push_str(&format!("    declared by: {}\n", declared.join(", ")));
        }
        let used = list("used_by");
        if !used.is_empty() {
            out.push_str(&format!("    used by: {}\n", used.join(", ")));
        }
    }
    out
}

pub fn logs(target: &str) -> anyhow::Result<()> {
    let target = segment(target).map_err(anyhow::Error::msg)?;
    let v =
        call("GET", &format!("/api/process-plugins/{target}/logs")).map_err(anyhow::Error::msg)?;
    for inst in v["logs"].as_array().cloned().unwrap_or_default() {
        println!("── {}", inst["instance"].as_str().unwrap_or_default());
        for l in inst["lines"].as_array().cloned().unwrap_or_default() {
            println!("{}", l.as_str().unwrap_or_default());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The services view says who answers, and flags a required use
    /// with no provider.
    #[test]
    fn the_services_view_names_providers_and_gaps() {
        let v = json!([
            {"name": "appview.query", "provider": "process:atproto", "access": "any",
             "declared_by": ["atproto"], "used_by": [{"plugin": "irc", "optional": true}],
             "missing_required": false},
            {"name": "weather.now", "provider": null, "access": null,
             "declared_by": [], "used_by": [{"plugin": "forecast", "optional": false}],
             "missing_required": true}
        ]);
        let text = render_services(&v);
        assert!(
            text.contains("appview.query ← process:atproto (any)"),
            "{text}"
        );
        assert!(text.contains("used by: irc (optional)"), "{text}");
        assert!(text.contains("weather.now MISSING"), "{text}");
        assert_eq!(
            render_services(&json!([])),
            "no services: no plugin provides or uses one\n"
        );
    }

    /// A target goes into a URL path: nothing that could leave the
    /// `/api/process-plugins/` prefix gets through.
    #[test]
    fn a_target_is_a_name_not_a_path() {
        assert!(segment("echo").is_ok());
        assert!(segment("irc:synirc").is_ok());
        for bad in ["", ".", "..", "../tcl/eval", "a/b", "a b", "a?x=1", "a%2f"] {
            assert!(segment(bad).is_err(), "{bad} passed");
        }
    }

    #[test]
    fn status_names_state_pid_restarts_and_the_last_error() {
        let v = json!({"host": true, "instances": [
            {"plugin": "echo", "instance": "a", "state": "running", "pid": 42,
             "version": "0.1.0", "restarts": 2, "since": 0, "last_error": "boom",
             "lease": "singleton:echo:a"},
            {"plugin": "bad", "instance": "", "state": "refused", "pid": null,
             "version": "", "restarts": 0, "since": 0, "last_error": "protocol 99",
             "lease": null},
        ]});
        let out = render_status(&v);
        assert!(out.contains("echo:a"), "{out}");
        assert!(
            out.contains("pid 42") && out.contains("(2 restarts)"),
            "{out}"
        );
        assert!(out.contains("last error: boom"), "{out}");
        assert!(out.contains("bad ") && out.contains("refused"), "{out}");
        assert!(render_status(&json!({"host": false})).contains("no plugin host"));
    }
}
