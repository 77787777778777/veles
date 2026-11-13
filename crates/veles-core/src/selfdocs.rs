//! `veles_docs` — veles' own documentation, from inside the binary.
//!
//! WHY (the owner, 2026-11-12): "can we do something so that the bot has
//! info about its internals? People keep asking and it always says it
//! doesn't know, or it's sandboxed and doesn't have access to source".
//! Both answers were honest. The prompt said "You are veles … Rust core,
//! Tcl scripting layer" and nothing else about itself; a caller who is
//! not the owner runs in a sandbox container, so the model went looking
//! for its source there (`grep -rn stock::chart /opt/data`), found
//! nothing, and said so.
//!
//! The documentation ships in the tree the binary is built from, so the
//! binary carries it: every top-level page of `docs/`, the README and
//! this version's release notes, compiled in. No network, no file
//! access, no capability beyond `chat`: what is in here is published,
//! and the question is asked by exactly the people who have no other
//! way to look. The index also says where the documentation site and
//! the public source are, and that the Tcl a channel has accumulated is
//! read with `tcl_state_eval`, which is the other half of the same
//! question ("what does stock::chart use").
//!
//! CORE tier: a model that has to discover the tool first answers
//! "I don't know" instead.

use crate::tools::Tool;
use async_trait::async_trait;
use serde_json::{json, Value};

/// The documentation site (the README names it).
pub const DOCS_SITE: &str = "https://veles-docs.7778777.online/";
/// The public source repository.
pub const SOURCE: &str = "https://github.com/77787777778777/veles";

/// The most one answer returns, in characters.
const MAX_ANSWER: usize = 12_000;
/// How much of a page's opening a page request shows before its outline.
const MAX_INTRO: usize = 2_500;
/// How many sections a search returns, and how much of each.
const MAX_HITS: usize = 6;
const MAX_EXCERPT: usize = 900;

/// One bundled page.
pub struct Page {
    pub name: &'static str,
    pub text: &'static str,
}

macro_rules! page {
    ($name:literal, $path:literal) => {
        Page {
            name: $name,
            text: include_str!(concat!("../../../", $path)),
        }
    };
}

/// Every page. `docs_pages_are_all_bundled` holds this list to the
/// directory, so a new page cannot be written and left out.
pub const PAGES: &[Page] = &[
    page!("overview", "README.md"),
    Page {
        name: "release-notes",
        text: include_str!(concat!(
            "../../../docs/releases/v",
            env!("CARGO_PKG_VERSION"),
            ".md"
        )),
    },
    page!("acl", "docs/acl.md"),
    page!("apps", "docs/apps.md"),
    page!("build-memory", "docs/build-memory.md"),
    page!("channel-tcl", "docs/channel-tcl.md"),
    page!("command-center", "docs/command-center.md"),
    page!("devices", "docs/devices.md"),
    page!("extending", "docs/extending.md"),
    page!("extension-api-changes", "docs/extension-api-changes.md"),
    page!("i18n-runtime", "docs/i18n-runtime.md"),
    page!("install", "docs/install.md"),
    page!("media-publishing", "docs/media-publishing.md"),
    page!("phone", "docs/phone.md"),
    page!("plugin-plan", "docs/plugin-plan.md"),
    page!("plugins", "docs/plugins.md"),
    page!("pool", "docs/pool.md"),
    page!("recovery", "docs/recovery.md"),
    page!("session-mail", "docs/session-mail.md"),
    page!("slopdrop-bridge", "docs/slopdrop-bridge.md"),
];

// ── the source this binary was built from ─────────────────────────────
//
// The owner, the same day: it should also include source information,
// to answer the kinds of questions people in the channel had had "about
// the tcl and about available commands". A caller in a sandbox cannot read
// the host's source tree and must not, and the tree is public anyway;
// so the binary carries a snapshot of it. The binary crate packs the
// snapshot at build time (crates/veles-cli/build.rs: the crates'
// sources, the desktop's Tcl, the plugins tree and the deployment
// files), and hands it over at startup. veles-core is not the one that
// packs it, or every edit anywhere in the tree would rebuild veles-core.

/// One file of the snapshot.
pub struct SourceFile {
    pub path: String,
    pub text: String,
}

static SOURCE_ARCHIVE: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();
static SOURCE_FILES: std::sync::OnceLock<Vec<SourceFile>> = std::sync::OnceLock::new();

/// Hand over the packed snapshot (once, at startup; a second call is
/// ignored). The format is [`unpack_source`]'s.
pub fn install_source(archive: &'static [u8]) {
    let _ = SOURCE_ARCHIVE.set(archive);
}

/// A snapshot as the build script packs it: a gzip stream of records,
/// each `u32 LE path length, path, u32 LE text length, text`, paths
/// relative to the workspace root with `/` separators, sorted. Anything
/// that does not read as that is an empty snapshot, not a panic.
pub fn unpack_source(archive: &[u8]) -> Vec<SourceFile> {
    use std::io::Read;
    let mut raw = Vec::new();
    if flate2::read::GzDecoder::new(archive)
        .read_to_end(&mut raw)
        .is_err()
    {
        return Vec::new();
    }
    let mut files = Vec::new();
    let mut at = 0usize;
    let take = |at: &mut usize| -> Option<&[u8]> {
        let len = u32::from_le_bytes(raw.get(*at..*at + 4)?.try_into().ok()?) as usize;
        let bytes = raw.get(*at + 4..*at + 4 + len)?;
        *at += 4 + len;
        Some(bytes)
    };
    while at < raw.len() {
        let (Some(path), Some(text)) = (take(&mut at), take(&mut at)) else {
            return Vec::new();
        };
        let (Ok(path), Ok(text)) = (std::str::from_utf8(path), std::str::from_utf8(text)) else {
            continue;
        };
        files.push(SourceFile {
            path: path.to_string(),
            text: text.to_string(),
        });
    }
    files
}

/// The snapshot's files, unpacked on first use. Nothing is cached until
/// a snapshot has been handed over, so asking early does not pin "none".
fn source_files() -> &'static [SourceFile] {
    match SOURCE_ARCHIVE.get() {
        None => &[],
        Some(archive) => SOURCE_FILES.get_or_init(|| unpack_source(archive)),
    }
}

/// How many lines a `file` answer shows at most, and how many matches a
/// `grep`.
const MAX_LINES: usize = 220;
const MAX_MATCHES: usize = 40;

fn no_source() -> String {
    format!(
        "veles_docs: this build carries no source snapshot (a development or test build). \
         The source is public: {SOURCE} (tag v{}).",
        env!("CARGO_PKG_VERSION")
    )
}

/// The files under a path prefix, or matching a name, with their sizes.
fn source_listing(files: &[SourceFile], prefix: &str) -> String {
    let prefix = prefix.trim().trim_start_matches("./").trim_end_matches('/');
    let under: Vec<&SourceFile> = files
        .iter()
        .filter(|f| prefix.is_empty() || f.path.starts_with(prefix))
        .collect();
    if prefix.is_empty() || under.len() > 150 {
        // Too many to list: the directories, with how much each holds.
        let depth = prefix.split('/').filter(|p| !p.is_empty()).count() + 2;
        let mut dirs: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
        for f in &under {
            let dir: Vec<&str> = f.path.split('/').take(depth).collect();
            let key = if dir.len() < f.path.split('/').count() {
                format!("{}/", dir.join("/"))
            } else {
                f.path.clone()
            };
            let e = dirs.entry(key).or_default();
            e.0 += 1;
            e.1 += f.text.len();
        }
        let rows: Vec<String> = dirs
            .iter()
            .map(|(d, (n, bytes))| format!("  {d}  ({n} file(s), {} KB)", bytes / 1024))
            .collect();
        return format!(
            "{} source file(s){}:\n{}\n\nfile=<a directory above> lists it; file=<a path> \
             opens a file; grep=<text> searches all of them.",
            under.len(),
            if prefix.is_empty() {
                String::new()
            } else {
                format!(" under {prefix}")
            },
            clipped(&rows.join("\n"), MAX_ANSWER, "name a directory")
        );
    }
    let rows: Vec<String> = under
        .iter()
        .map(|f| format!("  {}  ({} lines)", f.path, f.text.lines().count()))
        .collect();
    format!(
        "{} file(s) under {prefix}:\n{}",
        under.len(),
        rows.join("\n")
    )
}

/// The lines that open a definition, for a file too long to show whole.
fn definitions(file: &SourceFile) -> Vec<(usize, &str)> {
    let tcl = file.path.ends_with(".tcl");
    file.text
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let t = line.trim_start();
            if tcl {
                t.starts_with("proc ") || t.starts_with("namespace eval ")
            } else {
                let t = t
                    .strip_prefix("pub(crate) ")
                    .or_else(|| t.strip_prefix("pub "))
                    .unwrap_or(t);
                let t = t.strip_prefix("async ").unwrap_or(t);
                [
                    "fn ", "struct ", "enum ", "trait ", "impl ", "impl<", "mod ", "const ",
                    "static ", "type ",
                ]
                .iter()
                .any(|k| t.starts_with(k))
                    && line.len() - line.trim_start().len() <= 4
            }
        })
        .map(|(i, line)| (i + 1, line.trim_end()))
        .collect()
}

fn numbered(file: &SourceFile, from: usize, to: usize) -> String {
    file.text
        .lines()
        .enumerate()
        .skip(from.saturating_sub(1))
        .take(to.saturating_sub(from.saturating_sub(1)))
        .map(|(i, line)| format!("{:>5}  {line}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A source file: whole when short, its opening and its definitions
/// when long, or the lines asked for.
fn source_answer(path: &str, lines: Option<&str>) -> String {
    let files = source_files();
    if files.is_empty() {
        return no_source();
    }
    let want = path.trim().trim_start_matches("./");
    if want.is_empty() || want == "list" {
        return source_listing(files, "");
    }
    let exact = files.iter().find(|f| f.path == want);
    let by_suffix: Vec<&SourceFile> = files.iter().filter(|f| f.path.ends_with(want)).collect();
    let file = match (exact, by_suffix.as_slice()) {
        (Some(f), _) => f,
        (None, [one]) => *one,
        (None, [])
            if files
                .iter()
                .any(|f| f.path.starts_with(want.trim_end_matches('/'))) =>
        {
            return source_listing(files, want);
        }
        (None, []) => {
            let name = want.rsplit('/').next().unwrap_or(want);
            let near: Vec<String> = files
                .iter()
                .filter(|f| f.path.contains(name))
                .take(12)
                .map(|f| format!("  {}", f.path))
                .collect();
            return format!(
                "veles_docs: no source file `{want}`.{}\nfile=list shows the tree.",
                if near.is_empty() {
                    String::new()
                } else {
                    format!(" Close:\n{}", near.join("\n"))
                }
            );
        }
        (None, many) => {
            return format!(
                "veles_docs: `{want}` names {} files:\n{}",
                many.len(),
                many.iter()
                    .map(|f| format!("  {}", f.path))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    };
    let total = file.text.lines().count();
    if let Some(range) = lines.map(str::trim).filter(|r| !r.is_empty()) {
        let mut parts = range
            .split(['-', ':', ','])
            .map(|p| p.trim().parse::<usize>().ok());
        let from = parts.next().flatten().unwrap_or(1).max(1);
        let to = parts
            .next()
            .flatten()
            .unwrap_or(from + MAX_LINES - 1)
            .max(from);
        let to = to.min(from + MAX_LINES - 1).min(total);
        if from > total {
            return format!("veles_docs: {} has {total} lines", file.path);
        }
        return format!(
            "{} lines {from}-{to} of {total}\n{}",
            file.path,
            clipped(&numbered(file, from, to), MAX_ANSWER, "ask for fewer lines")
        );
    }
    if total <= MAX_LINES {
        return format!(
            "{} ({total} lines)\n{}",
            file.path,
            clipped(
                &numbered(file, 1, total),
                MAX_ANSWER,
                "ask for a range with lines=a-b"
            )
        );
    }
    let defs = definitions(file);
    let rows: Vec<String> = defs
        .iter()
        .map(|(n, line)| format!("{n:>5}  {line}"))
        .collect();
    format!(
        "{} ({total} lines; lines=a-b shows a range, at most {MAX_LINES} at a time)\n\
         — it opens —\n{}\n— definitions —\n{}",
        file.path,
        clipped(&numbered(file, 1, 40), 4_000, "the file goes on"),
        clipped(
            &rows.join("\n"),
            MAX_ANSWER - 4_500,
            "grep=<name> finds one"
        ),
    )
}

/// Lines of the source that contain `needle`, case-insensitively, in
/// the files whose path contains `within` (all of them when empty).
fn source_grep(needle: &str, within: Option<&str>) -> String {
    let files = source_files();
    if files.is_empty() {
        return no_source();
    }
    let needle = needle.trim().to_lowercase();
    if needle.chars().count() < 2 {
        return "veles_docs: grep needs at least two characters".to_string();
    }
    let within = within
        .map(|w| w.trim().trim_start_matches("./"))
        .filter(|w| !w.is_empty());
    let mut shown: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut in_files = 0usize;
    for f in files {
        if within.is_some_and(|w| !f.path.contains(w)) {
            continue;
        }
        let mut here = 0usize;
        for (i, line) in f.text.lines().enumerate() {
            if line.to_lowercase().contains(&needle) {
                total += 1;
                here += 1;
                if shown.len() < MAX_MATCHES {
                    let text: String = line.trim().chars().take(200).collect();
                    shown.push(format!("{}:{}: {text}", f.path, i + 1));
                }
            }
        }
        in_files += usize::from(here > 0);
    }
    if total == 0 {
        return format!(
            "veles_docs: `{needle}` is not in the source{}. query=<words> searches the \
             documentation pages instead.",
            within.map(|w| format!(" under {w}")).unwrap_or_default()
        );
    }
    format!(
        "{total} line(s) in {in_files} file(s){}:\n{}{}",
        within.map(|w| format!(" under {w}")).unwrap_or_default(),
        shown.join("\n"),
        if total > shown.len() {
            format!(
                "\n[… {} more: narrow it with file=<part of a path>, or a longer text]",
                total - shown.len()
            )
        } else {
            String::new()
        }
    )
}

// ── pages a frontend adds at startup ──────────────────────────────────

static LIVE_PAGES: std::sync::OnceLock<std::sync::Mutex<Vec<(String, String)>>> =
    std::sync::OnceLock::new();

/// Add a page whose text exists only at run time, or lives in a crate
/// above this one (the command table is in veles-frontends). Named once:
/// a second page of the same name replaces the first.
pub fn install_page(name: &str, text: String) {
    let mut pages = LIVE_PAGES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    pages.retain(|(n, _)| n != name);
    pages.push((name.to_string(), text));
}

fn live_page(name: &str) -> Option<String> {
    let want = name.trim().to_lowercase();
    LIVE_PAGES
        .get()?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(n, _)| *n == want)
        .map(|(_, text)| text.clone())
}

fn live_page_names() -> Vec<String> {
    LIVE_PAGES
        .get()
        .map(|p| {
            p.lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(n, _)| n.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// A heading and what stands under it, up to the next heading.
struct Section<'a> {
    level: usize,
    heading: &'a str,
    /// Byte range of the section in the page, heading line included.
    start: usize,
    end: usize,
}

/// The page's sections, in order. Text before the first heading is a
/// section with an empty heading. Lines inside a fenced block are not
/// headings (a `# comment` in a shell example is not one).
fn sections(text: &str) -> Vec<Section<'_>> {
    let mut out: Vec<Section> = Vec::new();
    let mut fenced = false;
    let mut at = 0usize;
    let mut open = Section {
        level: 0,
        heading: "",
        start: 0,
        end: 0,
    };
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
        let is_heading =
            !fenced && (1..=4).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ');
        if is_heading {
            open.end = at;
            if open.end > open.start {
                out.push(open);
            }
            open = Section {
                level: hashes,
                heading: trimmed[hashes..].trim(),
                start: at,
                end: at,
            };
        }
        at += line.len();
    }
    open.end = text.len();
    if open.end > open.start {
        out.push(open);
    }
    out
}

/// The page's title: its first heading, without markup. The release
/// notes open with a date line and "Fixes", which is no title.
fn title(page: &Page) -> String {
    if page.name == "release-notes" {
        return format!("What changed in veles {}", env!("CARGO_PKG_VERSION"));
    }
    sections(page.text)
        .iter()
        .find(|s| s.level > 0)
        .map(|s| s.heading.trim_matches(['*', '`', ' ']).to_string())
        .unwrap_or_else(|| page.name.to_string())
}

/// `text` cut to at most `max` characters on a character boundary, with
/// what was left out said.
fn clipped(text: &str, max: usize, more: &str) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => format!(
            "{}\n[… {} more characters: {more}]",
            text[..cut].trim_end(),
            text[cut..].chars().count()
        ),
    }
}

fn find_page(name: &str) -> Option<&'static Page> {
    let want = name.trim().to_lowercase();
    let want = want.trim_end_matches(".md");
    PAGES.iter().find(|p| p.name == want).or_else(|| {
        PAGES
            .iter()
            .find(|p| p.name.replace('-', "") == want.replace(['-', ' ', '_'], ""))
    })
}

fn page_list() -> String {
    let mut rows: Vec<String> = live_page_names()
        .into_iter()
        .map(|name| {
            let first = live_page(&name).unwrap_or_default();
            let head = first
                .lines()
                .next()
                .unwrap_or("")
                .trim_start_matches(['#', ' ']);
            format!("  {name} — {head}")
        })
        .collect();
    rows.extend(PAGES.iter().map(|p| format!("  {} — {}", p.name, title(p))));
    rows.join("\n")
}

/// What veles is, this build, where everything else is, and the pages.
pub fn index() -> String {
    format!(
        "veles {version} — a personal AI agent: a Rust core with a Tcl scripting layer, \
         reached through a desktop app, a terminal, IRC, Telegram and other connectors, on one \
         machine or a pool of them.\n\
         Documentation site: {DOCS_SITE} (these pages and more, in 47 languages)\n\
         Source: {SOURCE} (public; this build is version {version}, tag v{version})\n\n\
         Pages built into this binary (veles_docs page=<name>):\n{pages}\n\n\
         One part of a page: page=<name> section=<words of its heading>. \
         Every page at once: query=<words>.\n\n\
         {source}\n\n\
         Not in here: the Tcl procs and variables a channel has accumulated (what a \
         channel's `tcl …` commands run). Read those with tcl_state_eval, for example \
         `info body <proc>` or `info procs <pattern>`; page=channel-tcl says what else is \
         behind them.",
        version = env!("CARGO_PKG_VERSION"),
        pages = page_list(),
        source = match source_files().len() {
            0 => format!("This build carries no source snapshot; the source is at {SOURCE}."),
            n => format!(
                "The source this binary was built from is in here too ({n} files: the crates, \
                 the desktop's Tcl, the plugins, the deployment files): grep=<text> finds \
                 lines in it (file=<part of a path> narrows that), file=<path> opens a file \
                 (lines=a-b for a range), file=list shows the tree."
            ),
        },
    )
}

/// A page: its opening and its outline, or one of its sections.
fn page_answer(page: &Page, section: Option<&str>) -> String {
    let secs = sections(page.text);
    if let Some(want) = section.map(str::trim).filter(|s| !s.is_empty()) {
        let words: Vec<String> = want
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let hit = secs.iter().position(|s| {
            let h = s.heading.to_lowercase();
            s.level > 0 && words.iter().all(|w| h.contains(w.as_str()))
        });
        let Some(i) = hit else {
            return format!(
                "veles_docs: page `{}` has no section whose heading contains `{want}`. Its \
                 sections:\n{}",
                page.name,
                outline(&secs)
            );
        };
        // The section with everything under it: up to the next heading
        // of its own level or above.
        let end = secs[i + 1..]
            .iter()
            .find(|s| s.level <= secs[i].level)
            .map(|s| s.start)
            .unwrap_or(page.text.len());
        return clipped(
            page.text[secs[i].start..end].trim_end(),
            MAX_ANSWER,
            "ask for one of its subsections by its own heading",
        );
    }
    let intro_end = secs
        .iter()
        .filter(|s| s.level > 0)
        .nth(1)
        .map(|s| s.start)
        .unwrap_or(page.text.len());
    format!(
        "{}\n\n— sections of `{}` (veles_docs page={} section=<words of a heading>) —\n{}",
        clipped(
            page.text[..intro_end].trim_end(),
            MAX_INTRO,
            "the sections below hold the rest"
        ),
        page.name,
        page.name,
        outline(&secs)
    )
}

fn outline(secs: &[Section]) -> String {
    secs.iter()
        .filter(|s| (1..=3).contains(&s.level))
        .map(|s| format!("{}{}", "  ".repeat(s.level), s.heading))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The sections, across every page, that say the most about `query`.
fn search(query: &str) -> String {
    let words: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '-'))
        .filter(|w| w.chars().count() >= 2)
        .map(String::from)
        .collect();
    if words.is_empty() {
        return "veles_docs: the query has no word of two letters or more".to_string();
    }
    let live: Vec<(String, String)> = live_page_names()
        .into_iter()
        .filter_map(|n| live_page(&n).map(|t| (n, t)))
        .collect();
    let pages = live
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .chain(PAGES.iter().map(|p| (p.name, p.text)));
    let mut hits: Vec<(usize, &str, String, String)> = Vec::new();
    for (page_name, page_text) in pages {
        for s in sections(page_text) {
            let body = &page_text[s.start..s.end];
            let lower = body.to_lowercase();
            let heading = s.heading.to_lowercase();
            let mut score = 0usize;
            let mut all = true;
            for w in &words {
                let n = lower.matches(w.as_str()).count();
                all &= n > 0;
                score += n.min(8) + if heading.contains(w.as_str()) { 12 } else { 0 };
            }
            // A section that has every word outranks any that lacks one.
            if all {
                score += 100;
            }
            if score > 0 {
                // The excerpt opens at the first word found.
                let first = words
                    .iter()
                    .filter_map(|w| lower.find(w.as_str()))
                    .min()
                    .unwrap_or(0);
                let from = body[..first].rfind('\n').map(|i| i + 1).unwrap_or(0);
                hits.push((
                    score,
                    page_name,
                    s.heading.to_string(),
                    clipped(body[from..].trim(), MAX_EXCERPT, "ask for the section"),
                ));
            }
        }
    }
    if hits.is_empty() {
        return format!(
            "veles_docs: nothing in the built-in pages mentions `{query}`. The documentation \
             site ({DOCS_SITE}) and the source ({SOURCE}) hold more. Pages here:\n{}",
            page_list()
        );
    }
    hits.sort_by_key(|hit| std::cmp::Reverse(hit.0));
    let mut out = String::new();
    for (_, page, heading, excerpt) in hits.into_iter().take(MAX_HITS) {
        let heading = if heading.is_empty() {
            "(opening)".to_string()
        } else {
            heading
        };
        out.push_str(&format!("── {page} › {heading}\n{excerpt}\n\n"));
    }
    clipped(
        out.trim_end(),
        MAX_ANSWER,
        "narrow the query or ask for one section",
    )
}

pub struct VelesDocsTool;

#[async_trait]
impl Tool for VelesDocsTool {
    fn name(&self) -> &str {
        "veles_docs"
    }

    fn description(&self) -> &str {
        "Your own documentation and source, built into this binary: what you are, how your \
         parts work, your commands, what changed in this version. Use it when asked about \
         your internals, before saying you do not know or cannot see them. No arguments: \
         the index."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "page": {"type": "string", "description": "a page from the index"},
                "section": {"type": "string", "description": "words of a heading on that page"},
                "query": {"type": "string", "description": "search every page"},
                "grep": {"type": "string", "description": "find text in your source"},
                "file": {"type": "string", "description": "a source path (or part of one, or list)"},
                "lines": {"type": "string", "description": "a-b, with file"}
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> String {
        let text = |key: &str| args[key].as_str().map(str::trim).filter(|s| !s.is_empty());
        if let Some(needle) = text("grep") {
            return source_grep(needle, text("file"));
        }
        if let Some(path) = text("file") {
            return source_answer(path, text("lines"));
        }
        match (text("page"), text("query")) {
            (Some("index"), _) | (None, None) => index(),
            (Some(name), _) => {
                if let Some(live) = live_page(name) {
                    return clipped(&live, MAX_ANSWER, "query=<words> searches it");
                }
                match find_page(name) {
                    Some(page) => page_answer(page, text("section")),
                    None => format!(
                        "veles_docs: there is no page `{name}`. Pages:\n{}",
                        page_list()
                    ),
                }
            }
            (None, Some(query)) => search(query),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ask(args: Value) -> String {
        VelesDocsTool.call(args).await
    }

    /// The index answers the questions people ask a bot in a channel
    /// (what are you, where is your source, what can I read about you)
    /// without a network or a file system, and names the tool that reads
    /// what a channel's `tcl` commands run.
    #[tokio::test]
    async fn the_index_says_what_veles_is_and_where_everything_is() {
        let index = ask(json!({})).await;
        assert!(
            index.starts_with(&format!(
                "veles {} — a personal AI agent",
                env!("CARGO_PKG_VERSION")
            )),
            "{index}"
        );
        assert!(
            index.contains(DOCS_SITE) && index.contains(SOURCE),
            "{index}"
        );
        for page in [
            "overview",
            "release-notes",
            "install",
            "acl",
            "pool",
            "plugins",
            "slopdrop-bridge",
        ] {
            assert!(
                index.contains(&format!("\n  {page} — ")),
                "{page} is not listed: {index}"
            );
        }
        assert!(index.contains("tcl_state_eval"), "{index}");
        assert!(
            index.contains("\n  release-notes — What changed in veles "),
            "{index}"
        );
        // `page=index` is the same answer. (Its first line: other tests
        // of this process add pages and a snapshot while this one runs.)
        assert_eq!(
            ask(json!({"page": "index"})).await.lines().next(),
            index.lines().next()
        );
        assert!(
            index.chars().count() < 3_000,
            "the index must stay small: {}",
            index.len()
        );
    }

    /// A page opens with its introduction and its outline, a section is
    /// returned whole with what stands under it, and neither can flood a
    /// turn.
    #[tokio::test]
    async fn a_page_gives_its_outline_and_a_section_its_text() {
        let page = ask(json!({"page": "command-center"})).await;
        assert!(page.contains("— sections of `command-center`"), "{page}");
        assert!(page.contains("Deployment consistency"), "{page}");
        assert!(
            page.chars().count() <= MAX_INTRO + 6_000,
            "{}",
            page.chars().count()
        );

        let section =
            ask(json!({"page": "command-center", "section": "deployment consistency"})).await;
        assert!(
            section.starts_with("### Deployment consistency"),
            "{section}"
        );
        assert!(section.contains("SNAPSHOT_OF"), "{section}");
        assert!(
            !section.contains("### Observing nodes"),
            "the next section is not part of it: {section}"
        );
        // Case, a trailing .md and a missing hyphen do not matter.
        assert_eq!(
            ask(json!({"page": "Command-Center.md", "section": "Deployment"})).await,
            section
        );
        assert!(ask(json!({"page": "commandcenter"}))
            .await
            .contains("— sections of `command-center`"));

        // The longest page still fits one answer.
        let longest = PAGES.iter().max_by_key(|p| p.text.len()).unwrap();
        for s in sections(longest.text).iter().filter(|s| s.level > 0) {
            let out = ask(json!({"page": longest.name, "section": s.heading})).await;
            assert!(
                out.chars().count() <= MAX_ANSWER + 200,
                "{} › {}: {}",
                longest.name,
                s.heading,
                out.chars().count()
            );
        }

        // What is not there is said, with what is.
        let missing = ask(json!({"page": "command-center", "section": "no such heading"})).await;
        assert!(
            missing.contains("has no section") && missing.contains("Deployment consistency"),
            "{missing}"
        );
        let unknown = ask(json!({"page": "nonsense"})).await;
        assert!(
            unknown.contains("there is no page `nonsense`") && unknown.contains("  install — "),
            "{unknown}"
        );
    }

    /// A search finds the section that says the most, across pages, and
    /// says where to look further when nothing does.
    #[tokio::test]
    async fn a_search_finds_the_section_across_pages() {
        let hits = ask(json!({"query": "SNAPSHOT_OF published build"})).await;
        assert!(hits.starts_with("── "), "{hits}");
        let first = hits.lines().next().unwrap();
        assert!(
            first.contains("command-center") || first.contains("release-notes"),
            "the best hit is the section about it: {first}"
        );
        assert!(
            hits.chars().count() <= MAX_ANSWER + 200,
            "{}",
            hits.chars().count()
        );
        assert!(hits.matches("── ").count() <= MAX_HITS, "{hits}");

        let none = ask(json!({"query": "zzqqxx-not-a-word"})).await;
        assert!(
            none.contains("nothing in the built-in pages mentions") && none.contains(DOCS_SITE),
            "{none}"
        );
        assert!(ask(json!({"query": "a ."}))
            .await
            .contains("no word of two letters"));
    }

    /// The snapshot format the build script writes, read back; damage
    /// is an empty snapshot, never a panic.
    fn packed(files: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut raw = Vec::new();
        for (path, text) in files {
            raw.extend_from_slice(&(path.len() as u32).to_le_bytes());
            raw.extend_from_slice(path.as_bytes());
            raw.extend_from_slice(&(text.len() as u32).to_le_bytes());
            raw.extend_from_slice(text.as_bytes());
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn a_packed_snapshot_reads_back_and_damage_is_empty() {
        let archive = packed(&[
            ("crates/a/src/lib.rs", "//! a\npub fn one() {}\n"),
            ("plugins/x.tcl", "proc x {} {}\n"),
        ]);
        let files = unpack_source(&archive);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "crates/a/src/lib.rs");
        assert_eq!(files[1].text, "proc x {} {}\n");
        assert!(unpack_source(b"not gzip").is_empty());
        assert!(unpack_source(&archive[..archive.len() / 2]).is_empty());
        let mut cut = Vec::new();
        {
            use std::io::Write;
            let mut gz = flate2::write::GzEncoder::new(&mut cut, flate2::Compression::fast());
            gz.write_all(&[9, 0, 0, 0, b'x']).unwrap();
            gz.finish().unwrap();
        }
        assert!(
            unpack_source(&cut).is_empty(),
            "a record that runs past the end"
        );
        // Damage after a good record is still damage: half a snapshot
        // would answer "not in the source" about the half that is gone.
        let mut tail = Vec::new();
        {
            use std::io::Write;
            let mut raw = Vec::new();
            for part in ["a.rs", "fn a() {}\n"] {
                raw.extend_from_slice(&(part.len() as u32).to_le_bytes());
                raw.extend_from_slice(part.as_bytes());
            }
            raw.extend_from_slice(&[200, 0, 0, 0, b'x']);
            let mut gz = flate2::write::GzEncoder::new(&mut tail, flate2::Compression::fast());
            gz.write_all(&raw).unwrap();
            gz.finish().unwrap();
        }
        assert!(
            unpack_source(&tail).is_empty(),
            "one good record, then a broken one"
        );
    }

    /// The source the binary was built from, asked the way a model asks
    /// it: where is this, show me that file, show me those lines. (The
    /// questions of 2026-11-12: "what does stock::chart use for its data
    /// api", answered by a deployment file; "what commands are there".)
    /// A unit test has no snapshot of its own, so this one installs a
    /// small one; veles-cli's source_snapshot test reads the real one.
    #[tokio::test]
    async fn the_source_is_searched_listed_and_read_by_line() {
        let long: String = (1..=400)
            .map(|i| match i {
                1 => "//! The long file.\n".to_string(),
                50 => "pub fn fifty() {}\n".to_string(),
                300 => "    fn nested_three_hundred() {}\n".to_string(),
                _ => format!("// line {i}\n"),
            })
            .collect();
        let archive: &'static [u8] = Box::leak(
            packed(&[
                ("crates/veles-x/src/lib.rs", "//! x\npub fn chart_url() -> &'static str {\n    \"https://query1.example/v8/chart\"\n}\n"),
                ("crates/veles-x/src/long.rs", &long),
                ("deploy/vps/stock-proxy.filter", "# Yahoo's chart API, which stock::chart calls\n^query[12]\\.finance\\.yahoo\\.com$\n"),
                ("plugins/veles_api.tcl", "proc veles::hello {} { return hi }\nnamespace eval veles {}\n"),
            ])
            .into_boxed_slice(),
        );
        install_source(archive);

        let index = ask(json!({})).await;
        assert!(
            index.contains("The source this binary was built from is in here too (4 files"),
            "{index}"
        );

        // Where is it.
        let hits = ask(json!({"grep": "STOCK::CHART"})).await;
        assert!(hits.starts_with("1 line(s) in 1 file(s)"), "{hits}");
        assert!(
            hits.contains("deploy/vps/stock-proxy.filter:1: # Yahoo's chart API"),
            "{hits}"
        );
        // The text is matched whatever its case, on either side.
        let mixed = ask(json!({"grep": "yahoo's CHART api"})).await;
        assert!(mixed.starts_with("1 line(s) in 1 file(s)"), "{mixed}");
        let narrowed = ask(json!({"grep": "chart", "file": "crates/"})).await;
        assert!(
            narrowed.contains("crates/veles-x/src/lib.rs:2:") && !narrowed.contains("deploy/"),
            "{narrowed}"
        );
        assert!(ask(json!({"grep": "nowhere-zz"}))
            .await
            .contains("is not in the source"));
        assert!(ask(json!({"grep": "x"}))
            .await
            .contains("at least two characters"));

        // The tree, a directory, a file by its path or its unique tail.
        let tree = ask(json!({"file": "list"})).await;
        assert!(tree.starts_with("4 source file(s)"), "{tree}");
        assert!(
            tree.contains("crates/veles-x/") && tree.contains("deploy/vps/"),
            "{tree}"
        );
        let dir = ask(json!({"file": "crates/veles-x"})).await;
        assert!(
            dir.contains("crates/veles-x/src/long.rs  (400 lines)"),
            "{dir}"
        );
        let short = ask(json!({"file": "stock-proxy.filter"})).await;
        assert!(
            short.starts_with("deploy/vps/stock-proxy.filter (2 lines)"),
            "{short}"
        );
        assert!(short.contains("    2  ^query[12]"), "{short}");

        // A long file opens with its head and its definitions, and a
        // range is exactly that range, never more than the cap.
        let outline = ask(json!({"file": "crates/veles-x/src/long.rs"})).await;
        assert!(outline.contains("(400 lines; lines=a-b"), "{outline}");
        assert!(outline.contains("   50  pub fn fifty() {}"), "{outline}");
        assert!(
            outline.contains("  300      fn nested_three_hundred() {}"),
            "{outline}"
        );
        assert!(!outline.contains("// line 200"), "{outline}");
        let range = ask(json!({"file": "long.rs", "lines": "49-51"})).await;
        assert_eq!(
            range,
            "crates/veles-x/src/long.rs lines 49-51 of 400\n   49  // line 49\n   50  pub fn fifty() {}\n   51  // line 51"
        );
        let capped = ask(json!({"file": "long.rs", "lines": "1-400"})).await;
        assert!(
            capped.starts_with(&format!(
                "crates/veles-x/src/long.rs lines 1-{MAX_LINES} of 400"
            )),
            "{capped}"
        );
        assert!(ask(json!({"file": "long.rs", "lines": "900"}))
            .await
            .contains("has 400 lines"));

        // What is not there says what is near.
        let near = ask(json!({"file": "crates/veles-x/src/lng.rs"})).await;
        assert!(near.contains("no source file"), "{near}");
        let two = ask(json!({"file": ".rs"})).await;
        assert!(two.contains("names 2 files"), "{two}");
    }

    /// A page a frontend adds at startup (the command table lives above
    /// this crate) is listed, opened and searched like a built-in one.
    #[tokio::test]
    async fn a_page_added_at_startup_is_listed_opened_and_searched() {
        install_page(
            "livetest-commands",
            "# Commands on this surface\n\n## /model\n\nshow or switch the model (zzlivemark)\n"
                .into(),
        );
        let index = ask(json!({})).await;
        assert!(
            index.contains("\n  livetest-commands — Commands on this surface"),
            "{index}"
        );
        let page = ask(json!({"page": "livetest-commands"})).await;
        assert!(page.contains("## /model"), "{page}");
        let hits = ask(json!({"query": "zzlivemark"})).await;
        assert!(hits.starts_with("── livetest-commands › /model"), "{hits}");
        install_page("livetest-commands", "# Replaced\n".into());
        assert_eq!(
            ask(json!({"page": "livetest-commands"})).await,
            "# Replaced\n"
        );
    }

    /// A heading is not one inside a fenced block: a `# comment` in a
    /// shell example would otherwise cut the section it is in.
    #[test]
    fn a_comment_in_a_code_block_is_not_a_heading() {
        let text = "# Heading one\n\nintro\n\n## Heading two\n\n```sh\n# not a heading\nls\n```\n\nafter\n\n## Heading three\n\nx\n";
        let secs = sections(text);
        let headings: Vec<&str> = secs.iter().map(|s| s.heading).collect();
        assert_eq!(
            headings,
            vec!["Heading one", "Heading two", "Heading three"]
        );
        let run = &text[secs[1].start..secs[1].end];
        assert!(
            run.contains("# not a heading") && run.contains("after"),
            "{run}"
        );
    }

    /// Every top-level page of `docs/`, the README and this version's
    /// release notes are in the binary: a page written and not listed in
    /// PAGES is one the bot cannot read.
    #[test]
    fn docs_pages_are_all_bundled() {
        let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
        let mut on_disk: Vec<String> = std::fs::read_dir(&docs)
            .unwrap_or_else(|e| panic!("{}: {e}", docs.display()))
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.strip_suffix(".md").map(String::from)
            })
            .collect();
        on_disk.sort();
        assert!(
            on_disk.len() >= 10,
            "the docs directory was not read: {on_disk:?}"
        );
        for stem in &on_disk {
            assert!(
                PAGES.iter().any(|p| p.name == stem),
                "docs/{stem}.md is not in selfdocs::PAGES: the bot cannot read it"
            );
        }
        for page in PAGES {
            assert!(page.text.len() > 200, "{} is empty", page.name);
            assert!(!title(page).is_empty());
        }
        let notes = PAGES.iter().find(|p| p.name == "release-notes").unwrap();
        assert!(
            notes
                .text
                .contains(&format!("veles {}", env!("CARGO_PKG_VERSION"))),
            "the release notes are this version's"
        );
    }
}
