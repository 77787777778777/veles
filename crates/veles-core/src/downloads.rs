//! Files a tool fetched for the model. A web page is read; a PDF is
//! downloaded instead, into the session's own workspace, and the model
//! reads it there with its own tools (USER, 2026-10-12: "it should just
//! download the pdf if the response is a pdf", "it's fine that the agent
//! does the text extract itself").
//!
//! A tool that downloads stages the bytes on ITS node and puts one line
//! in its output, `[download <token>]`. Two more places act on that line:
//!
//! - a re-homed call's node attaches the staged files to its reply
//!   ([`attach`], `remotetool::execute`), and the calling node stages
//!   them on its side ([`receive`], `remotetool::call_on`). The bytes
//!   ride beside the text, never in it: a small file inside the reply,
//!   a large one (a 60 MB scan is not rare, USER) pulled after it in
//!   [`PART_BYTES`] parts ([`PART_OP`], [`serve_part`]), each a call of
//!   its own, and checked whole against its sha256;
//! - the session's node, at the bottom of the tool chain
//!   (`ToolRegistry::dispatch_default`), puts each file where the
//!   session's commands run ([`deliver`]): its sandbox box, or the host
//!   when its commands run there. The line becomes the file's path.
//!
//! The token is the capability: 128 random bits naming a file in this
//! node's staging directory, taken once. A page whose text copies the
//! line's shape names nothing staged, so its line only says so.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The largest file brought to a session.
pub const MAX_BYTES: usize = 128 * 1024 * 1024;

/// The most one cluster message carries of a file. A message carries it
/// base64'd twice (in its JSON, then in the cluster fence), next to up
/// to 512 KiB of text, and has to stay under the A2A body limit
/// (32 MiB). A file up to this size rides in the reply itself; a larger
/// one is pulled after it, part by part.
pub const PART_BYTES: usize = 8 * 1024 * 1024;

/// The internal operation a calling node pulls a large file's parts
/// with. In no tool registry; `remotetool::decide` gates it as the tool
/// that downloads (`web_extract`) and runs it before any tool lookup.
pub const PART_OP: &str = "download_part";

/// How long a staged file waits to be delivered. Every new stage sweeps
/// out what is older: a file whose line was lost is not kept forever.
const STAGE_TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

const LINE_PREFIX: &str = "[download ";

/// What a staged file is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// The name it had where it came from (`Published.pdf`).
    pub name: String,
    pub mime: String,
    /// Where it was fetched from.
    pub url: String,
}

fn staging_dir() -> PathBuf {
    crate::config::cache_dir().join("download-staging")
}

fn valid_token(t: &str) -> bool {
    t.len() == 32 && t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The line a tool's output carries for a staged file.
pub fn line(token: &str) -> String {
    format!("{LINE_PREFIX}{token}]")
}

/// The token `l` names, when the whole line is a download line.
fn token_of(l: &str) -> Option<&str> {
    l.strip_prefix(LINE_PREFIX)?
        .strip_suffix(']')
        .filter(|t| valid_token(t))
}

/// Keep `bytes` for delivery. Returns the token for [`line`].
pub fn stage(bytes: &[u8], meta: &Meta) -> Result<String> {
    if bytes.len() > MAX_BYTES {
        anyhow::bail!(
            "{} is {}, over the {} a download may be",
            meta.name,
            human_size(bytes.len()),
            human_size(MAX_BYTES)
        );
    }
    let token = crate::challenge::random_hex(16).context("no randomness for a download token")?;
    put(&token, bytes, meta)?;
    Ok(token)
}

/// Write a staged file and its meta under `token`.
fn put(token: &str, bytes: &[u8], meta: &Meta) -> Result<()> {
    let dir = staging_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    sweep(&dir);
    write_private(&dir.join(token), bytes)?;
    write_private(
        &dir.join(format!("{token}.json")),
        serde_json::to_string(meta)?.as_bytes(),
    )?;
    Ok(())
}

/// A new file only this user can read: it may hold what a person's
/// cookies fetched.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    o.open(path)?.write_all(bytes)
}

fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STAGE_TTL);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// A staged file's meta and bytes, left in place.
fn peek(token: &str) -> Option<(Meta, Vec<u8>)> {
    if !valid_token(token) {
        return None;
    }
    let dir = staging_dir();
    let meta: Meta =
        serde_json::from_slice(&std::fs::read(dir.join(format!("{token}.json"))).ok()?).ok()?;
    Some((meta, std::fs::read(dir.join(token)).ok()?))
}

/// Drop a staged file without reading it.
fn discard(token: &str) {
    if valid_token(token) {
        let dir = staging_dir();
        let _ = std::fs::remove_file(dir.join(token));
        let _ = std::fs::remove_file(dir.join(format!("{token}.json")));
    }
}

/// A staged file, taken: it is gone from staging afterwards.
fn take(token: &str) -> Option<(Meta, Vec<u8>)> {
    if !valid_token(token) {
        return None;
    }
    let dir = staging_dir();
    let meta_path = dir.join(format!("{token}.json"));
    let meta: Meta = serde_json::from_slice(&std::fs::read(&meta_path).ok()?).ok()?;
    let bytes = std::fs::read(dir.join(token)).ok()?;
    let _ = std::fs::remove_file(dir.join(token));
    let _ = std::fs::remove_file(meta_path);
    Some((meta, bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// The files `output` names, for a re-homed call's reply: a small one
/// taken from staging here and carried whole, a large one described
/// (size, sha256, parts) and left staged for [`serve_part`].
pub fn attach(output: &str) -> Vec<Value> {
    output
        .lines()
        .filter_map(token_of)
        .filter_map(|t| {
            let (meta, bytes) = peek(t)?;
            if bytes.len() <= PART_BYTES {
                discard(t);
                return Some(json!({ "token": t, "meta": meta, "b64": b64(&bytes) }));
            }
            Some(json!({
                "token": t,
                "meta": meta,
                "size": bytes.len(),
                "sha256": sha256_hex(&bytes),
                "parts": bytes.len().div_ceil(PART_BYTES),
            }))
        })
        .collect()
}

/// A large file's part, for the node it was announced to. The last part
/// takes the file out of staging.
pub fn serve_part(args: &Value) -> Value {
    use std::io::{Read as _, Seek as _};
    let err = |why: &str| json!({"status": "error", "error": why});
    let token = args["token"].as_str().unwrap_or_default();
    let index = args["index"].as_u64().unwrap_or(u64::MAX) as usize;
    if !valid_token(token) {
        return err("no such download here");
    }
    let Ok(mut f) = std::fs::File::open(staging_dir().join(token)) else {
        return err("no such download here");
    };
    let size = f.metadata().map(|m| m.len() as usize).unwrap_or(0);
    let parts = size.div_ceil(PART_BYTES);
    if index >= parts {
        return err("no such part");
    }
    let start = index * PART_BYTES;
    let mut part = vec![0u8; PART_BYTES.min(size - start)];
    if f.seek(std::io::SeekFrom::Start(start as u64))
        .and_then(|_| f.read_exact(&mut part))
        .is_err()
    {
        return err("the part could not be read");
    }
    if index + 1 == parts {
        discard(token);
    }
    json!({"status": "ok", "part": b64(&part)})
}

/// A file a reply announced, still to be pulled.
#[derive(Debug, Clone, PartialEq)]
pub struct Announced {
    pub token: String,
    meta: Meta,
    size: usize,
    sha256: String,
    pub parts: usize,
}

/// A reply's files: the small ones staged here under the tokens their
/// lines name, the large ones returned to be pulled part by part. A
/// malformed or oversized one is dropped, and its line then says that
/// nothing is staged under it.
pub fn receive(files: &Value) -> Vec<Announced> {
    let mut pull = Vec::new();
    for f in files.as_array().into_iter().flatten() {
        let Some(token) = f["token"].as_str().filter(|t| valid_token(t)) else {
            continue;
        };
        let Ok(meta) = serde_json::from_value::<Meta>(f["meta"].clone()) else {
            continue;
        };
        if let Some(b) = f["b64"].as_str() {
            if let Some(bytes) = unb64(b).filter(|b| b.len() <= PART_BYTES) {
                let _ = put(token, &bytes, &meta);
            }
            continue;
        }
        let size = f["size"].as_u64().unwrap_or(0) as usize;
        let sha256 = f["sha256"].as_str().unwrap_or_default().to_string();
        if size == 0 || size > MAX_BYTES || sha256.len() != 64 {
            continue;
        }
        pull.push(Announced {
            token: token.to_string(),
            meta,
            size,
            sha256,
            parts: size.div_ceil(PART_BYTES),
        });
    }
    pull
}

/// The parts of `a`, as pulled, put together and checked: staged here
/// when they are the file the reply announced, else a note under its
/// token that says why it is not (`deliver` reads it).
pub fn accept(a: &Announced, parts: Result<Vec<String>>) {
    let checked = parts.and_then(|parts| {
        let mut bytes = Vec::with_capacity(a.size);
        for p in parts {
            bytes.extend(unb64(&p).context("a part is not base64")?);
            anyhow::ensure!(bytes.len() <= a.size, "more bytes than announced");
        }
        anyhow::ensure!(
            bytes.len() == a.size && sha256_hex(&bytes) == a.sha256,
            "the parts are not the file that was announced"
        );
        Ok(bytes)
    });
    let staged = checked.and_then(|bytes| put(&a.token, &bytes, &a.meta));
    if let Err(e) = staged {
        note_failed(&a.token, &format!("{e:#}"));
    }
}

/// Why a file announced under `token` did not arrive.
pub fn note_failed(token: &str, why: &str) {
    if !valid_token(token) {
        return;
    }
    let dir = staging_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = write_private(&dir.join(format!("{token}.failed")), why.as_bytes());
    }
}

fn failure(token: &str) -> Option<String> {
    let path = staging_dir().join(format!("{token}.failed"));
    let why = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(path);
    Some(why)
}

/// Put every file `output` names where `session_id`'s commands run, and
/// turn each line into what the model needs: the path, and how to read
/// it.
pub async fn deliver(session_id: &str, output: String) -> String {
    if !output.contains(LINE_PREFIX) {
        return output;
    }
    let mut out: Vec<String> = Vec::new();
    for l in output.lines() {
        match token_of(l) {
            Some(t) => out.push(deliver_one(session_id, t).await),
            None => out.push(l.to_string()),
        }
    }
    out.join("\n")
}

async fn deliver_one(session_id: &str, token: &str) -> String {
    let Some((meta, bytes)) = take(token) else {
        return match failure(token) {
            Some(why) => format!("[a file was downloaded, but it did not arrive here: {why}]"),
            None => "[a download line, but no file is staged under it on this node]".into(),
        };
    };
    let name = safe_name(&meta.name, token, &meta.mime);
    let what = format!(
        "{} ({}, {})",
        meta.name.rsplit('/').next().unwrap_or(&meta.name),
        human_size(bytes.len()),
        meta.mime
    );
    let zone = crate::user_envs::zone_for(Some(session_id), crate::user_envs::ZoneArg::Unset);
    let placed = match &zone {
        crate::user_envs::Zone::Sandbox { key, .. } => {
            let path = format!("/work/downloads/{name}");
            crate::user_envs::write_bytes(key, &path, &bytes)
                .await
                .map(|()| path)
        }
        crate::user_envs::Zone::Host => host_place(&name, &bytes),
    };
    match placed {
        Ok(path) => format!(
            "[downloaded {what} from {} to {path} where your commands run. It is not \
             text yet{}]",
            meta.url,
            read_hint(&meta.mime, &path)
        ),
        Err(e) => format!(
            "[downloaded {what} from {}, but it could not be put where your commands \
             run: {e:#}]",
            meta.url
        ),
    }
}

/// The host's place for downloads: regenerable, so under the cache.
fn host_place(name: &str, bytes: &[u8]) -> Result<String> {
    let dir = crate::config::cache_dir().join("downloads");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    write_private(&path, bytes)?;
    Ok(path.display().to_string())
}

fn read_hint(mime: &str, path: &str) -> String {
    if mime.contains("pdf") {
        format!(
            ": read it in your terminal with `pdftotext -layout {path} -` (from \
             poppler-utils, which `apt-get install -y poppler-utils` adds where it is \
             missing)"
        )
    } else {
        String::new()
    }
}

/// A file name that is only a name: no directory, nothing a shell or a
/// path would read twice, and the token's first six digits so two
/// `Published.pdf` from two portals do not overwrite each other.
pub fn safe_name(raw: &str, token: &str, mime: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '.' || c == '_');
    let (stem, ext) = match cleaned.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.is_empty() && e.len() <= 8 => {
            (s.to_string(), e.to_ascii_lowercase())
        }
        _ => (cleaned.to_string(), String::new()),
    };
    let ext = if ext.is_empty() && mime.contains("pdf") {
        "pdf".to_string()
    } else {
        ext
    };
    let stem: String = match stem.chars().take(60).collect::<String>() {
        s if s.is_empty() => "download".into(),
        s => s,
    };
    let tag: String = token.chars().take(6).collect();
    if ext.is_empty() {
        format!("{stem}-{tag}")
    } else {
        format!("{stem}-{tag}.{ext}")
    }
}

/// A PDF by its bytes, whatever the server called it: `%PDF-` within the
/// first KiB, where the format allows it to start.
pub fn is_pdf(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(1024)]
        .windows(5)
        .any(|w| w == b"%PDF-")
}

/// The last path segment of `url`, without its query: the name a
/// browser would save it under.
pub fn name_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let path = path.split_once("://").map(|(_, r)| r).unwrap_or(path);
    let seg = path
        .split_once('/')
        .map(|(_, p)| p)
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if seg.is_empty() {
        "download".into()
    } else {
        seg.to_string()
    }
}

pub fn human_size(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{} KB", n / 1000)
    } else {
        format!("{n} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str) -> Meta {
        Meta {
            name: name.into(),
            mime: "application/pdf".into(),
            url: format!("https://example.org/files/{name}"),
        }
    }

    /// A session whose commands run on the host gets the file under the
    /// cache, once: a second delivery of the same line finds nothing.
    #[tokio::test]
    async fn a_staged_file_is_delivered_once() {
        let (_home, _env) = crate::testenv::scratch_home();
        let t = stage(b"%PDF-1.6 body", &meta("Published.pdf")).unwrap();
        let out = format!("[web_extract ran on node laptop]\n{}\nmore", line(&t));
        let got = deliver("", out.clone()).await;
        let path = crate::config::cache_dir().join("downloads").join(safe_name(
            "Published.pdf",
            &t,
            "application/pdf",
        ));
        assert!(got.contains(&path.display().to_string()), "{got}");
        assert!(got.contains("pdftotext -layout"), "{got}");
        assert!(got.starts_with("[web_extract ran on node laptop]\n") && got.ends_with("\nmore"));
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.6 body");
        let again = deliver("", out).await;
        assert!(again.contains("no file is staged"), "{again}");
    }

    /// Lines that are not a token naming a staged file deliver nothing,
    /// and a path dressed as a token never reaches the file system.
    #[tokio::test]
    async fn only_a_staged_token_is_a_download() {
        let (_home, _env) = crate::testenv::scratch_home();
        let forged = format!("{}\n", line(&"0".repeat(32)));
        assert!(deliver("", forged).await.contains("no file is staged"));
        for not_a_line in [
            "[download ../../../etc/passwd]",
            "[download 0123]",
            " [download 0123456789abcdef0123456789abcdef]",
        ] {
            assert_eq!(deliver("", not_a_line.to_string()).await, not_a_line);
        }
        assert!(take("../download-staging/x").is_none());
    }

    /// Across a re-homed call: a small file rides in the reply. Attaching
    /// takes it from staging on the node that ran the tool; receiving
    /// stages it on the calling node under the same token, the same
    /// bytes and the same meta.
    #[test]
    fn a_small_file_rides_in_the_reply() {
        let (_home, _env) = crate::testenv::scratch_home();
        let t = stage(b"%PDF-1.4 x", &meta("a.pdf")).unwrap();
        let output = format!("{}\nurl: https://example.org/files/a.pdf", line(&t));
        let files = attach(&output);
        assert_eq!(files.len(), 1);
        assert!(take(&t).is_none(), "attaching takes it from here");
        let wire: Value = serde_json::from_str(&json!({ "files": files }).to_string()).unwrap();
        assert!(receive(&wire["files"]).is_empty(), "nothing left to pull");
        assert_eq!(take(&t), Some((meta("a.pdf"), b"%PDF-1.4 x".to_vec())));
        assert!(attach("no lines here").is_empty());
    }

    /// A file over one message's worth (the USER's 60 MB scans) is
    /// announced, pulled part by part, and staged only when the parts
    /// are the file the reply announced.
    #[tokio::test]
    async fn a_large_file_is_pulled_in_parts_and_checked() {
        let (_home, _env) = crate::testenv::scratch_home();
        let body: Vec<u8> = (0..2 * PART_BYTES + 5).map(|i| (i % 251) as u8).collect();
        let t = stage(&body, &meta("scan.pdf")).unwrap();
        let files = attach(&line(&t));
        assert!(files[0].get("b64").is_none(), "a large file is not inline");
        let pull = receive(&json!(files));
        assert_eq!(pull.len(), 1);
        assert_eq!(pull[0].parts, 3);
        let parts: Vec<String> = (0..3)
            .map(|i| {
                let v = serve_part(&json!({"token": t, "index": i}));
                assert_eq!(v["status"], "ok", "{v}");
                v["part"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            serve_part(&json!({"token": t, "index": 0}))["status"],
            "error",
            "the last part took it out of staging"
        );
        accept(&pull[0], Ok(parts.clone()));
        assert_eq!(take(&t).map(|(_, b)| b), Some(body.clone()));

        // A part that is not what was announced, though as long:
        // nothing is staged, and the line says why.
        let mut bad = parts;
        bad[1] = b64(&vec![0u8; PART_BYTES]);
        accept(&pull[0], Ok(bad));
        let got = deliver("", line(&t)).await;
        assert!(
            got.contains("did not arrive") && got.contains("not the file"),
            "{got}"
        );
        accept(&pull[0], Err(anyhow::anyhow!("node laptop did not answer")));
        assert!(deliver("", line(&t)).await.contains("did not answer"));
    }

    /// What a peer sends is checked like anything from outside: a bad
    /// token, an inline file over a message's worth, broken base64, a bad
    /// meta and an announcement past the cap are not staged or pulled.
    #[test]
    fn a_reply_cannot_stage_what_a_tool_could_not() {
        let (_home, _env) = crate::testenv::scratch_home();
        let m = serde_json::to_value(meta("x.pdf")).unwrap();
        let files = json!([
            {"token": "../../x", "meta": m, "b64": b64(b"%PDF")},
            {"token": "1".repeat(32), "meta": m, "b64": b64(&vec![0u8; PART_BYTES + 1])},
            {"token": "2".repeat(32), "meta": m, "b64": "not base64!"},
            {"token": "3".repeat(32), "meta": {"name": 1}, "b64": b64(b"%PDF")},
            {"token": "4".repeat(32), "meta": m, "size": MAX_BYTES + 1, "sha256": "a".repeat(64)},
            {"token": "5".repeat(32), "meta": m, "size": 10, "sha256": "short"},
        ]);
        assert!(receive(&files).is_empty());
        assert!(
            !staging_dir().join("../../x").exists(),
            "a token shaped like a path wrote outside staging"
        );
        for t in ["1", "2", "3"] {
            assert!(take(&t.repeat(32)).is_none());
        }
        assert!(stage(&vec![0u8; MAX_BYTES + 1], &meta("big.pdf")).is_err());
        assert_eq!(
            serve_part(&json!({"token": "../x", "index": 0}))["status"],
            "error"
        );
    }

    #[test]
    fn names_are_only_names() {
        let t = "abcdef0123456789abcdef0123456789";
        let pdf = "application/pdf";
        assert_eq!(safe_name("Published.pdf", t, pdf), "Published-abcdef.pdf");
        assert_eq!(safe_name("../../etc/passwd", t, ""), "passwd-abcdef");
        assert_eq!(safe_name("..", t, pdf), "download-abcdef.pdf");
        assert_eq!(safe_name("a b;rm -rf.PDF", t, pdf), "a_b_rm_-rf-abcdef.pdf");
        assert_eq!(safe_name("", t, pdf), "download-abcdef.pdf");
        assert_eq!(
            name_from_url("https://pure.example/ws/portal/60371196/Published.pdf?x=1#p"),
            "Published.pdf"
        );
        assert_eq!(name_from_url("https://example.org"), "download");
        assert_eq!(name_from_url("https://example.org/"), "download");
    }
}
