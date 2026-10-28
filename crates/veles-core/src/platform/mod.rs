//! The platform layer: one API, one body per operating system
//! (PLAN-windows.md §1, USER 2026-10-13: "think about how to make it
//! portable rather than two independent codebases").
//!
//! Everything the engine needs from the OS that unix and Windows answer
//! differently lives here, and ONLY here: process liveness and killing,
//! process trees, the local IPC socket, private files, the module
//! loader's view of mapped images, the shell, hidden terminal input,
//! the hostname. A caller elsewhere in core never writes `cfg(unix)`
//! for these; it calls the one name and gets the platform's answer. The
//! unix bodies are the code that was inline before (moved, not
//! rewritten), so Linux behaviour is unchanged by construction.
//!
//! What a body may NOT do: degrade silently. Where Windows has no
//! answer (a signal, `/proc`), the function says so in its type —
//! [`mapped_ranges`] returns `None` for "cannot tell" and the module
//! host keeps the library mapped, which is the safe side; it never
//! returns an empty list that reads as "nothing mapped".

use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// The extension a module or the runtime carries: `so` or `dll`.
pub const DYLIB_EXT: &str = if cfg!(windows) { "dll" } else { "so" };

/// The runtime's file name, as a module's import table names it
/// (`DT_NEEDED` on ELF, the import directory on PE).
pub const RUNTIME_FILE: &str = if cfg!(windows) {
    "veles_runtime.dll"
} else {
    "libveles_runtime.so"
};

/// The section the module key lives in. PE image section names are
/// limited to 8 bytes and `link.exe` truncates longer ones silently,
/// so Windows gets a short spelling; ELF keeps the one every deploy
/// script reads (`readelf -x .veles_module_key`).
pub const KEY_SECTION: &str = if cfg!(windows) {
    ".vmodkey"
} else {
    ".veles_module_key"
};

/// The platform name the prompt, `doctor` and the node row use.
pub const OS: &str = if cfg!(target_os = "android") {
    "android"
} else if cfg!(windows) {
    "windows"
} else if cfg!(target_os = "linux") {
    "linux"
} else if cfg!(target_os = "freebsd") {
    "freebsd"
} else {
    "unix"
};

/// A search-path variable (`VELES_MODULE_PATH`, `VELES_PLUGIN_PATH`),
/// split the way the platform's PATH is: `:` on unix, `;` on Windows,
/// where a `:` split would cut `C:\…` in half.
pub fn split_search_path(var: &str) -> Vec<PathBuf> {
    std::env::var_os(var)
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The ranges the module file at `path` (canonical) is mapped at, or
/// `None` when this platform cannot say. The unload proof reads it:
/// an unknown answer keeps the library mapped.
pub fn mapped_ranges(path: &Path) -> Option<Vec<(usize, usize)>> {
    mapped_ranges_impl(path)
}

/// What the models are told about this node, in one stable line
/// (PLAN-windows.md §6). Stable per process so the prompt prefix stays
/// cacheable: nothing here changes while the gateway runs.
pub struct Facts {
    /// `linux`, `android`, `windows`, `freebsd`.
    pub os: &'static str,
    /// The shell the terminal tool runs, by name (`bash`, `pwsh`, …).
    pub shell: String,
    /// What a model used to Linux reaches for and will not find here.
    pub absent: &'static [&'static str],
    /// One sentence of the things that are different, not missing.
    pub notes: &'static str,
}

impl Facts {
    /// The prompt's line.
    pub fn line(&self) -> String {
        let mut s = format!("Platform: {} (shell: {}).", self.os, self.shell);
        if !self.absent.is_empty() {
            s.push_str(" Not on this node: ");
            s.push_str(&self.absent.join(", "));
            s.push('.');
        }
        if !self.notes.is_empty() {
            s.push(' ');
            s.push_str(self.notes);
        }
        s
    }
}

/// This node's facts. On the phone they depend on whether the userland
/// package answers (`userland::in_use`: settled at the engine's start,
/// so the line stays stable for the process).
pub fn facts() -> Facts {
    let userland = crate::userland::in_use();
    Facts {
        os: OS,
        shell: shell_label(crate::shell::shell_binary()),
        absent: os_absent(userland),
        notes: os_notes(userland),
    }
}

/// The shell's name for the facts line: its file stem — except the
/// userland's ulsh, which the APK ships as `libulsh.so` and whose
/// commands run in Alpine: `ulsh (Alpine)`, not `libulsh`.
pub fn shell_label(binary: &str) -> String {
    if crate::userland::is_ulsh(binary) {
        return "ulsh (Alpine)".to_string();
    }
    Path::new(binary)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| binary.to_string())
}

#[cfg(unix)]
fn os_absent(userland: bool) -> &'static [&'static str] {
    absent_for(OS, userland)
}

#[cfg(unix)]
fn os_notes(userland: bool) -> &'static str {
    notes_for(OS, userland)
}

#[cfg(windows)]
fn os_absent(_userland: bool) -> &'static [&'static str] {
    ABSENT
}

#[cfg(windows)]
fn os_notes(_userland: bool) -> &'static str {
    NOTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_path_splits_like_path_and_drops_empty_entries() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let a = if cfg!(windows) { "C:\\a" } else { "/a" };
        let b = if cfg!(windows) { "D:\\b c" } else { "/b c" };
        std::env::set_var("VELES_TEST_SPLIT", format!("{a}{sep}{sep}{b}{sep}"));
        let got = split_search_path("VELES_TEST_SPLIT");
        std::env::remove_var("VELES_TEST_SPLIT");
        assert_eq!(got, vec![PathBuf::from(a), PathBuf::from(b)]);
        assert!(split_search_path("VELES_TEST_SPLIT_UNSET").is_empty());
    }

    #[test]
    fn the_facts_line_names_the_platform_and_the_shell() {
        let f = facts();
        let line = f.line();
        assert!(
            line.starts_with(&format!("Platform: {OS} (shell: ")),
            "{line}"
        );
        assert!(!f.shell.is_empty());
    }

    /// The phone's facts line names the userland's shell for what it is
    /// (PLAN-phone-userland.md T3 design item 5), not by the APK's file
    /// name.
    #[test]
    fn the_userland_shell_is_named_ulsh_not_libulsh() {
        assert_eq!(
            shell_label("/data/app/~~a==/chat.janitor.veles-b==/lib/arm64/libulsh.so"),
            "ulsh (Alpine)"
        );
        assert_eq!(
            shell_label("/data/user/0/chat.janitor.veles/files/bin/bash"),
            "bash"
        );
        assert_eq!(shell_label("sh"), "sh");
    }

    #[test]
    fn our_own_pid_is_alive_and_an_impossible_one_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX - 1));
    }

    #[test]
    fn the_hostname_is_not_empty() {
        assert!(!hostname().is_empty());
    }

    #[test]
    fn a_private_file_round_trips_and_a_private_dir_exists() {
        let dir = std::env::temp_dir().join(format!("veles-platform-{}", std::process::id()));
        private_dir(&dir).unwrap();
        assert!(dir.is_dir());
        let f = dir.join("secret");
        write_private(&f, b"hush").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"hush");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_file_replaces_an_existing_private_file() {
        let dir = std::env::temp_dir().join(format!("veles-replace-{}", std::process::id()));
        private_dir(&dir).unwrap();
        let path = dir.join("state");
        let staged = dir.join("staged");
        write_private(&path, b"before").unwrap();
        write_private(&staged, b"after").unwrap();
        replace_file(&staged, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
        assert!(!staged.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ipc_round_trips_a_line_between_two_threads() {
        use std::io::{BufRead, BufReader, Write};
        let dir = std::env::temp_dir().join(format!("veles-ipc-{}", std::process::id()));
        private_dir(&dir).unwrap();
        let marker = dir.join("1.sock");
        let mut listener = ipc_bind(&marker).unwrap();
        let server = std::thread::spawn(move || {
            let mut s = listener.accept().unwrap();
            assert!(s.peer_same_user());
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            writeln!(s, "echo {}", line.trim()).unwrap();
            s.flush().unwrap();
        });
        let mut c = ipc_connect(&marker).unwrap();
        c.set_timeout(std::time::Duration::from_secs(2)).unwrap();
        writeln!(c, "hello").unwrap();
        c.flush().unwrap();
        let mut reply = String::new();
        BufReader::new(&c).read_line(&mut reply).unwrap();
        assert_eq!(reply.trim(), "echo hello");
        server.join().unwrap();
        ipc_unbind(&marker);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
