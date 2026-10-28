//! Link Racket CS into the host: `libracketcs.a` from `VELES_RACKET_HOME`
//! (the flake's trimmed racket-minimal, or any Racket CS 9.3 install with
//! lib/libracketcs.a, lib/racket/{petite,scheme,racket}.boot and
//! share/racket/collects) and the libraries beneath it (lz4, iconv, zlib,
//! ncurses; m, dl, pthread, rt), whose directories `VELES_RACKET_LIBS`
//! lists colon-separated. The home is baked in as the default the engine
//! boots from (`VELES_RACKET_HOME` at run time overrides it); the shared
//! libraries' RUNPATH comes from the flake's RUSTFLAGS, like Tcl's.
fn main() {
    println!("cargo:rerun-if-env-changed=VELES_RACKET_HOME");
    println!("cargo:rerun-if-env-changed=VELES_RACKET_LIBS");
    let home = match std::env::var("VELES_RACKET_HOME") {
        Ok(h) if !h.is_empty() => h,
        _ => panic!(
            "VELES_RACKET_HOME is not set: it names a Racket CS 9.3 install \
             (lib/libracketcs.a, lib/racket/*.boot, share/racket/collects); \
             the dev shell and the package build set it from the flake's racketRuntime"
        ),
    };
    let lib = std::path::Path::new(&home).join("lib");
    assert!(
        lib.join("libracketcs.a").exists(),
        "{}: no libracketcs.a (VELES_RACKET_HOME={home})",
        lib.display()
    );
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static=racketcs");
    for dir in std::env::var("VELES_RACKET_LIBS")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
    {
        println!("cargo:rustc-link-search=native={dir}");
    }
    for l in ["lz4", "iconv", "z", "ncurses", "m", "dl", "pthread", "rt"] {
        println!("cargo:rustc-link-lib={l}");
    }
    println!("cargo:rustc-env=VELES_RACKET_HOME={home}");
}
