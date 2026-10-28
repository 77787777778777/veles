//! Settings by touch (PLAN-sessions-and-phone.md A2, round P2): every
//! option this binary reads, as a form a window can draw without knowing
//! any of them.
//!
//! Typing TOML on a phone's keyboard is not setup. The option surface
//! already exists (`config_surface::knobs`, derived from the structs that
//! read config.toml, so it cannot rot); this turns it into fields: each
//! one's kind, read off its default by the TOML library, the value
//! config.toml holds now, and when a change takes effect. The window
//! sends typed JSON back through `/api/config/set`, which keeps comments,
//! backs the file up and refuses a result that would not parse.
//!
//! Left out, each on purpose:
//! - arrays of tables (`[[custom_providers]]`): a record, not a field;
//! - credentials (a key named like one): they belong in the vault, asked
//!   for privately, never typed into a settings page beside the others.

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::config_surface::{Knob, Shape};

/// Knobs the binary knows and core cannot derive: each connector's own
/// `[platforms.*]` options. The binary registers them once at start
/// (veles-cli's `blob_surface`); core's own come from `config_surface`.
static EXTRA: OnceLock<Mutex<Vec<Knob>>> = OnceLock::new();

pub fn register_knobs(knobs: Vec<Knob>) {
    let cell = EXTRA.get_or_init(|| Mutex::new(Vec::new()));
    let mut all = cell.lock().unwrap_or_else(|e| e.into_inner());
    for k in knobs {
        if !all.iter().any(|have| have.path == k.path) {
            all.push(k);
        }
    }
}

fn registered() -> Vec<Knob> {
    EXTRA
        .get()
        .map(|c| c.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .unwrap_or_default()
}

/// A TOML literal ("5", "true", "\"x\"", "[\"a\"]") as JSON, through the
/// TOML library. `None` when it is not one.
pub fn literal_json(lit: &str) -> Option<Value> {
    let v: toml_edit::Value = lit.trim().parse().ok()?;
    let doc: toml::Table = format!("v = {v}").parse().ok()?;
    serde_json::to_value(doc.get("v")?).ok()
}

/// What control a value wants.
fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "string",
        Value::Array(a) if a.iter().all(|x| x.is_string()) => "list",
        _ => "other",
    }
}

/// The form: `{"sections": [{"name", "knobs": [{path, key, kind,
/// default, value, set, effect, note, env}]}]}`: sections in order, each
/// once, and keys in order within them. `kind` is `text` when neither a default nor a value says
/// what the option holds: the window sends what was typed as `raw`, and
/// the gateway reads it as a TOML literal.
pub fn form() -> Value {
    let doc = crate::configedit::read_doc().unwrap_or_default();
    let mut knobs = crate::config_surface::knobs();
    knobs.extend(crate::config_surface::platform_surface());
    for k in registered() {
        if !knobs.iter().any(|have| have.path == k.path) {
            knobs.push(k);
        }
    }
    knobs.sort_by(|a, b| a.path.cmp(&b.path));
    knobs.dedup_by(|a, b| a.path == b.path);
    // Grouped by section, then by key: path order alone would split a
    // section around its sub-tables (agent.a, agent.compaction.x, agent.z).
    knobs.sort_by(|a, b| (a.section(), a.key()).cmp(&(b.section(), b.key())));

    let mut sections: Vec<(String, Vec<Value>)> = Vec::new();
    for k in knobs {
        if k.shape == Shape::TableArray || crate::config_tool::names_a_secret(&k.path) {
            continue;
        }
        let segs: Vec<&str> = k.path.split('.').collect();
        let value = crate::configedit::get_json(&doc, &segs);
        let default = k.default.as_deref().and_then(literal_json);
        let kind = default
            .as_ref()
            .or(value.as_ref())
            .map(kind_of)
            .unwrap_or("text");
        let effect = crate::configedit::effect_for(&k.path);
        let field = json!({
            "path": k.path,
            "key": k.key(),
            "kind": kind,
            "default": default,
            "value": value,
            "set": value.is_some(),
            "effect": effect.as_str(),
            "note": k.note,
            "env": k.env,
        });
        let section = k.section().to_string();
        match sections.last_mut() {
            Some((name, fields)) if *name == section => fields.push(field),
            _ => sections.push((section, vec![field])),
        }
    }
    json!({
        "sections": sections
            .into_iter()
            .map(|(name, knobs)| json!({"name": name, "knobs": knobs}))
            .collect::<Vec<_>>()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(form: &'a Value, path: &str) -> Option<&'a Value> {
        form["sections"]
            .as_array()?
            .iter()
            .flat_map(|s| s["knobs"].as_array().into_iter().flatten())
            .find(|k| k["path"] == path)
    }

    #[test]
    fn literals_are_read_by_the_toml_library() {
        assert_eq!(literal_json("5"), Some(json!(5)));
        assert_eq!(literal_json("true"), Some(json!(true)));
        assert_eq!(literal_json("0.5"), Some(json!(0.5)));
        assert_eq!(literal_json("\"a b\""), Some(json!("a b")));
        assert_eq!(literal_json("[\"a\", \"b\"]"), Some(json!(["a", "b"])));
        assert_eq!(literal_json("a word"), None);
    }

    #[test]
    fn every_option_is_a_typed_field_with_its_value_now() {
        let (_home, _g) = crate::testenv::scratch_home();
        std::fs::write(
            crate::configedit::config_path(),
            "[agent.guards]\nmax_iterations = 42\n[agent.compaction]\ntrigger_percent = 61\n",
        )
        .unwrap();
        register_knobs(vec![
            Knob::blob(
                "platforms.formtest.extra.greeting",
                Some("\"hi\"".into()),
                None,
            ),
            Knob::blob("platforms.formtest.extra.token", Some("\"\"".into()), None),
        ]);
        let f = form();

        let t = field(&f, "agent.guards.max_iterations").expect("a typed option is a field");
        assert_eq!(t["kind"], "int", "{t}");
        assert_eq!(t["value"], json!(42), "{t}");
        assert_eq!(t["set"], json!(true), "{t}");
        assert!(t["default"].is_number(), "{t}");
        assert_eq!(t["key"], "max_iterations");
        assert_eq!(t["effect"], "new_sessions", "{t}");
        // Typed by its default, not by what the file happens to hold.
        let p = field(&f, "agent.compaction.trigger_percent").expect("a float option");
        assert_eq!(p["kind"], "float", "{p}");
        assert_eq!(p["value"], json!(61), "{p}");

        let b = f["sections"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["knobs"].as_array().unwrap().iter())
            .find(|k| k["kind"] == "bool")
            .expect("some option is a switch");
        assert!(b["default"].is_boolean(), "{b}");

        let g = field(&f, "platforms.formtest.extra.greeting").expect("a registered knob");
        assert_eq!(g["kind"], "string");
        assert_eq!(g["set"], json!(false));

        // Records and credentials are not fields.
        assert!(field(&f, "custom_providers").is_none());
        assert!(field(&f, "platforms.formtest.extra.token").is_none());
        assert!(f["sections"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["knobs"].as_array().unwrap().iter())
            .all(|k| !crate::config_tool::names_a_secret(k["path"].as_str().unwrap())));
        // Sections come in path order, each one once.
        let names: Vec<&str> = f["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, sorted);
    }
}
