//! Named profile plans: the typed, reviewable statement of what one
//! profile run does, and the runner that executes the unprivileged half.
//!
//! THE THREE RULES THIS MODULE ENFORCES (PLAN-plugins-installer.md
//! Phase 8, `R11`):
//!
//! 1. **A profile never grows its own executor.** Unprivileged work is
//!    re-expressed in terms of what the foundation already executes
//!    ([`Action`]) or the existing services
//!    ([`veles_core::setup::apply_with`], [`InstallService`]). The
//!    component step carries the `plan_sig` that
//!    `InstallService::install_component` already refuses to run stale.
//! 2. **Every privileged side effect is reviewable.** The unprivileged
//!    [`Runner`] never executes a system step. Native system requests
//!    revalidate under lock in the explicit CLI handoff; the typed
//!    container-host builder supplies its own separately reviewed script.
//! 3. **A partial plan says so.** The runner refuses an incomplete plan
//!    instead of half-applying it. Container-host prepares only its typed
//!    confined host plan; PDS, media and pool components remain separate.
//!    `nixos` EMITS a module snippet and never edits `/etc/nixos` — a rule
//!    [`ProfilePlan::validate`] enforces against the plan itself, so no
//!    future frontend can reintroduce it by accident.
//!
//! Fail-fast is structural: the script is `set -eu`, no rendered line
//! may carry a self-escalation, and nothing is retried. Everything the
//! runner writes goes under ONE caller-supplied root, so a test can
//! exercise the whole sequence with the host untouched.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Component as PathComponent, Path, PathBuf};

use crate::{Action, ComponentSpec, InstallService, Status};

/// The host's own declarations. The installer NEVER writes under it;
/// the nixos profile prints a snippet instead.
const NIXOS_DIR: &str = "/etc/nixos";

/// The unit name `veles gateway install` already owns.
const GATEWAY_UNIT: &str = "veles-gateway";

// ── the profile choice ───────────────────────────────────────────────

/// The profile the operator picked. Distinct from [`crate::Profile`]
/// (the `install.toml` record) only in naming: Phase 8's "workstation"
/// is this "desktop". [`NamedProfile::record`] is that mapping, in one
/// place, so a record written by one profile is read back by the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NamedProfile {
    /// Native install, user unit, desktop — no root action at all.
    #[default]
    Desktop,
    /// Native install, gateway unit for an explicit user.
    Server,
    /// The generalised confined container-host plan. Gateway application
    /// components remain separately reviewed installations.
    ContainerHost,
    /// Emit `configuration.nix` text; apply nothing.
    Nixos,
}

impl NamedProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Server => "server",
            Self::ContainerHost => "container-host",
            Self::Nixos => "nixos",
        }
    }

    /// The `install.toml` spelling. `desktop` records as `workstation`,
    /// the name the record has carried since it was introduced:
    /// renaming the variant must not strand an existing record.
    pub fn record(self) -> crate::Profile {
        match self {
            Self::Desktop => crate::Profile::Workstation,
            Self::Server => crate::Profile::Server,
            Self::ContainerHost => crate::Profile::ContainerHost,
            Self::Nixos => crate::Profile::Nixos,
        }
    }

    pub fn from_record(profile: crate::Profile) -> Self {
        match profile {
            crate::Profile::Workstation => Self::Desktop,
            crate::Profile::Server => Self::Server,
            crate::Profile::ContainerHost => Self::ContainerHost,
            crate::Profile::Nixos => Self::Nixos,
        }
    }

    /// What this profile does NOT do yet. Empty means the plan is a
    /// complete statement of the profile; anything here makes
    /// [`ProfilePlan::complete`] false and the runner refuse.
    fn plan_gaps(self) -> &'static [&'static str] {
        match self {
            Self::Desktop | Self::Server | Self::ContainerHost | Self::Nixos => &[],
        }
    }
}

// ── the host the plan is for ─────────────────────────────────────────

/// The platform shape a plan is computed for. A caller decision, not a
/// runtime probe, so a cross-target plan stays possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Target {
    Linux,
    FreeBsd,
    Nixos,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::FreeBsd => "freebsd",
            Self::Nixos => "nixos",
        }
    }
}

/// The absolute paths a plan is computed against. Both halves are
/// explicit so a test can aim the whole thing at a temporary root.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostLayout {
    pub target: Target,
    /// The installed binary the unit runs.
    pub bin: PathBuf,
    /// The account the gateway runs as, when the profile names one.
    pub user: Option<String>,
    /// Optional system-service group; no same-named group is assumed.
    #[serde(default)]
    pub group: Option<String>,
    /// `config.toml`, `.env` and the user unit live here.
    pub config_dir: PathBuf,
    /// Component state and the managed installation record live here.
    /// System services use one VELES_HOME for both config and data.
    pub data_dir: PathBuf,
    /// System service rather than a per-user unit.
    #[serde(default)]
    pub system_service: bool,
    /// Local root for unprivileged writes. Logical absolute host paths
    /// outside it are staged below <write_root>/root/.
    pub write_root: PathBuf,
    /// Per-user unit directory, normally `$XDG_CONFIG_HOME/systemd/user`.
    /// Kept separate from veles' own config directory.
    #[serde(default = "default_user_unit_dir")]
    pub user_unit_dir: PathBuf,
    /// System unit directory (Linux).
    pub system_unit_dir: PathBuf,
    /// `/usr/local/etc/rc.d` (FreeBSD).
    pub rc_dir: PathBuf,
    /// The REVIEWED rc.d script to install — a source path, never a
    /// regenerated body: `scripts/install/units/veles.rc` is the one
    /// that has been read and tested.
    pub rc_source: Option<PathBuf>,
}

fn default_user_unit_dir() -> PathBuf {
    PathBuf::from("/tmp/veles-invalid-user-unit-root")
}

impl HostLayout {
    /// The one supported native Linux system-service layout. Callers may
    /// stage it under another write_root, but the logical target is fixed.
    pub fn linux_system(home: PathBuf, user: String, group: Option<String>) -> Self {
        Self {
            target: Target::Linux,
            bin: home.join("install/current/bin/veles"),
            user: Some(user),
            group,
            config_dir: home.clone(),
            data_dir: home.clone(),
            system_service: true,
            write_root: home.clone(),
            user_unit_dir: home.join(".config/systemd/user"),
            system_unit_dir: PathBuf::from("/etc/systemd/system"),
            rc_dir: PathBuf::from("/usr/local/etc/rc.d"),
            rc_source: None,
        }
    }

    pub fn local(&self, path: &Path) -> PathBuf {
        if path.is_relative() || path.starts_with(&self.write_root) {
            return path.to_path_buf();
        }
        self.write_root
            .join("root")
            .join(path.strip_prefix("/").unwrap_or(path))
    }
    pub fn local_config_dir(&self) -> PathBuf {
        self.local(&self.config_dir)
    }
    pub fn local_data_dir(&self) -> PathBuf {
        self.local(&self.data_dir)
    }
    pub fn local_config_path(&self) -> PathBuf {
        self.local_config_dir().join("config.toml")
    }
    pub fn local_user_unit(&self) -> PathBuf {
        self.local(&self.user_unit())
    }
    fn user_unit(&self) -> PathBuf {
        self.user_unit_dir.join(format!("{GATEWAY_UNIT}.service"))
    }
    fn config(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    /// The account the gateway runs as. A profile that needs one
    /// refuses rather than guessing root.
    fn named_user(&self) -> Result<&str> {
        let user = self
            .user
            .as_deref()
            .context("this profile needs an explicit user to run the gateway as")?;
        crate::valid_component_name(user)
            .with_context(|| format!("invalid gateway user {user:?}"))?;
        Ok(user)
    }
}

// ── unprivileged steps ───────────────────────────────────────────────

/// Per-step tool configuration that is not part of the payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ToolConfig {
    /// `Some(user)`: the step is for that account's own configuration.
    pub as_user: Option<String>,
}

/// One unprivileged step. Each is reviewable on its own; the privileged
/// half lives in [`ProfilePlan::system`] and never appears here.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Step {
    /// The setup plan, applied through `veles_core::setup::apply_with`:
    /// one config write, one `.env` merge, no second wizard.
    Config {
        path: PathBuf,
        /// sha256 of `setup.config_toml`: the review shows the exact
        /// bytes, and the runner proves the carried plan is the one that
        /// was reviewed.
        sha256: String,
        mode: Option<u32>,
        setup: veles_core::setup::SetupPlan,
    },
    /// A local regular-file write, executed by the foundation's own
    /// [`Action`]: idempotent, symlink-refusing, owner-only while temp.
    UserFile {
        path: PathBuf,
        contents: Vec<u8>,
        mode: Option<u32>,
        expected_sha256: Option<String>,
    },
    /// A reviewed native core bundle, installed through the existing
    /// bundle lifecycle rather than a binary copied by the profile.
    CoreBundle { source: PathBuf, plan_sig: String },
    /// Anything the foundation already plans and executes unchanged.
    /// Kept as the action itself, never flattened into shell text.
    Action(Action),
    /// Install one reviewed component through
    /// `InstallService::install_component`, whose `plan_sig` gate is the
    /// only thing that may bless an artifact.
    Component {
        name: String,
        /// The exact spec and artifact identity reviewed by the caller.
        /// The runner never rediscovers a different component by name.
        spec: Box<ComponentSpec>,
        plan_sig: String,
        version: String,
        tool: ToolConfig,
    },
}

impl serde::Serialize for Step {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Config {
                path, sha256, mode, ..
            } => ("config", path, sha256, mode).serialize(s),
            Self::UserFile {
                path,
                contents,
                mode,
                expected_sha256,
            } => ("file", path, crate::sha256(contents), mode, expected_sha256).serialize(s),
            Self::CoreBundle { source, plan_sig } => ("core", source, plan_sig).serialize(s),
            Self::Action(action) => ("action", action).serialize(s),
            Self::Component {
                name,
                spec,
                plan_sig,
                version,
                tool,
            } => ("component", name, spec, plan_sig, version, tool).serialize(s),
        }
    }
}

impl Step {
    pub fn describe(&self) -> String {
        match self {
            Self::CoreBundle { source, .. } => {
                format!("install reviewed core bundle {}", source.display())
            }
            Self::Config { path, sha256, .. } => format!(
                "apply the reviewed setup plan to {} (sha256 {sha256})",
                path.display()
            ),
            Self::UserFile {
                path,
                contents,
                mode,
                ..
            } => format!(
                "write {} ({} bytes{})",
                path.display(),
                contents.len(),
                mode.map(|m| format!(", mode {m:o}")).unwrap_or_default()
            ),
            Self::Action(action) => action.describe(),
            Self::Component {
                name,
                version,
                tool,
                ..
            } => format!(
                "install component {name} {version}{}",
                tool.as_user
                    .as_deref()
                    .map(|u| format!(" as {u}"))
                    .unwrap_or_default()
            ),
        }
    }
}

// ── privileged steps ─────────────────────────────────────────────────

/// One privileged side effect. ALL of them render into ONE script; the
/// planner and the runner both refuse to run any of them, and
/// [`ProfilePlan::validate`] refuses the shapes that would make the
/// script self-escalating or non-fail-fast.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemStep {
    /// Complete script from the typed container-host builder.
    EmbeddedScript {
        text: String,
        sha256: String,
        reason: String,
    },
    /// One argv, no shell. Rendered word by word.
    Command { argv: Vec<String>, reason: String },
    /// A root-installed file. `source` installs a reviewed artifact from
    /// the repo; without one the contents are rendered inline through a
    /// temporary file.
    File {
        destination: PathBuf,
        contents: String,
        mode: u32,
        source: Option<PathBuf>,
    },
    /// An exact symlink, never a replacement of a foreign file or link.
    Link { target: PathBuf, link: PathBuf },
}

impl SystemStep {
    /// Execute the typed link form only. Other system steps remain in
    /// their existing profile/native/container executors.
    pub fn apply_link(&self, root: &Path) -> Result<()> {
        let Self::Link { target, link } = self else {
            bail!("only a typed link can use this executor")
        };
        anyhow::ensure!(
            target.is_absolute() && normalized(target) && link.is_absolute() && normalized(link)
        );
        let full = root.join(link.strip_prefix("/")?);
        let parent = full.parent().context("system link needs a parent")?;
        if !parent.exists() {
            crate::ownership::ensure_dir(parent, 0o755, &crate::ownership::process_account())?;
        } else {
            let meta = std::fs::symlink_metadata(parent)?;
            anyhow::ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "system link parent is not a real directory"
            );
        }
        if let Ok(meta) = std::fs::symlink_metadata(&full) {
            anyhow::ensure!(
                meta.file_type().is_symlink() && std::fs::read_link(&full)? == *target,
                "system link already exists with different contents: {}",
                full.display()
            );
            return Ok(());
        }
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, &full)
            .with_context(|| format!("creating reviewed link {}", full.display()));
        #[cfg(not(unix))]
        bail!("reviewed host links are Linux-only: {}", full.display())
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Link { target, link } => {
                format!("link {} -> {}", link.display(), target.display())
            }
            Self::EmbeddedScript { reason, sha256, .. } => format!("{reason} (sha256 {sha256})"),
            Self::Command { argv, reason } => format!("{} ({reason})", argv.join(" ")),
            Self::File {
                destination,
                mode,
                source,
                ..
            } => format!(
                "install {} (mode {mode:o})",
                source
                    .as_ref()
                    .map(|s| format!("{} -> {}", s.display(), destination.display()))
                    .unwrap_or_else(|| destination.display().to_string())
            ),
        }
    }
}

/// A file the plan EMITS for a frontend to show or save. The nixos
/// profile produces one; nothing writes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EmittedFile {
    pub name: String,
    pub contents: String,
    pub sha256: String,
}

/// The whole reviewable statement of one profile run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProfilePlan {
    pub profile: NamedProfile,
    pub target: Target,
    /// Ordered; the runner executes them in this order and stops at the
    /// first failure.
    pub user: Vec<Step>,
    /// Ordered; rendered into [`ProfilePlan::script`] and executed by
    /// nobody here.
    pub system: Vec<SystemStep>,
    /// Emitted text (the nixos module snippet).
    pub files: Vec<EmittedFile>,
    /// Named gaps. Non-empty implies `complete == false`.
    pub unsupported: Vec<String>,
    /// What the operator is told, verbatim, including the enable line
    /// this installer deliberately does not type for them.
    pub notes: Vec<String>,
    /// False when the profile is not implemented yet: the runner refuses
    /// an incomplete plan rather than applying half of one.
    pub complete: bool,
}

/// The one reviewed privileged document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub text: String,
    /// sha256 of `text`. A frontend shows this, and the operator can
    /// compare it against the file they read.
    pub sha256: String,
    /// The steps, in rendered order. A non-empty `text` is not proof of
    /// anything; this is what was actually put in it.
    pub steps: Vec<SystemStep>,
}

/// Everything [`plan`] needs. Explicit rather than ambient, so a test
/// aims it at a temporary root.
pub struct ProfileInput<'a> {
    pub profile: NamedProfile,
    pub host: &'a HostLayout,
    pub setup: &'a veles_core::setup::SetupPlan,
    pub components: &'a [ComponentSpec],
    /// An optional foundation plan (action.rs) whose actions are folded
    /// in. Only `privileged_script: None` is accepted: the profile owns
    /// the single script, and a second block of shell text would be a
    /// second, unreviewed privileged path.
    pub plan: Option<&'a crate::Plan>,
}

// ── planning ─────────────────────────────────────────────────────────

/// Compute the plan for one profile. Pure: reads component artifacts to
/// derive their signatures, writes nothing.
pub fn plan(input: &ProfileInput<'_>) -> Result<ProfilePlan> {
    if input.profile != NamedProfile::Nixos && input.host.target == Target::Nixos {
        bail!(
            "this is a NixOS host: install through the flake's services.veles module (the nixos profile emits the snippet) instead of a native {} install",
            input.profile.as_str()
        );
    }
    if let Some(foundation) = input.plan {
        if foundation.privileged_script.is_some() {
            bail!(
                "the supplied plan already carries a privileged script; the profile owns the one reviewed script and will not adopt a second block of shell text"
            );
        }
    }
    let mut out = ProfilePlan {
        profile: input.profile,
        target: input.host.target,
        user: Vec::new(),
        system: Vec::new(),
        files: Vec::new(),
        unsupported: input
            .profile
            .plan_gaps()
            .iter()
            .map(|gap| (*gap).to_string())
            .collect(),
        notes: Vec::new(),
        complete: input.profile.plan_gaps().is_empty(),
    };
    for spec in input.components {
        if let Some(error) = &spec.describe_error {
            bail!(
                "component {} could not be described ({error}); it cannot be planned until it answers --describe",
                spec.name
            );
        }
        crate::valid_component_name(&spec.name)?;
        crate::valid_version(&spec.manifest.version)
            .with_context(|| format!("component {} version", spec.name))?;
    }
    // A foundation plan supplied alongside the emitting profile would be
    // silently dropped by the early return below. Refuse it instead: a
    // caller that passed one expects it to be applied, and "nothing was
    // applied" must mean nothing was ASKED for either.
    if input.plan.is_some() && input.profile == NamedProfile::Nixos {
        bail!(
            "the nixos profile emits a module snippet and applies nothing, so it cannot fold in a supplied action plan"
        );
    }
    match input.profile {
        NamedProfile::Nixos => {
            let snippet = module_snippet(input);
            out.files.push(EmittedFile {
                name: format!("veles-{}.nix", input.host.target.as_str()),
                sha256: crate::sha256(snippet.as_bytes()),
                contents: snippet,
            });
            out.notes.push(format!(
                "nothing was applied: put the snippet in {NIXOS_DIR}/configuration.nix (or a file it imports) and run `nixos-rebuild switch` yourself. This installer never edits {NIXOS_DIR}."
            ));
            out.notes.push(
                "config.toml is NOT translated into services.veles.settings automatically; paste the sections you need, and keep secrets in services.veles.environmentFile."
                    .into(),
            );
            // The emitting profile returns before the profile-agnostic
            // checks at the bottom of this function, so it validates its
            // own shape here rather than skipping them.
            out.validate()?;
            return Ok(out);
        }
        NamedProfile::Desktop => {
            setup_steps(input, &mut out)?;
            out.notes.push(
                "no root action is needed: the gateway runs as you, from your own config dir."
                    .into(),
            );
        }
        NamedProfile::Server => {
            setup_steps(input, &mut out)?;
            server_steps(input, &mut out)?;
        }
        NamedProfile::ContainerHost => {
            // The typed container builder owns the host plan. Native
            // config, components, linger and installation records belong
            // to the gateway installation, not this host preparation.
            out.notes.push("Only the typed confined host plan is prepared. Gateway configuration, PDS, media and pool components are separately reviewed work; no native install record is written.".into());
            // ProfileRequest attaches the typed host script after this
            // planner returns; validation belongs after that attachment.
            anyhow::ensure!(
                input.plan.is_none(),
                "container-host accepts typed container options, not a second action plan"
            );
            return Ok(out);
        }
    }
    if let Some(foundation) = input.plan {
        fold_foundation(input, foundation, &mut out)?;
    }
    out.validate()?;
    Ok(out)
}

/// The setup plan and the selected components: identical on every native
/// profile, which is the point of having one profile concept.
fn setup_steps(input: &ProfileInput<'_>, out: &mut ProfilePlan) -> Result<()> {
    out.user.push(Step::Config {
        path: input.host.local_config_path(),
        sha256: crate::sha256(input.setup.config_toml.as_bytes()),
        mode: Some(0o600),
        setup: input.setup.clone(),
    });
    for spec in input.components {
        out.user.push(Step::Component {
            name: spec.name.clone(),
            spec: Box::new(spec.clone()),
            plan_sig: component_signature(spec)?,
            version: spec.manifest.version.clone(),
            tool: ToolConfig::default(),
        });
    }
    Ok(())
}

/// The same signature `InstallService::preview_component` produces, so
/// the run is refused if the artifact changed after review:
/// `sha256(serde_json(spec, sha256(artifact bytes)))`.
fn component_signature(spec: &ComponentSpec) -> Result<String> {
    let source = spec
        .source
        .as_ref()
        .context("component has no artifact to install")?;
    let bytes = std::fs::read(source)
        .with_context(|| format!("reading reviewed component {}", source.display()))?;
    Ok(crate::sha256(&serde_json::to_vec(&(
        spec,
        crate::sha256(&bytes),
    ))?))
}

fn server_steps(input: &ProfileInput<'_>, out: &mut ProfilePlan) -> Result<()> {
    match input.host.target {
        Target::FreeBsd => {
            let source = input.host.rc_source.clone().context(
                "the FreeBSD profile installs the reviewed rc.d script; no source path was given",
            )?;
            out.system.push(SystemStep::File {
                destination: input.host.rc_dir.join("veles"),
                mode: 0o555,
                contents: String::new(),
                source: Some(source),
            });
            out.notes.push(
                "enable it yourself: `sysrc veles_enable=YES && service veles start` (this installer does not type it)."
                    .into(),
            );
            return Ok(());
        }
        Target::Nixos => unreachable!("a nixos host was refused before this point"),
        Target::Linux => {}
    }
    let user = input.host.named_user()?.to_string();
    if input.host.system_service {
        let unit = system_unit(input.host)?;
        out.user.push(Step::UserFile {
            path: input.host.local(
                &input
                    .host
                    .system_unit_dir
                    .join(format!("{GATEWAY_UNIT}.service")),
            ),
            contents: unit.clone().into_bytes(),
            mode: Some(0o644),
            expected_sha256: None,
        });
        out.system.push(SystemStep::File {
            destination: input
                .host
                .system_unit_dir
                .join(format!("{GATEWAY_UNIT}.service")),
            contents: unit.clone(),
            mode: 0o644,
            source: Some(
                input.host.local(
                    &input
                        .host
                        .system_unit_dir
                        .join(format!("{GATEWAY_UNIT}.service")),
                ),
            ),
        });
        out.system.push(SystemStep::Command {
            argv: vec!["id".into(), "-u".into(), user.clone()],
            reason: "the explicitly named service account must already exist".into(),
        });
        out.notes.push(format!("the system unit is NOT enabled: `systemctl enable --now {GATEWAY_UNIT}` as root. No service was started."));
        return Ok(());
    }
    // A user unit survives logout only with linger, which is a root
    // action. It is ONE explicit command, and the unit is NOT enabled:
    // enable is the operator's line to type, printed in the notes.
    out.user.push(Step::UserFile {
        path: input.host.local_user_unit(),
        contents: user_unit(input.host)?.into_bytes(),
        mode: Some(0o644),
        expected_sha256: None,
    });
    out.system.push(SystemStep::Command {
        argv: vec!["loginctl".into(), "enable-linger".into(), user.clone()],
        reason: format!("keep {user}'s systemd user manager alive across logouts"),
    });
    out.notes.push(format!(
        "the unit is installed but NOT enabled: `systemctl --user enable --now {GATEWAY_UNIT}` (this installer does not type it)."
    ));
    Ok(())
}

/// The gateway unit, rendered from the host layout. User scope is
/// deliberate: it is the same unit `veles gateway install` writes, so
/// the installer and the CLI cannot disagree about where it lives.
fn user_unit(host: &HostLayout) -> Result<String> {
    let bin = host
        .bin
        .to_str()
        .context("the install prefix is not valid UTF-8")?;
    let config_dir = host
        .config_dir
        .to_str()
        .context("the config dir is not valid UTF-8")?;
    let mut unit = String::from(
        "# veles gateway — user unit. Written by `veles install`.\n\
         [Unit]\n\
         Description=Veles Agent gateway\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         StartLimitIntervalSec=120\n\
         StartLimitBurst=5\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=",
    );
    unit.push_str(bin);
    unit.push_str(
        " gateway run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         KillSignal=SIGTERM\n\
         TimeoutStopSec=30\n\
         Environment=LANG=C.UTF-8\n\
         Environment=LC_ALL=C.UTF-8\n\
         EnvironmentFile=-",
    );
    unit.push_str(config_dir);
    unit.push_str(
        "/.env\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    );
    Ok(unit)
}

fn system_unit(host: &HostLayout) -> Result<String> {
    let user = host.named_user()?;
    let home = host
        .data_dir
        .to_str()
        .context("service VELES_HOME is not UTF-8")?;
    let bin = host.bin.to_str().context("service binary is not UTF-8")?;
    let group = match &host.group {
        Some(group) => {
            crate::valid_component_name(group).context("invalid gateway group")?;
            format!("Group={group}\n")
        }
        None => String::new(),
    };
    for (label, value) in [("VELES_HOME", home), ("binary", bin)] {
        anyhow::ensure!(
            !value
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '\n' | '\r' | '"' | '%')),
            "system service {label} contains whitespace or a specifier"
        );
    }
    Ok(format!("# veles gateway — reviewed system unit; activation is separate.\n[Unit]\nDescription=Veles Agent gateway\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nUser={user}\n{group}WorkingDirectory={home}\nEnvironment=VELES_HOME={home}\nEnvironment=LANG=C.UTF-8\nEnvironment=LC_ALL=C.UTF-8\nEnvironmentFile=-{home}/.env\nExecStart={bin} gateway run\nRestart=on-failure\nRestartSec=5\nKillSignal=SIGTERM\nTimeoutStopSec=30\n\n[Install]\nWantedBy=multi-user.target\n", user=user, home=home, bin=bin))
}

/// Fold a foundation plan in: unprivileged work becomes actions (which
/// the foundation already executes idempotently), privileged work goes
/// into the one script, and anything with no executor is NAMED instead
/// of silently dropped.
fn fold_foundation(
    input: &ProfileInput<'_>,
    foundation: &crate::Plan,
    out: &mut ProfilePlan,
) -> Result<()> {
    for action in &foundation.actions {
        match action {
            Action::Command {
                argv,
                as_user,
                privileged,
            } => {
                anyhow::ensure!(!argv.is_empty(), "a planned command has no argv");
                if *privileged {
                    out.system.push(SystemStep::Command {
                        argv: argv.clone(),
                        reason: as_user
                            .as_deref()
                            .map(|u| format!("privileged command as {u}"))
                            .unwrap_or_else(|| "privileged command".into()),
                    });
                } else {
                    out.user.push(Step::Action(action.clone()));
                }
            }
            Action::SystemdUnit {
                name,
                contents,
                privileged,
            } => {
                crate::valid_component_name(name)?;
                let name = if name.ends_with(".service") {
                    name.clone()
                } else {
                    format!("{name}.service")
                };
                if *privileged {
                    out.system.push(SystemStep::File {
                        destination: input.host.system_unit_dir.join(&name),
                        contents: contents.clone(),
                        mode: 0o644,
                        source: None,
                    });
                } else {
                    out.user.push(Step::UserFile {
                        path: input.host.config_dir.join("systemd/user").join(&name),
                        contents: contents.clone().into_bytes(),
                        mode: Some(0o644),
                        expected_sha256: None,
                    });
                }
            }
            Action::WriteFile { .. } => out.user.push(Step::Action(action.clone())),
            other => {
                // Package, Quadlet, Image, Download, ConfigEdit, Secret:
                // each needs a package manager, an image store or the
                // vault, and none of those has a profile executor yet.
                // Name it — never flatten it into shell text and call it
                // planned.
                out.unsupported
                    .push(format!("no profile executor for: {}", other.describe()));
                out.complete = false;
            }
        }
    }
    Ok(())
}

/// `configuration.nix` text for the nixos profile. Emitted, never
/// applied, and deliberately not a translation of `config.toml`: a
/// half-translated settings attrset would look authoritative and be
/// wrong about where secrets belong.
fn module_snippet(input: &ProfileInput<'_>) -> String {
    let user = input.host.user.as_deref().unwrap_or("veles");
    let config = input.host.config();
    let mut text = String::from(
        "# veles — generated by `veles install` (nixos profile).\n\
         #\n\
         # REVIEW this, then put it in ",
    );
    text.push_str(NIXOS_DIR);
    text.push_str("/configuration.nix (or a file it imports) and run\n# `nixos-rebuild switch` yourself. The installer does not edit ");
    text.push_str(NIXOS_DIR);
    text.push_str(" and does not run nixos-rebuild.\n{\n  services.veles = {\n    enable = true;\n    scope = \"system\";\n    user = \"");
    text.push_str(user);
    text.push_str("\";\n    # settings = the [model] / [platforms] sections of\n    #   ");
    text.push_str(&config.display().to_string());
    text.push_str(
        "\n    # API keys and tokens live OUTSIDE the store, in this file (create it,\n    # mode 0600, with e.g. OPENAI_API_KEY=...). The module refuses a system\n    # gateway with neither settings nor an environment file.\n    environmentFile = \"/var/lib/secrets/veles.env\";\n    # The vault defaults to /var/lib/veles/vault (system scope). To keep\n    # the node keys on persistent storage instead:\n    #   vault.identityDir = \"/persist/veles/vault\";\n  };\n}\n\
         #\n\
         # The module's units are static (NixOS generates them): start one with\n\
         #   systemctl start veles-gateway\n\
         # and follow services.veles.gateway.enable for boot behaviour —\n\
         # NOT `systemctl enable`, which NixOS shadows rather than enables.\n",
    );
    text
}

// ── validation ───────────────────────────────────────────────────────

/// Programs the script may never invoke: a script that escalates is a
/// second, unreviewed door.
const SELF_ESCALATORS: &[&str] = &["sudo", "doas", "pkexec", "su", "runuser"];

/// Verbs that make a service start or become enabled. Both are the
/// operator's lines to type, never the installer's.
const ENABLERS: &[&str] = &[
    "enable",
    "enable-now",
    "--now",
    "start",
    "restart",
    "reload-or-restart",
];

/// No empty path, no `.`/`..`/prefix component. Components are never
/// resolved, so there is nothing for a later join to reinterpret.
fn normalized(path: &Path) -> bool {
    // A Windows drive prefix (`C:`) is part of an absolute path there; the
    // desktop profile runs on Windows too.
    !path.as_os_str().is_empty()
        && path.components().all(|c| {
            matches!(
                c,
                PathComponent::Prefix(_) | PathComponent::RootDir | PathComponent::Normal(_)
            )
        })
}

fn under_nixos(path: &Path) -> bool {
    path.starts_with(NIXOS_DIR)
}

impl ProfilePlan {
    /// The plan's own consistency check. Called by [`plan`], by
    /// [`ProfilePlan::script`] and by [`Runner::run`], so no frontend can
    /// assemble a plan that skips it.
    ///
    /// The `/etc/nixos` refusal is checked before the profile's own
    /// shape: "the nixos profile applies nothing" and "nothing writes
    /// host declarations" are different rules, and a frontend that broke
    /// the second one deserves the specific error about it.
    pub fn validate(&self) -> Result<()> {
        if !self.unsupported.is_empty() && self.complete {
            bail!(
                "a plan with named gaps cannot claim completeness: {:?}",
                self.unsupported
            );
        }
        if self.target == Target::Nixos && self.profile != NamedProfile::Nixos {
            bail!("a nixos host has no native profile plan");
        }
        for step in &self.user {
            let path = match step {
                Step::Config { path, sha256, .. } => {
                    anyhow::ensure!(
                        crate::valid_hash(sha256),
                        "the config step carries an invalid digest"
                    );
                    path
                }
                Step::UserFile { path, .. } => path,
                Step::Action(Action::WriteFile { path, .. }) => path,
                Step::CoreBundle { source, plan_sig } => {
                    anyhow::ensure!(
                        source.is_absolute() && normalized(source) && crate::valid_hash(plan_sig),
                        "invalid reviewed core bundle step"
                    );
                    continue;
                }
                Step::Action(other) => bail!(
                    "{} has no user-side executor in a profile plan",
                    other.describe()
                ),
                Step::Component { name, plan_sig, .. } => {
                    crate::valid_component_name(name)?;
                    anyhow::ensure!(
                        crate::valid_hash(plan_sig),
                        "component {name} carries an invalid plan signature"
                    );
                    continue;
                }
            };
            anyhow::ensure!(
                !under_nixos(path),
                "refusing to plan a write under {NIXOS_DIR} ({})",
                path.display()
            );
            // A relative path is an action path (the foundation's own
            // convention: relative to the install root). An absolute one
            // must be normalized, so no `..` survives to be resolved
            // later against a different root.
            anyhow::ensure!(
                normalized(path),
                "user step path {} is not normalized",
                path.display()
            );
        }
        for step in &self.system {
            match step {
                SystemStep::EmbeddedScript { text, sha256, .. } => {
                    anyhow::ensure!(
                        text.starts_with("#!/bin/sh\n") && text.contains("\nset -eu\n"),
                        "embedded script must be a fail-fast /bin/sh document"
                    );
                    anyhow::ensure!(
                        crate::sha256(text.as_bytes()) == *sha256,
                        "embedded script changed after review"
                    );
                    if self.profile != NamedProfile::ContainerHost {
                        anyhow::ensure!(
                            !text.lines().any(|line| SELF_ESCALATORS
                                .contains(&line.split_whitespace().next().unwrap_or(""))),
                            "embedded script must not escalate itself"
                        );
                    }
                }
                SystemStep::Command { argv, .. } => {
                    let Some(program) = argv.first() else {
                        bail!("a planned privileged command has no argv");
                    };
                    let program = Path::new(program)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| program.clone());
                    anyhow::ensure!(
                        !SELF_ESCALATORS.contains(&program.as_str()),
                        "the reviewed script must not escalate itself: {program}"
                    );
                    anyhow::ensure!(
                        !argv
                            .iter()
                            .any(|word| ENABLERS.contains(&word.as_str())),
                        "enabling or starting a service is the operator's line to type, not the installer's: {}",
                        argv.join(" ")
                    );
                }
                SystemStep::Link { target, link } => {
                    anyhow::ensure!(
                        target.is_absolute()
                            && normalized(target)
                            && link.is_absolute()
                            && normalized(link)
                            && !under_nixos(target)
                            && !under_nixos(link),
                        "a system link needs absolute normalized non-NixOS paths"
                    );
                }
                SystemStep::File {
                    destination,
                    contents: _,
                    mode,
                    source,
                } => {
                    anyhow::ensure!(
                        destination.is_absolute() && normalized(destination),
                        "a system file destination must be an absolute normalized path: {}",
                        destination.display()
                    );
                    anyhow::ensure!(
                        !under_nixos(destination),
                        "refusing to plan a write under {NIXOS_DIR} ({})",
                        destination.display()
                    );
                    anyhow::ensure!(
                        *mode & !0o7777 == 0,
                        "invalid mode {mode:o} for {}",
                        destination.display()
                    );
                    if let Some(source) = source {
                        anyhow::ensure!(
                            source.is_absolute() && normalized(source),
                            "an install source must be an absolute normalized path: {}",
                            source.display()
                        );
                    }
                }
            }
        }
        if self.profile == NamedProfile::ContainerHost {
            anyhow::ensure!(
                self.user.is_empty(),
                "container-host must not infer native config, core or component installation"
            );
            anyhow::ensure!(
                !self.system.is_empty()
                    && self
                        .system
                        .iter()
                        .all(|s| matches!(s, SystemStep::EmbeddedScript { .. })),
                "container-host carries only its typed host script"
            );
            anyhow::ensure!(
                self.files.is_empty(),
                "container-host is not the NixOS emitter"
            );
        }
        if self.profile == NamedProfile::Nixos {
            if !self.user.is_empty() || !self.system.is_empty() {
                bail!("the nixos profile emits a module snippet; it applies nothing");
            }
            if self.files.is_empty() {
                bail!("the nixos profile must emit the module snippet, and emitted nothing");
            }
        }
        Ok(())
    }

    /// Render the ONE privileged script. `Ok(None)` means the plan has no
    /// privileged work — not that the work was dropped.
    pub fn script(&self) -> Result<Option<Script>> {
        self.validate()?;
        if self.system.is_empty() {
            return Ok(None);
        }
        let mut text = String::from(
            "#!/bin/sh\n\
             # veles installer — the reviewed privileged half of the plan.\n\
             #\n\
             # NOTHING here has run. Read every line, then run it yourself:\n\
             #   sudo sh <this file>\n\
             #\n\
             # set -eu: the first failure stops the script. Nothing is\n\
             # retried, and no error is ignored.\n\
             set -eu\n\n",
        );
        for (n, step) in self.system.iter().enumerate() {
            text.push_str(&format!(
                "# step {}: {}\n",
                n + 1,
                one_line(&step.describe())
            ));
            match step {
                SystemStep::EmbeddedScript { text: embedded, .. } => {
                    for line in embedded.lines().skip(1) {
                        if line.trim() == "set -eu" {
                            continue;
                        }
                        text.push_str(line);
                        text.push('\n');
                    }
                }
                SystemStep::Command { argv, .. } => {
                    let words = argv
                        .iter()
                        .map(|w| shell_word(w))
                        .collect::<Result<Vec<_>>>()?;
                    text.push_str(&words.join(" "));
                    text.push('\n');
                }
                SystemStep::Link { target, link } => {
                    let target = shell_word(&path_text(target))?;
                    let link = shell_word(&path_text(link))?;
                    text.push_str(&format!("if [ -L {link} ]; then [ \"$(readlink {link})\" = {target} ]; elif [ -e {link} ]; then exit 1; else ln -s {target} {link}; fi\n"));
                }
                SystemStep::File {
                    destination,
                    contents,
                    mode,
                    source,
                } => {
                    let parent = destination
                        .parent()
                        .context("a system file destination has no parent directory")?;
                    text.push_str(&format!("mkdir -p {}\n", shell_word(&path_text(parent))?));
                    let destination_text = shell_word(&path_text(destination))?;
                    match source {
                        Some(source) => {
                            let source_text = shell_word(&path_text(source))?;
                            let expected = if contents.is_empty() {
                                crate::sha256(&std::fs::read(source).with_context(|| {
                                    format!("reading reviewed source {}", source.display())
                                })?)
                            } else {
                                crate::sha256(contents.as_bytes())
                            };
                            // The staged source is data reviewed by the
                            // caller. Refuse changed bytes. A new target is
                            // installed no-clobber; an existing unit is an
                            // explicit reviewed replacement.
                            text.push_str(&format!("[ -f {source_text} ]\n"));
                            text.push_str(&format!(
                                "printf '%s  %s\\n' {} {} | sha256sum -c -\n",
                                shell_word(&expected)?,
                                shell_word(&path_text(source))?
                            ));
                            let temporary = shell_word(&format!(
                                "{}.veles-install-{}",
                                path_text(destination),
                                std::process::id()
                            ))?;
                            text.push_str(&format!("if [ -e {temporary} ]; then exit 1; fi\n"));
                            text.push_str(&format!(
                                "install -m {:04o} {source_text} {temporary}\n",
                                mode
                            ));
                            text.push_str(&format!("if [ -e {destination_text} ]; then mv -f {temporary} {destination_text}; else mv -n {temporary} {destination_text}; fi\n"));
                            text.push_str(&format!("[ ! -e {temporary} ]\n"));
                        }
                        None => {
                            // Interpolation would put the contents through a
                            // second round of quoting rules; a quoted heredoc
                            // takes the bytes literally.
                            let temporary = shell_word(&format!(
                                "{}.veles-install-{}",
                                path_text(destination),
                                std::process::id()
                            ))?;
                            text.push_str(&format!("cat > {temporary} <<'VELES_INSTALL_EOF'\n"));
                            text.push_str(contents);
                            if !contents.ends_with('\n') {
                                text.push('\n');
                            }
                            text.push_str("VELES_INSTALL_EOF\n");
                            text.push_str(&format!("chmod {:04o} {temporary}\n", mode));
                            text.push_str(&format!("mv -f {temporary} {destination_text}\n"));
                        }
                    }
                }
            }
            text.push('\n');
        }
        text.push_str("# Done. Nothing was enabled or started by the installer.\n");
        Ok(Some(Script {
            sha256: crate::sha256(text.as_bytes()),
            steps: self.system.clone(),
            text,
        }))
    }

    /// What a review screen shows. No secret value can reach it: the
    /// setup plan's `.env` template names variables, and the config body
    /// itself is reduced to a digest.
    pub fn review(&self) -> Value {
        let script = self.script().ok().flatten();
        json!({
            "profile": self.profile.as_str(),
            "record_profile": self.profile.record(),
            "target": self.target.as_str(),
            "complete": self.complete,
            "user_steps": self.user.iter().map(Step::describe).collect::<Vec<_>>(),
            "system_steps": self.system.iter().map(SystemStep::describe).collect::<Vec<_>>(),
            "emitted": self.files.iter().map(|f| json!({
                "name": f.name, "sha256": f.sha256, "bytes": f.contents.len(),
            })).collect::<Vec<_>>(),
            "privileged_script": script.as_ref().map(|s| json!({
                "sha256": s.sha256, "text": s.text, "steps": s.steps.len(),
            })).unwrap_or(Value::Null),
            "unsupported": self.unsupported,
            "notes": self.notes,
        })
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One shell word for /bin/sh.
///
/// The rule is a WHITELIST, so the output is safe because of what it
/// allows rather than because of what someone remembered to escape: a
/// word made only of `[A-Za-z0-9_./:@%+,=-]` renders bare and readable
/// (`loginctl enable-linger veles`), anything else is single-quoted.
/// `;`, `|`, `&`, `$`, backticks, `*`, `#`, a quote and a newline all
/// fall outside the set, so none of them can ever be a metacharacter.
/// A newline or NUL is refused rather than quoted: the script's
/// line-per-step structure is what makes it reviewable.
fn shell_word(word: &str) -> Result<String> {
    anyhow::ensure!(
        !word.contains('\n') && !word.contains('\0'),
        "refusing to render a multi-line shell word: {word:?}"
    );
    let bare = !word.is_empty()
        && !word.starts_with('=')
        && word.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'_' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b',' | b'-' | b'='
                )
        });
    if bare {
        Ok(word.to_string())
    } else {
        Ok(format!("'{}'", word.replace('\'', r"'\''")))
    }
}

// ── running ──────────────────────────────────────────────────────────

/// What happened to one step.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepResult {
    /// Applied now.
    Done,
    /// A dry run: it would have been applied.
    WouldDo,
    /// Nothing to do, with the reason.
    Skipped(String),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StepOutcome {
    /// 1-based, in execution order.
    pub index: usize,
    pub what: String,
    pub result: StepResult,
}

/// The result of one run. `script` is the privileged half, returned
/// UNEXECUTED so a frontend can save or print it.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub dry_run: bool,
    pub steps: Vec<StepOutcome>,
    pub script: Option<Script>,
    /// Emitted file names. Nothing wrote them; they are review output.
    pub emitted: Vec<String>,
}

/// Executes unprivileged steps, in order, under ONE root, and stops at
/// the first failure.
///
/// It CANNOT execute a [`SystemStep`]: those are rendered into the
/// reviewed script and returned, and no method here shells out. That is
/// the whole privilege model, and there is no flag that turns it off.
#[derive(Debug, Clone)]
pub struct Runner {
    root: PathBuf,
    dry_run: bool,
    service: Option<InstallService>,
}

impl Runner {
    /// `root` is the only directory this runner may write under. Every
    /// step destination is checked against it BEFORE anything happens,
    /// so a plan assembled by hand cannot escape it.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            dry_run: false,
            service: None,
        }
    }
    /// Nothing is written or executed; the sequence is still computed.
    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }
    /// The installer service the component steps go through. Absent, a
    /// component step is an error rather than an improvised install.
    pub fn service(mut self, service: InstallService) -> Self {
        self.service = Some(service);
        self
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The absolute destination of a step. A foundation action's relative
    /// path is relative to this root, exactly as in the foundation.
    fn destination(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    fn confine(&self, path: &Path) -> Result<PathBuf> {
        anyhow::ensure!(
            normalized(path),
            "step path {} is not normalized",
            path.display()
        );
        let absolute = self.destination(path);
        anyhow::ensure!(
            absolute.is_absolute(),
            "step path {} is not absolute",
            path.display()
        );
        anyhow::ensure!(
            absolute.starts_with(&self.root) && absolute != self.root,
            "step path {} is outside the plan root {}; the installer writes only under its own root",
            absolute.display(),
            self.root.display()
        );
        Ok(absolute)
    }

    /// Run the plan. Incomplete plans and NixOS targets are refused
    /// BEFORE the first side effect.
    pub fn run(&self, plan: &ProfilePlan) -> Result<RunReport> {
        plan.validate()?;
        if plan.target == Target::Nixos {
            bail!(
                "the nixos profile emits a module snippet ({NIXOS_DIR} is never edited by this installer); apply it with nixos-rebuild yourself"
            );
        }
        if !plan.complete {
            bail!(
                "this {} plan is not implemented: {} — nothing was executed",
                plan.profile.as_str(),
                plan.unsupported.join("; ")
            );
        }
        // Every destination is confined before the first write, so a plan
        // that would escape fails with nothing half-applied.
        for step in &plan.user {
            if let Some(path) = step_path(step) {
                self.confine(path)?;
            }
        }
        let mut steps = Vec::new();
        for (n, step) in plan.user.iter().enumerate() {
            let result = self.run_step(step).with_context(|| {
                format!("step {} of {}: {}", n + 1, plan.user.len(), step.describe())
            })?;
            steps.push(StepOutcome {
                index: n + 1,
                what: step.describe(),
                result,
            });
        }
        Ok(RunReport {
            dry_run: self.dry_run,
            steps,
            script: plan.script()?,
            emitted: plan.files.iter().map(|f| f.name.clone()).collect(),
        })
    }

    fn run_step(&self, step: &Step) -> Result<StepResult> {
        match step {
            Step::Config {
                path,
                sha256,
                setup,
                mode,
            } => {
                anyhow::ensure!(
                    crate::sha256(setup.config_toml.as_bytes()) == *sha256,
                    "the config step's digest does not match the setup plan it carries"
                );
                anyhow::ensure!(
                    path.file_name().is_some_and(|n| n == "config.toml"),
                    "the setup writer owns config.toml; {} is not one",
                    path.display()
                );
                let dir = path
                    .parent()
                    .context("the config step needs a parent directory")?;
                // Idempotence first: re-running a plan whose config is
                // already in place is a skip, not a stale-base refusal.
                match std::fs::read(path) {
                    Ok(bytes) if crate::sha256(&bytes) == *sha256 => {
                        return Ok(StepResult::Skipped(
                            "config.toml already matches the reviewed plan".into(),
                        ))
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
                }
                if self.dry_run {
                    return Ok(StepResult::WouldDo);
                }
                // The existing setup writer: config + .env merge, one base
                // signature, no second wizard. No secret values are
                // supplied here — an installer does not invent them.
                let report = veles_core::setup::apply_with(setup, dir, &BTreeMap::new())
                    .context("applying the setup plan (config.toml and .env)")?;
                anyhow::ensure!(
                    crate::sha256(&std::fs::read(&report.config_path)?) == *sha256,
                    "{} does not match the reviewed setup plan after the write",
                    report.config_path.display()
                );
                #[cfg(unix)]
                if let Some(mode) = mode {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(
                        &report.config_path,
                        std::fs::Permissions::from_mode(*mode),
                    )
                    .context("restricting config.toml to its planned mode")?;
                }
                #[cfg(not(unix))]
                let _ = mode;
                Ok(StepResult::Done)
            }
            Step::UserFile {
                path,
                contents,
                mode,
                expected_sha256,
            } => {
                if self.dry_run {
                    return Ok(StepResult::WouldDo);
                }
                let relative = self
                    .confine(path)?
                    .strip_prefix(&self.root)
                    .context("step path did not strip to the plan root")?
                    .to_path_buf();
                let action = Action::WriteFile {
                    path: relative,
                    contents: contents.clone(),
                    mode: *mode,
                    expected_sha256: expected_sha256.clone(),
                };
                let outcome = action.apply(&self.root)?;
                if let Some(service) = &self.service {
                    service.note_profile_recovery(path)?;
                }
                match outcome {
                    Status::Done => Ok(StepResult::Done),
                    other => Ok(StepResult::Skipped(format!("{other:?}"))),
                }
            }
            Step::Action(action) => {
                if self.dry_run {
                    return Ok(StepResult::WouldDo);
                }
                // The foundation executes paths relative to a root it
                // owns. An absolute destination is turned back into that
                // form here, so confinement has exactly one rule.
                let local = match action {
                    Action::WriteFile {
                        path,
                        contents,
                        mode,
                        expected_sha256,
                    } => Action::WriteFile {
                        path: self
                            .confine(path)?
                            .strip_prefix(&self.root)
                            .context("action path did not strip to the plan root")?
                            .to_path_buf(),
                        contents: contents.clone(),
                        mode: *mode,
                        expected_sha256: expected_sha256.clone(),
                    },
                    other => other.clone(),
                };
                match local.apply(&self.root)? {
                    Status::Done => Ok(StepResult::Done),
                    other => Ok(StepResult::Skipped(format!("{other:?}"))),
                }
            }
            Step::CoreBundle { source, plan_sig } => {
                if self.dry_run {
                    return Ok(StepResult::WouldDo);
                }
                let service = self
                    .service
                    .as_ref()
                    .context("core bundle requires the installer service")?;
                let review = service.preview_bundle(source)?;
                anyhow::ensure!(
                    review.plan_sig == *plan_sig,
                    "core bundle changed after profile preview; review a fresh plan"
                );
                let mut record = service.record()?;
                crate::install_reviewed_bundle(
                    &service.paths.install_root,
                    source,
                    plan_sig,
                    &mut record,
                )?;
                service.note_profile_recovery(&service.paths.install_root.join("current"))?;
                record.save(&service.record_path())?;
                service.note_profile_recovery(&service.record_path())?;
                Ok(StepResult::Done)
            }
            Step::Component {
                name,
                spec,
                plan_sig,
                ..
            } => {
                if self.dry_run {
                    return Ok(StepResult::WouldDo);
                }
                let service = self.service.as_ref().with_context(|| {
                    format!("component {name} can only be installed through the installer service")
                })?;
                anyhow::ensure!(
                    spec.name == *name,
                    "component {name} does not match its reviewed spec"
                );
                service.install_reviewed_component(spec, plan_sig).with_context(|| {
                    format!(
                        "component {name}: the reviewed artifact plan no longer matches (re-run the plan if the artifact changed)"
                    )
                })?;
                Ok(StepResult::Done)
            }
        }
    }
}

fn step_path(step: &Step) -> Option<&Path> {
    match step {
        Step::Config { path, .. } | Step::UserFile { path, .. } => Some(path),
        Step::Action(Action::WriteFile { path, .. }) => Some(path),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_are_whitelisted_never_escaped_by_remembering() {
        assert_eq!(shell_word("plain").unwrap(), "plain");
        assert_eq!(shell_word("/usr/bin/veles").unwrap(), "/usr/bin/veles");
        assert_eq!(shell_word("it's").unwrap(), r"'it'\''s'");
        assert_eq!(shell_word("a;b").unwrap(), "'a;b'");
        assert_eq!(shell_word("$(id)").unwrap(), "'$(id)'");
        assert_eq!(shell_word("a|b").unwrap(), "'a|b'");
        assert_eq!(shell_word("").unwrap(), "''");
        assert!(shell_word("two\nlines").is_err());
        assert!(shell_word("nul\0byte").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn confinement_is_component_wise_not_a_string_prefix() {
        let runner = Runner::new(PathBuf::from("/tmp/root"));
        assert!(runner.confine(Path::new("/tmp/root/file")).is_ok());
        assert!(runner.confine(Path::new("/tmp/root")).is_err());
        assert!(runner.confine(Path::new("/tmp/root2/file")).is_err());
        assert!(runner
            .confine(Path::new("/tmp/root/../etc/passwd"))
            .is_err());
        assert_eq!(
            runner.confine(Path::new("etc/one")).unwrap(),
            PathBuf::from("/tmp/root/etc/one")
        );
    }

    #[test]
    fn the_script_refuses_enabling_and_self_escalation() {
        let mut plan = ProfilePlan {
            profile: NamedProfile::Server,
            target: Target::Linux,
            user: vec![],
            system: vec![SystemStep::Command {
                argv: vec!["systemctl".into(), "--user".into(), "enable".into()],
                reason: "x".into(),
            }],
            files: vec![],
            unsupported: vec![],
            notes: vec![],
            complete: true,
        };
        assert!(plan.validate().is_err());
        plan.system = vec![SystemStep::Command {
            argv: vec!["sudo".into(), "true".into()],
            reason: "x".into(),
        }];
        assert!(plan.validate().is_err());
        plan.system = vec![SystemStep::File {
            destination: PathBuf::from("/etc/nixos/configuration.nix"),
            contents: "{}\n".into(),
            mode: 0o644,
            source: None,
        }];
        assert!(plan
            .validate()
            .unwrap_err()
            .to_string()
            .contains("/etc/nixos"));
        plan.system = vec![SystemStep::Command {
            argv: vec!["loginctl".into(), "enable-linger".into(), "veles".into()],
            reason: "x".into(),
        }];
        plan.validate().unwrap();
        let script = plan.script().unwrap().unwrap();
        assert!(script.text.contains("loginctl enable-linger veles"));
        assert!(script.text.contains("set -eu"));
        // The header tells the operator how to run it WITH sudo; no line
        // of the script itself may invoke one. Comments are not steps.
        for line in script
            .text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            assert!(
                !line.starts_with("sudo"),
                "the script never escalates itself: {line}"
            );
        }
    }
}
