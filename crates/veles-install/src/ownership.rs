//! Ownership for the explicit native system handoff. Never recursively
//! traverse a caller-controlled tree and never follow a symlink to chown.
use anyhow::{Context, Result};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Account {
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    pub home: std::path::PathBuf,
}

impl Account {
    /// Host uid which maps to container uid 10001 under this account's
    /// subuid range. The range is live host data, never inferred from a
    /// site's current numeric allocation.
    pub fn container_uid_10001(&self, user: &str, subuid_file: &Path) -> Result<libc::uid_t> {
        Self::container_id_10001(user, subuid_file, "subuid")
    }

    pub fn container_gid_10001(&self, user: &str, subgid_file: &Path) -> Result<libc::gid_t> {
        Self::container_id_10001(user, subgid_file, "subgid")
    }

    fn container_id_10001(user: &str, path: &Path, kind: &str) -> Result<libc::uid_t> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the gateway account's {kind} range"))?;
        let mut matches = text.lines().filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let start = fields.next()?.parse::<libc::uid_t>().ok()?;
            let count = fields.next()?.parse::<libc::uid_t>().ok()?;
            (name == user && count >= 10001).then_some(start + 10000)
        });
        let uid = matches.next().with_context(|| format!(
            "account {user} has no {kind} range covering container id 10001; prepare the confined host first"
        ))?;
        anyhow::ensure!(
            matches.next().is_none(),
            "account {user} has ambiguous {kind} ranges"
        );
        Ok(uid)
    }
}

/// The account this process acts as. The privileged handoff runs as root,
/// so host directories it creates (ACME webroots, /etc/wireguard) are
/// root's; a test fixture running unprivileged owns them itself. Never
/// an escalation: it names who already performs the write.
pub fn process_account() -> Account {
    Account {
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        home: std::path::PathBuf::from("/"),
    }
}

pub fn account(user: &str, group: Option<&str>) -> Result<Account> {
    let user = std::ffi::CString::new(user).context("service account contains NUL")?;
    // getpwnam_r avoids a process-global static passwd buffer.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 16 * 1024];
    let rc = unsafe {
        libc::getpwnam_r(
            user.as_ptr(),
            &mut pwd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    anyhow::ensure!(
        rc == 0 && !result.is_null(),
        "service account does not exist"
    );
    // Copy pw_dir before getgrnam_r reuses the same scratch buffer. A
    // pointer kept across that call is not a valid account home.
    anyhow::ensure!(
        !pwd.pw_dir.is_null(),
        "service account has no home directory"
    );
    let home = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) }
        .to_bytes()
        .to_vec();
    anyhow::ensure!(
        !home.is_empty(),
        "service account has an empty home directory"
    );
    let gid = if let Some(group) = group {
        let group = std::ffi::CString::new(group).context("service group contains NUL")?;
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let rc = unsafe {
            libc::getgrnam_r(
                group.as_ptr(),
                &mut grp,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        anyhow::ensure!(rc == 0 && !result.is_null(), "service group does not exist");
        grp.gr_gid
    } else {
        pwd.pw_gid
    };
    let home = std::path::PathBuf::from(std::ffi::OsString::from_vec(home));
    anyhow::ensure!(home.is_absolute(), "service account home is not absolute");
    Ok(Account {
        uid: pwd.pw_uid,
        gid,
        home,
    })
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn open_dir(path: &Path) -> Result<std::os::fd::OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let c_path =
        std::ffi::CString::new(path.as_os_str().as_bytes()).context("path contains NUL")?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(errno())
            .with_context(|| format!("opening managed directory {}", path.display()));
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

fn open_parent(path: &Path) -> Result<(std::os::fd::OwnedFd, std::ffi::CString)> {
    use std::os::unix::ffi::OsStrExt;
    let parent = path.parent().context("managed path needs a parent")?;
    let name = path.file_name().context("managed path needs a filename")?;
    let name = std::ffi::CString::new(name.as_bytes()).context("path contains NUL")?;
    Ok((open_dir(parent)?, name))
}

pub fn ensure_dir(path: &Path, mode: u32, owner: &Account) -> Result<()> {
    anyhow::ensure!(path.is_absolute(), "managed directory must be absolute");
    anyhow::ensure!(
        !path.components().any(|part| matches!(
            part,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )),
        "managed directory is not normalized"
    );
    match std::fs::symlink_metadata(path) {
        Ok(meta) => anyhow::ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "managed path is not a real directory: {}",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let (parent, name) = open_parent(path)?;
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) } != 0 {
                return Err(errno()).with_context(|| format!("creating {}", path.display()));
            }
        }
        Err(e) => return Err(e.into()),
    }
    chown(path, owner)
}

/// Set ownership on the named object itself, never through a symlink.
pub fn chown(path: &Path, owner: &Account) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_file() || metadata.is_dir(),
        "refusing to chown non-regular managed file {}",
        path.display()
    );
    chown_link(path, owner)
}

/// Change the link's own ownership, not its target. The target is owned
/// separately as part of the reviewed payload tree.
pub fn chown_link(path: &Path, owner: &Account) -> Result<()> {
    let (parent, name) = open_parent(path)?;
    let rc = unsafe {
        libc::fchownat(
            parent.as_raw_fd(),
            name.as_ptr(),
            owner.uid,
            owner.gid,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    anyhow::ensure!(
        rc == 0,
        "cannot set ownership for {}: {}",
        path.display(),
        errno()
    );
    Ok(())
}

/// Walk the review-created bundle and set each object's ownership without
/// following links. Bundle validation has already refused special files.
pub fn chown_reviewed_tree(root: &Path, owner: &Account) -> Result<()> {
    chown(root, owner)?;
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let kind = std::fs::symlink_metadata(&path)?.file_type();
        if kind.is_dir() {
            chown_reviewed_tree(&path, owner)?;
        } else if kind.is_symlink() {
            chown_link(&path, owner)?;
        } else {
            chown(&path, owner)?;
        }
    }
    Ok(())
}

use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
