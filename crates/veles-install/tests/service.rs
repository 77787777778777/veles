#![cfg(unix)]
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use veles_install::{ArtifactTrust, InstallService, Paths};

fn fixture(root: &Path, version: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    let exe = root.join("veles-plugin-fixture");
    let script = format!("#!/bin/sh\nif [ \"$1\" = --describe ]; then\nprintf '%s\\n' '{{\"name\":\"fixture\",\"version\":\"{version}\",\"protocol\":1,\"description\":\"fixture-{version}\",\"config_sections\":[\"platforms.fixture\"],\"knobs\":[{{\"path\":\"platforms.fixture.server\",\"default\":\"\\\"one\\\"\",\"shape\":\"value\",\"note\":null,\"env\":null}},{{\"path\":\"platforms.fixture.token\",\"default\":null,\"shape\":\"unset\",\"note\":null,\"env\":null}}]}}'\nexit 0\nfi\n{body}\n");
    std::fs::write(&exe, script).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        root.join("plugin.toml"),
        format!("name = \"fixture\"\nkind = \"process\"\nentry = \"veles-plugin-fixture\"\ndescription = \"fixture-{version}\"\n"),
    )
    .unwrap();
    exe
}

fn service(temp: &tempfile::TempDir) -> InstallService {
    let cfg = temp.path().join("config");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let paths = Paths::for_roots(cfg, data);
    InstallService::new(paths)
}

fn answers() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([("platforms.fixture.server".into(), serde_json::json!("two"))])
}

#[test]
fn preview_never_returns_existing_config_or_secret_values_and_stale_apply_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    let root = s.paths.process_roots[0].join("fixture");
    fixture(&root, "1", "exit 0");
    let config = s.paths.config_dir.join("config.toml");
    std::fs::write(
        &config,
        "# keep\n[platforms.fixture]\ntoken = \"never-show-this-value\"\n",
    )
    .unwrap();
    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    let review = serde_json::to_string(&plan.review()).unwrap();
    assert!(review.contains("platforms.fixture.server"));
    assert!(!review.contains("never-show-this-value"));
    assert!(!review.contains("config.toml\\n"));
    assert_eq!(plan.review()["config_text"], serde_json::Value::Null);
    let base = plan.config.as_ref().unwrap().base_sig.clone();
    let applied = s
        .apply_config_answers("fixture", "", &answers(), &Some(base), &plan.plan_sig)
        .unwrap();
    assert_eq!(applied["ok"], true);
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("# keep"));
    assert!(text.contains("server = \"two\""));
    assert!(text.contains("never-show-this-value"));
    let second = s.preview_config("fixture", "", &answers()).unwrap();
    std::fs::write(&config, "# operator changed it\n").unwrap();
    let err = s
        .apply_config_answers(
            "fixture",
            "",
            &answers(),
            &second.config.as_ref().map(|c| c.base_sig.clone()),
            &second.plan_sig,
        )
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("changed after preview"),
        "{err:#}"
    );
    assert_eq!(
        std::fs::read_to_string(config).unwrap(),
        "# operator changed it\n"
    );
}

#[test]
fn component_upgrade_uses_the_real_manifest_and_rollback_restores_the_old_executable() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    let root = s.paths.process_roots[0].join("fixture");
    fixture(&root, "1", "exit 0");
    let plan = s.preview_component("fixture").unwrap();
    let first = s
        .install_component("fixture", plan["plan_sig"].as_str())
        .unwrap();
    assert_eq!(first["ok"], true);
    let manifest_path = root.join("plugin.toml");
    let installed_entry = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(installed_entry.contains("/install/components/fixture/"));
    let old_exe = first["component"]["source"].as_str().unwrap().to_string();
    assert!(Path::new(&old_exe).is_file());
    assert!(s.verify().unwrap()["ok"].as_bool().unwrap());
    std::fs::write(
        &manifest_path,
        format!(
            "name = \"fixture\"\nkind = \"process\"\nentry = \"{}\"\n",
            root.join("plugin").display()
        ),
    )
    .unwrap();
    let tampered = s.verify().unwrap();
    assert_eq!(
        tampered["ok"], false,
        "retained hash alone must not bless a changed live selection: {tampered}"
    );
    std::fs::write(&manifest_path, &installed_entry).unwrap();

    let upgrade = temp.path().join("upgrade/fixture");
    fixture(&upgrade, "2", "exit 0");
    let mut paths = s.paths.clone();
    paths.process_roots = vec![upgrade.parent().unwrap().to_path_buf()];
    let upgraded_service = InstallService::new(paths);
    let plan = upgraded_service.preview_component("fixture").unwrap();
    let second = upgraded_service
        .install_component("fixture", plan["plan_sig"].as_str())
        .unwrap();
    assert_ne!(second["component"]["source"], first["component"]["source"]);
    assert!(std::fs::read_to_string(&manifest_path)
        .unwrap()
        .contains(second["component"]["source"].as_str().unwrap()));
    let review = upgraded_service.preview_rollback("fixture").unwrap();
    let rolled = upgraded_service
        .rollback("fixture", review["plan_sig"].as_str().unwrap())
        .unwrap();
    assert_eq!(rolled["component"]["source"].as_str().unwrap(), old_exe);
    assert!(std::fs::read_to_string(&manifest_path)
        .unwrap()
        .contains(&old_exe));
    let restored_manifest = std::fs::read_to_string(&manifest_path).unwrap();
    assert_eq!(
        restored_manifest, installed_entry,
        "rollback restores the old manifest metadata as well as its executable"
    );
    assert!(upgraded_service.verify().unwrap()["ok"].as_bool().unwrap());

    let foreign_manifest = restored_manifest.replace("entry = ", "entry = \"foreign\" # ");
    assert_ne!(foreign_manifest, restored_manifest);
    std::fs::write(&manifest_path, &foreign_manifest).unwrap();
    let record_before = std::fs::read(s.paths.config_dir.join("install.toml")).unwrap();
    let plan = upgraded_service.preview_component("fixture").unwrap();
    let error = upgraded_service
        .install_component("fixture", plan["plan_sig"].as_str())
        .unwrap_err();
    assert!(format!("{error:#}").contains("selection"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(&manifest_path).unwrap(),
        foreign_manifest
    );
    assert_eq!(
        std::fs::read(s.paths.config_dir.join("install.toml")).unwrap(),
        record_before
    );
}

#[test]
fn config_only_rollback_restores_the_private_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    fixture(&s.paths.process_roots[0].join("fixture"), "1", "exit 0");
    let config = s.paths.config_dir.join("config.toml");
    std::fs::write(&config, "# original\n").unwrap();
    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    s.apply_config_answers(
        "fixture",
        "",
        &answers(),
        &plan.config.as_ref().map(|c| c.base_sig.clone()),
        &plan.plan_sig,
    )
    .unwrap();
    assert!(std::fs::read_to_string(&config)
        .unwrap()
        .contains("server = \"two\""));
    let review = s.preview_rollback("fixture").unwrap();
    s.rollback("fixture", review["plan_sig"].as_str().unwrap())
        .unwrap();
    assert_eq!(std::fs::read_to_string(config).unwrap(), "# original\n");
}

#[test]
fn missing_config_is_removed_on_rollback_and_newer_edits_are_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    fixture(&s.paths.process_roots[0].join("fixture"), "1", "exit 0");
    let config = s.paths.config_dir.join("config.toml");
    assert!(!config.exists());
    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    s.apply_config_answers(
        "fixture",
        "",
        &answers(),
        &plan.config.as_ref().map(|c| c.base_sig.clone()),
        &plan.plan_sig,
    )
    .unwrap();
    assert!(config.exists());
    let record = s.record().unwrap();
    assert_eq!(
        record.history.last().unwrap().config_file_existed,
        Some(false)
    );
    let review = s.preview_rollback("fixture").unwrap();
    s.rollback("fixture", review["plan_sig"].as_str().unwrap())
        .unwrap();
    assert!(
        !config.exists(),
        "a config created by the operation must be removed, not made empty"
    );

    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    s.apply_config_answers(
        "fixture",
        "",
        &answers(),
        &plan.config.as_ref().map(|c| c.base_sig.clone()),
        &plan.plan_sig,
    )
    .unwrap();
    std::fs::write(
        &config,
        "# a newer unrelated edit\n[tui]\nlocale = \"en\"\n",
    )
    .unwrap();
    assert!(s
        .rollback(
            "fixture",
            s.preview_rollback("fixture").unwrap()["plan_sig"]
                .as_str()
                .unwrap()
        )
        .is_err());
    assert!(std::fs::read_to_string(config)
        .unwrap()
        .contains("newer unrelated"));
}

#[test]
fn existing_empty_config_is_restored_instead_of_deleted_on_rollback() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    fixture(&s.paths.process_roots[0].join("fixture"), "1", "exit 0");
    let config = s.paths.config_dir.join("config.toml");
    std::fs::write(&config, "").unwrap();
    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    s.apply_config_answers(
        "fixture",
        "",
        &answers(),
        &plan.config.as_ref().map(|c| c.base_sig.clone()),
        &plan.plan_sig,
    )
    .unwrap();
    assert!(
        s.record()
            .unwrap()
            .history
            .last()
            .unwrap()
            .config_file_existed
            == Some(true)
    );
    let review = s.preview_rollback("fixture").unwrap();
    s.rollback("fixture", review["plan_sig"].as_str().unwrap())
        .unwrap();
    assert!(config.is_file());
    assert_eq!(std::fs::read(&config).unwrap(), b"");
}

#[test]
fn failed_rollback_restores_the_config_and_original_record() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    fixture(&s.paths.process_roots[0].join("fixture"), "1", "exit 0");
    let config = s.paths.config_dir.join("config.toml");
    std::fs::write(&config, "# original\n").unwrap();
    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    s.apply_config_answers(
        "fixture",
        "",
        &answers(),
        &plan.config.as_ref().map(|c| c.base_sig.clone()),
        &plan.plan_sig,
    )
    .unwrap();
    let configured = std::fs::read_to_string(&config).unwrap();
    let record_before = std::fs::read(s.paths.config_dir.join("install.toml")).unwrap();
    veles_install::fail_next_record_write_for_test();
    let review = s.preview_rollback("fixture").unwrap();
    let err = s
        .rollback("fixture", review["plan_sig"].as_str().unwrap())
        .unwrap_err();
    assert!(format!("{err:#}").contains("restored"), "{err:#}");
    assert_eq!(std::fs::read_to_string(config).unwrap(), configured);
    assert_eq!(
        std::fs::read(s.paths.config_dir.join("install.toml")).unwrap(),
        record_before
    );
}

#[cfg(unix)]
#[test]
fn rollback_preserves_an_operator_config_symlink_and_restores_its_target() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    let root = s.paths.process_roots[0].join("fixture");
    fixture(&root, "1", "exit 0");
    let plan = s.preview_component("fixture").unwrap();
    s.install_component("fixture", plan["plan_sig"].as_str())
        .unwrap();
    let target = temp.path().join("operator-config.toml");
    let config = s.paths.config_dir.join("config.toml");
    std::fs::write(&target, "# before installer\n").unwrap();
    std::os::unix::fs::symlink(&target, &config).unwrap();
    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    s.apply_config_answers(
        "fixture",
        "",
        &answers(),
        &plan.config.as_ref().map(|c| c.base_sig.clone()),
        &plan.plan_sig,
    )
    .unwrap();
    assert!(std::fs::symlink_metadata(&config)
        .unwrap()
        .file_type()
        .is_symlink());
    let review = s.preview_rollback("fixture").unwrap();
    s.rollback("fixture", review["plan_sig"].as_str().unwrap())
        .unwrap();
    assert!(std::fs::symlink_metadata(&config)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "# before installer\n"
    );
}

#[test]
fn failed_record_write_restores_config_and_component_selection() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    let root = s.paths.process_roots[0].join("fixture");
    fixture(&root, "1", "exit 0");
    let manifest = root.join("plugin.toml");
    let original_manifest = std::fs::read_to_string(&manifest).unwrap();
    let original_config = s.paths.config_dir.join("config.toml");
    std::fs::write(&original_config, "# original\n").unwrap();
    veles_install::fail_next_record_write_for_test();
    let plan = s.preview_component("fixture").unwrap();
    let error = s
        .install_component("fixture", plan["plan_sig"].as_str())
        .unwrap_err();
    assert!(format!("{error:#}").contains("injected"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        original_manifest
    );
    assert!(!s.paths.config_dir.join("install.toml").exists());

    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    veles_install::fail_next_record_write_for_test();
    let error = s
        .apply_config_answers(
            "fixture",
            "",
            &answers(),
            &plan.config.as_ref().map(|c| c.base_sig.clone()),
            &plan.plan_sig,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("injected"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(original_config).unwrap(),
        "# original\n"
    );
}

#[test]
fn legacy_config_history_cannot_replace_an_operator_file_with_an_empty_one() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    fixture(&s.paths.process_roots[0].join("fixture"), "1", "exit 0");
    let config = s.paths.config_dir.join("config.toml");
    std::fs::write(&config, "# original\n").unwrap();
    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    s.apply_config_answers(
        "fixture",
        "",
        &answers(),
        &plan.config.as_ref().map(|c| c.base_sig.clone()),
        &plan.plan_sig,
    )
    .unwrap();
    let after = std::fs::read_to_string(&config).unwrap();
    let mut record = s.record().unwrap();
    let history = record.history.last_mut().unwrap();
    history.config_snapshot = None;
    history.config_file_existed = None;
    record
        .save(&s.paths.config_dir.join("install.toml"))
        .unwrap();
    let error = s.preview_rollback("fixture").unwrap_err();
    assert!(
        format!("{error:#}").contains("cannot prove whether config.toml existed"),
        "{error:#}"
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), after);
}

#[test]
fn a_changed_rollback_record_cannot_reuse_an_old_review() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    fixture(&s.paths.process_roots[0].join("fixture"), "1", "exit 0");
    let config = s.paths.config_dir.join("config.toml");
    std::fs::write(&config, "# original\n").unwrap();
    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    s.apply_config_answers(
        "fixture",
        "",
        &answers(),
        &plan.config.as_ref().map(|c| c.base_sig.clone()),
        &plan.plan_sig,
    )
    .unwrap();
    let review = s.preview_rollback("fixture").unwrap();
    let signature = review["plan_sig"].as_str().unwrap().to_string();
    let config_before = std::fs::read(&config).unwrap();
    let record_before_tamper = std::fs::read(s.paths.config_dir.join("install.toml")).unwrap();
    let mut record = s.record().unwrap();
    record.history.last_mut().unwrap().options_before.insert(
        "platforms.fixture.server".into(),
        serde_json::json!("other"),
    );
    record
        .save(&s.paths.config_dir.join("install.toml"))
        .unwrap();
    let tampered = std::fs::read(s.paths.config_dir.join("install.toml")).unwrap();
    assert_ne!(
        tampered, record_before_tamper,
        "fixture must actually alter the reviewed record"
    );
    let error = s.rollback("fixture", &signature).unwrap_err();
    assert!(
        format!("{error:#}").contains("rollback state changed after preview"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(config).unwrap(), config_before);
    let actual = std::fs::read_to_string(s.paths.config_dir.join("install.toml")).unwrap();
    let expected = String::from_utf8(tampered).unwrap();
    assert_eq!(
        actual, expected,
        "a refused rollback must leave the operator's later record edit untouched"
    );
}

#[test]
fn deleting_a_reviewed_empty_config_invalidates_its_component_preview() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    fixture(&s.paths.process_roots[0].join("fixture"), "1", "exit 0");
    let config = s.paths.config_dir.join("config.toml");
    std::fs::write(&config, "").unwrap();
    let plan = s.preview_config("fixture", "", &answers()).unwrap();
    std::fs::remove_file(&config).unwrap();
    let error = s.apply_config(&plan).unwrap_err();
    assert!(
        format!("{error:#}").contains("changed after preview"),
        "{error:#}"
    );
}

#[test]
fn a_changed_bundle_cannot_be_applied_with_the_old_review() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    let source = temp.path().join("bundle");
    std::fs::create_dir_all(source.join("bin")).unwrap();
    std::fs::write(
        source.join("BUNDLE-INFO.json"),
        "{\"version\":\"1\",\"revision\":\"a\",\"architecture\":\"x86_64\"}",
    )
    .unwrap();
    std::fs::write(source.join("bin/veles"), "one\n").unwrap();
    let plan = s.preview_bundle(&source).unwrap();
    std::fs::write(source.join("bin/veles"), "changed after review\n").unwrap();
    let error = s.install_local_bundle(&source, &plan.plan_sig).unwrap_err();
    assert!(
        format!("{error:#}").contains("changed after preview"),
        "{error:#}"
    );
    assert!(!s.paths.install_root.join("current").exists());
    assert!(!s.paths.config_dir.join("install.toml").exists());
}

#[test]
fn bytes_changed_during_bundle_copy_are_not_selected() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    let source = temp.path().join("bundle");
    std::fs::create_dir_all(source.join("bin")).unwrap();
    std::fs::write(
        source.join("BUNDLE-INFO.json"),
        "{\"version\":\"1\",\"revision\":\"a\",\"architecture\":\"x86_64\"}",
    )
    .unwrap();
    std::fs::write(source.join("bin/veles"), "one\n").unwrap();
    let plan = s.preview_bundle(&source).unwrap();
    let changed = s.paths.install_root.join(format!(
        "releases/.1-a.staging-{}/bin/veles",
        std::process::id()
    ));
    veles_install::after_bundle_copy_for_test(move || {
        // Change the already copied tree, not the source read during preview.
        std::fs::write(&changed, "changed during copy\n").unwrap();
    });
    let error = s.install_local_bundle(&source, &plan.plan_sig).unwrap_err();
    assert!(
        format!("{error:#}").contains("changed during staged copy"),
        "{error:#}"
    );
    assert!(!s.paths.install_root.join("current").exists());
    assert!(!s.paths.config_dir.join("install.toml").exists());
}

#[test]
fn a_changed_component_artifact_cannot_be_applied_with_the_old_review() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    let root = s.paths.process_roots[0].join("fixture");
    let executable = fixture(&root, "1", "exit 0");
    let plan = s.preview_component("fixture").unwrap();
    // Keep --describe valid: corrupting the script would test the
    // earlier describe failure, not the reviewed-artifact signature.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&executable)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(b"\n# changed after review\n")
        })
        .unwrap();
    let error = s
        .install_component("fixture", plan["plan_sig"].as_str())
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("changed after preview"),
        "{error:#}"
    );
    assert!(!s.paths.config_dir.join("install.toml").exists());
}

#[test]
fn manifest_version_cannot_escape_the_component_store() {
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    fixture(
        &s.paths.process_roots[0].join("fixture"),
        "../../../escaped",
        "exit 0",
    );
    let plan = s.preview_component("fixture").unwrap();
    let err = s
        .install_component("fixture", plan["plan_sig"].as_str())
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("invalid component version"),
        "{err:#}"
    );
    assert!(!temp.path().join("escaped").exists());
    assert!(!temp.path().join("config").join("install.toml").exists());
}

#[test]
fn archive_install_requires_real_verification_not_a_source_flag() {
    let temp = tempfile::tempdir().unwrap();
    let mut record = veles_install::InstallRecord::empty();
    let root = temp.path().join("install");
    let archive = temp.path().join("missing.tar.zst");
    let err = veles_install::install_bundle_archive(
        &root,
        &archive,
        ArtifactTrust::LocalDirectory,
        &mut record,
    )
    .unwrap_err();
    assert!(err.to_string().contains("signed-inventory"));
}

/// The portable Linux bundle is a Nix closure run under nix-user-chroot,
/// which mounts `<bundle>/nix` at `/nix`: its store paths link to each
/// other ABSOLUTELY (`/nix/store/<hash>-gcc-libgcc/lib/libgcc_s.so`).
/// Refusing every absolute target made `veles install bundle` reject the
/// real release archive outright (install.sh's first real run,
/// 2026-10-27), while every fixture with relative links passed. Store
/// links are kept verbatim; any other absolute or escaping target is
/// still refused.
#[test]
fn a_portable_bundle_keeps_its_absolute_nix_store_links_and_refuses_others() {
    let bundle = |temp: &tempfile::TempDir, target: &str| {
        let source = temp.path().join("bundle");
        let lib = source.join("nix/store/aaaa-app/lib");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(source.join("nix/store/bbbb-libgcc/lib")).unwrap();
        std::fs::write(source.join("nix/store/bbbb-libgcc/lib/libgcc_s.so"), "so\n").unwrap();
        std::fs::write(
            source.join("BUNDLE-INFO.json"),
            "{\"version\":\"1\",\"revision\":\"a\",\"architecture\":\"x86_64\"}",
        )
        .unwrap();
        std::fs::write(source.join("bin/veles"), "launcher\n").unwrap();
        std::os::unix::fs::symlink(target, lib.join("libgcc_s.so")).unwrap();
        source
    };
    let temp = tempfile::tempdir().unwrap();
    let s = service(&temp);
    let source = bundle(&temp, "/nix/store/bbbb-libgcc/lib/libgcc_s.so");
    let plan = s.preview_bundle(&source).unwrap();
    s.install_local_bundle(&source, &plan.plan_sig).unwrap();
    let installed = s
        .paths
        .install_root
        .join("current/nix/store/aaaa-app/lib/libgcc_s.so");
    assert_eq!(
        std::fs::read_link(&installed).unwrap(),
        Path::new("/nix/store/bbbb-libgcc/lib/libgcc_s.so")
    );
    for bad in [
        "/etc/passwd",
        "/nix/store/../../etc/passwd",
        "/nix/store",
        "/nix/var/x",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let s = service(&temp);
        let source = bundle(&temp, bad);
        let error = s.preview_bundle(&source).unwrap_err();
        assert!(
            format!("{error:#}").contains("bundle symlink"),
            "{bad}: {error:#}"
        );
    }
}
