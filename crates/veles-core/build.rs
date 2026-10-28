//! Bake the module build key into veles-core (PLAN-plugins-installer.md
//! M2).
//!
//! `VELES_BUILD_KEY` is a blake3 hash over everything that decides the
//! layout of what libveles_runtime.so exports and cargo's `-C metadata`
//! does NOT cover:
//!
//! - the sources of every path crate inside the runtime (a source edit
//!   leaves cargo's metadata hash, and so every mangled symbol name,
//!   unchanged, so a stale module would link and run against changed
//!   layouts): veles-core, veles-frontends, veles-types, veles-runtime,
//!   veles-plugin-abi and vendor/clib, each crate's `*.rs` under src/,
//!   its build.rs and its Cargo.toml;
//! - Cargo.lock (every registry and git crate's exact version);
//! - `rustc -vV`, the profile, the target, the RUSTFLAGS in force;
//! - veles-core's own enabled features.
//!
//! What the key cannot see (the features cargo unified onto the
//! runtime's OTHER dependencies in this build) does show in the mangled
//! symbol names, and the host checks those separately before dlopen
//! (`module::check`).
//!
//! `module::BUILD_KEY` is a `const`, so `export!` copies the value into
//! the module at the MODULE's compile time; the host compares it with
//! its own.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Path crates compiled into libveles_runtime.so, relative to the
/// workspace root. A new path dependency of the runtime's graph belongs
/// here; `cargo metadata` lists them.
const SHARED_PATH_CRATES: &[&str] = &[
    "crates/veles-core",
    "crates/veles-frontends",
    "crates/veles-types",
    "crates/veles-runtime",
    "crates/veles-plugin-abi",
    "vendor/clib",
];

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crates/veles-core has the workspace root two levels up")
        .to_path_buf();

    let rustc_vv =
        std::process::Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
            .arg("-vV")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "<unknown rustc>".into());
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    // What cargo passes rustc, \x1f-separated.
    let rustflags = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    let features: BTreeSet<String> = std::env::vars()
        .filter_map(|(k, _)| k.strip_prefix("CARGO_FEATURE_").map(str::to_string))
        .collect();

    let mut h = blake3::Hasher::new();
    let mut field = |name: &str, value: &[u8]| {
        h.update(name.as_bytes());
        h.update(&(value.len() as u64).to_le_bytes());
        h.update(value);
    };
    field("v", b"2");
    field("rustc", rustc_vv.as_bytes());
    field("profile", profile.as_bytes());
    field("target", target.as_bytes());
    field("rustflags", rustflags.as_bytes());
    field(
        "features",
        features
            .into_iter()
            .collect::<Vec<_>>()
            .join(",")
            .as_bytes(),
    );

    let lock = workspace.join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    field("lock", &std::fs::read(&lock).unwrap_or_default());

    for krate in SHARED_PATH_CRATES {
        let root = workspace.join(krate);
        // A directory rerun-if-changed makes cargo rescan it, so a new
        // file counts as a change too.
        println!("cargo:rerun-if-changed={}", root.join("src").display());
        let mut files = BTreeSet::new();
        collect_rs(&root.join("src"), &mut files);
        for extra in ["build.rs", "Cargo.toml"] {
            let p = root.join(extra);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
                files.insert(p);
            }
        }
        for f in files {
            let rel = f.strip_prefix(&workspace).unwrap_or(&f);
            field("file", rel.to_string_lossy().as_bytes());
            field("bytes", &std::fs::read(&f).unwrap_or_default());
        }
    }

    println!("cargo:rustc-env=VELES_BUILD_KEY={}", h.finalize().to_hex());
}

fn collect_rs(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.insert(p);
        }
    }
}
