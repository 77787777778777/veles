use veles_install::{Action, Status};

#[cfg(unix)]
#[test]
fn ownership_assignment_never_follows_a_managed_symlink() {
    use std::os::unix::fs::MetadataExt;
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    std::fs::write(&outside, "keep").unwrap();
    let link = temp.path().join("managed");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let owner =
        veles_install::test_account(veles_install::current_user_name().unwrap().as_str(), None)
            .unwrap();
    let error = veles_install::test_chown_managed(&link, &owner).unwrap_err();
    assert!(format!("{error:#}").contains("non-regular"), "{error:#}");
    assert_eq!(
        std::fs::metadata(&outside).unwrap().uid(),
        std::fs::metadata(&link).unwrap().uid()
    );
}

#[cfg(unix)]
#[test]
fn reviewed_tree_ownership_handles_links_without_touching_their_target() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("bin/real"), "keep").unwrap();
    std::os::unix::fs::symlink("real", root.join("bin/link")).unwrap();
    let owner =
        veles_install::test_account(veles_install::current_user_name().unwrap().as_str(), None)
            .unwrap();
    veles_install::test_chown_reviewed_tree(&root, &owner).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("bin/real")).unwrap(),
        "keep"
    );
    assert!(std::fs::symlink_metadata(root.join("bin/link"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(unix)]
#[test]
fn rootless_secret_ownership_uses_the_accounts_subuid_range() {
    let temp = tempfile::tempdir().unwrap();
    let subuid = temp.path().join("subuid");
    let user = veles_install::current_user_name().unwrap();
    std::fs::write(
        &subuid,
        format!("other:100000:65536\n{user}:200000:65536\n"),
    )
    .unwrap();
    let account = veles_install::test_account(&user, None).unwrap();
    assert_eq!(account.container_uid_10001(&user, &subuid).unwrap(), 210000);
    let subgid = temp.path().join("subgid");
    std::fs::write(&subgid, format!("{user}:300000:65536\n")).unwrap();
    assert_eq!(account.container_gid_10001(&user, &subgid).unwrap(), 310000);
}

#[cfg(unix)]
#[test]
fn typed_system_link_refuses_a_foreign_target_or_symlinked_parent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("host");
    std::fs::create_dir_all(root.join("etc/nginx/sites-enabled")).unwrap();
    let step = veles_install::profiles::SystemStep::Link {
        target: "/etc/nginx/sites-available/veles-pds.conf".into(),
        link: "/etc/nginx/sites-enabled/veles-pds.conf".into(),
    };
    step.apply_link(&root).unwrap();
    step.apply_link(&root).unwrap();
    let full = root.join("etc/nginx/sites-enabled/veles-pds.conf");
    assert_eq!(
        std::fs::read_link(&full).unwrap(),
        std::path::PathBuf::from("/etc/nginx/sites-available/veles-pds.conf")
    );
    std::fs::remove_file(&full).unwrap();
    std::fs::write(&full, "foreign").unwrap();
    assert!(step.apply_link(&root).is_err());
    assert_eq!(std::fs::read_to_string(&full).unwrap(), "foreign");
}

#[cfg(unix)]
#[test]
fn managed_directory_creation_refuses_a_symlinked_parent() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let parent = temp.path().join("parent");
    std::os::unix::fs::symlink(&outside, &parent).unwrap();
    let owner =
        veles_install::test_account(veles_install::current_user_name().unwrap().as_str(), None)
            .unwrap();
    let error =
        veles_install::test_ensure_managed_dir(&parent.join("managed"), 0o700, &owner).unwrap_err();
    assert!(
        format!("{error:#}").contains("opening managed directory"),
        "{error:#}"
    );
    assert!(!outside.join("managed").exists());
}

#[test]
fn ordinary_apply_cannot_execute_a_privileged_command() {
    let root = tempfile::tempdir().unwrap();
    let action = Action::Command {
        argv: vec!["/bin/sh".into(), "-c".into(), "exit 99".into()],
        as_user: None,
        privileged: true,
    };
    let error = action.apply(root.path()).unwrap_err().to_string();
    assert!(
        error.contains("no executor"),
        "unexpected ordinary apply result: {error}"
    );
}

#[test]
fn privileged_executor_refuses_an_unmarked_command() {
    let root = tempfile::tempdir().unwrap();
    let action = Action::Command {
        argv: vec!["/bin/sh".into(), "-c".into(), "exit 99".into()],
        as_user: None,
        privileged: false,
    };
    let error = action
        .apply_privileged(root.path())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("not marked privileged"),
        "unexpected command refusal: {error}"
    );
}

#[test]
fn privileged_executor_does_not_silently_overwrite_a_changed_file() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("unit");
    std::fs::write(&destination, b"old").unwrap();
    let action = Action::WriteFile {
        path: "unit".into(),
        contents: b"reviewed".to_vec(),
        mode: Some(0o644),
        expected_sha256: None,
    };
    assert!(action
        .apply_privileged(root.path())
        .unwrap_err()
        .to_string()
        .contains("different content"));
    assert_eq!(std::fs::read(&destination).unwrap(), b"old");
}

#[test]
fn privileged_executor_can_write_the_reviewed_file_under_a_test_root() {
    let root = tempfile::tempdir().unwrap();
    let action = Action::WriteFile {
        path: "etc/veles-test".into(),
        contents: b"reviewed\n".to_vec(),
        mode: Some(0o600),
        expected_sha256: None,
    };
    assert_eq!(action.apply_privileged(root.path()).unwrap(), Status::Done);
    assert_eq!(
        std::fs::read(root.path().join("etc/veles-test")).unwrap(),
        b"reviewed\n"
    );
}
