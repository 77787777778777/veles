//! Named profiles composed from the existing installer operations.
//!
//! Previews are read-only. A local profile uses the current managed roots;
//! a privileged profile is a signed request for an explicit CLI handoff.
//! The HTTP/Tk surfaces never execute that handoff or choose arbitrary
//! output paths. No staged install record is relocated to another root.
use crate::profiles::{
    HostLayout, NamedProfile, ProfileInput, ProfilePlan, Runner, Step, SystemStep, Target,
};
use crate::{ComponentKind, ComponentRequest, ComponentSpec, InstallService, Paths};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceScope {
    User,
    #[default]
    System,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostAnswers {
    #[serde(default)]
    pub user: Option<String>,
    /// Group for a system service. It need not share the account name.
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub scope: ServiceScope,
    /// The gateway account's VELES_HOME for a container-host plan. The
    /// matching quadlet mount/environment is reviewed in that plan; this
    /// profile does not infer or rewrite it.
    #[serde(default)]
    pub target_root: Option<PathBuf>,
    /// Host components are separate from native executable plugins. They
    /// can only be prepared for the explicit CLI handoff, never applied
    /// by the ordinary local runner or the desktop API.
    #[serde(default)]
    pub host_components: Vec<crate::host_components::HostComponent>,
    #[serde(default)]
    pub container: Option<crate::container_host::ContainerHostOptions>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileAnswers {
    pub profile: NamedProfile,
    #[serde(default)]
    pub host: HostAnswers,
    #[serde(default)]
    pub setup: veles_core::setup::SetupAnswers,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default)]
    pub options: BTreeMap<String, BTreeMap<String, Value>>,
}

#[derive(Debug, Clone)]
pub struct ProfileRequest {
    pub answers: ProfileAnswers,
    pub host: HostLayout,
    pub run_root: PathBuf,
    pub components: Vec<ComponentSpec>,
    pub core: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ReviewedProfile {
    pub request: ProfileRequest,
    pub plan: ProfilePlan,
    pub plan_sig: String,
    pub base_sig: String,
    pub staged_states: BTreeMap<PathBuf, Option<String>>,
    pub source_states: BTreeMap<PathBuf, Option<String>>,
    pub config_sha256: String,
    pub review: Value,
}

impl ReviewedProfile {
    pub fn review(&self) -> Value {
        self.review.clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileHandoff {
    pub answers: ProfileAnswers,
    pub host: HostLayout,
    pub run_root: PathBuf,
    pub components: Vec<ComponentSpec>,
    pub core: PathBuf,
    pub base_sig: String,
    pub plan_sig: String,
}

impl ProfileHandoff {
    /// Rebuild a read-only review for the CLI confirmation screen.
    pub fn request_for_cli(&self) -> ProfileRequest {
        self.request()
    }

    fn request(&self) -> ProfileRequest {
        ProfileRequest {
            answers: self.answers.clone(),
            host: self.host.clone(),
            run_root: self.run_root.clone(),
            components: self.components.clone(),
            core: self.core.clone(),
        }
    }
}

impl ProfileRequest {
    pub fn local(answers: ProfileAnswers, core: PathBuf) -> Result<Self> {
        let paths = Paths::current();
        let host = native_layout(&answers, &paths)?;
        let run_root = host.write_root.clone();
        let discovery = InstallService::new(paths);
        let components = if matches!(
            answers.profile,
            NamedProfile::Nixos | NamedProfile::ContainerHost
        ) {
            Vec::new()
        } else {
            answers
                .components
                .iter()
                .map(|name| discovery.component(name))
                .collect::<Result<Vec<_>>>()?
        };
        Ok(Self {
            answers,
            host,
            run_root,
            components,
            core,
        })
    }

    /// CLI system/container profiles have a fixed logical host. Only the
    /// typed setup answers and explicitly chosen account/container options
    /// cross this boundary; an answers file cannot select an arbitrary host
    /// directory or installation record.
    pub fn host(mut answers: ProfileAnswers) -> Result<Self> {
        anyhow::ensure!(
            matches!(
                answers.profile,
                NamedProfile::Server | NamedProfile::ContainerHost
            ),
            "a host profile must be server or container-host"
        );
        // A caller cannot smuggle native artifact metadata through the CLI
        // handoff. `ProfileRequest::host` deliberately owns an empty
        // component list, so silently dropping a serialized one would make
        // a substitution look like a valid, reviewed request.
        anyhow::ensure!(
            answers.components.is_empty() && answers.options.is_empty(),
            "host profile requests do not accept native component selections or options"
        );
        if answers.profile == NamedProfile::Server {
            let user = answers
                .host
                .user
                .clone()
                .context("a system server requires an explicit --answers user")?;
            let host = HostLayout::linux_system(
                PathBuf::from("/var/lib/veles"),
                user,
                answers.host.group.clone(),
            );
            let request = Self {
                answers,
                host: host.clone(),
                run_root: host.write_root,
                components: Vec::new(),
                core: PathBuf::new(),
            };
            anyhow::ensure!(
                request.answers.host.scope == ServiceScope::System,
                "a host server profile must choose system scope"
            );
            return Ok(request);
        }
        let container = answers
            .host
            .container
            .as_ref()
            .context("container-host requires complete typed options")?;
        answers.host.scope = ServiceScope::User;
        let mut host = HostLayout::linux_system(
            container.gateway.home.clone(),
            container.gateway.name.clone(),
            None,
        );
        host.system_service = false;
        Ok(Self {
            answers,
            host: host.clone(),
            run_root: host.write_root,
            components: Vec::new(),
            core: PathBuf::new(),
        })
    }

    fn target(&self) -> InstallService {
        InstallService::new(Paths::for_roots(
            self.host.config_dir.clone(),
            self.host.data_dir.clone(),
        ))
    }

    /// Every answer a profile does not use is REFUSED, never dropped: a
    /// request that silently ignores serialized data would let the review
    /// show one thing while the operator believed they asked for another.
    fn refuse_unused_answers(&self) -> Result<()> {
        let host = &self.answers.host;
        let profile = self.answers.profile.as_str();
        let refuse = |field: &str, why: &str| -> Result<()> {
            anyhow::bail!("the {profile} profile does not accept host.{field}: {why}")
        };
        match self.answers.profile {
            NamedProfile::Desktop => {
                if host.container.is_some() {
                    refuse(
                        "container",
                        "container options belong to the container-host profile",
                    )?;
                }
                if host.target_root.is_some() {
                    refuse(
                        "target_root",
                        "a desktop install uses this account's own directories",
                    )?;
                }
                if host.group.is_some() {
                    refuse("group", "only a system service takes a group")?;
                }
            }
            NamedProfile::Server => {
                if host.container.is_some() {
                    refuse(
                        "container",
                        "container options belong to the container-host profile",
                    )?;
                }
                if host.target_root.is_some() {
                    refuse("target_root", "a system server's VELES_HOME is /var/lib/veles; a user server uses the account's own directories")?;
                }
                if !host.host_components.is_empty() {
                    refuse("host_components", "host components are their own request: `veles install host-component --answers <file>`")?;
                }
                if host.scope == ServiceScope::User && host.group.is_some() {
                    refuse("group", "only a system service takes a group")?;
                }
            }
            NamedProfile::ContainerHost => {
                for (field, set) in [
                    ("user", host.user.is_some()),
                    ("group", host.group.is_some()),
                    ("target_root", host.target_root.is_some()),
                ] {
                    if set {
                        refuse(
                            field,
                            "the gateway account and its home are named in host.container.gateway",
                        )?;
                    }
                }
            }
            NamedProfile::Nixos => {
                for (field, set) in [
                    ("container", host.container.is_some()),
                    ("target_root", host.target_root.is_some()),
                    ("group", host.group.is_some()),
                ] {
                    if set {
                        refuse(
                            field,
                            "the nixos profile only emits a services.veles snippet",
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        self.refuse_unused_answers()?;
        anyhow::ensure!(
            self.answers.host.host_components.is_empty()
                || self.answers.profile != NamedProfile::Nixos,
            "NixOS is emit-only; host components must be declared in its module, not applied by this installer"
        );
        anyhow::ensure!(
            self.answers.host.host_components.is_empty()
                || self.answers.profile != NamedProfile::ContainerHost,
            "host components are a separate reviewed request after the confined container host is prepared"
        );
        let mut host_names = std::collections::BTreeSet::new();
        for component in &self.answers.host.host_components {
            let _preview = component.preview()?;
            anyhow::ensure!(
                host_names.insert(component.name()),
                "host component {} is selected more than once",
                component.name()
            );
        }
        if !self.answers.host.host_components.is_empty() {
            anyhow::ensure!(
                self.answers.profile == NamedProfile::Desktop && !self.host.system_service,
                "host components are a standalone request (`veles install host-component`), not part of a {} profile",
                self.answers.profile.as_str()
            );
        }
        anyhow::ensure!(
            (self.answers.profile == NamedProfile::Nixos) == (self.host.target == Target::Nixos),
            "only the emit-only nixos profile may target NixOS"
        );
        anyhow::ensure!(
            self.host.bin.is_absolute()
                && self.host.config_dir.is_absolute()
                && self.host.data_dir.is_absolute(),
            "profile host paths must be absolute"
        );
        anyhow::ensure!(
            self.run_root.is_absolute() && self.run_root != Path::new("/"),
            "profile runner root must be a non-root absolute path"
        );
        if self.answers.profile == NamedProfile::ContainerHost {
            let container = self
                .answers
                .host
                .container
                .as_ref()
                .context("container-host requires complete typed options")?;
            container.validate()?;
            anyhow::ensure!(!self.host.system_service && self.answers.host.scope == ServiceScope::User, "container-host owns accounts and quadlets, not a native system service or installation record");
            anyhow::ensure!(
                self.host.config_dir == self.host.data_dir
                    && self.host.data_dir == self.host.write_root,
                "container-host bookkeeping stays in its input workspace"
            );
            anyhow::ensure!(
                self.core.as_os_str().is_empty(),
                "container-host has no native core bundle"
            );
            anyhow::ensure!(
                container.network.is_some(),
                "container-host requires its shared network"
            );
            anyhow::ensure!(
                container
                    .quadlets
                    .iter()
                    .any(|q| q.user == container.gateway.name),
                "container-host requires a gateway quadlet"
            );
            anyhow::ensure!(self.answers.components.is_empty() && self.components.is_empty(),
                "configure container components separately after the host plan; they are not native profile components");
            return Ok(());
        }
        if self.answers.profile != NamedProfile::Nixos
            && self.answers.host.host_components.is_empty()
        {
            anyhow::ensure!(
                self.core.is_absolute(),
                "native profiles require an absolute reviewed core bundle"
            );
        }
        if !self.answers.host.host_components.is_empty() {
            anyhow::ensure!(self.core.as_os_str().is_empty()
                && self.answers.components.is_empty() && self.components.is_empty(),
                "a host-component request is separate from native core, component and configuration installation");
            anyhow::ensure!(
                self.host.data_dir.is_absolute()
                    && self.host.write_root.is_absolute()
                    && self.host.write_root != Path::new("/"),
                "host components need an explicit managed bookkeeping target"
            );
        }
        if self.host.system_service {
            anyhow::ensure!(
                self.host.config_dir == self.host.data_dir,
                "system VELES_HOME must own config, .env, install.toml and data in one directory"
            );
            anyhow::ensure!(
                self.host.bin == self.host.data_dir.join("install/current/bin/veles"),
                "system unit must execute the managed current core"
            );
            anyhow::ensure!(
                self.host.system_unit_dir == Path::new("/etc/systemd/system")
                    || self.host.system_unit_dir.starts_with(&self.run_root),
                "system units must target /etc/systemd/system or an explicit temporary test root"
            );
            anyhow::ensure!(
                self.host.data_dir == Path::new("/var/lib/veles")
                    || self.host.data_dir.starts_with(&self.run_root),
                "system VELES_HOME must be /var/lib/veles or under an explicit temporary test root"
            );
        }
        let mut names = std::collections::BTreeSet::new();
        for spec in &self.components {
            crate::valid_component_name(&spec.name)?;
            anyhow::ensure!(
                names.insert(&spec.name) && self.answers.components.contains(&spec.name),
                "component {} is not selected exactly once",
                spec.name
            );
            anyhow::ensure!(
                spec.describe_error.is_none() && spec.source.is_some(),
                "component {} has no reviewed artifact",
                spec.name
            );
        }
        anyhow::ensure!(
            names.len() == self.answers.components.len(),
            "a selected component is unavailable"
        );
        for (component, options) in &self.answers.options {
            anyhow::ensure!(
                self.answers.components.contains(component),
                "options supplied for unselected component {component}"
            );
            for name in options.keys() {
                anyhow::ensure!(
                    !veles_core::config_tool::names_a_secret(name),
                    "{name} is a secret; use setup/vault, not component options"
                );
            }
        }
        Ok(())
    }

    pub fn review(self) -> Result<ReviewedProfile> {
        self.review_ref()
    }

    /// Rebuild the same reviewed plan without consuming the request. This
    /// is used after acquiring the target lock, where the target and inputs
    /// must be revalidated against the actual current files.
    pub fn review_ref(&self) -> Result<ReviewedProfile> {
        self.validate()?;
        let container_only = self.answers.profile == NamedProfile::ContainerHost;
        let target = self.target();
        let existing = if container_only {
            None
        } else {
            read_existing(&target.paths.config_path())?
        };
        let base_sig = veles_core::setup::base_sig(existing.as_deref());
        let setup = if container_only {
            veles_core::setup::SetupPlan {
                config_toml: String::new(),
                env_text: String::new(),
                secrets: Vec::new(),
                notes: Vec::new(),
                allowlists: Vec::new(),
                mode: veles_core::setup::SetupMode::Fresh,
                base_sig: String::new(),
            }
        } else {
            veles_core::setup::setup_plan(&self.answers.setup, existing.as_deref())?
        };
        let mut config_toml = setup.config_toml.clone();
        let mut component_requests = Vec::new();
        for spec in &self.components {
            let values = self
                .answers
                .options
                .get(&spec.name)
                .cloned()
                .unwrap_or_default();
            let request = ComponentRequest {
                name: spec.name.clone(),
                instance: String::new(),
                values,
            };
            let plan = crate::plan_config_text(
                &config_toml,
                &target.paths.config_path(),
                spec,
                &request.values,
                "",
            )?;
            config_toml = plan.config_toml;
            component_requests.push(request);
        }
        let core = if matches!(
            self.answers.profile,
            NamedProfile::Nixos | NamedProfile::ContainerHost
        ) || !self.answers.host.host_components.is_empty()
        {
            None
        } else {
            Some(target.preview_bundle(&self.core)?)
        };
        let mut component_sigs = BTreeMap::new();
        for spec in &self.components {
            let bytes = std::fs::read(spec.source.as_ref().context("component has no artifact")?)?;
            component_sigs.insert(
                spec.name.clone(),
                signature(&(spec, crate::sha256(&bytes)))?,
            );
        }
        let mut plan = crate::profiles::plan(&ProfileInput {
            profile: self.answers.profile,
            host: &self.host,
            setup: &setup,
            components: &self.components,
            plan: None,
        })?;
        if !matches!(
            self.answers.profile,
            NamedProfile::Nixos | NamedProfile::ContainerHost
        ) && self.answers.host.host_components.is_empty()
        {
            let mut user = Vec::with_capacity(plan.user.len() + 1);
            user.push(Step::CoreBundle {
                source: self.core.clone(),
                plan_sig: core.as_ref().unwrap().plan_sig.clone(),
            });
            for step in plan.user {
                match step {
                    Step::Component {
                        name,
                        spec,
                        version,
                        tool,
                        ..
                    } => user.push(Step::Component {
                        plan_sig: component_sigs[&name].clone(),
                        name,
                        spec,
                        version,
                        tool,
                    }),
                    Step::Config { path, mode, .. } => {
                        let mut merged = setup.clone();
                        merged.config_toml = config_toml.clone();
                        user.push(Step::Config {
                            sha256: crate::sha256(config_toml.as_bytes()),
                            path,
                            mode,
                            setup: merged,
                        });
                    }
                    other => user.push(other),
                }
            }
            plan.user = user;
        }
        if let Some(container) = &self.answers.host.container {
            let host_plan = container.plan()?;
            plan.system.push(SystemStep::EmbeddedScript {
                text: host_plan.script,
                sha256: host_plan.script_sha256,
                reason: "typed container-host plan; unexecuted until the privileged CLI handoff"
                    .into(),
            });
            plan.notes.push("PDS, media publishing, pool, gateway configuration and SearXNG are separately reviewed components, not implied by this host plan.".into());
        }
        if self.host.system_service && self.answers.profile == NamedProfile::Server {
            let user = self
                .host
                .user
                .as_deref()
                .context("system server requires a service account")?;
            let group = self.host.group.as_deref().unwrap_or(user);
            crate::valid_component_name(user).context("invalid service account")?;
            crate::valid_component_name(group).context("invalid service group")?;
            plan.system.insert(
                0,
                SystemStep::Command {
                    argv: vec!["id".into(), "-u".into(), user.into()],
                    reason: "the explicitly named service account must already exist".into(),
                },
            );
            // Ownership is assigned to the explicitly created managed
            // paths by the handoff, never recursively through caller data.
            plan.system.push(SystemStep::Command {
                argv: vec![
                    "test".into(),
                    "-x".into(),
                    self.host.bin.display().to_string(),
                ],
                reason: "the reviewed core supplies the gateway executable".into(),
            });
        }
        if !self.answers.host.host_components.is_empty() {
            // A host-component request carries no native steps: it neither
            // writes this account's config nor installs a core. Its effects
            // are the components' own reviewed previews.
            plan.user.clear();
            plan.system.clear();
            plan.notes = vec!["Host components only: applied as root by `veles install profile-handoff`, as one transaction that restores its files and compensates its service effects on failure. This account's configuration is not changed.".into()];
        }
        plan.validate()?;
        // The previews are part of the signature: they carry the rendered
        // host files and the digest of any input file (a pool member's
        // client config), so a changed input refuses the handoff.
        let host_previews = self
            .answers
            .host
            .host_components
            .iter()
            .map(crate::host_components::HostComponent::preview)
            .collect::<Result<Vec<_>>>()?;
        let source_states = source_states(&plan)?;
        let target_states = target_states(&plan)?;
        let staged_states = staged_states(&plan)?;
        let config_sha256 = crate::sha256(config_toml.as_bytes());
        let plan_sig = signature(&(
            &self.answers,
            &self.host,
            &self.run_root,
            &core.as_ref().map(|p| p.plan_sig.clone()),
            &component_sigs,
            &base_sig,
            &config_sha256,
            &source_states,
            &target_states,
            &staged_states,
            &plan,
            &host_previews,
        ))?;
        let mut review = plan.review();
        review["host_components"] = json!(host_previews);
        review["preview"] = json!(true);
        review["confirm_required"] = json!(true);
        review["plan_sig"] = json!(plan_sig);
        review["base_sig"] = json!(base_sig);
        review["core"] = json!(core
            .as_ref()
            .map(|c| json!({"bundle":c.info,"plan_sig":c.plan_sig})));
        review["scope"] = json!(self.answers.host.scope);
        review["selected_components"] = json!(self.answers.components);
        review["config_toml"] = Value::Null;
        review["diff"] = json!(veles_core::setup::preview_diff(
            &crate::redact_config(existing.as_deref().unwrap_or("")),
            &crate::redact_config(&config_toml)
        ));
        review["env_text"] = json!(if container_only {
            String::new()
        } else {
            setup.env_text.clone()
        });
        let mut secret_names = if container_only {
            Vec::new()
        } else {
            setup.secrets.clone()
        };
        for component in &host_previews {
            if let Some(names) = component["secrets"].as_array() {
                secret_names.extend(
                    names
                        .iter()
                        .filter_map(|name| name.as_str().map(str::to_string)),
                );
            }
        }
        secret_names.sort();
        secret_names.dedup();
        review["secrets"] = json!(secret_names);
        review["applied"] = json!(false);
        review["privileged_pending"] =
            json!(!plan.system.is_empty() || !self.answers.host.host_components.is_empty());
        review["note"] = json!("Preview only. Privileged scripts are unexecuted. A signed request is not an installed or running service.");
        Ok(ReviewedProfile {
            request: self.clone(),
            plan,
            plan_sig,
            base_sig,
            staged_states,
            source_states,
            config_sha256,
            review,
        })
    }
}

impl InstallService {
    pub fn current_profile_answers(&self, profile: NamedProfile) -> Result<ProfileAnswers> {
        let existing = read_existing(&self.paths.config_path())?;
        let setup = match existing {
            Some(ref text) if !text.trim().is_empty() => veles_core::setup::current_answers(text)?,
            _ => veles_core::setup::SetupAnswers::default(),
        };
        Ok(ProfileAnswers {
            profile,
            host: HostAnswers::default(),
            setup,
            components: Vec::new(),
            options: BTreeMap::new(),
        })
    }

    pub fn preview_profile(&self, request: ProfileRequest) -> Result<ReviewedProfile> {
        request.review()
    }

    pub fn apply_profile(
        &self,
        request: ProfileRequest,
        expected_base: &str,
        plan_sig: &str,
        secrets: &BTreeMap<String, String>,
    ) -> Result<Value> {
        let target = request.target();
        let reviewed = request.review_ref()?;
        anyhow::ensure!(
            reviewed.base_sig == expected_base,
            "config.toml changed after profile preview; review a fresh plan"
        );
        anyhow::ensure!(
            reviewed.plan_sig == plan_sig,
            "profile answers or artifacts changed after preview; review a fresh plan"
        );
        for (path, expected) in &reviewed.staged_states {
            anyhow::ensure!(
                file_hash(path)? == *expected,
                "staged profile input {} changed after preview; review a fresh plan",
                path.display()
            );
        }
        if reviewed.plan.profile == NamedProfile::Nixos {
            return Ok(
                json!({"ok":true,"status":"emitted","applied":false,"emitted":reviewed.plan.files,
                "note":"Import the reviewed module and run nixos-rebuild yourself; nothing was applied."}),
            );
        }
        if reviewed.plan.profile == NamedProfile::ContainerHost
            || reviewed.request.host.system_service
            || !reviewed.request.answers.host.host_components.is_empty()
        {
            let handoff = ProfileHandoff {
                answers: reviewed.request.answers.clone(),
                host: reviewed.request.host.clone(),
                run_root: reviewed.request.run_root.clone(),
                components: reviewed.request.components.clone(),
                core: reviewed.request.core.clone(),
                base_sig: reviewed.base_sig.clone(),
                plan_sig: reviewed.plan_sig.clone(),
            };
            let path = self
                .paths
                .data_dir
                .join("install-profile-requests")
                .join(format!("{}.json", reviewed.plan_sig));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
                veles_core::platform::private_dir(parent)?;
            }
            veles_core::platform::write_private(&path, &serde_json::to_vec_pretty(&handoff)?)?;
            return Ok(
                json!({"ok":true,"status":"handoff-required","applied":false,"staged":true,"privileged_pending":true,
                "request_path":path,"plan_sig":reviewed.plan_sig,
                "command":format!("veles install profile-handoff --request {} --yes", path.display()),
                "privileged_script":reviewed.plan.script()?.map(|s|json!({"sha256":s.sha256,"text":s.text,"executed":false})),
                "note":"Review the signed request and run the CLI handoff explicitly. No host changes or service activation were performed by this request."}),
            );
        }
        let _lock = target.lock()?;
        let reviewed = request.review_ref()?;
        anyhow::ensure!(reviewed.base_sig == expected_base && reviewed.plan_sig == plan_sig,
            "profile target or artifacts changed while waiting for the installer lock; review a fresh plan");
        for (path, expected) in &reviewed.staged_states {
            anyhow::ensure!(
                file_hash(path)? == *expected,
                "staged profile input {} changed after preview; review a fresh plan",
                path.display()
            );
        }
        self.apply_local_profile(&reviewed, secrets)
    }

    fn apply_local_profile(
        &self,
        reviewed: &ReviewedProfile,
        secrets: &BTreeMap<String, String>,
    ) -> Result<Value> {
        anyhow::ensure!(
            reviewed.plan.complete,
            "profile is incomplete: {}",
            reviewed.plan.unsupported.join("; ")
        );
        let target = reviewed.request.target();
        let config_before = read_existing(&target.paths.config_path())?;
        let before = target.record()?;
        let paths = recovery_paths(&target.paths, &before, &reviewed.plan)?;
        let recovery = std::sync::Arc::new(std::sync::Mutex::new(crate::Recovery::capture(
            paths.clone(),
        )?));
        let service = target.clone().with_profile_transaction(recovery.clone());
        let result = (|| -> Result<Value> {
            apply_profile_config(&service, reviewed, secrets)?;
            let mut local = reviewed.plan.clone();
            local
                .user
                .retain(|step| !matches!(step, Step::Config { .. }));
            let report = Runner::new(reviewed.request.run_root.clone())
                .service(service.clone())
                .run(&local)?;
            record_profile_components(
                &service,
                reviewed,
                config_before.as_deref().unwrap_or(""),
                config_before.is_some(),
            )?;
            drop(service);
            record_profile(&target, reviewed.plan.profile)?;
            let mut guard = recovery
                .lock()
                .map_err(|_| anyhow::anyhow!("profile recovery lock poisoned"))?;
            for path in &paths {
                guard.mark_after(path)?;
            }
            drop(guard);
            let script = report
                .script
                .as_ref()
                .map(|s| json!({"sha256":s.sha256,"text":s.text,"executed":false}));
            Ok(
                json!({"ok":true,"status":"applied","applied":true,"staged":false,
                "steps":report.steps,"privileged_script":script,"privileged_pending":script.is_some(),"service_running":false,
                "record_path":target.record_path(),"note":"Local profile steps completed. The privileged linger action, if any, and service activation remain separate."}),
            )
        })();
        finish_shared(recovery, result)
    }
}

/// Execute only a signed request reviewed by the operator in the CLI.
/// HTTP and Tk call `apply_profile`, which can only create this request.
pub fn privileged_handoff(
    handoff: &ProfileHandoff,
    secrets: &BTreeMap<String, String>,
) -> Result<Value> {
    let request = handoff.request();
    let reviewed = request.review()?;
    anyhow::ensure!(reviewed.base_sig == handoff.base_sig && reviewed.plan_sig == handoff.plan_sig,
        "profile request, target state or artifact changed after preview; refusing privileged handoff");
    for (path, expected) in &reviewed.source_states {
        anyhow::ensure!(
            file_hash(path)? == *expected,
            "privileged source {} changed after preview; refusing handoff",
            path.display()
        );
    }
    if !reviewed.request.answers.host.host_components.is_empty() {
        anyhow::ensure!(
            reviewed.plan.profile == NamedProfile::Desktop
                && !reviewed.request.host.system_service
                && reviewed.plan.system.is_empty()
                && reviewed.plan.user.is_empty(),
            "a host-component request carries no native profile steps"
        );
        #[cfg(unix)]
        {
            // The host root is fixed here. Staged request metadata never
            // becomes the payload's root, and the lock lives in a
            // root-owned directory, not in the requesting account's data.
            return crate::host_components::apply_all(
                &reviewed.request.answers.host.host_components,
                Path::new("/"),
                secrets,
                &mut crate::host_components::ProcessCommands,
            );
        }
        #[cfg(not(unix))]
        anyhow::bail!("host components require a Unix host");
    }
    if reviewed.plan.profile == NamedProfile::ContainerHost {
        let container = reviewed
            .request
            .answers
            .host
            .container
            .as_ref()
            .context("container-host request has no typed options")?;
        let host_plan = container.plan()?;
        return run_container_script(&reviewed.plan, &container_host_paths(&host_plan), |text| {
            crate::Action::Command {
                argv: vec!["/bin/sh".into(), "-c".into(), text.to_string()],
                as_user: None,
                privileged: true,
            }
            .apply_privileged(Path::new("/"))
            .map(|_| ())
        });
    }
    anyhow::ensure!(
        reviewed.request.host.system_service,
        "a native privileged handoff requires an explicit system-service target"
    );
    let target = handoff.request().target();
    #[cfg(unix)]
    {
        let owner = crate::ownership::account(
            reviewed
                .request
                .host
                .user
                .as_deref()
                .context("system account missing")?,
            reviewed.request.host.group.as_deref(),
        )?;
        for root in [&target.paths.config_dir, &target.paths.install_root] {
            if std::fs::symlink_metadata(root).is_ok() {
                crate::ownership::ensure_dir(root, 0o700, &owner)?;
            }
        }
    }
    let _lock = target.lock()?;
    let reviewed = handoff.request().review()?;
    anyhow::ensure!(
        reviewed.base_sig == handoff.base_sig && reviewed.plan_sig == handoff.plan_sig,
        "target changed while waiting for the installer lock; refusing privileged handoff"
    );
    for (path, expected) in &reviewed.source_states {
        anyhow::ensure!(
            file_hash(path)? == *expected,
            "privileged source {} changed after preview; refusing handoff",
            path.display()
        );
    }
    let bak_existed = target
        .paths
        .record_path()
        .with_extension("toml.bak")
        .exists();
    let config_before = read_existing(&target.paths.config_path())?;
    let before = target.record()?;
    let paths = recovery_paths(&target.paths, &before, &reviewed.plan)?;
    let core_review = reviewed
        .plan
        .user
        .iter()
        .find_map(|step| match step {
            Step::CoreBundle { source, .. } => {
                Some(crate::review_bundle(source, &target.paths.install_root))
            }
            _ => None,
        })
        .transpose()?;
    let recovery = std::sync::Arc::new(std::sync::Mutex::new(crate::Recovery::capture(
        paths.clone(),
    )?));
    let service = target.clone().with_profile_transaction(recovery.clone());
    let result = (|| -> Result<Value> {
        let user = reviewed
            .request
            .host
            .user
            .as_deref()
            .context("system handoff requires a service account")?;
        let owner = crate::ownership::account(user, reviewed.request.host.group.as_deref())?;
        for step in &reviewed.plan.system {
            if let SystemStep::Command { argv, .. } = step {
                if matches!(argv.first().map(String::as_str), Some("id")) {
                    run_privileged_command(argv)?;
                }
            }
        }
        for directory in managed_directories(&target.paths, &reviewed.plan, core_review.as_ref())? {
            // A directory this run creates is part of its rollback: removed
            // again on failure when nothing foreign was put in it.
            let created = std::fs::symlink_metadata(&directory).is_err();
            if created {
                recovery
                    .lock()
                    .map_err(|_| anyhow::anyhow!("profile recovery lock poisoned"))?
                    .track(&directory)?;
            }
            crate::ownership::ensure_dir(&directory, 0o700, &owner)?;
            if created {
                service.note_profile_recovery(&directory)?;
            }
        }
        // Secrets for a system service go to ITS .env. Through the setup
        // writer they would land in this root process's own vault, an
        // account the service never reads (found on a real host).
        apply_profile_config(&service, &reviewed, &BTreeMap::new())?;
        for (name, value) in secrets {
            anyhow::ensure!(
                reviewed.review["secrets"]
                    .as_array()
                    .is_some_and(|names| names.iter().any(|n| n == name)),
                "{name} is not a secret this profile asked for"
            );
            if !value.trim().is_empty() {
                veles_core::authn::write_dotenv_in(&target.paths.config_dir, name, value.trim())?;
            }
        }
        service.note_profile_recovery(&target.paths.config_dir.join(".env"))?;
        for step in &reviewed.plan.user {
            match step {
                Step::CoreBundle { source, plan_sig } => {
                    service.install_local_bundle(source, plan_sig)?;
                }
                Step::Component { spec, plan_sig, .. } => {
                    service.install_reviewed_component(spec, plan_sig)?;
                }
                Step::Config { .. } | Step::UserFile { .. } | Step::Action(_) => {}
            }
        }
        record_profile_components(
            &service,
            &reviewed,
            config_before.as_deref().unwrap_or(""),
            config_before.is_some(),
        )?;
        // Through the transaction: a later failure must find install.toml as
        // this step left it, or recovery refuses to restore it.
        record_profile(&service, reviewed.plan.profile)?;
        own_managed_files(&target.paths, &reviewed.plan, core_review.as_ref(), &owner)?;
        for step in &reviewed.plan.system {
            match step {
                SystemStep::File {
                    destination,
                    contents,
                    mode,
                    source,
                } => {
                    // The reviewed contents are authoritative. A staged copy exists
                    // only for the printed script; when present it must match.
                    // Only a source-only step (the FreeBSD rc script) is read.
                    let bytes = match source {
                        Some(path)
                            if path != destination && (contents.is_empty() || path.exists()) =>
                        {
                            std::fs::read(path).with_context(|| {
                                format!("reading the reviewed source {}", path.display())
                            })?
                        }
                        _ => contents.as_bytes().to_vec(),
                    };
                    if !contents.is_empty() {
                        anyhow::ensure!(
                            bytes == contents.as_bytes(),
                            "staged source differs from the reviewed content for {}",
                            destination.display()
                        );
                    }
                    let relative = destination
                        .strip_prefix("/")
                        .context("privileged file target must be absolute")?;
                    crate::Action::WriteFile {
                        path: relative.to_path_buf(),
                        contents: bytes,
                        mode: Some(*mode),
                        expected_sha256: file_hash(destination)?,
                    }
                    .apply_privileged(Path::new("/"))?;
                    service.note_profile_recovery(destination)?;
                }
                SystemStep::Command { argv, .. } => {
                    if !matches!(argv.first().map(String::as_str), Some("id" | "install")) {
                        run_privileged_command(argv)?;
                    }
                }
                SystemStep::Link { link, .. } => {
                    step.apply_link(Path::new("/"))?;
                    service.note_profile_recovery(link)?;
                }
                SystemStep::EmbeddedScript { .. } => {
                    anyhow::bail!("native system profile cannot contain a container script")
                }
            }
        }
        anyhow::ensure!(
            !crate::fail_after_system_steps_for_test(),
            "injected failure after the privileged system steps"
        );
        drop(service);
        let mut guard = recovery
            .lock()
            .map_err(|_| anyhow::anyhow!("profile recovery lock poisoned"))?;
        for path in &paths {
            guard.mark_after(path)?;
        }
        drop(guard);
        Ok(
            json!({"ok":true,"status":"applied","applied":true,"service_running":false,
            "record_path":target.record_path(),"note":"The reviewed native system profile completed. The gateway unit was not enabled or started. File/config recovery does not undo command effects such as ownership changes."}),
        )
    })();
    let result = finish_shared(recovery, result);
    // record.save keeps the previous install.toml as install.toml.bak. A
    // failed first install must not leave that backup behind.
    if result.is_err() && !bak_existed {
        let _ = std::fs::remove_file(target.paths.record_path().with_extension("toml.bak"));
    }
    result
}

fn managed_directories(
    paths: &Paths,
    plan: &ProfilePlan,
    core: Option<&crate::BundlePlan>,
) -> Result<Vec<PathBuf>> {
    let mut out = vec![
        paths.config_dir.clone(),
        paths.install_root.clone(),
        paths.install_root.join("releases"),
        paths.install_root.join("components"),
    ];
    for step in &plan.user {
        match step {
            Step::CoreBundle { .. } => {
                let _ = core.context("system profile lacks its reviewed core metadata")?;
            }
            Step::Component { name, spec, .. } if spec.source.is_some() => {
                out.push(paths.install_root.join("components").join(name));
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn own_managed_files(
    paths: &Paths,
    plan: &ProfilePlan,
    core: Option<&crate::BundlePlan>,
    owner: &crate::ownership::Account,
) -> Result<()> {
    // The record backup and the installer lock are created by this root
    // run too; left root-owned, the service account could not run a later
    // `veles install` update of its own installation.
    for file in [
        paths.config_path(),
        paths.config_dir.join(".env"),
        paths.record_path(),
        paths.record_path().with_extension("toml.bak"),
        paths.install_root.join(".operation.lock"),
    ] {
        if std::fs::symlink_metadata(&file).is_ok() {
            crate::ownership::chown(&file, owner)?;
        }
    }
    for step in &plan.user {
        match step {
            Step::CoreBundle { .. } => {
                let review = core.context("system profile lacks its reviewed core metadata")?;
                crate::ownership::chown_reviewed_tree(
                    &paths
                        .install_root
                        .join("releases")
                        .join(format!("{}-{}", review.info.version, review.info.revision)),
                    owner,
                )?;
                crate::ownership::chown_link(&paths.install_root.join("current"), owner)?;
            }
            Step::Component { spec, .. } => {
                let source = spec.source.as_ref().context("component has no source")?;
                let hash = crate::sha256(&std::fs::read(source)?);
                crate::ownership::chown_reviewed_tree(
                    &paths
                        .install_root
                        .join("components")
                        .join(&spec.name)
                        .join(format!("{}-{}", spec.manifest.version, &hash[..12])),
                    owner,
                )?;
                let selection = match spec.kind {
                    ComponentKind::Module => paths.data_dir.join("modules").join(&spec.name),
                    ComponentKind::ProcessPlugin => {
                        paths.data_dir.join("process-plugins").join(&spec.name)
                    }
                };
                crate::ownership::ensure_dir(&selection, 0o700, owner)?;
                crate::ownership::chown(&selection.join("plugin.toml"), owner)?;
                if spec.kind == ComponentKind::Module {
                    crate::ownership::chown_link(&selection.join("current"), owner)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn apply_profile_config(
    service: &InstallService,
    reviewed: &ReviewedProfile,
    secrets: &BTreeMap<String, String>,
) -> Result<()> {
    if reviewed.plan.profile == NamedProfile::ContainerHost {
        return Ok(());
    }
    let target = &service.paths;
    let existing = read_existing(&target.config_path())?;
    let setup =
        veles_core::setup::setup_plan(&reviewed.request.answers.setup, existing.as_deref())?;
    let mut config = setup.config_toml.clone();
    for spec in &reviewed.request.components {
        let values = reviewed
            .request
            .answers
            .options
            .get(&spec.name)
            .cloned()
            .unwrap_or_default();
        config =
            crate::plan_config_text(&config, &target.config_path(), spec, &values, "")?.config_toml;
    }
    anyhow::ensure!(
        crate::sha256(config.as_bytes()) == reviewed.config_sha256,
        "configuration plan changed after review"
    );
    let mut setup = setup;
    setup.config_toml = config.clone();
    let result = veles_core::setup::apply_with(&setup, &target.config_dir, secrets);
    if result.is_err() {
        service.note_profile_recovery(&target.config_path())?;
        service.note_profile_recovery(&target.config_dir.join(".env"))?;
    }
    result?;
    service.note_profile_recovery(&target.config_path())?;
    service.note_profile_recovery(&target.config_dir.join(".env"))?;
    Ok(())
}

fn record_profile_components(
    service: &InstallService,
    reviewed: &ReviewedProfile,
    before: &str,
    existed: bool,
) -> Result<()> {
    let after = reviewed
        .plan
        .user
        .iter()
        .find_map(|step| match step {
            Step::Config { setup, .. } => Some(setup.config_toml.as_str()),
            _ => None,
        })
        .context("profile component history lacks its reviewed configuration")?;
    for spec in &reviewed.request.components {
        let values = reviewed
            .request
            .answers
            .options
            .get(&spec.name)
            .cloned()
            .unwrap_or_default();
        service.record_config_applied(spec, "", &values, before, after, existed)?;
        service.note_profile_recovery(&service.record_path())?;
    }
    Ok(())
}

fn record_profile(service: &InstallService, profile: NamedProfile) -> Result<()> {
    let mut record = service.record()?;
    if record.profile != profile.record() {
        record.profile = profile.record();
        record.history.push(crate::HistoryEntry {
            operation: "profile".into(),
            component: "profile".into(),
            from_version: None,
            to_version: profile.as_str().into(),
            config_before_sha256: None,
            config_after_sha256: None,
            config_snapshot: None,
            options_before: Default::default(),
            config_file_existed: None,
        });
        record.save(&service.record_path())?;
    }
    service.note_profile_recovery(&service.record_path())
}

fn run_privileged_command(argv: &[String]) -> Result<()> {
    crate::Action::Command {
        argv: argv.to_vec(),
        as_user: None,
        privileged: true,
    }
    .apply_privileged(Path::new("/"))?;
    Ok(())
}

fn recovery_paths(
    paths: &Paths,
    before: &crate::InstallRecord,
    plan: &ProfilePlan,
) -> Result<Vec<PathBuf>> {
    let mut out = vec![
        paths.config_path(),
        paths.config_dir.join(".env"),
        paths.record_path(),
        paths.install_root.join("current"),
    ];
    for step in &plan.user {
        match step {
            Step::Config { path, .. } | Step::UserFile { path, .. } => out.push(path.clone()),
            Step::Action(crate::Action::WriteFile { path, .. }) => out.push(path.clone()),
            Step::Component { name, spec, .. } => {
                let artifact = spec
                    .source
                    .as_ref()
                    .and_then(|path| std::fs::read(path).ok())
                    .map(|bytes| crate::sha256(&bytes));
                if let Some(hash) = artifact {
                    out.push(
                        paths
                            .install_root
                            .join("components")
                            .join(name)
                            .join(format!("{}-{}", spec.manifest.version, &hash[..12])),
                    );
                }
                out.push(paths.data_dir.join("modules").join(name).join("current"));
                out.push(
                    paths
                        .data_dir
                        .join("modules")
                        .join(name)
                        .join("plugin.toml"),
                );
                out.push(
                    paths
                        .data_dir
                        .join("process-plugins")
                        .join(name)
                        .join("plugin.toml"),
                );
            }
            Step::Action(_) => {}
            Step::CoreBundle { source, .. } => {
                let review = crate::review_bundle(source, &paths.install_root)?;
                out.push(
                    paths
                        .install_root
                        .join("releases")
                        .join(format!("{}-{}", review.info.version, review.info.revision)),
                );
            }
        }
    }
    for component in &before.components {
        if let Some(kind) = component.kind {
            match kind {
                ComponentKind::Module => {
                    out.push(
                        paths
                            .data_dir
                            .join("modules")
                            .join(&component.name)
                            .join("current"),
                    );
                    out.push(
                        paths
                            .data_dir
                            .join("modules")
                            .join(&component.name)
                            .join("plugin.toml"),
                    );
                }
                ComponentKind::ProcessPlugin => out.push(
                    paths
                        .data_dir
                        .join("process-plugins")
                        .join(&component.name)
                        .join("plugin.toml"),
                ),
            }
        }
    }
    for step in &plan.system {
        match step {
            SystemStep::File { destination, .. } => out.push(destination.clone()),
            // A reviewed link is a host change like a file: captured, so a
            // later failure removes the link this run created.
            SystemStep::Link { link, .. } => out.push(link.clone()),
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn finish_shared<T>(
    recovery: std::sync::Arc<std::sync::Mutex<crate::Recovery>>,
    result: Result<T>,
) -> Result<T> {
    let recovery = std::sync::Arc::try_unwrap(recovery)
        .map_err(|_| anyhow::anyhow!("profile recovery still shared"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("profile recovery lock poisoned"))?;
    recovery.finish(result)
}

fn source_states(plan: &ProfilePlan) -> Result<BTreeMap<PathBuf, Option<String>>> {
    let mut out = BTreeMap::new();
    for step in &plan.system {
        if let SystemStep::File {
            source: Some(path),
            contents,
            ..
        } = step
        {
            let state = file_hash(path)?;
            if contents.is_empty() {
                anyhow::ensure!(
                    state.is_some(),
                    "reviewed source file {} is missing",
                    path.display()
                );
            }
            out.insert(path.clone(), state);
        }
    }
    Ok(out)
}

fn target_states(plan: &ProfilePlan) -> Result<BTreeMap<PathBuf, Option<String>>> {
    let mut out = BTreeMap::new();
    for step in &plan.system {
        if let SystemStep::File { destination, .. } = step {
            out.insert(destination.clone(), file_hash(destination)?);
        }
    }
    Ok(out)
}

fn staged_states(plan: &ProfilePlan) -> Result<BTreeMap<PathBuf, Option<String>>> {
    let mut out = BTreeMap::new();
    for step in &plan.user {
        if let Step::UserFile { path, .. } = step {
            out.insert(path.clone(), file_hash(path)?);
        }
    }
    Ok(out)
}

fn file_hash(path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "reviewed file {} is not a regular file",
                path.display()
            );
            Ok(Some(crate::sha256(&std::fs::read(path)?)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("inspecting reviewed file {}", path.display())),
    }
}

fn read_existing(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        // An installed system service's VELES_HOME is private to its account:
        // repairing or updating it is prepared as root, not as the operator.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(e).with_context(|| {
            format!(
                "reading target configuration {} (it belongs to the service account: prepare this request with sudo)",
                path.display()
            )
        }),
        Err(e) => {
            Err(e).with_context(|| format!("reading target configuration {}", path.display()))
        }
    }
}

fn signature<T: Serialize>(value: &T) -> Result<String> {
    Ok(crate::sha256(&serde_json::to_vec(value)?))
}

/// Build native paths from explicit answers and the current installation.
/// The API uses the same function; an HTTP caller cannot supply a layout.
pub fn native_layout(answers: &ProfileAnswers, paths: &Paths) -> Result<HostLayout> {
    let target = match answers.profile {
        NamedProfile::Nixos => Target::Nixos,
        _ if cfg!(target_os = "freebsd") => Target::FreeBsd,
        _ => Target::Linux,
    };
    let system_service = answers.profile == NamedProfile::ContainerHost
        || (answers.profile == NamedProfile::Server && answers.host.scope == ServiceScope::System);
    let user = if let Some(user) = &answers.host.user {
        Some(user.clone())
    } else if answers.profile == NamedProfile::ContainerHost {
        answers
            .host
            .container
            .as_ref()
            .map(|c| c.gateway.name.clone())
    } else if answers.profile == NamedProfile::Nixos {
        Some("veles".into())
    } else {
        Some(crate::current_user_name().context("name the gateway account explicitly")?)
    };
    let (config_dir, data_dir) = if system_service {
        if answers.profile == NamedProfile::ContainerHost {
            let root = answers
                .host
                .target_root
                .clone()
                .or_else(|| {
                    answers
                        .host
                        .container
                        .as_ref()
                        .map(|c| c.gateway.home.clone())
                })
                .context("container-host needs a gateway target root")?;
            (root.clone(), root)
        } else {
            (
                PathBuf::from("/var/lib/veles"),
                PathBuf::from("/var/lib/veles"),
            )
        }
    } else {
        (paths.config_dir.clone(), paths.data_dir.clone())
    };
    let write_root = if system_service {
        paths.data_dir.join("install-profile-staging")
    } else {
        common_root(&paths.config_dir, &paths.data_dir)?
    };
    let user_unit_dir = if system_service {
        paths.config_dir.join("systemd/user")
    } else {
        veles_core::config::config_base().join("systemd/user")
    };
    Ok(HostLayout {
        target,
        bin: data_dir.join("install/current/bin/veles"),
        user,
        group: answers.host.group.clone(),
        config_dir: config_dir.clone(),
        data_dir: data_dir.clone(),
        system_service,
        write_root,
        user_unit_dir,
        system_unit_dir: PathBuf::from("/etc/systemd/system"),
        rc_dir: PathBuf::from("/usr/local/etc/rc.d"),
        rc_source: None,
    })
}

fn common_root(a: &Path, b: &Path) -> Result<PathBuf> {
    for candidate in a.ancestors() {
        if b.starts_with(candidate) && candidate != Path::new("/") {
            return Ok(candidate.to_path_buf());
        }
    }
    anyhow::bail!("profile config and data directories have no managed common root")
}

/// Every host file the typed container-host plan renders: the files its
/// script installs and the quadlets. Accounts, linger, subordinate id
/// ranges, packages and enabled units are not files and are not reversed.
fn container_host_paths(plan: &crate::container_host::ContainerHostPlan) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = plan
        .files
        .iter()
        .map(|file| file.path.clone())
        .chain(plan.quadlets.iter().map(|quadlet| quadlet.path.clone()))
        .chain(plan.firewall.iter().map(|file| file.path.clone()))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn run_container_script(
    plan: &ProfilePlan,
    paths: &[PathBuf],
    run: impl FnMut(&str) -> Result<()>,
) -> Result<Value> {
    let mut scripts = Vec::new();
    for step in &plan.system {
        match step {
            SystemStep::EmbeddedScript { text, .. } => scripts.push(text.clone()),
            _ => anyhow::bail!("container-host cannot carry native system steps"),
        }
    }
    anyhow::ensure!(
        !scripts.is_empty(),
        "container-host request has no host plan"
    );
    run_reviewed_container_script(&scripts, paths, run)
}

/// Run the reviewed container-host script with its rendered host files
/// captured first. The script is fail-fast but not itself transactional:
/// when it stops, every captured file it changed is put back (or removed
/// when it was new) before the error is returned. Public only so the
/// rollback contract is testable without a real host.
#[doc(hidden)]
pub fn run_reviewed_container_script(
    scripts: &[String],
    paths: &[PathBuf],
    mut run: impl FnMut(&str) -> Result<()>,
) -> Result<Value> {
    let mut recovery = crate::Recovery::capture(paths.iter().cloned())?;
    let result = (|| -> Result<()> {
        for script in scripts {
            run(script)?;
        }
        Ok(())
    })();
    if result.is_err() {
        // The script wrote an unknown prefix of its files. Mark them all
        // as this operation's state so recovery restores each one.
        for path in paths {
            let _ = recovery.mark_after(path);
        }
    }
    recovery
        .finish(result)
        .map_err(|error| {
            error.context("accounts, linger, subordinate id ranges, packages and units the script enabled are not reversed; inspect them before retrying")
        })?;
    Ok(
        json!({"ok":true,"status":"host-script-applied","applied":true,"service_running":Value::Null,
        "rollback":"on failure the script's rendered host files are restored",
        "note":"The reviewed host script exited successfully. This is not a container/runtime health check; PDS, media, pool and gateway setup remain separate."}),
    )
}
