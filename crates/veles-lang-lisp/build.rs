//! Link ECL into the host: `libecl.so` from `VELES_ECL_HOME` (the flake's
//! trimmed ecl, or any ECL install with lib/libecl.so and
//! lib/ecl-<version>/). The library's RUNPATH comes from the flake's
//! RUSTFLAGS, like Tcl's. The home is baked in as the default the engine
//! boots from (`VELES_ECL_HOME` at run time overrides it).
fn main() {
    println!("cargo:rerun-if-env-changed=VELES_ECL_HOME");
    let home = match std::env::var("VELES_ECL_HOME") {
        Ok(h) if !h.is_empty() => h,
        _ => panic!(
            "VELES_ECL_HOME is not set: it names an ECL install (lib/libecl.so, \
             lib/ecl-<version>/); the dev shell and the package build set it from \
             the flake's eclRuntime"
        ),
    };
    let lib = std::path::Path::new(&home).join("lib");
    assert!(
        lib.join("libecl.so").exists(),
        "{}: no libecl.so (VELES_ECL_HOME={home})",
        lib.display()
    );
    check_option_indices(&home);
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=ecl");
    println!("cargo:rustc-env=VELES_ECL_HOME={home}");
}

/// The engine sets ECL options by index (src/ffi.rs `opt`); the indices
/// are the order of `enum ecl_option` in the install's external.h, read
/// here so an ECL that reorders them fails the build, not a boot.
fn check_option_indices(home: &str) {
    let header = std::path::Path::new(home).join("include/ecl/external.h");
    println!("cargo:rerun-if-changed={}", header.display());
    let text =
        std::fs::read_to_string(&header).unwrap_or_else(|e| panic!("{}: {e}", header.display()));
    let start = text
        .find("ECL_OPT_INCREMENTAL_GC = 0")
        .unwrap_or_else(|| panic!("{}: no `enum ecl_option`", header.display()));
    let names: Vec<&str> = text[start..]
        .split(',')
        .map(|t| t.trim().trim_end_matches(" = 0").trim())
        .take_while(|t| t.starts_with("ECL_OPT_") && !t.starts_with("ECL_OPT_LIMIT"))
        .collect();
    for (name, index) in [
        ("ECL_OPT_TRAP_SIGSEGV", 1),
        ("ECL_OPT_TRAP_SIGFPE", 2),
        ("ECL_OPT_TRAP_SIGINT", 3),
        ("ECL_OPT_TRAP_SIGILL", 4),
        ("ECL_OPT_TRAP_SIGBUS", 5),
        ("ECL_OPT_TRAP_SIGPIPE", 6),
        ("ECL_OPT_SIGNAL_HANDLING_THREAD", 8),
        ("ECL_OPT_BOOTED", 10),
        ("ECL_OPT_HEAP_SIZE", 19),
    ] {
        assert_eq!(
            names.get(index).copied(),
            Some(name),
            "{}: {name} is not option {index} (the enum reads {names:?}); update src/ffi.rs",
            header.display()
        );
    }
}
