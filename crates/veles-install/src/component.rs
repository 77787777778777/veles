//! Component manifests and configuration planning. Manifest Knobs are the
//! only generic option surface; component names are not frontend-specific.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use veles_types::plugin::{Manifest, PROTOCOL};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    Module,
    ProcessPlugin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentSpec {
    pub name: String,
    pub kind: ComponentKind,
    pub manifest: Manifest,
    pub source: Option<PathBuf>,
    /// Listing a component must not run it. A failed describe stays a
    /// visible error on that row; selecting/configuring it is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub describe_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPlan {
    pub base_sig: String,
    pub config_path: PathBuf,
    pub config_toml: String,
    pub changes: Vec<String>,
}

/// The shared setup signature distinguishes a missing file (`None`) from
/// an existing empty file (`Some("")`). Component previews preserve that
/// distinction too. It is content identity, not inode/recreation proof.
fn config_signature(existing: Option<&str>) -> String {
    veles_core::setup::base_sig(existing)
}

fn validate_knob(knob: &veles_types::Knob, value: &serde_json::Value) -> Result<()> {
    use veles_types::Shape;
    let default = knob
        .default
        .as_deref()
        .map(|d| {
            format!("v = {d}")
                .parse::<toml::Value>()
                .map(|v| v.get("v").cloned().unwrap_or(toml::Value::String(d.into())))
        })
        .transpose()
        .with_context(|| format!("{} has an invalid default in its manifest", knob.path))?;
    let same_kind = |v: &serde_json::Value, d: &toml::Value| match d {
        toml::Value::String(_) => v.is_string(),
        toml::Value::Integer(_) => v.as_i64().is_some(),
        toml::Value::Float(_) => v.is_number(),
        toml::Value::Boolean(_) => v.is_boolean(),
        toml::Value::Datetime(_) => v.is_string(),
        toml::Value::Array(_) => v.is_array(),
        toml::Value::Table(_) => v.is_object(),
    };
    let ok = match knob.shape {
        Shape::Value | Shape::Documented => default.as_ref().is_none_or(|d| same_kind(value, d)),
        Shape::ScalarArray => value.as_array().is_some_and(|a| {
            a.iter()
                .all(|v| !v.is_array() && !v.is_object() && !v.is_null())
        }),
        Shape::TableArray => value
            .as_array()
            .is_some_and(|a| a.iter().all(serde_json::Value::is_object)),
        Shape::Unset => value.is_null(),
    };
    anyhow::ensure!(
        ok,
        "{} has a value incompatible with {:?} / its default",
        knob.path,
        knob.shape
    );
    Ok(())
}

/// Plan only scalar/array Knobs actually declared by this component.
/// `setup` remains the one provider/secret wizard; it is not bypassed.
pub fn plan_config(
    path: &Path,
    spec: &ComponentSpec,
    values: &BTreeMap<String, serde_json::Value>,
) -> Result<ConfigPlan> {
    plan_config_for_instance(path, spec, values, "")
}

pub fn plan_config_for_instance(
    path: &Path,
    spec: &ComponentSpec,
    values: &BTreeMap<String, serde_json::Value>,
    instance: &str,
) -> Result<ConfigPlan> {
    let existing = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    plan_config_text_with_base(&existing, path, spec, values, instance, path.is_file())
}

/// Plan against supplied text. The combined setup installer uses this to
/// merge provider and component answers before ONE config write.
pub fn plan_config_text(
    existing: &str,
    path: &Path,
    spec: &ComponentSpec,
    values: &BTreeMap<String, serde_json::Value>,
    instance: &str,
) -> Result<ConfigPlan> {
    plan_config_text_with_base(existing, path, spec, values, instance, true)
}

fn plan_config_text_with_base(
    existing: &str,
    path: &Path,
    spec: &ComponentSpec,
    values: &BTreeMap<String, serde_json::Value>,
    instance: &str,
    file_existed: bool,
) -> Result<ConfigPlan> {
    if !instance.is_empty() {
        veles_core::plugins::validate_name(instance)?;
        let veles_types::plugin::Instances::PerEntry { .. } = &spec.manifest.instances else {
            bail!(
                "{} is single-instance; an instance name is not valid",
                spec.name
            )
        };
    }
    crate::valid_component_name(&spec.name)?;
    crate::valid_version(&spec.manifest.version)?;
    anyhow::ensure!(
        spec.manifest.protocol == PROTOCOL,
        "{} uses plugin protocol {}, expected {}",
        spec.name,
        spec.manifest.protocol,
        PROTOCOL
    );
    anyhow::ensure!(
        spec.manifest.name == spec.name,
        "component name {} does not match manifest {}",
        spec.name,
        spec.manifest.name
    );
    let mut doc = existing
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))?;
    let declared: BTreeMap<_, _> = spec
        .manifest
        .knobs
        .iter()
        .map(|k| (k.path.clone(), k))
        .collect();
    let mut changes = Vec::new();
    let mut instance_row = None;
    if !instance.is_empty() {
        let veles_types::plugin::Instances::PerEntry {
            array,
            name: name_key,
        } = &spec.manifest.instances
        else {
            bail!(
                "{} is single-instance; an instance name is not valid",
                spec.name
            )
        };
        let array_path: Vec<String> = array.split('.').map(str::to_string).collect();
        let borrowed: Vec<&str> = array_path.iter().map(String::as_str).collect();
        if doc.get(&array_path[0]).is_none() {
            doc.insert(
                &array_path[0],
                toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
            );
        }
        let tables = veles_core::configedit::get_item_mut(doc.as_item_mut(), &borrowed)
            .and_then(toml_edit::Item::as_array_of_tables_mut)
            .with_context(|| format!("{array} must be an array of tables for a named instance"))?;
        let mut index = tables
            .iter()
            .position(|t| t.get(name_key).and_then(toml_edit::Item::as_str) == Some(instance));
        if index.is_none() {
            let mut t = toml_edit::Table::new();
            t.insert(name_key, toml_edit::value(instance));
            tables.push(t);
            index = Some(tables.len() - 1);
        }
        instance_row = Some((array_path, index.unwrap()));
        changes.push(format!("instance {instance}"));
    }
    for (name, value) in values {
        let Some(knob) = declared.get(name) else {
            bail!("{name} is not declared by component {}", spec.name)
        };
        validate_knob(knob, value)?;
        let segments: Vec<&str> = name.split('.').collect();
        anyhow::ensure!(
            segments.iter().all(|p| !p.is_empty()),
            "invalid option path {name}"
        );
        let mut config_sections = spec.manifest.config_sections.clone();
        if let veles_types::plugin::Instances::PerEntry { array, .. } = &spec.manifest.instances {
            config_sections.push(array.clone());
        }
        anyhow::ensure!(
            config_sections
                .iter()
                .any(|section| name == section || name.starts_with(&format!("{section}."))),
            "{name} is outside the config sections declared by {}",
            spec.name
        );
        // A per-entry instance overrides the same ordinary scalar keys
        // inside its table. `platforms.irc.server` becomes
        // `server` in its [[platforms.irc.networks]] row, not an
        // invented `platforms.irc.networks.server` knob.
        let segments = if let Some((array_path, row)) = &instance_row {
            let suffix = name
                .strip_prefix(&format!(
                    "{}.",
                    spec.manifest
                        .config_sections
                        .first()
                        .map(String::as_str)
                        .unwrap_or("")
                ))
                .with_context(|| format!("{name} cannot be overridden per instance"))?;
            let borrowed: Vec<&str> = array_path.iter().map(String::as_str).collect();
            let table = veles_core::configedit::get_item_mut(doc.as_item_mut(), &borrowed)
                .and_then(toml_edit::Item::as_array_of_tables_mut)
                .and_then(|tables| tables.get_mut(*row))
                .context("named instance row disappeared during planning")?;
            let path: Vec<&str> = suffix.split('.').collect();
            let before = veles_core::configedit::get_json_item(
                &toml_edit::Item::Table(table.clone()),
                &path,
            );
            if before.as_ref() != Some(value) {
                changes.push(format!("{instance}: {name}"));
                let mut item = toml_edit::Item::Table(table.clone());
                veles_core::configedit::set_item(
                    &mut item,
                    &path,
                    toml_edit::Item::Value(veles_core::configedit::json_to_toml(value)?),
                )?;
                if let toml_edit::Item::Table(updated) = item {
                    *table = updated;
                }
            }
            continue;
        } else {
            segments
        };
        let before = veles_core::configedit::get_json(&doc, &segments);
        if before.as_ref() != Some(value) {
            changes.push(name.clone());
            veles_core::configedit::set_value(
                &mut doc,
                &segments,
                veles_core::configedit::json_to_toml(value)?,
            )?;
        }
    }
    let rendered = doc.to_string();
    toml::from_str::<veles_core::config::VelesConfig>(&rendered)
        .context("planned configuration would be unreadable")?;
    Ok(ConfigPlan {
        base_sig: config_signature(file_existed.then_some(existing)),
        config_path: path.to_path_buf(),
        config_toml: rendered,
        changes,
    })
}

/// Installer previews are a non-secret view even when an existing
/// config predates the vault. Keep comments and non-secret values so
/// the diff is useful; replace every credential-looking key's value.
pub fn redact_config(text: &str) -> String {
    let Ok(document) = text.parse::<toml_edit::ImDocument<String>>() else {
        return "<unreadable TOML withheld from installer preview>\n".into();
    };
    fn collect(item: &toml_edit::Item, out: &mut Vec<(std::ops::Range<usize>, bool)>) {
        match item {
            toml_edit::Item::Table(table) => {
                for (key, child) in table.iter() {
                    if veles_core::config_tool::names_a_secret(key) {
                        if let Some(span) = child.span() {
                            out.push((span, !matches!(child, toml_edit::Item::Value(_))));
                        }
                    } else {
                        collect(child, out);
                    }
                }
            }
            toml_edit::Item::ArrayOfTables(tables) => {
                for table in tables.iter() {
                    collect(&toml_edit::Item::Table(table.clone()), out);
                }
            }
            toml_edit::Item::Value(_) | toml_edit::Item::None => {}
        }
    }
    let mut spans = Vec::new();
    // ImDocument retains source spans; converting to DocumentMut clears
    // them and would make redaction silently return the original values.
    for (key, child) in document.iter() {
        if veles_core::config_tool::names_a_secret(key) {
            if let Some(span) = child.span() {
                spans.push((span, !matches!(child, toml_edit::Item::Value(_))));
            }
        } else {
            collect(child, &mut spans);
        }
    }
    spans.sort_by_key(|(span, _)| span.start);
    let mut out = String::new();
    let mut cursor = 0;
    for (span, table) in spans {
        if span.start < cursor
            || span.end > text.len()
            || span.end < span.start
            || !text.is_char_boundary(span.start)
            || !text.is_char_boundary(span.end)
        {
            return "<complex TOML withheld from installer preview>\n".into();
        }
        out.push_str(&text[cursor..span.start]);
        out.push_str(if table {
            "# table withheld: credential-shaped key\n"
        } else {
            "\"<redacted>\""
        });
        cursor = span.end;
    }
    out.push_str(&text[cursor..]);
    out
}

pub fn apply_config(path: &Path, plan: &ConfigPlan) -> Result<()> {
    anyhow::ensure!(
        path == plan.config_path,
        "configuration plan is for a different path"
    );
    let existing = if path.exists() {
        Some(std::fs::read_to_string(path)?)
    } else {
        None
    };
    anyhow::ensure!(
        config_signature(existing.as_deref()) == plan.base_sig,
        "config.toml changed after preview; review the new plan before applying"
    );
    if existing.as_deref() == Some(plan.config_toml.as_str()) {
        return Ok(());
    }
    // Reuse the existing backup/private-file writer. No hand-rolled
    // config writer; plan_config already validated the exact bytes.
    veles_core::configedit::write_with_backup(
        path,
        existing.as_deref().unwrap_or(""),
        &plan.config_toml,
    )?;
    Ok(())
}

/// Discover every module/process component at explicit roots. Discovery
/// failures for one malformed plugin do not hide the others, matching the
/// gateway; describe failures remain named errors for the selected plan.
pub fn discover_components(
    module_roots: &[PathBuf],
    process_roots: &[PathBuf],
) -> Vec<ComponentSpec> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for p in veles_core::modulehost::discover(module_roots) {
        if !seen.insert(p.name.clone()) {
            continue;
        }
        // Discovery must not dlopen untrusted code merely to list choices.
        // `load_component` performs describe only when selected by the user.
        out.push(ComponentSpec {
            name: p.name,
            kind: ComponentKind::Module,
            manifest: Manifest::default(),
            source: Some(p.so),
            describe_error: None,
        });
    }
    for p in veles_core::pluginhost::discover(process_roots) {
        if !seen.insert(p.name.clone()) {
            continue;
        }
        out.push(ComponentSpec {
            name: p.name,
            kind: ComponentKind::ProcessPlugin,
            manifest: Manifest::default(),
            source: Some(p.exe),
            describe_error: None,
        });
    }
    out
}

/// Resolve the declared manifest only for a selected component. Process
/// descriptions use the existing bounded `--describe`; module descriptions
/// retain the existing ABI/build-key checks and are never a generic dlopen.
pub fn load_component(spec: &mut ComponentSpec) -> Result<()> {
    let source = spec
        .source
        .as_ref()
        .context("component has no executable to describe")?;
    spec.manifest = match spec.kind {
        ComponentKind::Module => {
            veles_core::modulehost::describe(source).map_err(|e| anyhow::anyhow!("{e}"))?
        }
        ComponentKind::ProcessPlugin => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(veles_core::pluginhost::describe(source))
                .map_err(|e| anyhow::anyhow!("{e}"))?
        }
    };
    anyhow::ensure!(
        spec.manifest.protocol == PROTOCOL,
        "unsupported plugin protocol for {}",
        spec.name
    );
    anyhow::ensure!(
        spec.manifest.name == spec.name,
        "manifest name does not match component {}",
        spec.name
    );
    Ok(())
}

#[cfg(test)]
mod redaction_tests {
    #[test]
    fn secrets_are_withheld_in_multiline_arrays_and_inline_tables() {
        let input = "# keep\n[one]\ntoken = \"one-secret\"\npassword = [\"two-secret\", \"still-secret\"]\nprivate_key = '''\nthree-secret\n'''\nserver = \"visible\"\n";
        let out = super::redact_config(input);
        for value in ["one-secret", "two-secret", "still-secret", "three-secret"] {
            assert!(!out.contains(value), "{out}");
        }
        assert!(out.contains("server = \"visible\""), "{out}");
        assert!(out.contains("# keep"), "{out}");
        let changed = input.replace("visible", "changed");
        assert!(super::redact_config(&changed).contains("server = \"changed\""));
    }
}
