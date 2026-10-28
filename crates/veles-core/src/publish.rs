//! Publishing something too big for a chat line as a FILE, and handing
//! back a URL for it.
//!
//! WHY THIS IS A MODULE
//!
//! The copy-into-a-web-root logic already existed, inside the IRC
//! adapter, taking an `IrcConfig` — so it was reachable from exactly
//! one surface and only for a file that already sat on disk. USER,
//! 2026-09-12:
//!
//!   *"Does `veles` have a `more` equivalent? It probably should, or at
//!   least the full response as a markdown file published to the media
//!   publishing endpoint"* … *"the publishing as markdown thing might
//!   also be useful later when I want to add a bluesky frontend, so
//!   ideally that would be a module"*
//!
//! So the naming rules and the allowlist live here, in core, where any
//! frontend can reach them, and `veles-irc` uses them rather than
//! keeping a second copy. Two allowlists would be one allowlist and one
//! bug: the whole point of the list is that it is the only place the
//! question "may this extension survive into a web root" is answered.
//!
//! WHERE IT PUBLISHES TO. The served directory is configured as
//! `platforms.irc.media_public_dir` / `media_public_url`. That it lives
//! under the IRC platform is history, not design — IRC was the surface
//! that needed a link first, and the endpoint itself is one shared web
//! root (`veles-media.7778777.online`, rsync'd to the VPS). Reading it
//! is confined to [`destination`] so that moving it to a `[media]`
//! section later is one function, not a search.

use std::path::PathBuf;

/// Reduce a name to something that means nothing to a path, a shell or
/// `markdown_to_irc`.
///
/// `_` becomes `-` because an italicised link is a 404.
pub fn sanitize_name(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .map(|c| if c == '_' { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        .take(80)
        .collect();
    let kept = kept.trim_matches('.');
    if kept.is_empty() {
        "media".to_string()
    } else {
        kept.to_string()
    }
}

/// Extensions a published file may keep, because a web server telling a
/// browser the truth about them is harmless.
///
/// Everything else becomes `.bin`. This is an ALLOWLIST rather than a
/// blocklist of `html|svg|js|…` on purpose: the dangerous set grows
/// (xhtml, xht, mhtml, svgz, a content type some future browser learns
/// to execute) and the harmless set does not, so a blocklist is a list
/// that is wrong later.
pub const PUBLISHABLE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "avif", "ico", // pictures
    "mp4", "webm", "mov", "mkv", // video
    "mp3", "wav", "ogg", "oga", "opus", "flac", "m4a", "aac", // audio
    "pdf", "txt", "log", "csv", // documents a channel legitimately wants
    // Markdown is text, and the vhost serves it as an attachment with
    // `nosniff` like everything else here — no browser renders it in
    // place, so it carries none of the risk `.html` does.
    "md",
];

/// The name a file is published under: sanitized, then held to
/// [`PUBLISHABLE_EXTS`].
///
/// The served directory is a WEB ROOT, so the extension is an
/// instruction to a browser rather than decoration. Renaming rather
/// than refusing is deliberate: the file still reaches the reader, it
/// just arrives as a download instead of as a page.
pub fn safe_published_name(raw: &str) -> String {
    let stem = sanitize_name(raw);
    let ext = stem
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !ext.is_empty() && PUBLISHABLE_EXTS.contains(&ext.as_str()) {
        stem
    } else {
        format!("{stem}.bin")
    }
}

/// Nothing published may exceed this. Matches the IRC adapter's cap.
pub const MAX_PUBLISH_BYTES: u64 = 64 * 1024 * 1024;

/// The served directory and the URL it answers on, or `None` when
/// publishing is not configured.
///
/// The ONE place the config keys are read — see the module docs.
pub fn destination(cfg: &crate::config::VelesConfig) -> Option<(PathBuf, String)> {
    let irc = cfg.platforms.get("irc")?;
    let dir = irc
        .get("media_public_dir")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let url = irc
        .get("media_public_url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some((PathBuf::from(dir), url.trim_end_matches('/').to_string()))
}

/// Write `body` into the served directory under a fresh name and return
/// its URL.
///
/// `stem` is a suggestion; it is sanitized and held to the allowlist, so
/// a caller cannot name its way out of the web root.
///
/// `None` for every way it can fail — unconfigured, unwritable,
/// oversized, a thousand name collisions — each logged, because "the
/// channel got a note instead of a link" is only debuggable from this
/// side.
pub fn text(cfg: &crate::config::VelesConfig, stem: &str, body: &str) -> Option<String> {
    let (dir, base) = match destination(cfg) {
        Some(d) => d,
        None => {
            tracing::warn!(
                "publish: media_public_dir/url are not configured, so {} bytes were not published",
                body.len()
            );
            return None;
        }
    };
    if body.len() as u64 > MAX_PUBLISH_BYTES {
        tracing::warn!("publish: {} bytes is over the cap", body.len());
        return None;
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("publish: cannot create {}: {e}", dir.display());
        return None;
    }
    let name = safe_published_name(stem);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let unique = (0..1000u32)
        .map(|n| {
            if n == 0 {
                format!("{stamp}-{name}")
            } else {
                format!("{stamp}-{n}-{name}")
            }
        })
        .find(|candidate| !dir.join(candidate).exists())?;
    let target = dir.join(&unique);
    if let Err(e) = std::fs::write(&target, body) {
        tracing::warn!("publish: cannot write {}: {e}", target.display());
        return None;
    }
    // 0644 explicitly: the umask of a systemd service is not the
    // operator's, and a file the web server cannot read is a 403 with no
    // explanation anywhere the operator would look.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)) {
            tracing::warn!(
                "publish: wrote {} but could not make it readable: {e}",
                target.display()
            );
        }
    }
    Some(format!("{base}/{unique}"))
}

/// The same, wrapped in a fenced code block.
///
/// Program output is not prose: a drawing loses its shape the moment a
/// renderer reflows it, and #coven's stored procs draw. A fence keeps
/// the columns in any markdown viewer and costs a reader of the raw
/// file two lines.
pub fn code_block(cfg: &crate::config::VelesConfig, stem: &str, body: &str) -> Option<String> {
    // A body containing its own fence would end the block early; the
    // longest run of backticks in it decides how long ours must be.
    let longest = body
        .split('\n')
        .filter_map(|l| {
            let t = l.trim_start();
            if t.starts_with("```") {
                Some(t.chars().take_while(|c| *c == '`').count())
            } else {
                None
            }
        })
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    text(cfg, stem, &format!("{fence}\n{body}\n{fence}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(dir: &std::path::Path) -> crate::config::VelesConfig {
        crate::config::VelesConfig {
            platforms: serde_yaml::from_str(&format!(
                "irc:\n  media_public_dir: {}\n  media_public_url: https://media.example.net/\n",
                dir.display()
            ))
            .expect("platform block"),
            ..Default::default()
        }
    }

    #[test]
    fn the_allowlist_keeps_markdown_and_renames_anything_executable() {
        assert_eq!(safe_published_name("notes.md"), "notes.md");
        assert_eq!(safe_published_name("chart.png"), "chart.png");
        // The reason the list exists: a web root serving this as a page
        // under the operator's own domain is the whole attack.
        assert_eq!(safe_published_name("evil.html"), "evil.html.bin");
        assert_eq!(safe_published_name("evil.svg"), "evil.svg.bin");
    }

    #[test]
    fn a_name_cannot_climb_out_of_the_web_root() {
        let n = safe_published_name("../../etc/passwd");
        assert!(!n.contains('/'), "no separators survive: {n}");
        assert!(!n.contains(".."), "and no traversal: {n}");
    }

    #[test]
    fn unconfigured_publishing_is_none_rather_than_a_panic() {
        let c = crate::config::VelesConfig::default();
        assert!(destination(&c).is_none());
        assert!(text(&c, "x.md", "hello").is_none());
    }

    #[test]
    fn a_published_file_lands_in_the_root_and_the_url_points_at_it() {
        let tmp = tempfile::tempdir().expect("tmp");
        let c = cfg_with(tmp.path());
        let url = text(&c, "answer.md", "hello world").expect("published");
        assert!(
            url.starts_with("https://media.example.net/"),
            "the trailing slash on the base must not double: {url}"
        );
        let name = url.rsplit('/').next().unwrap();
        let body = std::fs::read_to_string(tmp.path().join(name)).expect("on disk");
        assert_eq!(body, "hello world");
        assert!(name.ends_with("answer.md"), "{name}");
    }

    /// A drawing keeps its columns only inside a fence.
    #[test]
    fn a_code_block_fences_the_body() {
        let tmp = tempfile::tempdir().expect("tmp");
        let c = cfg_with(tmp.path());
        let url = code_block(&c, "art.md", " __\n(  )").expect("published");
        let name = url.rsplit('/').next().unwrap();
        let body = std::fs::read_to_string(tmp.path().join(name)).expect("on disk");
        assert_eq!(body, "```\n __\n(  )\n```\n");
    }

    /// …and a body that contains a fence must not end the block early,
    /// which would let its tail escape the code block and be rendered.
    #[test]
    fn a_body_containing_a_fence_gets_a_longer_one() {
        let tmp = tempfile::tempdir().expect("tmp");
        let c = cfg_with(tmp.path());
        let url = code_block(&c, "art.md", "```\nnot the end\n```").expect("published");
        let name = url.rsplit('/').next().unwrap();
        let body = std::fs::read_to_string(tmp.path().join(name)).expect("on disk");
        assert!(
            body.starts_with("````\n"),
            "outer fence must be longer: {body:?}"
        );
        assert!(body.ends_with("\n````\n"), "{body:?}");
    }

    #[test]
    fn two_publishes_of_the_same_name_do_not_collide() {
        let tmp = tempfile::tempdir().expect("tmp");
        let c = cfg_with(tmp.path());
        let a = text(&c, "same.md", "first").expect("a");
        let b = text(&c, "same.md", "second").expect("b");
        assert_ne!(a, b, "a second publish must not overwrite the first");
        let an = a.rsplit('/').next().unwrap();
        let bn = b.rsplit('/').next().unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(an)).unwrap(),
            "first"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(bn)).unwrap(),
            "second"
        );
    }
}
