use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    #[default]
    Workstation,
    Server,
    ContainerHost,
    Nixos,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Component {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::ComponentKind>,
    pub version: String,
    #[serde(default)]
    pub options: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Box<Component>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub operation: String,
    pub component: String,
    pub from_version: Option<String>,
    pub to_version: String,
    pub config_before_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_after_sha256: Option<String>,
    /// Private snapshot under the install data root, never config text in
    /// install.toml itself. The snapshot can contain the same secrets as
    /// config.toml, so it is owner-only and excluded from plan previews.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_snapshot: Option<String>,
    #[serde(default)]
    pub options_before: BTreeMap<String, serde_json::Value>,
    /// Whether the config file existed before this operation. Additive
    /// schema-1 recovery information; old records default to unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_file_existed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallRecord {
    pub schema: u32,
    pub profile: Profile,
    #[serde(default)]
    pub components: Vec<Component>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    #[serde(skip)]
    pub(crate) recovery: Option<std::sync::Arc<std::sync::Mutex<crate::Recovery>>>,
}

impl PartialEq for InstallRecord {
    fn eq(&self, other: &Self) -> bool {
        self.schema == other.schema
            && self.profile == other.profile
            && self.components == other.components
            && self.history == other.history
    }
}

impl InstallRecord {
    /// Empty schema-1 record for a new installation. Runtime recovery
    /// state is deliberately not part of the public record schema.
    pub fn empty() -> Self {
        Self {
            schema: 1,
            profile: Profile::Workstation,
            components: Vec::new(),
            history: Vec::new(),
            recovery: None,
        }
    }

    pub(crate) fn note_payload(&self, path: &Path) -> Result<()> {
        if let Some(recovery) = &self.recovery {
            recovery
                .lock()
                .map_err(|_| anyhow::anyhow!("profile recovery lock poisoned"))?
                .mark_after(path)?;
        }
        Ok(())
    }
}

fn secret_name(path: &str) -> bool {
    let last = path.rsplit('.').next().unwrap_or(path).to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "api_key",
        "private_key",
        "plc_key",
    ]
    .iter()
    .any(|s| last == *s || last.ends_with(&format!("_{s}")))
}

fn validate_component(c: &Component) -> Result<()> {
    crate::valid_component_name(&c.name)?;
    crate::valid_version(&c.version)
        .with_context(|| format!("invalid version for component {}", c.name))?;
    for key in c.options.keys() {
        anyhow::ensure!(
            !secret_name(key),
            "{} is a secret; put its value in the vault, not install.toml",
            key
        );
    }
    for (path, hash) in &c.artifacts {
        anyhow::ensure!(
            crate::valid_hash(hash),
            "invalid SHA-256 for {} artifact {path}",
            c.name
        );
    }
    if let Some(source) = &c.source {
        let source = Path::new(source);
        anyhow::ensure!(
            source.is_absolute()
                && source.components().all(|part| matches!(
                    part,
                    std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                        | std::path::Component::Normal(_)
                )),
            "{} has a non-normalized absolute source path",
            c.name
        );
        // Absolute paths are accepted for existing installations, but
        // parent traversal is not. Root confinement is checked against
        // the ACTUAL Paths at verify/rollback, not this generic marker.
        anyhow::ensure!(
            c.source.is_none() || c.kind.is_some() || c.name == "core",
            "untyped record source must belong to the core bundle"
        );
    }
    if let Some(p) = &c.previous {
        validate_component(p)?;
    }
    Ok(())
}

impl InstallRecord {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.schema == 1,
            "unsupported install.toml schema {}; this installer supports schema 1",
            self.schema
        );
        let mut names = std::collections::BTreeSet::new();
        for c in &self.components {
            validate_component(c)?;
            anyhow::ensure!(names.insert(&c.name), "duplicate component {}", c.name);
        }
        for h in &self.history {
            for key in h.options_before.keys() {
                anyhow::ensure!(!secret_name(key), "history contains a secret option {key}");
            }
            if let Some(h) = &h.config_before_sha256 {
                anyhow::ensure!(crate::valid_hash(h), "invalid history config hash");
            }
            if let Some(h) = &h.config_after_sha256 {
                anyhow::ensure!(crate::valid_hash(h), "invalid history config-after hash");
            }
            if h.config_file_existed == Some(false) && h.config_snapshot.is_some() {
                anyhow::bail!("history says no config existed but records a snapshot");
            }
            if let Some(name) = &h.config_snapshot {
                anyhow::ensure!(
                    name.len() == 69 && name.ends_with(".toml") && crate::valid_hash(&name[..64]),
                    "invalid private config snapshot name"
                );
            }
        }
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let record: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        record.validate()?;
        Ok(record)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        if crate::fail_record_write_for_test() {
            anyhow::bail!("injected install-record write failure");
        }
        self.validate()?;
        let text = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Ok(previous) = std::fs::read_to_string(path) {
            let backup = path.with_extension("toml.bak");
            veles_core::platform::write_private(&backup, previous.as_bytes())?;
        }
        let temp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            veles_core::platform::write_private(&temp, text.as_bytes())?;
            veles_core::platform::replace_file(&temp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}
