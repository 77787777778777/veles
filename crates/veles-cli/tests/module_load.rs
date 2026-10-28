//! The module loader's checks against REAL module files
//! (PLAN-plugins-installer.md M2, R1).
//!
//! The echo module is a dev-dependency of veles-cli, so cargo builds it
//! in the same invocation as this test, against the same runtime. That
//! is the only way a module and a host agree (module.rs says why). The
//! refusals are checked on copies of it with one thing changed, and each
//! must happen from the FILE, before dlopen.

use std::path::{Path, PathBuf};
use veles_runtime::veles_core::module::{self, Refusal};

fn echo_so() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    // `libveles_module_echo.so`, or `veles_module_echo.dll` on Windows.
    let so = exe.parent().expect("deps dir").join(format!(
        "{}veles_module_echo{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    assert!(
        so.exists(),
        "the echo module is not built: {}",
        so.display()
    );
    so
}

/// A copy of the echo module with every occurrence of `from` replaced
/// by `to` (same length).
fn patched_copy(dir: &Path, from: &[u8], to: &[u8]) -> PathBuf {
    assert_eq!(from.len(), to.len());
    let mut bytes = std::fs::read(echo_so()).unwrap();
    let mut hits = 0;
    let mut i = 0;
    while i + from.len() <= bytes.len() {
        if &bytes[i..i + from.len()] == from {
            bytes[i..i + from.len()].copy_from_slice(to);
            hits += 1;
            i += from.len();
        } else {
            i += 1;
        }
    }
    assert!(hits > 0, "nothing to patch");
    let p = dir.join("patched.so");
    std::fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn the_echo_module_of_this_build_passes() {
    let f = module::inspect(&echo_so()).expect("inspect");
    assert_eq!(f.key.as_deref(), Some(module::BUILD_KEY));
    assert!(
        f.needed.iter().any(|n| n == module::RUNTIME_SONAME),
        "{:?}",
        f.needed
    );
    assert!(!f.core_imports.is_empty(), "the echo module calls core");
    module::check(&f).expect("same build");
}

#[test]
fn another_key_is_refused_from_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let key = module::BUILD_KEY.as_bytes();
    let mut other = key.to_vec();
    other[0] = if other[0] == b'0' { b'1' } else { b'0' };
    let p = patched_copy(dir.path(), key, &other);
    let r = module::check(&module::inspect(&p).unwrap()).unwrap_err();
    assert!(matches!(r, Refusal::KeyMismatch { .. }), "{r:?}");
    assert!(
        r.to_string().starts_with(module::KEY_MISMATCH_MESSAGE),
        "{r}"
    );
}

/// A module whose references into core are not what this runtime
/// defines: what a module built by another cargo invocation looks like.
#[test]
fn a_module_from_another_build_is_refused_from_the_file() {
    let f = module::inspect(&echo_so()).unwrap();
    let sym = f.core_imports[0].clone();
    // Change one character of the crate disambiguator or hash.
    let at = sym.find("veles_core").expect("names core") - 4;
    let mut other = sym.clone().into_bytes();
    other[at] = if other[at] == b'A' { b'B' } else { b'A' };
    let dir = tempfile::tempdir().unwrap();
    let p = patched_copy(dir.path(), sym.as_bytes(), &other);
    let r = module::check(&module::inspect(&p).unwrap()).unwrap_err();
    assert!(matches!(r, Refusal::OtherBuild { .. }), "{r:?}");
}

/// Loaded, the module finds a command this process registered (one copy
/// of core's statics), and after dlclose it is no longer mapped.
#[test]
fn the_module_shares_core_and_unmaps() {
    use veles_runtime::veles_core::commands;
    let marker = format!("module-load-test-{}", std::process::id());
    commands::register_command(commands::PluginCommand {
        name: marker.clone(),
        description: String::new(),
        body: commands::CommandBody::Native(std::sync::Arc::new(|_| String::new())),
    })
    .unwrap();
    let so = echo_so().canonicalize().unwrap();
    module::check(&module::inspect(&so).unwrap()).expect("same build");
    let lib = unsafe { libloading::Library::new(&so) }.unwrap();
    let found = {
        let probe: libloading::Symbol<fn(&str) -> bool> =
            unsafe { lib.get(b"veles_spike_statics") }.unwrap();
        probe(&marker)
    };
    assert!(found, "the module saw another copy of core");
    drop(lib);
    // The platform layer's answer (`/proc/self/maps` on Linux, the module
    // table on Windows): `Some(empty)` is "not mapped"; `None` is "cannot
    // tell", which the host treats as mapped and this test must not pass.
    let ranges = veles_runtime::veles_core::platform::mapped_ranges(&so);
    assert!(
        ranges.as_ref().is_some_and(|r| r.is_empty()),
        "still mapped after dlclose: {ranges:?}"
    );
}

/// The gateway links no Rust dylib but the runtime. A crate with a dylib
/// form is linked DYNAMICALLY once the runtime is in the graph, and a
/// module the gateway maps at startup is the copy every later load of it
/// binds its symbols to: R4's dual-form veles-irc had the gateway map
/// libveles_irc.so at startup, so a rebuilt IRC module ran the gateway's
/// old IRC code. (velesPkg's postInstall checks the release build.)
#[test]
fn the_gateway_links_no_module() {
    let exe = assert_cmd::cargo::cargo_bin("veles");
    let f = module::inspect(&exe).expect("the veles binary");
    assert!(
        f.needed.iter().any(|n| n == module::RUNTIME_SONAME),
        "{:?}",
        f.needed
    );
    // `libveles_*.so`, or `veles_*.dll` on Windows (no `lib` prefix there).
    let modules: Vec<&String> = f
        .needed
        .iter()
        .filter(|n| {
            let l = n.to_ascii_lowercase();
            (l.starts_with("libveles_") || l.starts_with("veles_"))
                && l != module::RUNTIME_SONAME.to_ascii_lowercase()
        })
        .collect();
    assert!(modules.is_empty(), "the gateway links {modules:?}");
}
