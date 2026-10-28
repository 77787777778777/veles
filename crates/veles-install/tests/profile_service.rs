#![cfg(unix)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use veles_install::profiles::{HostLayout, NamedProfile, Target};
use veles_install::{
    InstallService, Paths, ProfileAnswers, ProfileHandoff, ProfileRequest, ServiceScope,
};

fn bundle(root: &Path) -> PathBuf {
    let source = root.join("source-bundle");
    std::fs::create_dir_all(source.join("bin")).unwrap();
    std::fs::write(source.join("bin/veles"), "reviewed core\n").unwrap();
    std::fs::set_permissions(
        source.join("bin/veles"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let architecture = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "aarch64"
    };
    std::fs::write(
        source.join("BUNDLE-INFO.json"),
        serde_json::to_vec(
            &serde_json::json!({"version":"1","revision":"fixture","architecture":architecture}),
        )
        .unwrap(),
    )
    .unwrap();
    source
}

fn layout(root: &Path) -> HostLayout {
    HostLayout {
        target: Target::Linux,
        bin: root.join("data/install/current/bin/veles"),
        user: Some("veles".into()),
        group: None,
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        system_service: false,
        write_root: root.to_path_buf(),
        user_unit_dir: root.join("config/systemd/user"),
        system_unit_dir: root.join("systemd"),
        rc_dir: root.join("rc.d"),
        rc_source: None,
    }
}

fn request(root: &Path) -> ProfileRequest {
    let answers = ProfileAnswers {
        profile: NamedProfile::Desktop,
        ..Default::default()
    };
    ProfileRequest {
        answers,
        host: layout(root),
        run_root: root.to_path_buf(),
        components: Vec::new(),
        core: root.join("source-bundle"),
    }
}

fn request_with_bundle(root: &Path) -> ProfileRequest {
    bundle(root);
    request(root)
}

#[test]
fn pds_selection_is_a_signed_handoff_not_an_installed_component() {
    let temp = tempfile::tempdir().unwrap();
    let service = InstallService::new(Paths::for_roots(
        temp.path().join("config"),
        temp.path().join("data"),
    ));
    let mut request = request_with_bundle(temp.path());
    request.core = std::path::PathBuf::new();
    request.answers.host.host_components =
        vec![veles_install::host_components::HostComponent::Pds(
            Box::new(veles_install::host_components::PdsOptions::new(
                "https://pds.example".into(),
                "pds.example".into(),
                "acme@example".into(),
                veles_install::container_host::PinnedImage::new(
                    "ghcr.io/jazware/vlpds:1.2.2",
                    &format!("sha256:{}", "a".repeat(64)),
                )
                .unwrap(),
                "https://s3.example".into(),
                "auto".into(),
                "bucket".into(),
                "pds".into(),
                "veles-gateway.service".into(),
                veles_install::current_user_name().unwrap(),
                "veles-net".into(),
                "veles-gateway".into(),
                None,
            )),
        )];
    let reviewed = service.preview_profile(request.clone()).unwrap();
    assert_eq!(
        reviewed.review["host_components"][0]["component"],
        "own-pds"
    );
    assert_eq!(
        reviewed.review["host_components"][0]["backup_gate_required"],
        true
    );
    assert!(reviewed.review["secrets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "VLPDS_S3_ACCESS_KEY"));
    let result = service
        .apply_profile(
            request,
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(result["status"], "handoff-required");
    assert_eq!(result["applied"], false);
    assert!(!temp.path().join("data/install.toml").exists());
    let request_path = result["request_path"].as_str().unwrap();
    let handoff: ProfileHandoff =
        serde_json::from_slice(&std::fs::read(request_path).unwrap()).unwrap();
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "VLPDS_S3_ACCESS_KEY".to_string(),
        "fixture-access".to_string(),
    );
    secrets.insert(
        "VLPDS_S3_SECRET_KEY".to_string(),
        "fixture-secret".to_string(),
    );
    let veles_install::host_components::HostComponent::Pds(options) =
        &handoff.answers.host.host_components[0]
    else {
        panic!("PDS handoff fixture changed component kind")
    };
    let mut options = options.clone();
    let root = temp.path().join("host-root");
    options.set_state_root_for_test(root.join("srv/veles/vlpds"));
    std::fs::create_dir_all(&root).unwrap();
    let quadlet = options.recovery_paths().unwrap()[0].clone();
    let home = quadlet.ancestors().nth(4).unwrap();
    std::fs::create_dir_all(root.join(home.strip_prefix("/").unwrap())).unwrap();
    let report = options.apply_files(&root, &secrets).unwrap();
    assert_eq!(report["status"], "files-prepared");
    assert_eq!(report["backup_required"], true);
    assert_eq!(report["service_running"], false);
    assert!(!temp.path().join("data/install.toml").exists());
    assert!(root.join("srv/veles/vlpds/secrets/kek").is_file());
    assert!(!root
        .join("srv/veles/vlpds/secrets/plc-rotation.key")
        .exists());
}

#[test]
fn local_user_unit_uses_the_xdg_config_base_not_the_veles_home() {
    let (config, data, _env) = veles_core::testenv::scratch_xdg();
    let paths = Paths::current();
    let mut answers = ProfileAnswers {
        profile: NamedProfile::Server,
        ..Default::default()
    };
    answers.host.scope = ServiceScope::User;
    let host = veles_install::native_layout(&answers, &paths).unwrap();
    assert_eq!(host.user_unit_dir, config.path().join("systemd/user"));
    assert_eq!(
        host.local_user_unit(),
        config.path().join("systemd/user/veles-gateway.service")
    );
    assert_eq!(host.config_dir, config.path().join("veles"));
    assert_eq!(host.data_dir, data.path().join("veles"));
}

#[test]
fn desktop_profile_installs_the_reviewed_core_in_its_own_root() {
    let temp = tempfile::tempdir().unwrap();
    let service = InstallService::new(Paths::for_roots(
        temp.path().join("config"),
        temp.path().join("data"),
    ));
    let reviewed = service
        .preview_profile(request_with_bundle(temp.path()))
        .unwrap();
    let text = reviewed.review().to_string();
    assert!(text.contains("preview"));
    assert!(
        !text.contains("reviewed core\\n"),
        "review leaked the core file body"
    );
    let result = service
        .apply_profile(
            request(temp.path()),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(result["applied"], true);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("data/install/current/bin/veles")).unwrap(),
        "reviewed core\n"
    );
    assert_eq!(
        service.record().unwrap().profile,
        veles_install::Profile::Workstation
    );
    assert!(service.verify().unwrap()["ok"].as_bool().unwrap());
}

#[test]
fn a_user_server_applies_the_unit_but_leaves_linger_pending() {
    let temp = tempfile::tempdir().unwrap();
    let service = InstallService::new(Paths::for_roots(
        temp.path().join("config"),
        temp.path().join("data"),
    ));
    bundle(temp.path());
    let mut answers = ProfileAnswers {
        profile: NamedProfile::Server,
        ..Default::default()
    };
    answers.host.scope = ServiceScope::User;
    let request = || ProfileRequest {
        answers: answers.clone(),
        host: layout(temp.path()),
        run_root: temp.path().to_path_buf(),
        components: Vec::new(),
        core: temp.path().join("source-bundle"),
    };
    let reviewed = service.preview_profile(request()).unwrap();
    let result = service
        .apply_profile(
            request(),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(result["status"], "applied");
    assert_eq!(result["applied"], true);
    assert_eq!(result["privileged_pending"], true);
    assert_eq!(result["privileged_script"]["executed"], false);
    assert!(temp
        .path()
        .join("config/systemd/user/veles-gateway.service")
        .is_file());
    assert!(result["note"].as_str().unwrap().contains("linger"));
}

#[test]
fn a_late_profile_failure_restores_the_user_unit_and_removes_new_payloads() {
    let temp = tempfile::tempdir().unwrap();
    let service = InstallService::new(Paths::for_roots(
        temp.path().join("config"),
        temp.path().join("data"),
    ));
    bundle(temp.path());
    let mut answers = ProfileAnswers {
        profile: NamedProfile::Server,
        ..Default::default()
    };
    answers.host.scope = ServiceScope::User;
    answers.components.push("fixture".into());
    let request = || ProfileRequest {
        answers: answers.clone(),
        host: layout(temp.path()),
        run_root: temp.path().to_path_buf(),
        components: vec![component(temp.path())],
        core: temp.path().join("source-bundle"),
    };
    let reviewed = service.preview_profile(request()).unwrap();
    veles_install::fail_next_record_write_for_test();
    let error = service
        .apply_profile(
            request(),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("restored"), "{error:#}");
    assert!(!temp
        .path()
        .join("config/systemd/user/veles-gateway.service")
        .exists());
    assert!(!temp.path().join("data/install/releases/1-fixture").exists());
    assert!(!temp.path().join("data/install/components").exists());
    assert!(!temp.path().join("config/config.toml").exists());
    assert!(!temp.path().join("config/install.toml").exists());
}

#[test]
fn a_changed_core_cannot_reuse_the_profile_plan() {
    let temp = tempfile::tempdir().unwrap();
    let service = InstallService::new(Paths::for_roots(
        temp.path().join("config"),
        temp.path().join("data"),
    ));
    let reviewed = service
        .preview_profile(request_with_bundle(temp.path()))
        .unwrap();
    std::fs::write(temp.path().join("source-bundle/bin/veles"), "changed\n").unwrap();
    let error = service
        .apply_profile(
            request(temp.path()),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("changed after preview"),
        "{error:#}"
    );
    assert!(!temp.path().join("data/install/current").exists());
    assert!(!temp.path().join("config/config.toml").exists());
}

fn system_request(root: &Path) -> ProfileRequest {
    let user = veles_install::current_user_name().unwrap();
    let mut answers = ProfileAnswers {
        profile: NamedProfile::Server,
        ..Default::default()
    };
    answers.host.scope = ServiceScope::System;
    answers.host.user = Some(user);
    answers.host.group = Some(
        std::process::Command::new("id")
            .arg("-gn")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap(),
    );
    let mut host = layout(root);
    host.user = answers.host.user.clone();
    host.group = answers.host.group.clone();
    host.system_service = true;
    host.system_unit_dir = root.join("caller/staging/target/etc/systemd/system");
    host.data_dir = root.join("caller/staging/var/lib/veles");
    host.config_dir = host.data_dir.clone();
    host.bin = host.data_dir.join("install/current/bin/veles");
    ProfileRequest {
        answers,
        host,
        run_root: root.join("caller/staging"),
        components: Vec::new(),
        core: root.join("source-bundle"),
    }
}

#[test]
fn a_system_handoff_revalidates_and_installs_in_the_live_target() {
    let temp = tempfile::tempdir().unwrap();
    let caller = InstallService::new(Paths::for_roots(
        temp.path().join("caller/config"),
        temp.path().join("caller/data"),
    ));
    let reviewed = caller
        .preview_profile({
            bundle(temp.path());
            system_request(temp.path())
        })
        .unwrap();
    let staged = caller
        .apply_profile(
            system_request(temp.path()),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(staged["status"], "handoff-required");
    assert_eq!(staged["applied"], false);
    let request_path = staged["request_path"].as_str().unwrap();
    assert!(Path::new(request_path).starts_with(temp.path()));
    assert!(!temp.path().join("data/config.toml").exists());
    let handoff: ProfileHandoff =
        serde_json::from_slice(&std::fs::read(request_path).unwrap()).unwrap();
    let result = veles_install::privileged_handoff(&handoff, &BTreeMap::new()).unwrap();
    assert_eq!(result["applied"], true);
    assert_eq!(result["service_running"], false);
    assert_eq!(
        std::fs::read_to_string(handoff.host.data_dir.join("install/current/bin/veles")).unwrap(),
        "reviewed core\n"
    );
    assert!(handoff.host.data_dir.join("config.toml").is_file());
    let unit = std::fs::read_to_string(handoff.host.system_unit_dir.join("veles-gateway.service"))
        .unwrap();
    assert!(unit.contains(&format!("VELES_HOME={}", handoff.host.data_dir.display())));
    assert!(unit.contains("gateway run"));
    assert!(!unit.contains("enable"));
    assert_eq!(
        handoff.host.config_dir, handoff.host.data_dir,
        "system VELES_HOME owns both config and data"
    );
    assert_eq!(
        handoff.host.system_unit_dir,
        temp.path().join("caller/staging/target/etc/systemd/system")
    );
    assert_eq!(
        handoff.host.bin,
        handoff.host.data_dir.join("install/current/bin/veles")
    );
    assert_eq!(
        InstallService::new(Paths::for_roots(
            handoff.host.config_dir.clone(),
            handoff.host.data_dir.clone()
        ))
        .record_path(),
        handoff.host.data_dir.join("install.toml")
    );
    assert!(
        !temp
            .path()
            .join("caller/data/install-profile-staging")
            .exists(),
        "the handoff must not use a staged install record"
    );
    let target = InstallService::new(Paths::for_roots(
        handoff.host.config_dir.clone(),
        handoff.host.data_dir.clone(),
    ));
    assert!(target.verify().unwrap()["ok"].as_bool().unwrap());
}

#[test]
fn a_tampered_handoff_is_refused_before_any_managed_write() {
    let temp = tempfile::tempdir().unwrap();
    let caller = InstallService::new(Paths::for_roots(
        temp.path().join("caller/config"),
        temp.path().join("caller/data"),
    ));
    let reviewed = caller
        .preview_profile({
            bundle(temp.path());
            system_request(temp.path())
        })
        .unwrap();
    let staged = caller
        .apply_profile(
            system_request(temp.path()),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    let path = PathBuf::from(staged["request_path"].as_str().unwrap());
    let mut handoff: ProfileHandoff =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    handoff.answers.setup.skipped = vec!["telegram".into()];
    std::fs::write(&path, serde_json::to_vec_pretty(&handoff).unwrap()).unwrap();
    let error = veles_install::privileged_handoff(&handoff, &BTreeMap::new()).unwrap_err();
    assert!(
        format!("{error:#}").contains("changed after preview"),
        "{error:#}"
    );
    assert!(!handoff.host.data_dir.join("install").exists());
    assert!(!handoff.host.data_dir.join("config.toml").exists());
}

#[test]
fn a_system_handoff_refuses_a_symlinked_managed_root() {
    let temp = tempfile::tempdir().unwrap();
    let caller = InstallService::new(Paths::for_roots(
        temp.path().join("caller/config"),
        temp.path().join("caller/data"),
    ));
    let reviewed = caller
        .preview_profile({
            bundle(temp.path());
            system_request(temp.path())
        })
        .unwrap();
    let staged = caller
        .apply_profile(
            system_request(temp.path()),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    let handoff: ProfileHandoff =
        serde_json::from_slice(&std::fs::read(staged["request_path"].as_str().unwrap()).unwrap())
            .unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), "keep").unwrap();
    std::fs::create_dir_all(handoff.host.data_dir.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &handoff.host.data_dir).unwrap();
    let error = veles_install::privileged_handoff(&handoff, &BTreeMap::new()).unwrap_err();
    assert!(
        format!("{error:#}").contains("symlink") || format!("{error:#}").contains("real directory"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("sentinel")).unwrap(),
        "keep"
    );
    assert!(!outside.join("config.toml").exists());
    assert!(!outside.join("install").exists());
}

#[test]
fn a_changed_target_refuses_the_handoff_before_writing() {
    let temp = tempfile::tempdir().unwrap();
    let caller = InstallService::new(Paths::for_roots(
        temp.path().join("caller/config"),
        temp.path().join("caller/data"),
    ));
    let reviewed = caller
        .preview_profile({
            bundle(temp.path());
            system_request(temp.path())
        })
        .unwrap();
    let managed = reviewed.request.host.data_dir.clone();
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(managed.join("config.toml"), "# changed by the operator\n").unwrap();
    let handoff = ProfileHandoff {
        answers: reviewed.request.answers.clone(),
        host: reviewed.request.host.clone(),
        run_root: reviewed.request.run_root.clone(),
        components: Vec::new(),
        core: reviewed.request.core.clone(),
        base_sig: reviewed.base_sig.clone(),
        plan_sig: reviewed.plan_sig.clone(),
    };
    let error = veles_install::privileged_handoff(&handoff, &BTreeMap::new()).unwrap_err();
    assert!(
        format!("{error:#}").contains("changed after preview"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read_to_string(managed.join("config.toml")).unwrap(),
        "# changed by the operator\n"
    );
    assert!(!managed.join("install/current").exists());
}

fn component(root: &Path) -> veles_install::ComponentSpec {
    let artifact = root.join("fixture-plugin");
    std::fs::write(&artifact, "#!/bin/sh\nif [ \"$1\" = --describe ]; then\nprintf '%s\\n' '{\"name\":\"fixture\",\"version\":\"1\",\"protocol\":1,\"config_sections\":[\"platforms.fixture\"],\"knobs\":[{\"path\":\"platforms.fixture.server\",\"default\":\"\\\"one\\\"\",\"shape\":\"value\",\"note\":null,\"env\":null}]}'\nfi\nexit 0\n").unwrap();
    std::fs::set_permissions(
        &artifact,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    veles_install::ComponentSpec {
        name: "fixture".into(),
        kind: veles_install::ComponentKind::ProcessPlugin,
        manifest: veles_types::plugin::Manifest {
            name: "fixture".into(),
            version: "1".into(),
            protocol: veles_types::plugin::PROTOCOL,
            config_sections: vec!["platforms.fixture".into()],
            knobs: vec![veles_types::Knob {
                path: "platforms.fixture.server".into(),
                default: Some("\"one\"".into()),
                shape: veles_types::Shape::Value,
                note: None,
                env: None,
            }],
            ..Default::default()
        },
        source: Some(artifact),
        describe_error: None,
    }
}

#[test]
fn a_component_is_installed_from_its_signed_request_spec() {
    let temp = tempfile::tempdir().unwrap();
    let service = InstallService::new(Paths::for_roots(
        temp.path().join("config"),
        temp.path().join("data"),
    ));
    bundle(temp.path());
    let mut answers = ProfileAnswers::default();
    answers.components.push("fixture".into());
    answers.options.insert(
        "fixture".into(),
        BTreeMap::from([("platforms.fixture.server".into(), serde_json::json!("two"))]),
    );
    let request = || ProfileRequest {
        answers: answers.clone(),
        host: layout(temp.path()),
        run_root: temp.path().to_path_buf(),
        components: vec![component(temp.path())],
        core: temp.path().join("source-bundle"),
    };
    let reviewed = service.preview_profile(request()).unwrap();
    let result = service
        .apply_profile(
            request(),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(result["applied"], true);
    let config = std::fs::read_to_string(temp.path().join("config/config.toml")).unwrap();
    assert!(config.contains("server = \"two\""), "{config}");
    let manifest =
        std::fs::read_to_string(temp.path().join("data/process-plugins/fixture/plugin.toml"))
            .unwrap();
    assert!(
        manifest.contains("install/components/fixture"),
        "{manifest}"
    );
    assert!(service.verify().unwrap()["ok"].as_bool().unwrap());
}

#[test]
fn container_host_prepares_the_typed_script_without_native_service_steps() {
    use veles_install::container_host as ch;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let image = ch::PinnedImage::new(
        "docker.io/library/alpine:3.20",
        &format!("sha256:{}", "a".repeat(64)),
    )
    .unwrap();
    let gateway = ch::RootlessUser::system("gw", root.join("storage/gw"));
    let sandbox = ch::RootlessUser::system("sbx", root.join("storage/sbx"));
    let mut options = ch::ContainerHostOptions::new(
        ch::StorageOptions::new(root.join("storage")),
        gateway,
        sandbox,
    );
    options.network = Some("velesnet".into());
    options.quadlets.push(ch::QuadletSpec {
        user: "gw".into(),
        name: "gateway".into(),
        container: ch::ContainerSpec {
            description: "gateway fixture".into(),
            image,
            extra: Vec::new(),
        },
    });
    let mut answers = ProfileAnswers {
        profile: NamedProfile::ContainerHost,
        ..Default::default()
    };
    answers.host.scope = ServiceScope::User;
    answers.host.container = Some(options);
    let mut host = layout(root);
    host.config_dir = host.data_dir.clone();
    host.write_root = host.data_dir.clone();
    host.bin = host.data_dir.join("install/current/bin/veles");
    let request = || ProfileRequest {
        answers: answers.clone(),
        host: host.clone(),
        run_root: host.write_root.clone(),
        components: Vec::new(),
        core: PathBuf::new(),
    };
    let service = InstallService::new(Paths::for_roots(
        root.join("caller-config"),
        root.join("caller-data"),
    ));
    let preview = service.preview_profile(request()).unwrap();
    assert!(preview.plan.system.iter().all(|s| matches!(
        s,
        veles_install::profiles::SystemStep::EmbeddedScript { .. }
    )));
    assert!(preview.plan.user.is_empty());
    let result = service
        .apply_profile(
            request(),
            &preview.base_sig,
            &preview.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(result["status"], "handoff-required");
    assert_eq!(result["applied"], false);
    assert_eq!(result["privileged_script"]["executed"], false);
    assert!(result["privileged_script"]["text"]
        .as_str()
        .unwrap()
        .contains("useradd"));
    assert!(!root.join("config/config.toml").exists());
    assert!(!root.join("data/install.toml").exists());
}

#[test]
fn nixos_only_emits_a_snippet_and_never_writes_a_host_root() {
    let temp = tempfile::tempdir().unwrap();
    let service = InstallService::new(Paths::for_roots(
        temp.path().join("caller/config"),
        temp.path().join("caller/data"),
    ));
    let mut host = layout(temp.path());
    host.target = Target::Nixos;
    let answers = ProfileAnswers {
        profile: NamedProfile::Nixos,
        ..Default::default()
    };
    let request = || ProfileRequest {
        answers: answers.clone(),
        host: host.clone(),
        run_root: temp.path().join("caller/staging"),
        components: Vec::new(),
        core: PathBuf::new(),
    };
    let reviewed = service.preview_profile(request()).unwrap();
    let result = service
        .apply_profile(
            request(),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(result["status"], "emitted");
    assert_eq!(result["applied"], false);
    assert!(result["emitted"][0]["contents"]
        .as_str()
        .unwrap()
        .contains("services.veles"));
    // A system service cannot use the invoking account's home: the snippet
    // leaves the vault at the module's system default.
    let snippet = result["emitted"][0]["contents"].as_str().unwrap();
    // The module asserts a system gateway has settings or an environment
    // file; the emitted snippet must evaluate, so it names one.
    assert!(
        snippet
            .lines()
            .any(|line| line.trim_start().starts_with("environmentFile = ")),
        "{snippet}"
    );
    assert!(
        !snippet
            .lines()
            .any(|line| line.trim_start().starts_with("vault.identityDir")),
        "{snippet}"
    );
    assert!(!temp.path().join("config").exists());
    assert!(!temp.path().join("data").exists());
}

#[test]
fn a_changed_staged_unit_is_refused_before_the_first_write() {
    let temp = tempfile::tempdir().unwrap();
    let service = InstallService::new(Paths::for_roots(
        temp.path().join("config"),
        temp.path().join("data"),
    ));
    let reviewed = service
        .preview_profile({
            bundle(temp.path());
            system_request(temp.path())
        })
        .unwrap();
    let staged_unit = reviewed
        .plan
        .user
        .iter()
        .find_map(|step| match step {
            veles_install::profiles::Step::UserFile { path, .. }
                if path.ends_with("veles-gateway.service") =>
            {
                Some(path.clone())
            }
            _ => None,
        })
        .unwrap();
    std::fs::create_dir_all(staged_unit.parent().unwrap()).unwrap();
    std::fs::write(&staged_unit, "changed outside the reviewed plan\n").unwrap();
    let error = service
        .apply_profile(
            system_request(temp.path()),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("changed after preview"),
        "{error:#}"
    );
    assert!(!temp.path().join("data/config.toml").exists());
    assert!(!temp.path().join("data/install/current").exists());
}

#[test]
fn unused_answers_are_refused_not_silently_ignored() {
    let temp = tempfile::tempdir().unwrap();
    bundle(temp.path());
    // A server whose answers name a target root it would never use.
    let mut server = system_request(temp.path());
    server.answers.host.target_root = Some("/srv/elsewhere".into());
    let error = server.review().unwrap_err();
    assert!(
        format!("{error:#}").contains("does not accept host.target_root"),
        "{error:#}"
    );
    // A container host whose answers name a native account.
    let mut container = container_request(temp.path());
    container.answers.host.user = Some("someone".into());
    let error = container.review().unwrap_err();
    assert!(
        format!("{error:#}").contains("does not accept host.user"),
        "{error:#}"
    );
    // A desktop whose answers carry container options.
    let mut desktop = request(temp.path());
    desktop.answers.host.container = container_request(temp.path()).answers.host.container;
    let error = desktop.review().unwrap_err();
    assert!(
        format!("{error:#}").contains("does not accept host.container"),
        "{error:#}"
    );
    // A server carrying host components: their own request, never dropped.
    let mut server = system_request(temp.path());
    server.answers.host.host_components =
        vec![veles_install::host_components::HostComponent::Media(
            veles_install::host_components::MediaOptions {
                public_url: "https://media.example".into(),
                publish_dir: "/srv/veles/media".into(),
                gateway_user: veles_install::current_user_name().unwrap(),
                acme_email: "acme@example".into(),
                max_age_days: None,
            },
        )];
    let error = server.review().unwrap_err();
    assert!(format!("{error:#}").contains("host-component"), "{error:#}");
}

#[test]
fn an_unknown_answer_field_is_refused() {
    let error = serde_json::from_value::<ProfileAnswers>(serde_json::json!({
        "profile": "server",
        "host": {"user": "veles", "scope": "system", "taget_root": "/x"}
    }))
    .unwrap_err();
    assert!(error.to_string().contains("taget_root"), "{error}");
    let error = serde_json::from_value::<ProfileAnswers>(serde_json::json!({
        "profile": "desktop", "componnets": ["x"]
    }))
    .unwrap_err();
    assert!(error.to_string().contains("componnets"), "{error}");
}

fn container_request(root: &Path) -> ProfileRequest {
    use veles_install::container_host as ch;
    let image = ch::PinnedImage::new(
        "docker.io/library/alpine:3.20",
        &format!("sha256:{}", "a".repeat(64)),
    )
    .unwrap();
    let mut options = ch::ContainerHostOptions::new(
        ch::StorageOptions::new(root.join("storage")),
        ch::RootlessUser::system("gw", root.join("storage/gw")),
        ch::RootlessUser::system("sbx", root.join("storage/sbx")),
    );
    options.network = Some("velesnet".into());
    options.quadlets.push(ch::QuadletSpec {
        user: "gw".into(),
        name: "gateway".into(),
        container: ch::ContainerSpec {
            description: "gateway fixture".into(),
            image,
            extra: Vec::new(),
        },
    });
    let mut answers = ProfileAnswers {
        profile: NamedProfile::ContainerHost,
        ..Default::default()
    };
    answers.host.scope = ServiceScope::User;
    answers.host.container = Some(options);
    let mut host = layout(root);
    host.config_dir = host.data_dir.clone();
    host.write_root = host.data_dir.clone();
    host.bin = host.data_dir.join("install/current/bin/veles");
    ProfileRequest {
        answers,
        host: host.clone(),
        run_root: host.write_root.clone(),
        components: Vec::new(),
        core: PathBuf::new(),
    }
}

fn staged_system_handoff_with_component(root: &Path) -> (ProfileHandoff, PathBuf) {
    let caller = InstallService::new(Paths::for_roots(
        root.join("caller/config"),
        root.join("caller/data"),
    ));
    bundle(root);
    let make = || {
        let mut request = system_request(root);
        request.answers.components = vec!["fixture".into()];
        request.components = vec![component(root)];
        request
    };
    let reviewed = caller.preview_profile(make()).unwrap();
    let staged = caller
        .apply_profile(
            make(),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    let path = PathBuf::from(staged["request_path"].as_str().unwrap());
    (
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap(),
        path,
    )
}

#[test]
fn the_cli_handoff_cannot_substitute_a_component_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let (handoff, _) = staged_system_handoff_with_component(temp.path());
    // 1. The request names another artifact than the reviewed one.
    let other = temp.path().join("other-plugin");
    std::fs::write(&other, "#!/bin/sh\necho substituted\n").unwrap();
    let mut substituted = handoff.clone();
    substituted.components[0].source = Some(other);
    let error = veles_install::privileged_handoff(&substituted, &BTreeMap::new()).unwrap_err();
    assert!(
        format!("{error:#}").contains("changed after preview"),
        "{error:#}"
    );
    // 2. The reviewed path now holds other bytes.
    let artifact = handoff.components[0].source.clone().unwrap();
    let original = std::fs::read(&artifact).unwrap();
    std::fs::write(&artifact, [original.as_slice(), b"\n# appended\n"].concat()).unwrap();
    let error = veles_install::privileged_handoff(&handoff, &BTreeMap::new()).unwrap_err();
    assert!(
        format!("{error:#}").contains("changed after preview"),
        "{error:#}"
    );
    // Neither attempt installed anything.
    assert!(!handoff.host.data_dir.join("install/components").exists());
    assert!(!handoff.host.data_dir.join("config.toml").exists());
}

#[test]
fn the_cli_handoff_cannot_substitute_a_host_component() {
    let temp = tempfile::tempdir().unwrap();
    let service = InstallService::new(Paths::for_roots(
        temp.path().join("config"),
        temp.path().join("data"),
    ));
    let make = || {
        let mut request = request(temp.path());
        request.core = PathBuf::new();
        request.answers.host.host_components =
            vec![veles_install::host_components::HostComponent::Media(
                veles_install::host_components::MediaOptions {
                    public_url: "https://media.example".into(),
                    publish_dir: "/srv/veles/media".into(),
                    gateway_user: veles_install::current_user_name().unwrap(),
                    acme_email: "acme@example".into(),
                    max_age_days: None,
                },
            )];
        request
    };
    let reviewed = service.preview_profile(make()).unwrap();
    assert!(
        reviewed.plan.user.is_empty(),
        "a host-component request has no native steps"
    );
    let staged = service
        .apply_profile(
            make(),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    let mut handoff: ProfileHandoff =
        serde_json::from_slice(&std::fs::read(staged["request_path"].as_str().unwrap()).unwrap())
            .unwrap();
    let veles_install::host_components::HostComponent::Media(media) =
        &mut handoff.answers.host.host_components[0]
    else {
        unreachable!()
    };
    media.publish_dir = "/var/www/elsewhere".into();
    let error = veles_install::privileged_handoff(&handoff, &BTreeMap::new()).unwrap_err();
    assert!(
        format!("{error:#}").contains("changed after preview"),
        "{error:#}"
    );
}

#[test]
fn a_failed_system_handoff_restores_its_unit_and_removes_what_it_created() {
    let temp = tempfile::tempdir().unwrap();
    let caller = InstallService::new(Paths::for_roots(
        temp.path().join("caller/config"),
        temp.path().join("caller/data"),
    ));
    bundle(temp.path());
    let reviewed = caller.preview_profile(system_request(temp.path())).unwrap();
    let staged = caller
        .apply_profile(
            system_request(temp.path()),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    let handoff: ProfileHandoff =
        serde_json::from_slice(&std::fs::read(staged["request_path"].as_str().unwrap()).unwrap())
            .unwrap();
    let unit = handoff.host.system_unit_dir.join("veles-gateway.service");
    veles_install::fail_after_system_steps_next_for_test();
    let error = veles_install::privileged_handoff(&handoff, &BTreeMap::new()).unwrap_err();
    assert!(format!("{error:#}").contains("restored"), "{error:#}");
    assert!(!unit.exists(), "the privileged unit file is removed again");
    assert!(!handoff.host.data_dir.join("config.toml").exists());
    assert!(!handoff.host.data_dir.join("install.toml").exists());
    assert!(
        !handoff.host.data_dir.join("install.toml.bak").exists(),
        "a failed first install leaves no record backup"
    );
    assert!(!handoff.host.data_dir.join("install/releases").exists());
    assert!(std::fs::symlink_metadata(handoff.host.data_dir.join("install/current")).is_err());
}

#[test]
fn a_failed_container_script_restores_the_host_files_it_rendered() {
    let temp = tempfile::tempdir().unwrap();
    let existing = temp.path().join("etc/tmpfiles.d/veles.conf");
    let fresh = temp
        .path()
        .join("home/gw/.config/containers/systemd/gateway.container");
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::fs::write(&existing, "# the operator's previous file\n").unwrap();
    let paths = vec![existing.clone(), fresh.clone()];
    let error = veles_install::run_reviewed_container_script(&["fixture".into()], &paths, |_| {
        std::fs::write(&existing, "rendered\n").unwrap();
        std::fs::create_dir_all(fresh.parent().unwrap()).unwrap();
        std::fs::write(&fresh, "rendered\n").unwrap();
        anyhow::bail!("the script stopped at step 7")
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("not reversed"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(&existing).unwrap(),
        "# the operator's previous file\n"
    );
    assert!(!fresh.exists());
    // Success keeps what the script wrote.
    veles_install::run_reviewed_container_script(&["fixture".into()], &paths, |_| {
        std::fs::write(&existing, "rendered\n").unwrap();
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "rendered\n");
}

#[test]
fn a_system_handoff_writes_the_reviewed_unit_when_no_staged_copy_exists() {
    // Production layout: the unit's logical destination is outside the
    // staging root, so its staged copy (for the printed script) was never
    // written. The reviewed contents are what the handoff installs.
    let temp = tempfile::tempdir().unwrap();
    let caller = InstallService::new(Paths::for_roots(
        temp.path().join("caller/config"),
        temp.path().join("caller/data"),
    ));
    bundle(temp.path());
    let make = || {
        let mut request = system_request(temp.path());
        request.host.write_root = temp.path().join("elsewhere");
        request
    };
    let reviewed = caller.preview_profile(make()).unwrap();
    let staged = caller
        .apply_profile(
            make(),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    let handoff: ProfileHandoff =
        serde_json::from_slice(&std::fs::read(staged["request_path"].as_str().unwrap()).unwrap())
            .unwrap();
    let result = veles_install::privileged_handoff(&handoff, &BTreeMap::new()).unwrap();
    assert_eq!(result["applied"], true);
    let unit = std::fs::read_to_string(handoff.host.system_unit_dir.join("veles-gateway.service"))
        .unwrap();
    assert!(unit.contains("gateway run"));
}

#[test]
fn a_system_handoff_writes_provider_secrets_into_the_services_env() {
    // The root handoff must not keep a system service's credentials in
    // its own vault: the service account reads only its VELES_HOME.
    let (_config, _data, _env) = veles_core::testenv::scratch_xdg();
    let temp = tempfile::tempdir().unwrap();
    let caller = InstallService::new(Paths::for_roots(
        temp.path().join("caller/config"),
        temp.path().join("caller/data"),
    ));
    bundle(temp.path());
    let reviewed = caller.preview_profile(system_request(temp.path())).unwrap();
    let name = reviewed.review["secrets"][0]
        .as_str()
        .expect("the default setup names a provider key")
        .to_string();
    let staged = caller
        .apply_profile(
            system_request(temp.path()),
            &reviewed.base_sig,
            &reviewed.plan_sig,
            &BTreeMap::new(),
        )
        .unwrap();
    let handoff: ProfileHandoff =
        serde_json::from_slice(&std::fs::read(staged["request_path"].as_str().unwrap()).unwrap())
            .unwrap();
    let secrets = BTreeMap::from([(name.clone(), "fixture-provider-key".to_string())]);
    veles_install::privileged_handoff(&handoff, &secrets).unwrap();
    let env = std::fs::read_to_string(handoff.host.data_dir.join(".env")).unwrap();
    assert!(
        env.contains(&format!("{name}=fixture-provider-key")),
        "{env}"
    );
    assert!(
        !veles_core::vault::local_values().contains_key(&name),
        "the root process's own vault must not receive the service's key"
    );
}
