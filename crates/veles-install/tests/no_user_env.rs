//! Its own test binary: it removes USER and LOGNAME from the process
//! environment, which no other test may observe. A service shell, a
//! container and the Nix build sandbox all run without USER.
#![cfg(unix)]

#[test]
fn a_profile_resolves_its_account_without_user_in_the_environment() {
    // This binary has exactly one test: no other thread reads the
    // environment while it changes.
    std::env::remove_var("USER");
    std::env::remove_var("LOGNAME");
    let name = veles_install::current_user_name().expect("the effective uid has a passwd entry");
    assert!(!name.is_empty());
    let temp = tempfile::tempdir().unwrap();
    let paths =
        veles_install::Paths::for_roots(temp.path().join("config"), temp.path().join("data"));
    let answers = veles_install::ProfileAnswers::default();
    let layout = veles_install::native_layout(&answers, &paths).unwrap();
    assert_eq!(layout.user.as_deref(), Some(name.as_str()));
}
