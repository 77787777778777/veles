//! Shared typed installation plans and the durable `install.toml` record.
//! Frontends call this crate; no frontend reconstructs privileged actions.
mod action;
mod bootstrap;
mod bundle;
mod component;
pub mod container_host;
pub mod host_components;
mod lock;
#[cfg(unix)]
mod ownership;
// Privileged host apply is Linux-only; the stub refuses it by name so the
// previews and unprivileged profiles still build on Windows.
#[cfg(not(unix))]
#[path = "ownership_stub.rs"]
mod ownership;
mod profile_service;
pub mod profiles;
mod record;
mod service;
pub mod templates;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support;
mod transaction;
mod workflow;

pub use action::{sha256, valid_hash, Action, Plan, Status};
pub use bootstrap::{ReleaseRequest, SignedPlan};
pub(crate) use bundle::install_reviewed_bundle;
pub use bundle::{
    install_bundle, install_bundle_archive, review_bundle, rollback, verify_component,
    ArtifactTrust, BundleInfo, BundlePlan,
};
pub use component::{
    apply_config, discover_components, load_component, plan_config, plan_config_for_instance,
    plan_config_text, redact_config, ComponentKind, ComponentSpec, ConfigPlan,
};
pub(crate) use lock::OperationLock;
pub use profile_service::{
    native_layout, privileged_handoff, run_reviewed_container_script, HostAnswers, ProfileAnswers,
    ProfileHandoff, ProfileRequest, ReviewedProfile, ServiceScope,
};
pub use profiles::NamedProfile;
pub use record::{Component, HistoryEntry, InstallRecord, Profile};
pub use service::{InstallService, Paths, ServicePlan};
pub(crate) use transaction::Recovery;
pub use workflow::{ComponentRequest, WorkflowPlan};

#[cfg(all(unix, debug_assertions))]
#[doc(hidden)]
pub use ownership::{
    account as test_account, chown as test_chown_managed,
    chown_reviewed_tree as test_chown_reviewed_tree, ensure_dir as test_ensure_managed_dir,
    Account as TestAccount,
};

/// A single safe path component on every supported host. Manifest data
/// is untrusted until this check; it is never a filesystem path by fiat.
pub fn valid_version(version: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !version.is_empty()
            && version.len() <= 160
            && version != "."
            && version != ".."
            && version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_')),
        "invalid component version {version:?}: use ASCII letters, digits, dot, dash or underscore"
    );
    Ok(())
}

#[cfg(debug_assertions)]
thread_local! {
    static FAIL_NEXT_RECORD_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Deterministic failure injection for installer tests. Never compiled
/// into a release installer; no environment-controlled behavior.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn fail_next_record_write_for_test() {
    FAIL_NEXT_RECORD_WRITE.with(|flag| flag.set(true));
}

#[cfg(debug_assertions)]
fn fail_record_write_for_test() -> bool {
    FAIL_NEXT_RECORD_WRITE.with(|flag| flag.replace(false))
}

#[cfg(not(debug_assertions))]
fn fail_record_write_for_test() -> bool {
    false
}

#[cfg(debug_assertions)]
thread_local! {
    static FAIL_AFTER_SYSTEM_STEPS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Fail a native system handoff after its privileged files and links are
/// written: the rollback test for privileged host files. Debug-only.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn fail_after_system_steps_next_for_test() {
    FAIL_AFTER_SYSTEM_STEPS.with(|flag| flag.set(true));
}

#[cfg(debug_assertions)]
fn fail_after_system_steps_for_test() -> bool {
    FAIL_AFTER_SYSTEM_STEPS.with(|flag| flag.replace(false))
}

#[cfg(not(debug_assertions))]
fn fail_after_system_steps_for_test() -> bool {
    false
}

#[cfg(debug_assertions)]
thread_local! {
    static BUNDLE_COPY_CALLBACK: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

/// Arm one inert post-copy mutation. No environment variable or release
/// build can enable this test seam.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn after_bundle_copy_for_test(callback: impl Fn() + 'static) {
    BUNDLE_COPY_CALLBACK.with(|slot| *slot.borrow_mut() = Some(Box::new(callback)));
}

#[cfg(debug_assertions)]
pub(crate) fn bundle_copy_hook() {
    BUNDLE_COPY_CALLBACK.with(|slot| {
        if let Some(callback) = slot.borrow_mut().take() {
            callback();
        }
    });
}

#[cfg(not(debug_assertions))]
pub(crate) fn bundle_copy_hook() {}

pub fn valid_component_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid component name {name:?}"
    );
    Ok(())
}

/// The account this process runs as: `$USER`, else `$LOGNAME`, else the
/// password database entry for the effective uid. A service shell, a
/// container or the Nix build sandbox may have no USER; that must not
/// turn into a refusal or a panic.
pub fn current_user_name() -> Option<String> {
    for var in ["USER", "LOGNAME"] {
        if let Ok(name) = std::env::var(var) {
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    #[cfg(unix)]
    {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0u8; 16 * 1024];
        let rc = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                &mut pwd,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if rc == 0 && !result.is_null() && !pwd.pw_name.is_null() {
            return unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }
                .to_str()
                .ok()
                .map(str::to_string);
        }
    }
    #[cfg(windows)]
    if let Ok(name) = std::env::var("USERNAME") {
        return Some(name);
    }
    None
}

#[cfg(test)]
mod tests {
    fn manifest(knob: &str) -> veles_types::plugin::Manifest {
        veles_types::plugin::Manifest {
            name: "irc".into(),
            version: "1".into(),
            protocol: 1,
            knobs: vec![veles_types::Knob::blob(
                knob,
                Some("\"irc.libera.chat\"".into()),
                Some("server"),
            )],
            config_sections: vec!["platforms.irc".into()],
            ..Default::default()
        }
    }

    #[test]
    fn component_options_edit_only_the_declared_knobs() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "# my settings\n[platforms.irc]\nnickname = \"keep-me\"\n",
        )
        .unwrap();
        let mut values = std::collections::BTreeMap::new();
        values.insert(
            "platforms.irc.server".into(),
            serde_json::json!("irc.example.net"),
        );
        let spec = crate::ComponentSpec {
            name: "irc".into(),
            kind: crate::ComponentKind::Module,
            manifest: manifest("platforms.irc.server"),
            source: None,
            describe_error: None,
        };
        let plan = crate::plan_config(&config, &spec, &values).unwrap();
        assert!(plan.config_toml.contains("server = \"irc.example.net\""));
        assert!(plan.config_toml.contains("# my settings"));
        assert!(plan.config_toml.contains("nickname = \"keep-me\""));
        values.insert(
            "platforms.telegram.token".into(),
            serde_json::json!("not-the-plugin"),
        );
        assert!(crate::plan_config(&config, &spec, &values)
            .unwrap_err()
            .to_string()
            .contains("not declared"));
    }

    #[cfg(unix)]
    #[test]
    fn installing_a_new_bundle_keeps_the_previous_for_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("install");
        let source = dir.path().join("bundle");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::write(
            source.join("BUNDLE-INFO.json"),
            "{\"version\":\"1\",\"revision\":\"a\",\"architecture\":\"x86_64\"}",
        )
        .unwrap();
        std::fs::write(source.join("bin/veles"), "v1\n").unwrap();
        let mut record = crate::InstallRecord {
            schema: 1,
            profile: crate::Profile::Workstation,
            components: vec![],
            history: vec![],
            recovery: None,
        };
        crate::install_bundle(
            &root,
            &source,
            crate::ArtifactTrust::LocalDirectory,
            &mut record,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("current/bin/veles")).unwrap(),
            "v1\n"
        );
        std::fs::write(source.join("bin/veles"), "v2\n").unwrap();
        std::fs::write(
            source.join("BUNDLE-INFO.json"),
            "{\"version\":\"2\",\"revision\":\"b\",\"architecture\":\"x86_64\"}",
        )
        .unwrap();
        crate::install_bundle(
            &root,
            &source,
            crate::ArtifactTrust::LocalDirectory,
            &mut record,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("current/bin/veles")).unwrap(),
            "v2\n"
        );
        crate::rollback(&root, &mut record, "core").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("current/bin/veles")).unwrap(),
            "v1\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_verified_archive_reaches_the_same_tree_install_and_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("install");
        let source = dir.path().join("bundle");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        let architecture = if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "x86_64"
        };
        std::fs::write(
            source.join("BUNDLE-INFO.json"),
            format!("{{\"version\":\"1\",\"revision\":\"a\",\"architecture\":\"{architecture}\"}}"),
        )
        .unwrap();
        std::fs::write(source.join("bin/veles"), "v1\n").unwrap();
        let archive = dir.path().join("bundle.tar.zst");
        let file = std::fs::File::create(&archive).unwrap();
        let encoder = zstd::stream::write::Encoder::new(file, 0).unwrap();
        let mut tar = tar::Builder::new(encoder);
        tar.append_dir_all(".", &source).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let mut record = crate::InstallRecord {
            schema: 1,
            profile: crate::Profile::Workstation,
            components: vec![],
            history: vec![],
            recovery: None,
        };
        crate::install_bundle_archive(
            &root,
            &archive,
            crate::ArtifactTrust::SignatureVerified,
            &mut record,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("current/bin/veles")).unwrap(),
            "v1\n"
        );
        assert!(crate::verify_component(&root, &record.components[0])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_changed_config_after_preview_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "# keep\n").unwrap();
        let spec = crate::ComponentSpec {
            name: "irc".into(),
            kind: crate::ComponentKind::Module,
            manifest: manifest("platforms.irc.server"),
            source: None,
            describe_error: None,
        };
        let mut values = std::collections::BTreeMap::new();
        values.insert(
            "platforms.irc.server".into(),
            serde_json::json!("irc.example.net"),
        );
        let plan = crate::plan_config(&config, &spec, &values).unwrap();
        std::fs::write(&config, "# operator won the race\n").unwrap();
        assert!(crate::apply_config(&config, &plan)
            .unwrap_err()
            .to_string()
            .contains("changed after preview"));
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "# operator won the race\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rollback_refuses_a_tampered_retained_payload() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("install");
        let source = dir.path().join("bundle");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::write(
            source.join("BUNDLE-INFO.json"),
            "{\"version\":\"1\",\"revision\":\"a\",\"architecture\":\"x86_64\"}",
        )
        .unwrap();
        std::fs::write(source.join("bin/veles"), "v1\n").unwrap();
        let mut record = crate::InstallRecord {
            schema: 1,
            profile: crate::Profile::Workstation,
            components: vec![],
            history: vec![],
            recovery: None,
        };
        crate::install_bundle(
            &root,
            &source,
            crate::ArtifactTrust::LocalDirectory,
            &mut record,
        )
        .unwrap();
        std::fs::write(
            source.join("BUNDLE-INFO.json"),
            "{\"version\":\"2\",\"revision\":\"b\",\"architecture\":\"x86_64\"}",
        )
        .unwrap();
        std::fs::write(source.join("bin/veles"), "v2\n").unwrap();
        crate::install_bundle(
            &root,
            &source,
            crate::ArtifactTrust::LocalDirectory,
            &mut record,
        )
        .unwrap();
        std::fs::write(root.join("releases/1-a/bin/veles"), "tampered\n").unwrap();
        assert!(crate::rollback(&root, &mut record, "core").is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("current/bin/veles")).unwrap(),
            "v2\n"
        );
    }

    #[test]
    fn per_entry_knobs_write_named_networks_without_replacing_others() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "# keep\n[[platforms.irc.networks]]\nname = \"old\"\nserver = \"old.example\"\n",
        )
        .unwrap();
        let mut spec = crate::ComponentSpec {
            name: "irc".into(),
            kind: crate::ComponentKind::Module,
            manifest: manifest("platforms.irc.server"),
            source: None,
            describe_error: None,
        };
        spec.manifest.instances = veles_types::plugin::Instances::PerEntry {
            array: "platforms.irc.networks".into(),
            name: "name".into(),
        };
        let mut values = std::collections::BTreeMap::new();
        values.insert(
            "platforms.irc.server".into(),
            serde_json::json!("new.example"),
        );
        let plan = crate::plan_config_for_instance(&config, &spec, &values, "new").unwrap();
        assert!(plan.config_toml.contains("name = \"old\""));
        assert!(plan.config_toml.contains("name = \"new\""));
        assert!(plan.config_toml.contains("server = \"new.example\""));
        assert!(plan.config_toml.contains("# keep"));
    }

    #[test]
    fn write_file_is_idempotent_and_refuses_foreign_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let action = crate::Action::WriteFile {
            path: std::path::PathBuf::from("etc/example"),
            contents: b"one\n".to_vec(),
            mode: Some(0o600),
            expected_sha256: None,
        };
        assert_eq!(action.check(&root).unwrap(), crate::Status::Missing);
        action.apply(&root).unwrap();
        assert_eq!(action.check(&root).unwrap(), crate::Status::Done);
        action.apply(&root).unwrap();
        std::fs::write(root.join("etc/example"), "someone else\n").unwrap();
        assert!(action
            .check(&root)
            .unwrap_err()
            .to_string()
            .contains("different"));
        assert!(action.apply(&root).is_err());
        assert_eq!(
            std::fs::read(root.join("etc/example")).unwrap(),
            b"someone else\n"
        );
    }

    #[test]
    fn install_record_round_trip_and_future_schema_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install.toml");
        let record = crate::InstallRecord {
            schema: 1,
            profile: crate::Profile::Workstation,
            components: vec![crate::Component {
                name: "core".into(),
                kind: None,
                version: "0.1.0-abc".into(),
                options: Default::default(),
                artifacts: Default::default(),
                source: None,
                previous: None,
            }],
            history: vec![],
            recovery: None,
        };
        record.save(&path).unwrap();
        assert_eq!(crate::InstallRecord::load(&path).unwrap(), record);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::write(&path, "schema = 99\nprofile = \"workstation\"\n").unwrap();
        assert!(crate::InstallRecord::load(&path)
            .unwrap_err()
            .to_string()
            .contains("schema 99"));
    }
}
