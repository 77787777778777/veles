//! Combined first-run/setup and component installation. Provider answers,
//! secrets and `.env` stay in `setup`; component Knobs are merged into the
//! same config plan so one base signature and one write cover both halves.
use crate::{ComponentSpec, InstallService};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentRequest {
    pub name: String,
    #[serde(default)]
    pub instance: String,
    #[serde(default)]
    pub values: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct WorkflowPlan {
    pub setup: veles_core::setup::SetupPlan,
    pub component: Option<(ComponentSpec, String, BTreeMap<String, Value>)>,
    pub config_toml: String,
    pub plan_sig: String,
}

impl InstallService {
    pub fn preview_workflow(
        &self,
        answers: &veles_core::setup::SetupAnswers,
        component: Option<&ComponentRequest>,
    ) -> Result<WorkflowPlan> {
        let existing = veles_core::setup::read_existing(&self.paths.config_path())?;
        let setup = veles_core::setup::setup_plan(answers, existing.as_deref())?;
        let mut config_toml = setup.config_toml.clone();
        let component = match component {
            None => None,
            Some(request) => {
                for key in request.values.keys() {
                    if veles_core::config_tool::names_a_secret(key) {
                        anyhow::bail!(
                            "{key} is a secret; use the setup/vault flow, not component options"
                        );
                    }
                }
                let spec = self.component(&request.name)?;
                let plan = crate::plan_config_text(
                    &config_toml,
                    &self.paths.config_path(),
                    &spec,
                    &request.values,
                    &request.instance,
                )?;
                config_toml = plan.config_toml;
                Some((spec, request.instance.clone(), request.values.clone()))
            }
        };
        let component_sig = component
            .as_ref()
            .map(|(spec, instance, values)| (&spec.manifest, instance, values));
        let plan_sig = crate::sha256(&serde_json::to_vec(&(
            answers,
            component_sig,
            &setup.base_sig,
            &config_toml,
        ))?);
        Ok(WorkflowPlan {
            setup,
            component,
            config_toml,
            plan_sig,
        })
    }

    /// Rebuild the combined plan from the answers, not from client-supplied
    /// config bytes. A changed file or manifest invalidates the review.
    pub fn apply_workflow(
        &self,
        answers: &veles_core::setup::SetupAnswers,
        component: Option<&ComponentRequest>,
        expect_base: &str,
        plan_sig: &str,
        secrets: &BTreeMap<String, String>,
    ) -> Result<Value> {
        let _lock = self.lock()?;
        let mut recovery = crate::Recovery::capture([
            self.paths.config_path(),
            self.record_path(),
            self.paths.config_dir.join(".env"),
        ])?;
        let result = self.apply_workflow_inner(
            answers,
            component,
            expect_base,
            plan_sig,
            secrets,
            &mut recovery,
        );
        recovery.finish(result).map_err(|error| {
            error.context("config, environment and installer files were recovered where possible; vault values already stored are not removed")
        })
    }

    fn apply_workflow_inner(
        &self,
        answers: &veles_core::setup::SetupAnswers,
        component: Option<&ComponentRequest>,
        expect_base: &str,
        plan_sig: &str,
        secrets: &BTreeMap<String, String>,
        recovery: &mut crate::Recovery,
    ) -> Result<Value> {
        let plan = self.preview_workflow(answers, component)?;
        anyhow::ensure!(
            plan.setup.base_sig == expect_base,
            "config.toml changed after preview; review a fresh plan"
        );
        anyhow::ensure!(
            plan.plan_sig == plan_sig,
            "setup or component answers changed after preview; review a fresh plan"
        );
        let config_was_present = self.paths.config_path().is_file();
        let before = if config_was_present {
            std::fs::read_to_string(self.paths.config_path())
                .with_context(|| format!("reading {}", self.paths.config_path().display()))?
        } else {
            String::new()
        };
        let mut setup = plan.setup.clone();
        setup.config_toml = plan.config_toml.clone();
        // `setup` owns its config backup, .env merge, vault policies and
        // secret-name checks. Component values never enter this map.
        let report = veles_core::setup::apply_with(&setup, &self.paths.config_dir, secrets)?;
        recovery.mark_after(&self.paths.config_path())?;
        recovery.mark_after(&self.paths.config_dir.join(".env"))?;
        if let Some((spec, instance, values)) = &plan.component {
            self.record_config_applied(
                spec,
                instance,
                values,
                &before,
                &plan.config_toml,
                config_was_present,
            )?;
            recovery.mark_after(&self.record_path())?;
        }
        Ok(json!({
            "ok": true,
            "action": "setup-and-components",
            "mode": report.mode,
            "config_path": report.config_path,
            "config_backup": report.config_backup,
            "env_path": report.env_path,
            "env_written": report.env_written,
            "env_added": report.env_added,
            "secrets_needed": report.secrets_needed,
            "secrets_stored": report.secrets_stored,
            "vault_path": veles_core::vault::vault_dir(),
            "notes": setup.notes,
            "record_path": self.record_path(),
        }))
    }
}

impl WorkflowPlan {
    pub fn review(&self, existing: &str) -> Value {
        let component = self.component.as_ref().map(|(spec, instance, values)| {
            let changes = values
                .iter()
                .filter(|(path, _)| !veles_core::config_tool::names_a_secret(path))
                .map(|(path, value)| {
                    format!(
                        "{}{} = {}",
                        if instance.is_empty() {
                            String::new()
                        } else {
                            format!("[{instance}] ")
                        },
                        path,
                        value
                    )
                })
                .collect::<Vec<_>>();
            let mut manifest = spec.manifest.clone();
            for knob in &mut manifest.knobs {
                if veles_core::config_tool::names_a_secret(&knob.path) {
                    knob.default = None;
                    knob.note = Some("secret: use the setup/vault flow".into());
                }
            }
            json!({"manifest": manifest, "instance": instance, "changes": changes})
        });
        let before = crate::redact_config(existing);
        let after = crate::redact_config(&self.config_toml);
        let diff = veles_core::setup::preview_diff(&before, &after);
        json!({
            "ok": true,
            "mode": self.setup.mode,
            "base_sig": self.setup.base_sig,
            "plan_sig": self.plan_sig,
            "diff": diff,
            // Fresh setup may show its new file exactly as before. For an
            // existing file the diff is the review; do not echo old text.
            "config_toml": Value::Null,
            "stored": self.setup.secrets.iter().filter(|name| veles_core::secrets::resolve(name).is_some()).cloned().collect::<Vec<_>>(),
            "env_text": self.setup.env_text,
            "secrets": self.setup.secrets,
            "notes": self.setup.notes,
            "component": component,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Paths;
    fn fixture(root: &std::path::Path) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::copy(
            veles_core::testenv::process_plugin_echo(),
            root.join("plugin"),
        )
        .unwrap();
        std::fs::write(
            root.join("plugin.toml"),
            "name = \"echo\"\nkind = \"process\"\nentry = \"plugin\"\n",
        )
        .unwrap();
    }

    #[test]
    fn combined_preview_and_apply_share_one_base_and_preserve_setup_secrets() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("config");
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(&config_dir).unwrap();
        fixture(&data_dir.join("process-plugins/echo"));
        let service = InstallService::new(Paths::for_roots(config_dir.clone(), data_dir));
        std::fs::write(
            config_dir.join("config.toml"),
            "# retain this comment\n[platforms.echo]\ntoken = \"existing-secret\"\n",
        )
        .unwrap();
        let answers = veles_core::setup::SetupAnswers {
            provider: veles_core::setup::ProviderChoice::Skip,
            ..Default::default()
        };
        let component = ComponentRequest {
            name: "echo".into(),
            instance: String::new(),
            values: BTreeMap::from([("platforms.echo.rooms.fail_start".into(), json!(true))]),
        };
        let plan = service
            .preview_workflow(&answers, Some(&component))
            .unwrap();
        let review =
            plan.review("# retain this comment\n[platforms.echo]\ntoken = \"existing-secret\"\n");
        assert_eq!(review["config_toml"], Value::Null);
        assert!(!review.to_string().contains("existing-secret"));
        assert!(review["diff"].as_str().unwrap().contains("fail_start"));
        let env_path = config_dir.join(".env");
        std::fs::write(&env_path, "# preserve this environment\n").unwrap();
        crate::fail_next_record_write_for_test();
        let failed = service
            .apply_workflow(
                &answers,
                Some(&component),
                &plan.setup.base_sig,
                &plan.plan_sig,
                &BTreeMap::new(),
            )
            .unwrap_err();
        assert!(format!("{failed:#}").contains("restored"), "{failed:#}");
        assert!(std::fs::read_to_string(config_dir.join("config.toml"))
            .unwrap()
            .contains("existing-secret"));
        assert!(!std::fs::read_to_string(config_dir.join("config.toml"))
            .unwrap()
            .contains("fail_start = true"));
        assert_eq!(
            std::fs::read_to_string(env_path).unwrap(),
            "# preserve this environment\n"
        );
        let report = service
            .apply_workflow(
                &answers,
                Some(&component),
                &plan.setup.base_sig,
                &plan.plan_sig,
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(report["ok"], true, "{report}");
        let text = std::fs::read_to_string(config_dir.join("config.toml")).unwrap();
        assert!(text.contains("# retain this comment"));
        assert!(text.contains("fail_start = true"));
        assert!(text.contains("existing-secret"));
        assert!(config_dir.join("config.toml.bak").exists());
        let second = service
            .preview_workflow(&answers, Some(&component))
            .unwrap();
        std::fs::write(config_dir.join("config.toml"), "# a concurrent edit\n").unwrap();
        assert!(service
            .apply_workflow(
                &answers,
                Some(&component),
                &second.setup.base_sig,
                &second.plan_sig,
                &BTreeMap::new()
            )
            .is_err());
        assert_eq!(
            std::fs::read_to_string(config_dir.join("config.toml")).unwrap(),
            "# a concurrent edit\n"
        );
    }
}
