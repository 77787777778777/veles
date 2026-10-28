//! Named configuration and state profiles for multi-machine setups.
//!
//! Selection is, in order: `--profile` (the CLI sets `VELES_PROFILE` before
//! boot), the environment, an existing profile matching the hostname, then
//! `default`. The default keeps the original XDG/forced-home layout. Every
//! other profile lives under `<state base>/profiles/<name>`; config, state,
//! skills, plugins, commands and cron resolve there through `config_dir()`
//! and `data_dir()`. It never inherits the default's or Hermes' writable
//! configuration. Shipped skills remain a read-only common base.

use std::path::PathBuf;

/// The active profile name. Boot code should call [`validate_startup`]
/// before opening anything so an invalid name is reported, not hidden.
pub fn active_profile() -> String {
    selected_profile().unwrap_or_else(|e| format!("invalid:{e}"))
}

/// Resolve the selected profile without sanitizing a bad name into another
/// profile's path. `a/b`, `a\b` and `a-b` must never alias one another.
pub fn selected_profile() -> Result<String, String> {
    if let Ok(p) = std::env::var("VELES_PROFILE") {
        if !p.is_empty() {
            return validate_name(&p).map(|()| p);
        }
    }
    let host = hostname();
    let root = crate::config::unscoped_data_root().join("profiles");
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok("default".to_string());
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !entry.path().is_dir() || validate_name(&name).is_err() {
            continue;
        }
        if name.eq_ignore_ascii_case(&host) {
            return Ok(name);
        }
        if std::fs::read_to_string(entry.path().join(".hostname"))
            .is_ok_and(|h| h.trim().eq_ignore_ascii_case(&host))
        {
            return Ok(name);
        }
    }
    Ok("default".to_string())
}

/// The CLI's pre-boot validation: no config, credentials or store have been
/// opened yet, so rejecting a path-like profile cannot leak the default's
/// state into the requested one.
pub fn validate_startup() -> anyhow::Result<()> {
    selected_profile().map(|_| ()).map_err(anyhow::Error::msg)
}

/// The default local desktop API port for this profile. The default
/// profile keeps 8642. A named profile gets its own deterministic port,
/// so two profiles cannot accidentally adopt each other's gateway.
/// VELES_DESKTOP_PORT or [platforms.desktop.extra] port overrides this.
pub fn default_desktop_port() -> u16 {
    match selected_profile() {
        Ok(name) if name != "default" => {
            let hash = name
                .bytes()
                .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
            10000 + (hash % 50000) as u16
        }
        _ => 8642,
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid profile {name:?}: use one directory name of ASCII letters, digits, '-', '_' or '.', not a path"
        ))
    }
}

/// The selected profile's state root. This is a path query only: a new
/// profile is created by the normal directory/store bootstrap, not by
/// reading another profile's files.
pub fn profile_dir() -> PathBuf {
    crate::config::data_dir()
}

/// List the existing named profiles and whether each is selected. The
/// default is always present because its original root already exists.
pub fn list_profiles() -> Vec<(String, bool)> {
    let active = active_profile();
    let mut out = vec![("default".to_string(), active == "default")];
    if let Ok(entries) = std::fs::read_dir(crate::config::unscoped_data_root().join("profiles")) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if validate_name(name).is_ok() && name != "default" {
                        out.push((name.to_string(), name == active));
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The machine's hostname, through the shared platform implementation.
/// On Windows this is COMPUTERNAME, not a hard-coded localhost.
fn hostname() -> String {
    crate::platform::hostname()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;

    fn select(name: &str) -> crate::config::DirGuard {
        std::env::set_var("VELES_PROFILE", name);
        crate::config::set_dirs(None)
    }

    /// Real XDG selection, with NO DirOverride: the override is a test
    /// convenience for the default paths and would bypass profiles.
    fn scratch() -> (tempfile::TempDir, tempfile::TempDir, EnvGuard) {
        let g = crate::testenv::guard();
        let cfg = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::env::remove_var("VELES_HOME");
        std::env::set_var("XDG_CONFIG_HOME", cfg.path());
        std::env::set_var("XDG_DATA_HOME", data.path());
        std::env::remove_var("VELES_PROFILE");
        (cfg, data, g)
    }

    fn default_root(c: &tempfile::TempDir, _d: &tempfile::TempDir) -> PathBuf {
        c.path().join("veles")
    }

    #[test]
    fn selection_and_paths_keep_the_default_layout() {
        let (_c, _d, _g) = scratch();
        assert_eq!(selected_profile().unwrap(), "default");
        assert_eq!(profile_dir(), crate::config::data_dir());
        assert_eq!(crate::config::config_dir(), default_root(&_c, &_d));
        let expected_cache = std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty() && std::path::Path::new(v).is_absolute())
            .map(|v| PathBuf::from(v).join("veles"))
            .or_else(|| {
                cfg!(windows)
                    .then(|| std::env::var_os("LOCALAPPDATA"))
                    .flatten()
                    .filter(|v| !v.is_empty() && std::path::Path::new(v).is_absolute())
                    .map(|v| PathBuf::from(v).join("veles").join("cache"))
            })
            .unwrap_or_else(|| {
                directories::UserDirs::new()
                    .map(|d| d.home_dir().join(".cache").join("veles"))
                    .unwrap_or_else(|| PathBuf::from(".cache").join("veles"))
            });
        assert_eq!(crate::config::cache_dir(), expected_cache);
    }

    #[test]
    #[cfg(windows)]
    fn hostname_profile_selection_uses_the_windows_computer_name() {
        let (_c, _d, _g) = scratch();
        let old = std::env::var_os("COMPUTERNAME");
        std::env::set_var("COMPUTERNAME", "Winbox");
        std::fs::create_dir_all(crate::config::unscoped_data_root().join("profiles/winbox"))
            .unwrap();
        assert_eq!(hostname(), "Winbox");
        assert_eq!(selected_profile().unwrap(), "winbox");
        match old {
            Some(value) => std::env::set_var("COMPUTERNAME", value),
            None => std::env::remove_var("COMPUTERNAME"),
        }
    }

    #[test]
    fn named_profiles_have_a_separate_default_gateway_port() {
        let (_c, _d, _g) = scratch();
        // The default is what this test measures. An operator override
        // or another test's backup-port fixture must not decide it.
        std::env::remove_var("VELES_DESKTOP_PORT");
        assert_eq!(default_desktop_port(), 8642);
        let _p = select("laptop-test");
        assert_ne!(default_desktop_port(), 8642);
        assert_eq!(default_desktop_port(), default_desktop_port());
        let _other = select("other");
        assert_ne!(default_desktop_port(), 8642);
        assert_eq!(
            crate::config::desktop_port(&crate::config::VelesConfig::default()),
            default_desktop_port()
        );
    }

    #[test]
    fn a_named_profile_is_one_shared_config_and_state_root() {
        let (_c, _d, _g) = scratch();
        let _p = select("laptop-test");
        let root = crate::config::unscoped_data_root().join("profiles/laptop-test");
        assert_eq!(selected_profile().unwrap(), "laptop-test");
        assert_eq!(crate::config::config_dir(), root);
        assert_eq!(crate::config::data_dir(), root);
        assert_eq!(crate::config::cache_dir(), root.join("cache"));
        assert_eq!(profile_dir(), root);
        assert!(!root.join("profiles/laptop-test").exists());
        assert_eq!(crate::config::hermes_config_yaml(), None);
    }

    #[test]
    fn explicit_profiles_work_before_their_directory_exists() {
        let (_c, _d, _g) = scratch();
        let _p = select("fresh-profile");
        assert_eq!(active_profile(), "fresh-profile");
        assert!(crate::config::data_dir().ends_with("profiles/fresh-profile"));
        assert_eq!(list_profiles(), vec![("default".into(), false)]);
    }

    #[test]
    fn existing_profiles_are_listed_and_marked_active() {
        let (_c, _d, _g) = scratch();
        std::fs::create_dir_all(crate::config::unscoped_data_root().join("profiles/laptop-test"))
            .unwrap();
        let _p = select("laptop-test");
        assert_eq!(
            list_profiles(),
            vec![("default".into(), false), ("laptop-test".into(), true)]
        );
    }

    #[test]
    fn profile_names_cannot_escape_or_alias_a_path() {
        let (_c, _d, _g) = scratch();
        for name in [
            "",
            ".",
            "..",
            ".hidden",
            "../other",
            "a/b",
            "a\\b",
            "two words",
        ] {
            if name.is_empty() {
                continue; // unset-like empty env means hostname/default selection
            }
            let _p = select(name);
            assert!(selected_profile().is_err(), "accepted {name:?}");
            assert!(validate_startup().is_err(), "startup accepted {name:?}");
        }
    }

    #[test]
    fn config_skills_plugins_commands_and_state_do_not_leak_between_profiles() {
        let (c, d, _g) = scratch();
        let default = default_root(&c, &d);
        std::fs::create_dir_all(&default).unwrap();
        std::fs::write(
            default.join("config.toml"),
            "[model]\ndefault = 'default-model'\n",
        )
        .unwrap();
        assert_eq!(
            crate::config::load().unwrap().model.default.as_deref(),
            Some("default-model")
        );
        crate::store::Store::open(&default.join("state.db")).unwrap();

        let _p = select("other");
        let root = crate::config::data_dir();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("config.toml"),
            "[model]\ndefault = 'other-model'\n",
        )
        .unwrap();
        assert_eq!(
            crate::config::load().unwrap().model.default.as_deref(),
            Some("other-model")
        );
        for (name, body) in [
            (
                "skills/default-only/SKILL.md",
                "---\ndescription: default\n---\n",
            ),
            ("plugins/default-only/plugin.tcl", "# default\n"),
            ("commands/default-only.md", "default\n"),
            ("agents/default-only.md", "default\n"),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        assert!(crate::skills::SkillSet::load()
            .get("default-only")
            .is_some());
        assert!(!crate::config::config_dir()
            .join("config.toml")
            .eq(&default.join("config.toml")));
        crate::store::Store::open(&crate::config::data_dir().join("state.db")).unwrap();
        assert!(!default
            .join("state.db")
            .eq(&crate::config::data_dir().join("state.db")));
        assert!(!default
            .join("cron.json")
            .eq(&crate::config::data_dir().join("cron.json")));

        let _p = select("third");
        assert_eq!(crate::config::load().unwrap().model.default, None);
        assert!(crate::skills::SkillSet::load()
            .get("default-only")
            .is_none());
        assert!(!crate::config::config_dir()
            .join("plugins/default-only")
            .exists());
        assert!(!crate::config::config_dir()
            .join("commands/default-only.md")
            .exists());
        assert!(!crate::config::config_dir()
            .join("agents/default-only.md")
            .exists());
    }

    #[test]
    fn a_forced_home_scopes_profiles_below_it_without_moving_default() {
        let (_h, _g) = crate::testenv::scratch_home();
        let _real_env = crate::config::set_dirs(None);
        assert_eq!(crate::config::config_dir(), crate::config::data_dir());
        let _p = select("other");
        let root = crate::config::unscoped_data_root().join("profiles/other");
        assert_eq!(crate::config::config_dir(), root);
        assert_eq!(crate::config::data_dir(), root);
    }
}
