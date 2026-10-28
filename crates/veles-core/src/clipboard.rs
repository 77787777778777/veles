//! The system clipboard and the system URL opener, for the terminal
//! surfaces (`/copy`, `/paste`, opening a link from the TUI). The
//! desktop window has Tk's own clipboard; this is for processes without
//! one, and it is a "which helper program" question that differs per
//! platform: the X11/Wayland tools on Linux, `clip`/`Get-Clipboard` and
//! `start` on Windows, `pbcopy`/`open` on macOS.

use std::io::Write as _;
use std::process::{Command, Stdio};

/// The clipboard's text, or `None` when no helper answered.
pub fn read() -> Option<String> {
    for (cmd, args) in readers() {
        let out = Command::new(cmd)
            .args(args.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

/// Put `text` on the clipboard. `Ok` names the helper that took it.
pub fn write(text: &str) -> Result<&'static str, String> {
    for (cmd, args) in writers() {
        let spawned = Command::new(cmd)
            .args(args.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = spawned else { continue };
        let fed = child
            .stdin
            .as_mut()
            .map(|s| s.write_all(text.as_bytes()).is_ok())
            .unwrap_or(false);
        if fed && child.wait().map(|s| s.success()).unwrap_or(false) {
            return Ok(cmd);
        }
    }
    Err(format!(
        "no clipboard helper worked (tried {})",
        writers()
            .iter()
            .map(|(c, _)| *c)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Open `url` with the system's opener. `$BROWSER` first, everywhere.
pub fn open_url(url: &str) -> std::io::Result<()> {
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no opener found");
    if let Ok(b) = std::env::var("BROWSER") {
        if !b.trim().is_empty() {
            match Command::new(b.trim())
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
        }
    }
    for (cmd, args) in openers() {
        match Command::new(cmd)
            .args(args.iter())
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

type Helper = (&'static str, &'static [&'static str]);

#[cfg(windows)]
fn readers() -> &'static [Helper] {
    &[
        (
            "pwsh",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-Clipboard -Raw",
            ],
        ),
        (
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-Clipboard -Raw",
            ],
        ),
    ]
}
#[cfg(windows)]
fn writers() -> &'static [Helper] {
    &[("clip", &[])]
}
#[cfg(windows)]
fn openers() -> &'static [Helper] {
    // `start` is a cmd builtin; the empty "" is its window title slot,
    // so a URL with `&` is not taken for one.
    &[("cmd", &["/D", "/C", "start", "\"\""])]
}

#[cfg(target_os = "macos")]
fn readers() -> &'static [Helper] {
    &[("pbpaste", &[])]
}
#[cfg(target_os = "macos")]
fn writers() -> &'static [Helper] {
    &[("pbcopy", &[])]
}
#[cfg(target_os = "macos")]
fn openers() -> &'static [Helper] {
    &[("open", &[])]
}

#[cfg(not(any(windows, target_os = "macos")))]
fn readers() -> &'static [Helper] {
    &[("xclip", &["-o"]), ("xsel", &["-o"]), ("wl-paste", &[])]
}
#[cfg(not(any(windows, target_os = "macos")))]
fn writers() -> &'static [Helper] {
    &[
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("wl-copy", &[]),
    ]
}
#[cfg(not(any(windows, target_os = "macos")))]
fn openers() -> &'static [Helper] {
    &[("xdg-open", &[]), ("open", &[]), ("sensible-browser", &[])]
}
