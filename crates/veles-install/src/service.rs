//! Shared installer service. CLI and desktop adapt requests to this layer;
//! neither reimplements configuration, artifact ownership or rollback rules.
use crate::*;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub install_root: PathBuf,
    pub module_roots: Vec<PathBuf>,
    pub process_roots: Vec<PathBuf>,
}
impl Paths {
    pub fn for_roots(config_dir: PathBuf, data_dir: PathBuf) -> Self {
        let install_root = data_dir.join("install");
        Self {
            config_dir,
            module_roots: vec![data_dir.join("modules")],
            process_roots: vec![data_dir.join("process-plugins")],
            data_dir,
            install_root,
        }
    }
    pub fn current() -> Self {
        let mut p = Self::for_roots(
            veles_core::config::config_dir(),
            veles_core::config::data_dir(),
        );
        // Same precedence as the gateway: VELES_MODULE_PATH /
        // VELES_PLUGIN_PATH, then data roots, then the executable's
        // prefix. Deduplicate WITHOUT sorting away that precedence.
        let module_default = veles_core::modulehost::default_roots();
        let process_default = veles_core::pluginhost::default_roots();
        let take =
            |roots: &[PathBuf], count: usize| roots.iter().take(count).cloned().collect::<Vec<_>>();
        let module_env = take(
            &module_default,
            veles_core::platform::split_search_path("VELES_MODULE_PATH").len(),
        );
        let process_env = take(
            &process_default,
            veles_core::platform::split_search_path("VELES_PLUGIN_PATH").len(),
        );
        let managed_modules = p.module_roots.clone();
        let managed_process = p.process_roots.clone();
        p.module_roots = module_env;
        p.process_roots = process_env;
        p.module_roots.extend(managed_modules);
        p.process_roots.extend(managed_process);
        p.module_roots.extend(module_default);
        p.process_roots.extend(process_default);
        dedup_ordered(&mut p.module_roots);
        dedup_ordered(&mut p.process_roots);
        p
    }
    pub(crate) fn record_path(&self) -> PathBuf {
        self.config_dir.join("install.toml")
    }
    pub(crate) fn config_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub(crate) fn snapshots(&self) -> PathBuf {
        self.install_root.join("config-snapshots")
    }
}

/// Internal plan. It deliberately does NOT serialize the whole config to
/// the desktop: existing config may contain credentials. `review()` emits
/// only requested non-secret changes, and apply recomputes from answers.
#[derive(Debug, Clone)]
pub struct ServicePlan {
    pub action: String,
    pub component: Option<ComponentSpec>,
    pub instance: String,
    pub values: BTreeMap<String, Value>,
    pub config: Option<ConfigPlan>,
    pub plan_sig: String,
    pub actions: Vec<String>,
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InstallService {
    pub paths: Paths,
    pub(crate) profile_recovery: Option<std::sync::Arc<std::sync::Mutex<Recovery>>>,
    pub(crate) profile_locked: bool,
}

fn record_at(path: &Path) -> Result<InstallRecord> {
    if path.exists() {
        InstallRecord::load(path)
    } else {
        Ok(InstallRecord::empty())
    }
}

fn signature<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(crate::sha256(&serde_json::to_vec(value)?))
}

fn dedup_ordered(paths: &mut Vec<PathBuf>) {
    let mut seen = std::collections::BTreeSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

fn manifest_without_secrets(
    manifest: &veles_types::plugin::Manifest,
) -> veles_types::plugin::Manifest {
    let mut m = manifest.clone();
    for k in &mut m.knobs {
        if veles_core::config_tool::names_a_secret(&k.path) {
            k.default = None;
            k.note = Some("secret: use the setup/vault flow".into());
        }
    }
    m
}

impl ServicePlan {
    pub fn review(&self) -> Value {
        let mut changes = Vec::new();
        for (path, value) in &self.values {
            // The planner refuses these before this function is reached;
            // retain a second boundary here because this is display code.
            if veles_core::config_tool::names_a_secret(path) {
                continue;
            }
            changes.push(format!(
                "{}{} = {}",
                if self.instance.is_empty() {
                    String::new()
                } else {
                    format!("[{}] ", self.instance)
                },
                path,
                value
            ));
        }
        let component = self.component.as_ref().map(|c| {
            json!({
                "name": c.name, "kind": c.kind,
                "manifest": manifest_without_secrets(&c.manifest),
            })
        });
        json!({
            "action": self.action, "component": component, "instance": self.instance,
            "base_sig": self.config.as_ref().map(|c| c.base_sig.clone()),
            "plan_sig": self.plan_sig,
            "config_path": self.config.as_ref().map(|c| c.config_path.clone()),
            "changes": changes, "actions": self.actions, "unsupported": self.unsupported,
            "config_text": Value::Null,
            "note": "only requested non-secret changes are shown; existing config text is not sent",
        })
    }
}

impl InstallService {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            profile_recovery: None,
            profile_locked: false,
        }
    }

    pub(crate) fn with_profile_transaction(
        mut self,
        recovery: std::sync::Arc<std::sync::Mutex<Recovery>>,
    ) -> Self {
        self.profile_recovery = Some(recovery);
        self.profile_locked = true;
        self
    }

    pub(crate) fn track_profile_path(&self, path: &Path) -> Result<()> {
        if let Some(recovery) = &self.profile_recovery {
            recovery
                .lock()
                .map_err(|_| anyhow::anyhow!("profile recovery lock is poisoned"))?
                .track(path)?;
        }
        Ok(())
    }

    pub fn note_profile_recovery(&self, path: &Path) -> Result<()> {
        if let Some(recovery) = &self.profile_recovery {
            recovery
                .lock()
                .map_err(|_| anyhow::anyhow!("profile recovery lock is poisoned"))?
                .mark_after(path)?;
        }
        Ok(())
    }
    pub fn record(&self) -> Result<InstallRecord> {
        let mut record = record_at(&self.paths.record_path())?;
        record.recovery = self.profile_recovery.clone();
        Ok(record)
    }

    pub fn state(&self) -> Result<Value> {
        let record = self.record()?;
        let installed: Vec<_> = record
            .components
            .iter()
            .filter(|c| c.source.is_some())
            .cloned()
            .collect();
        let configured: Vec<_> = record
            .components
            .iter()
            .filter(|c| c.source.is_none())
            .cloned()
            .collect();
        let mut capabilities = vec![
            "reviewed-local-bundle",
            "component-config",
            "component-install",
            "verify",
            "rollback",
            "named-profiles",
            "cli-reviewed-system-handoff",
            "cli-reviewed-container-host",
        ];
        let mut unsupported = vec![
            "http-privileged-execution",
            "nixos-apply",
            "service-activation",
            "container-application-components",
        ];
        if cfg!(target_os = "linux") {
            capabilities.push("signed-download-bootstrap");
        } else {
            unsupported.push("signed-download-bootstrap");
        }
        Ok(json!({
            "schema": record.schema, "profile": record.profile,
            "install_root": self.paths.install_root,
            "config_path": self.paths.config_path(),
            "record_path": self.paths.record_path(),
            "installed": installed,
            "configured_only": configured,
            "available": self.components(false)?,
            "history": record.history,
            "capabilities": capabilities,
            "unsupported": unsupported,
        }))
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }
    pub fn config_path(&self) -> PathBuf {
        self.paths.config_path()
    }
    pub fn record_path(&self) -> PathBuf {
        self.paths.record_path()
    }
    pub(crate) fn lock(&self) -> Result<OperationLock> {
        OperationLock::acquire(&self.paths.install_root)
    }

    pub(crate) fn record_config_applied(
        &self,
        spec: &ComponentSpec,
        instance: &str,
        values: &BTreeMap<String, Value>,
        before: &str,
        after: &str,
        config_was_present: bool,
    ) -> Result<Option<String>> {
        let mut record = self.record()?;
        let old = record
            .components
            .iter()
            .find(|c| c.name == spec.name)
            .cloned();
        let old_options = old.as_ref().map(|c| c.options.clone()).unwrap_or_default();
        let snapshot = if config_was_present {
            self.snapshot_config(before)?
        } else {
            None
        };
        record.history.push(HistoryEntry {
            operation: "configure".into(),
            component: spec.name.clone(),
            from_version: old.as_ref().map(|c| c.version.clone()),
            to_version: spec.manifest.version.clone(),
            config_before_sha256: Some(crate::sha256(before.as_bytes())),
            config_after_sha256: Some(crate::sha256(after.as_bytes())),
            config_snapshot: snapshot.clone(),
            options_before: old_options,
            config_file_existed: Some(config_was_present),
        });
        let mut options = old.as_ref().map(|c| c.options.clone()).unwrap_or_default();
        options.extend(values.iter().map(|(key, value)| {
            let path = if instance.is_empty() {
                key.clone()
            } else {
                format!("{}.{}", key, instance)
            };
            (path, value.clone())
        }));
        let managed = old.filter(|old| {
            old.kind == Some(spec.kind)
                && old.source.as_deref().is_some_and(|source| {
                    let path = Path::new(source);
                    path.is_file()
                        && path.starts_with(
                            self.paths.install_root.join("components").join(&spec.name),
                        )
                })
        });
        let (artifacts, source, version, previous) = match &managed {
            Some(old) => (
                old.artifacts.clone(),
                old.source.clone(),
                old.version.clone(),
                old.previous.clone(),
            ),
            None => (BTreeMap::new(), None, spec.manifest.version.clone(), None),
        };
        record.components.retain(|c| c.name != spec.name);
        record.components.push(Component {
            name: spec.name.clone(),
            kind: Some(spec.kind),
            version,
            options,
            artifacts,
            source,
            previous,
        });
        record.save(&self.paths.record_path())?;
        Ok(snapshot)
    }

    pub fn components(&self, describe: bool) -> Result<Vec<ComponentSpec>> {
        let mut out = Vec::new();
        for mut spec in
            crate::discover_components(&self.paths.module_roots, &self.paths.process_roots)
        {
            if describe {
                if let Err(e) = crate::load_component(&mut spec) {
                    spec.describe_error = Some(format!("{e:#}"));
                }
            }
            out.push(spec);
        }
        Ok(out)
    }

    pub fn component(&self, name: &str) -> Result<ComponentSpec> {
        valid_component_name(name)?;
        let mut spec = self
            .components(false)?
            .into_iter()
            .find(|s| s.name == name)
            .with_context(|| format!("component {name} was not discovered"))?;
        crate::load_component(&mut spec)?;
        Ok(spec)
    }

    pub fn preview_config(
        &self,
        name: &str,
        instance: &str,
        values: &BTreeMap<String, Value>,
    ) -> Result<ServicePlan> {
        for key in values.keys() {
            if veles_core::config_tool::names_a_secret(key) {
                bail!("{key} is a secret; use the setup/vault flow, not component options")
            }
        }
        let spec = self.component(name)?;
        let config =
            crate::plan_config_for_instance(&self.paths.config_path(), &spec, values, instance)?;
        let plan_sig = signature(&(
            name,
            instance,
            values,
            &spec.manifest,
            &config.base_sig,
            &config.config_toml,
        ))?;
        Ok(ServicePlan {
            action: "configure".into(), component: Some(spec), instance: instance.into(), values: values.clone(),
            plan_sig, config: Some(config),
            actions: vec!["write the reviewed component options through configedit; preserve unrelated config and back up config.toml".into()],
            unsupported: vec!["gateway restart/reload is requested separately".into()],
        })
    }

    /// Apply an internal plan after an interactive CLI confirmation.
    pub fn apply_config(&self, plan: &ServicePlan) -> Result<Value> {
        let component = plan
            .component
            .as_ref()
            .context("configuration plan lacks a component")?;
        self.apply_config_answers(
            &component.name,
            &plan.instance,
            &plan.values,
            &plan.config.as_ref().map(|c| c.base_sig.clone()),
            &plan.plan_sig,
        )
    }

    /// Desktop-safe apply: the client returns answers and two opaque
    /// signatures, never a whole config or an arbitrary output path.
    pub fn apply_config_answers(
        &self,
        name: &str,
        instance: &str,
        values: &BTreeMap<String, Value>,
        base_sig: &Option<String>,
        plan_sig: &str,
    ) -> Result<Value> {
        let _lock = self.lock()?;
        let mut recovery = Recovery::capture([self.paths.config_path(), self.paths.record_path()])?;
        let result = self.apply_config_answers_inner(
            name,
            instance,
            values,
            base_sig,
            plan_sig,
            &mut recovery,
        );
        recovery.finish(result)
    }

    fn apply_config_answers_inner(
        &self,
        name: &str,
        instance: &str,
        values: &BTreeMap<String, Value>,
        base_sig: &Option<String>,
        plan_sig: &str,
        recovery: &mut Recovery,
    ) -> Result<Value> {
        let fresh = self.preview_config(name, instance, values)?;
        let config = fresh.config.as_ref().unwrap();
        anyhow::ensure!(
            Some(&config.base_sig) == base_sig.as_ref(),
            "config.toml changed after preview; review a fresh plan"
        );
        anyhow::ensure!(
            fresh.plan_sig == plan_sig,
            "component answers changed after preview; review a fresh plan"
        );
        let config_was_present = self.paths.config_path().is_file();
        let before = if config_was_present {
            std::fs::read_to_string(self.paths.config_path())
                .with_context(|| format!("reading {}", self.paths.config_path().display()))?
        } else {
            String::new()
        };
        let spec = fresh.component.as_ref().unwrap();
        crate::apply_config(&self.paths.config_path(), config)?;
        recovery.mark_after(&self.paths.config_path())?;
        let snapshot = self.record_config_applied(
            spec,
            instance,
            values,
            &before,
            &config.config_toml,
            config_was_present,
        )?;
        recovery.mark_after(&self.paths.record_path())?;
        Ok(json!({
            "ok": true, "action": "configure", "component": name,
            "config_path": self.paths.config_path(),
            "config_backup": self.paths.config_path().with_extension("toml.bak"),
            "config_snapshot": snapshot, "record_path": self.paths.record_path(),
            "reload_required": true,
        }))
    }

    fn snapshot_config(&self, text: &str) -> Result<Option<String>> {
        // Empty but existing is a real pre-operation state. Its private
        // snapshot distinguishes it from a config that did not exist.
        let name = format!("{}.toml", crate::sha256(text.as_bytes()));
        let dir = self.paths.snapshots();
        std::fs::create_dir_all(&dir)?;
        veles_core::platform::private_dir(&dir)?;
        veles_core::platform::write_private(&dir.join(&name), text.as_bytes())?;
        Ok(Some(name))
    }

    pub fn preview_bundle(&self, source: &Path) -> Result<BundlePlan> {
        crate::review_bundle(source, &self.paths.install_root)
    }

    /// Signed bootstrap is deliberately a different operation from a
    /// reviewed local directory. Verification is computed in this crate;
    /// the caller cannot substitute ArtifactTrust::SignatureVerified.
    pub fn preview_signed_bundle(
        &self,
        request: &crate::ReleaseRequest,
    ) -> Result<crate::SignedPlan> {
        request.validate()?;
        let _lock = self.lock()?;
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        let stage = self
            .paths
            .data_dir
            .join("install-sources/.signed-download-staging")
            .join(nonce);
        crate::bootstrap::preview(request, &self.paths.install_root, &stage)
    }

    /// Apply only the plan just shown to the operator. The lock is not
    /// held while a human reads the preview or answers a prompt.
    pub fn apply_signed_bundle(&self, plan: crate::SignedPlan) -> Result<Value> {
        let _lock = self.lock()?;
        let mut recovery = Recovery::capture([
            self.paths.install_root.join("current"),
            self.paths.record_path(),
        ])?;
        let result = (|| {
            anyhow::ensure!(
                plan.destination_root == self.paths.install_root,
                "installation directory changed after signed preview; discard this plan and review a fresh one"
            );
            let mut record = self.record()?;
            let (component, verification) = plan.install(&self.paths.install_root, &mut record)?;
            recovery.mark_after(&self.paths.install_root.join("current"))?;
            record.save(&self.paths.record_path())?;
            recovery.mark_after(&self.paths.record_path())?;
            Ok(json!({
                "ok": true, "action": "install-signed-bundle", "component": component,
                "download": verification, "record_path": self.paths.record_path(),
                "current": self.paths.install_root.join("current"), "restart_required": true,
            }))
        })();
        recovery.finish(result)
    }

    pub fn install_local_bundle(&self, source: &Path, plan_sig: &str) -> Result<Value> {
        let _lock = (!self.profile_locked).then(|| self.lock()).transpose()?;
        let review = self.preview_bundle(source)?;
        let payload = self
            .paths
            .install_root
            .join("releases")
            .join(format!("{}-{}", review.info.version, review.info.revision));
        self.track_profile_path(&payload)?;
        anyhow::ensure!(
            review.plan_sig == plan_sig,
            "bundle changed after preview; review the new bundle before applying"
        );
        let mut recovery = Recovery::capture([
            self.paths.install_root.join("current"),
            self.paths.record_path(),
        ])?;
        let result = (|| {
            let mut record = self.record()?;
            let component = crate::install_reviewed_bundle(
                &self.paths.install_root,
                source,
                plan_sig,
                &mut record,
            )?;
            recovery.mark_after(&self.paths.install_root.join("current"))?;
            self.note_profile_recovery(&self.paths.install_root.join("current"))?;
            self.note_profile_recovery(&payload)?;
            record.save(&self.paths.record_path())?;
            recovery.mark_after(&self.paths.record_path())?;
            self.note_profile_recovery(&self.paths.record_path())?;
            Ok(
                json!({"ok": true, "action": "install-bundle", "component": component, "record_path": self.paths.record_path(), "current": self.paths.install_root.join("current"), "restart_required": true, "verification": "operator-reviewed local directory; not signed-download evidence"}),
            )
        })();
        recovery.finish(result)
    }

    /// Bind artifact installation to the bytes and manifest the operator
    /// reviewed. Re-describing at apply cannot bless changed payload.
    pub fn preview_component(&self, name: &str) -> Result<Value> {
        let spec = self.component(name)?;
        let source = spec.source.as_ref().context("component has no artifact")?;
        let bytes = std::fs::read(source)
            .with_context(|| format!("reading reviewed component {}", source.display()))?;
        Ok(json!({
            "preview": true,
            "confirm_required": true,
            "component": spec,
            "artifact_sha256": crate::sha256(&bytes),
            "plan_sig": signature(&(spec, crate::sha256(&bytes)))?,
        }))
    }

    pub fn install_component(&self, name: &str, plan_sig: Option<&str>) -> Result<Value> {
        let _lock = (!self.profile_locked).then(|| self.lock()).transpose()?;
        let expected = plan_sig.context("component installation requires a reviewed plan_sig")?;
        let review = self.preview_component(name)?;
        anyhow::ensure!(
            review["plan_sig"].as_str() == Some(expected),
            "component changed after preview; review a fresh artifact plan"
        );
        let spec: ComponentSpec = serde_json::from_value(review["component"].clone())?;
        let selection = match spec.kind {
            ComponentKind::Module => self
                .paths
                .data_dir
                .join("modules")
                .join(name)
                .join("current"),
            ComponentKind::ProcessPlugin => self
                .paths
                .data_dir
                .join("process-plugins")
                .join(name)
                .join("plugin.toml"),
        };
        let manifest = match spec.kind {
            ComponentKind::Module => self
                .paths
                .data_dir
                .join("modules")
                .join(name)
                .join("plugin.toml"),
            ComponentKind::ProcessPlugin => selection.clone(),
        };
        let mut recovery =
            Recovery::capture([selection.clone(), manifest, self.paths.record_path()])?;
        // Use the spec captured with the reviewed plan_sig. Re-discovering
        // after current changes can resolve the old installed module.
        let result = self.install_component_inner(&spec, &selection, &mut recovery);
        recovery.finish(result)
    }

    pub(crate) fn install_reviewed_component(
        &self,
        spec: &ComponentSpec,
        expected: &str,
    ) -> Result<Value> {
        let _lock = (!self.profile_locked).then(|| self.lock()).transpose()?;
        let source = spec
            .source
            .as_ref()
            .context("reviewed component has no artifact")?;
        let bytes = std::fs::read(source)
            .with_context(|| format!("reading reviewed component {}", source.display()))?;
        let review_sig = signature(&(spec, crate::sha256(&bytes)))?;
        anyhow::ensure!(
            review_sig == expected,
            "component changed after preview; review a fresh artifact plan"
        );
        let selection = match spec.kind {
            ComponentKind::Module => self
                .paths
                .data_dir
                .join("modules")
                .join(&spec.name)
                .join("current"),
            ComponentKind::ProcessPlugin => self
                .paths
                .data_dir
                .join("process-plugins")
                .join(&spec.name)
                .join("plugin.toml"),
        };
        let manifest = match spec.kind {
            ComponentKind::Module => self
                .paths
                .data_dir
                .join("modules")
                .join(&spec.name)
                .join("plugin.toml"),
            ComponentKind::ProcessPlugin => selection.clone(),
        };
        let mut recovery =
            Recovery::capture([selection.clone(), manifest, self.paths.record_path()])?;
        let result = self.install_component_inner(spec, &selection, &mut recovery);
        recovery.finish(result)
    }

    fn install_component_inner(
        &self,
        spec: &ComponentSpec,
        selection: &Path,
        recovery: &mut Recovery,
    ) -> Result<Value> {
        let name = spec.name.as_str();
        let source = spec.source.as_ref().context("component has no artifact")?;
        let bytes = std::fs::read(source)?;
        let hash = crate::sha256(&bytes);
        valid_version(&spec.manifest.version)?;
        let version = format!("{}-{}", spec.manifest.version, &hash[..12]);
        valid_version(&version)?;
        self.track_profile_path(
            &self
                .paths
                .install_root
                .join("components")
                .join(name)
                .join(&version),
        )?;
        let component_root = self
            .paths
            .install_root
            .join("components")
            .join(name)
            .join(&version);
        std::fs::create_dir_all(&component_root)?;
        let filename = source.file_name().unwrap_or_default();
        let artifact = component_root.join("payload").join(filename);
        let write = Action::WriteFile {
            path: PathBuf::from("payload").join(filename),
            contents: bytes,
            mode: Some(0o755),
            expected_sha256: Some(hash.clone()),
        };
        write.apply(&component_root)?;
        let mut record = self.record()?;
        let old = record.components.iter().find(|c| c.name == name).cloned();
        if let Some(previous) = old.as_ref() {
            let problems = self.selected_problems(previous);
            anyhow::ensure!(
                problems.is_empty(),
                "current component selection was changed outside the installer: {}",
                problems.join("; ")
            );
        }
        self.select_component(spec, &artifact, old.as_ref())?;
        self.note_profile_recovery(&component_root)?;
        recovery.mark_after(selection)?;
        self.note_profile_recovery(selection)?;
        if spec.kind == ComponentKind::Module {
            recovery.mark_after(
                &self
                    .paths
                    .data_dir
                    .join("modules")
                    .join(name)
                    .join("plugin.toml"),
            )?;
            self.note_profile_recovery(
                &self
                    .paths
                    .data_dir
                    .join("modules")
                    .join(name)
                    .join("plugin.toml"),
            )?;
        }
        let mut options = old.as_ref().map(|c| c.options.clone()).unwrap_or_default();
        for k in options.keys() {
            anyhow::ensure!(
                !veles_core::config_tool::names_a_secret(k),
                "stored option {k} is a secret"
            );
        }
        let component = Component {
            name: name.into(),
            kind: Some(spec.kind),
            version: spec.manifest.version.clone(),
            options: std::mem::take(&mut options),
            artifacts: BTreeMap::from([("executable".into(), hash)]),
            source: Some(artifact.display().to_string()),
            previous: old
                .filter(|o| o.source.as_deref() != Some(artifact.to_string_lossy().as_ref()))
                .map(Box::new),
        };
        record.components.retain(|c| c.name != name);
        record.components.push(component.clone());
        record.history.push(HistoryEntry {
            operation: "install".into(),
            component: name.into(),
            from_version: old_version(&component),
            to_version: component.version.clone(),
            config_before_sha256: None,
            config_after_sha256: None,
            config_snapshot: None,
            options_before: Default::default(),
            config_file_existed: None,
        });
        record.save(&self.paths.record_path())?;
        recovery.mark_after(&self.paths.record_path())?;
        self.note_profile_recovery(&self.paths.record_path())?;
        Ok(
            json!({"ok": true, "action": "install-component", "component": component, "reload_required": true}),
        )
    }

    fn select_component(
        &self,
        spec: &ComponentSpec,
        artifact: &Path,
        old: Option<&Component>,
    ) -> Result<()> {
        match spec.kind {
            ComponentKind::Module => {
                let dir = self.paths.data_dir.join("modules").join(&spec.name);
                let selected = veles_core::modulehost::install_file(&dir, artifact)?;
                let fallback = selected
                    .file_name()
                    .context("selected module has no fallback filename")?
                    .to_string_lossy()
                    .into_owned();
                self.write_plugin_manifest(
                    &dir,
                    &spec.name,
                    "module",
                    &fallback,
                    &spec.manifest.description,
                    old,
                )?;
            }
            ComponentKind::ProcessPlugin => {
                let dir = self.paths.data_dir.join("process-plugins").join(&spec.name);
                self.write_plugin_manifest(
                    &dir,
                    &spec.name,
                    "process",
                    &artifact.display().to_string(),
                    &spec.manifest.description,
                    old,
                )?;
            }
        }
        Ok(())
    }

    fn write_plugin_manifest(
        &self,
        dir: &Path,
        name: &str,
        kind: &str,
        entry: &str,
        description: &str,
        old: Option<&Component>,
    ) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("plugin.toml");
        let mut doc = if path.exists() {
            std::fs::read_to_string(&path)?.parse::<toml_edit::DocumentMut>()?
        } else {
            toml_edit::DocumentMut::new()
        };
        let existing = if path.exists() {
            Some(crate::sha256(&std::fs::read(&path)?))
        } else {
            None
        };
        if let (Some(old), Some(_current)) = (old, &existing) {
            if old.source.is_some() {
                let old_entry = doc
                    .get("entry")
                    .and_then(toml_edit::Item::as_str)
                    .unwrap_or("");
                let old_source = old.source.as_deref().unwrap_or("");
                // Validate the manifest's old fallback before replacing it.
                // `current` already names the new selection at this point.
                let fallback_matches = Path::new(old_entry).components().count() == 1
                    && old.artifacts.get("executable").is_some_and(|expected| {
                        std::fs::read(dir.join(old_entry))
                            .ok()
                            .map(|bytes| crate::sha256(&bytes) == *expected)
                            .unwrap_or(false)
                    });
                let managed = kind == "module"
                    && Path::new(old_source)
                        .starts_with(self.paths.install_root.join("components").join(name))
                    && fallback_matches;
                anyhow::ensure!(
                    old_entry == old_source || managed,
                    "{} plugin.toml was changed outside the installer (entry={old_entry:?}, retained={old_source:?}, expected_artifact={:?}, managed_root={})",
                    path.display(),
                    old.artifacts.get("executable"),
                    self.paths.install_root.join("components").join(name).display()
                );
            }
        }
        doc.insert("name", toml_edit::value(name));
        doc.insert("kind", toml_edit::value(kind));
        doc.insert("entry", toml_edit::value(entry));
        doc.insert("description", toml_edit::value(description));
        let root = dir.parent().unwrap().parent().unwrap().to_path_buf();
        let relative = path.strip_prefix(&root)?.to_path_buf();
        let action = Action::WriteFile {
            path: relative,
            contents: doc.to_string().into_bytes(),
            mode: Some(0o600),
            expected_sha256: existing,
        };
        action.apply(&root)?;
        Ok(())
    }

    pub fn preview_rollback(&self, name: &str) -> Result<Value> {
        valid_component_name(name)?;
        let record = self.record()?;
        let current = record
            .components
            .iter()
            .find(|c| c.name == name)
            .context("component is not installed")?;
        let restore = record
            .history
            .iter()
            .rev()
            .find(|h| h.component == name && h.operation == "configure")
            .map(|h| {
                let snapshot = match &h.config_snapshot {
                    Some(file) => {
                        crate::valid_version(file)?;
                        Some(crate::sha256(&std::fs::read(
                            self.paths.snapshots().join(file),
                        )?))
                    }
                    None if h.config_file_existed == Some(false) => Some(crate::sha256(b"")),
                    None => anyhow::bail!(
                        "legacy configure history cannot prove whether config.toml existed"
                    ),
                };
                Ok((
                    h.config_after_sha256.clone(),
                    snapshot,
                    h.config_file_existed,
                    h.options_before.clone(),
                ))
            })
            .transpose()?;
        if current.previous.is_none() && restore.is_none() {
            bail!("component {name} has no retained previous artifact or configuration");
        }
        let before = self
            .paths
            .config_path()
            .is_file()
            .then(|| std::fs::read(self.paths.config_path()))
            .transpose()?;
        let sig = signature(&(name, &current, &restore, before.as_deref()))?;
        Ok(json!({
            "preview": true, "confirm_required": true, "component": name,
            "from_version": current.version, "to_version": current.previous.as_ref().map(|c| &c.version),
            "restores_config_snapshot": restore.is_some(), "plan_sig": sig,
            "note": "restores a verified retained payload and any applicable private config snapshot; never a running-service claim",
        }))
    }

    pub fn rollback(&self, name: &str, plan_sig: &str) -> Result<Value> {
        let _lock = self.lock()?;
        let review = self.preview_rollback(name)?;
        anyhow::ensure!(
            review["plan_sig"].as_str() == Some(plan_sig),
            "rollback state changed after preview; review a fresh plan"
        );
        let mut recovery = Recovery::capture([
            self.paths.config_path(),
            self.paths.record_path(),
            self.paths
                .data_dir
                .join("process-plugins")
                .join(name)
                .join("plugin.toml"),
            self.paths
                .data_dir
                .join("modules")
                .join(name)
                .join("current"),
            self.paths
                .data_dir
                .join("modules")
                .join(name)
                .join("plugin.toml"),
            self.paths.install_root.join("current"),
        ])?;
        let result = self.rollback_inner(name, &mut recovery);
        recovery.finish(result)
    }

    fn rollback_inner(&self, name: &str, recovery: &mut Recovery) -> Result<Value> {
        let mut record = self.record()?;
        let current = record
            .components
            .iter()
            .find(|c| c.name == name)
            .cloned()
            .with_context(|| format!("component {name} is not installed"))?;
        if current.kind.is_some() && current.source.is_some() {
            let problems = self.selected_problems(&current);
            anyhow::ensure!(
                problems.is_empty(),
                "current component selection was changed outside the installer: {}",
                problems.join("; ")
            );
        }
        let config_was_present = self.paths.config_path().is_file();
        let before = if config_was_present {
            std::fs::read_to_string(self.paths.config_path())
                .with_context(|| format!("reading {}", self.paths.config_path().display()))?
        } else {
            String::new()
        };
        let latest_config = record
            .history
            .iter()
            .rev()
            .find(|h| h.component == name && h.operation == "configure")
            .cloned();
        let restore = if let Some(h) = &latest_config {
            if let Some(after) = &h.config_after_sha256 {
                anyhow::ensure!(
                    config_was_present && crate::sha256(before.as_bytes()) == *after,
                    "config.toml changed after the last component configuration; refusing to overwrite it during rollback"
                );
            }
            match &h.config_snapshot {
                Some(file) => Some(std::fs::read_to_string(self.paths.snapshots().join(file))?),
                None if h.config_file_existed == Some(false) => Some(String::new()),
                None => anyhow::bail!("legacy config history cannot prove whether config.toml existed; refusing to create an empty file during rollback"),
            }
        } else {
            None
        };
        if let Some(text) = &restore {
            toml::from_str::<veles_core::config::VelesConfig>(text)
                .context("retained config snapshot is unreadable")?;
        }
        let snapshot = if config_was_present {
            self.snapshot_config(&before)?
        } else {
            None
        };
        let restored = match current.kind {
            None if current.name == "core" => {
                crate::rollback(&self.paths.install_root, &mut record, name)?
            }
            Some(kind) if current.previous.is_some() && latest_config.is_none() => {
                let previous = current.previous.clone().unwrap();
                let problems = crate::verify_component(&self.paths.install_root, &previous)?;
                anyhow::ensure!(
                    problems.is_empty(),
                    "previous plugin payload failed verification: {}",
                    problems.join("; ")
                );
                let source = previous
                    .source
                    .as_ref()
                    .map(PathBuf::from)
                    .context("previous plugin has no retained artifact")?;
                // The record proves the old artifact's hash, not its
                // display metadata. Describe that verified payload before
                // changing the live selection, so rollback restores both
                // the old executable and its declared description.
                let mut retained = ComponentSpec {
                    name: name.into(),
                    kind,
                    manifest: Default::default(),
                    source: Some(source.clone()),
                    describe_error: None,
                };
                crate::load_component(&mut retained)
                    .context("describing verified retained component")?;
                let description = retained.manifest.description;
                match kind {
                    ComponentKind::Module => {
                        let dir = self.paths.data_dir.join("modules").join(name);
                        let selected = veles_core::modulehost::install_file(&dir, &source)?;
                        let fallback = selected
                            .file_name()
                            .context("restored module has no fallback filename")?
                            .to_string_lossy()
                            .into_owned();
                        self.write_plugin_manifest(
                            &dir,
                            name,
                            "module",
                            &fallback,
                            &description,
                            Some(&current),
                        )?;
                    }
                    ComponentKind::ProcessPlugin => {
                        let dir = self.paths.data_dir.join("process-plugins").join(name);
                        let mut doc = std::fs::read_to_string(dir.join("plugin.toml"))?
                            .parse::<toml_edit::DocumentMut>()?;
                        doc.insert("entry", toml_edit::value(source.display().to_string()));
                        doc.insert("description", toml_edit::value(&description));
                        let action = Action::WriteFile {
                            path: PathBuf::from("plugin.toml"),
                            contents: doc.to_string().into_bytes(),
                            mode: Some(0o600),
                            expected_sha256: Some(crate::sha256(&std::fs::read(
                                dir.join("plugin.toml"),
                            )?)),
                        };
                        action.apply(&dir)?;
                    }
                }
                let mut component = (*previous).clone();
                component.previous = Some(Box::new(current.clone()));
                record.components.retain(|c| c.name != name);
                record.components.push(component.clone());
                component
            }
            Some(_) if latest_config.is_some() => {
                // A config-only rollback retains the artifact but must
                // restore the options that accompanied the old config.
                let mut component = current.clone();
                component.options = latest_config
                    .as_ref()
                    .map(|h| h.options_before.clone())
                    .unwrap_or_default();
                record.components.retain(|c| c.name != name);
                record.components.push(component.clone());
                component
            }
            _ => anyhow::bail!("component {name} has no retained previous artifact to roll back"),
        };
        match current.kind {
            Some(ComponentKind::ProcessPlugin) => recovery.mark_after(
                &self
                    .paths
                    .data_dir
                    .join("process-plugins")
                    .join(name)
                    .join("plugin.toml"),
            )?,
            Some(ComponentKind::Module) => {
                recovery.mark_after(
                    &self
                        .paths
                        .data_dir
                        .join("modules")
                        .join(name)
                        .join("current"),
                )?;
                recovery.mark_after(
                    &self
                        .paths
                        .data_dir
                        .join("modules")
                        .join(name)
                        .join("plugin.toml"),
                )?;
            }
            None => recovery.mark_after(&self.paths.install_root.join("current"))?,
        }
        if let Some(text) = restore {
            let latest = latest_config
                .as_ref()
                .expect("restore requires a configure history entry");
            if latest.config_file_existed == Some(false) && text.is_empty() {
                match std::fs::remove_file(self.paths.config_path()) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(e)
                            .context("removing config created by the operation being rolled back")
                    }
                }
            } else {
                crate::apply_config(
                    &self.paths.config_path(),
                    &ConfigPlan {
                        base_sig: veles_core::setup::base_sig(
                            config_was_present.then_some(before.as_str()),
                        ),
                        config_path: self.paths.config_path(),
                        config_toml: text,
                        changes: vec!["rollback snapshot".into()],
                    },
                )?;
            }
            recovery.mark_after(&self.paths.config_path())?;
        }
        record.history.push(HistoryEntry {
            operation: "rollback".into(),
            component: name.into(),
            from_version: Some(current.version),
            to_version: restored.version.clone(),
            config_before_sha256: Some(crate::sha256(before.as_bytes())),
            config_after_sha256: Some(crate::sha256(
                std::fs::read(self.paths.config_path())
                    .unwrap_or_default()
                    .as_slice(),
            )),
            config_snapshot: snapshot,
            options_before: current.options,
            config_file_existed: Some(config_was_present),
        });
        record.save(&self.paths.record_path())?;
        recovery.mark_after(&self.paths.record_path())?;
        Ok(
            json!({"ok": true, "action": "rollback", "component": restored, "reload_required": true}),
        )
    }

    fn selected_problems(&self, component: &Component) -> Vec<String> {
        let Some(kind) = component.kind else {
            return Vec::new();
        };
        let Some(source) = component.source.as_ref() else {
            return vec![format!(
                "{} has no recorded selected artifact",
                component.name
            )];
        };
        let expected = match component.artifacts.get("executable") {
            Some(hash) => hash,
            None => return vec![format!("{} has no executable digest", component.name)],
        };
        let selected = match kind {
            ComponentKind::ProcessPlugin => {
                let path = self
                    .paths
                    .data_dir
                    .join("process-plugins")
                    .join(&component.name)
                    .join("plugin.toml");
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
                    .and_then(|doc| {
                        doc.get("entry")
                            .and_then(toml_edit::Item::as_str)
                            .map(PathBuf::from)
                    })
                    .unwrap_or_else(|| PathBuf::from("<missing plugin.toml entry>"))
            }
            ComponentKind::Module => veles_core::modulehost::current_so(
                &self.paths.data_dir.join("modules").join(&component.name),
            )
            .unwrap_or_else(|| PathBuf::from("<missing module current>")),
        };
        if kind == ComponentKind::ProcessPlugin && selected != Path::new(source) {
            return vec![format!(
                "{} selection {} differs from recorded {}",
                component.name,
                selected.display(),
                source
            )];
        }
        match std::fs::read(&selected) {
            Ok(bytes) if crate::sha256(&bytes) == *expected => Vec::new(),
            Ok(_) => vec![format!(
                "{} selected artifact SHA-256 differs",
                component.name
            )],
            Err(e) => vec![format!("{} selected artifact: {e}", component.name)],
        }
    }

    pub fn verify(&self) -> Result<Value> {
        let record = self.record()?;
        let mut results = Vec::new();
        let mut ok = true;
        for component in &record.components {
            if component.source.is_none() && component.artifacts.is_empty() {
                results.push(json!({"component": component.name, "kind": "configuration-only", "artifact_verified": false, "ok": true, "problems": []}));
                continue;
            }
            let mut problems = crate::verify_component(&self.paths.install_root, component)
                .unwrap_or_else(|e| vec![format!("{e:#}")]);
            if problems.is_empty() {
                problems.extend(self.selected_problems(component));
            }
            ok &= problems.is_empty();
            results.push(json!({"component": component.name, "version": component.version, "ok": problems.is_empty(), "problems": problems}));
        }
        Ok(
            json!({"ok": ok, "components": results, "doctor": "run `veles doctor --plugins --components` for runtime health; artifact integrity alone is not a health claim"}),
        )
    }
}

fn old_version(component: &Component) -> Option<String> {
    component.previous.as_ref().map(|p| p.version.clone())
}
