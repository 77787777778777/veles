//! Pack the source this binary is built from into the binary.
//!
//! The `veles_docs` tool (veles-core `selfdocs.rs`) answers "how do you
//! work" and "where is that implemented" from a snapshot of the tree: a
//! caller in a sandbox cannot read the host's checkout and must not, and
//! the tree is public anyway. This is the crate that packs it, because
//! it is the last one built: packed in veles-core, every edit anywhere
//! would rebuild veles-core.
//!
//! What goes in: the crates' sources (`src/`, `build.rs`, `Cargo.toml`;
//! not their tests, not the message catalogs), the desktop's Tcl (not
//! its suites and their fixtures), the plugins tree and the deployment
//! files. Text only. What a build's source does not have is left out: a
//! sandbox build sees the file set its derivation names.
//!
//! The format is `selfdocs::unpack_source`'s: a gzip stream of records,
//! `u32 LE path length, path, u32 LE text length, text`, paths relative
//! to the workspace root with `/`, sorted, no timestamps, so the same
//! tree packs to the same bytes.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The largest single file packed.
const MAX_FILE: u64 = 2 * 1024 * 1024;

fn wanted(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let name = parts.last().copied().unwrap_or("");
    match parts.first().copied() {
        Some("crates") => {
            // crates/<crate>/src/**, and build.rs / Cargo.toml beside it.
            match parts.get(2).copied() {
                Some("src") => !rel.contains("/locales/"),
                Some("build.rs") | Some("Cargo.toml") => parts.len() == 3,
                _ => false,
            }
        }
        Some("veles-desktop") => {
            name.ends_with(".tcl")
                && !name.ends_with("test.tcl")
                && !name.contains("fixtures")
                && !name.contains("captured")
        }
        Some("plugins") | Some("deploy") => true,
        _ => false,
    }
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        let path = entry.path();
        let rel = format!("{rel}/{name}");
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, &rel, out),
            Ok(t) if t.is_file() => {
                let small = entry
                    .metadata()
                    .map(|m| m.len() <= MAX_FILE)
                    .unwrap_or(false);
                if small && wanted(&rel) {
                    out.push((rel, path));
                }
            }
            _ => {}
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for top in ["crates", "veles-desktop/lib", "plugins", "deploy"] {
        let dir = root.join(top);
        // A path that does not exist reads as changed on every build.
        if dir.is_dir() {
            println!("cargo:rerun-if-changed={}", dir.display());
            walk(&dir, top, &mut files);
        }
    }
    files.sort();
    let mut raw: Vec<u8> = Vec::new();
    let mut packed = 0usize;
    for (rel, path) in &files {
        // Text only: anything that is not UTF-8 is not source to read.
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        raw.extend_from_slice(&(rel.len() as u32).to_le_bytes());
        raw.extend_from_slice(rel.as_bytes());
        raw.extend_from_slice(&(text.len() as u32).to_le_bytes());
        raw.extend_from_slice(text.as_bytes());
        packed += 1;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("source-snapshot.gz");
    let file = std::fs::File::create(&out).expect("create the snapshot");
    let mut gz = flate2::GzBuilder::new()
        .mtime(0)
        .write(file, flate2::Compression::best());
    gz.write_all(&raw).expect("write the snapshot");
    gz.finish().expect("finish the snapshot");
    println!("cargo:rustc-env=VELES_SOURCE_SNAPSHOT_FILES={packed}");
}
