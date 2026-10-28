//! Non-Unix stand-in for `ownership.rs`. Privileged host apply (system
//! server handoff, host components) is Linux-only; previews and the
//! unprivileged profiles still build and run on Windows. Every function
//! that would change ownership refuses with that reason instead of
//! pretending to have done it.
use anyhow::Result;
use std::path::Path;

const LINUX_ONLY: &str =
    "privileged host apply (system services, host components, account ownership) is Linux-only";

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Account {
    pub uid: u32,
    pub gid: u32,
    pub home: std::path::PathBuf,
}

#[allow(dead_code)]
impl Account {
    pub fn container_uid_10001(&self, _user: &str, _subuid_file: &Path) -> Result<u32> {
        anyhow::bail!(LINUX_ONLY)
    }

    pub fn container_gid_10001(&self, _user: &str, _subgid_file: &Path) -> Result<u32> {
        anyhow::bail!(LINUX_ONLY)
    }
}

pub fn process_account() -> Account {
    Account {
        uid: 0,
        gid: 0,
        home: std::path::PathBuf::from("/"),
    }
}

pub fn account(_user: &str, _group: Option<&str>) -> Result<Account> {
    anyhow::bail!(LINUX_ONLY)
}

pub fn ensure_dir(_path: &Path, _mode: u32, _owner: &Account) -> Result<()> {
    anyhow::bail!(LINUX_ONLY)
}

pub fn chown(_path: &Path, _owner: &Account) -> Result<()> {
    anyhow::bail!(LINUX_ONLY)
}

pub fn chown_link(_path: &Path, _owner: &Account) -> Result<()> {
    anyhow::bail!(LINUX_ONLY)
}

pub fn chown_reviewed_tree(_root: &Path, _owner: &Account) -> Result<()> {
    anyhow::bail!(LINUX_ONLY)
}
