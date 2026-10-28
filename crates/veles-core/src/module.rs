//! The in-process module contract (PLAN-plugins-installer.md M2).
//!
//! A module is a Rust dylib that links `libveles_runtime.so` and declares
//! itself with [`export!`]. That writes two things into the module:
//!
//! - the `veles_module` static ([`ExportedModule`]): the manifest and
//!   the constructor;
//! - the ELF section [`KEY_SECTION`]: the [`BUILD_KEY`] of the core the
//!   module was COMPILED against. `BUILD_KEY` is a `const`, so its value
//!   is copied into the module at the module's build; nothing about it
//!   is looked up in the running host.
//!
//! The host reads the module FILE before `dlopen` ([`inspect`], then
//! [`check`]), so a module from another build is refused before any of
//! its code (initialisers included) runs. The refusal for a changed core:
//!
//! ```text
//! core changed: rebuild the module with this core, or restart the
//! gateway on the new core
//! ```
//!
//! "The same build" means the same cargo invocation. Building a module
//! on its own (`cargo build -p <module>`) unifies features differently
//! from the gateway's build, so it compiles a different runtime (other
//! symbol names, possibly other layouts) even from identical sources; the
//! key cannot see that, and [`check`] catches it from the module's
//! undefined symbols instead. Build modules together with the gateway:
//! `cargo build -p veles-cli -p <module>`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// This build's key: blake3 over the runtime's path-crate sources,
/// Cargo.lock, `rustc -vV`, profile, target, RUSTFLAGS and core's
/// features (build.rs lists them). A `const`: [`export!`] copies it into
/// the module at the module's compile time.
pub const BUILD_KEY: &str = env!("VELES_BUILD_KEY");

/// The section [`export!`] writes the module's key into
/// (`platform::KEY_SECTION`: `.veles_module_key` on ELF, `.vmodkey` on
/// PE, whose image section names are 8 bytes).
pub const KEY_SECTION: &str = crate::platform::KEY_SECTION;

/// The runtime's file name, as a module's import table names it
/// (`DT_NEEDED` on ELF, the import directory on PE).
pub const RUNTIME_SONAME: &str = crate::platform::RUNTIME_FILE;

/// What the refusal reads when a module's key does not match the host's.
pub const KEY_MISMATCH_MESSAGE: &str =
    "core changed: rebuild the module with this core, or restart the gateway on the new core";

/// This process's build key.
pub fn build_key() -> &'static str {
    BUILD_KEY
}

/// `BUILD_KEY` as the fixed-size array the key section holds. A `const
/// fn`, evaluated while the MODULE compiles.
#[doc(hidden)]
pub const fn key_bytes(key: &str) -> [u8; 64] {
    let b = key.as_bytes();
    assert!(b.len() == 64, "a build key is 64 hex characters");
    let mut out = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        out[i] = b[i];
        i += 1;
    }
    out
}

/// Declare a module. In the module crate root:
///
/// ```rust,ignore
/// veles_core::export!(MyModule, my_manifest());
/// ```
///
/// Expands to the `#[no_mangle] pub static veles_module` the host reads
/// after `dlopen`, and the key section it reads before. `my_manifest()`
/// is called once, through a `LazyLock`, when the host first asks.
#[macro_export]
macro_rules! export {
    ($ty:ty, $manifest:expr) => {
        #[used]
        #[cfg_attr(windows, link_section = ".vmodkey")]
        #[cfg_attr(not(windows), link_section = ".veles_module_key")]
        static VELES_MODULE_KEY: [u8; 64] = $crate::module::key_bytes($crate::module::BUILD_KEY);

        #[no_mangle]
        pub static veles_module: $crate::module::ExportedModule =
            $crate::module::ExportedModule::new::<$ty>(|| $manifest, &VELES_MODULE_KEY);
    };
}

/// The exported symbol name. `#[no_mangle]` keeps it exactly this.
pub const EXPORT_SYMBOL: &str = "veles_module";

/// The `veles_module` static's type.
pub struct ExportedModule {
    /// The key section's own bytes. Referencing the section from the
    /// exported static also keeps the linker from discarding it.
    key: &'static [u8; 64],
    manifest: std::sync::LazyLock<veles_types::plugin::Manifest>,
    construct: fn() -> std::sync::Arc<dyn crate::rustplugins::VelesPlugin>,
}

impl ExportedModule {
    pub const fn new<T: crate::rustplugins::VelesPlugin + Default + 'static>(
        manifest: fn() -> veles_types::plugin::Manifest,
        key: &'static [u8; 64],
    ) -> Self {
        Self {
            key,
            manifest: std::sync::LazyLock::new(manifest),
            construct: || {
                std::sync::Arc::new(T::default())
                    as std::sync::Arc<dyn crate::rustplugins::VelesPlugin>
            },
        }
    }

    /// The key the module was compiled with (the key section's bytes).
    pub fn build_key(&self) -> &str {
        std::str::from_utf8(self.key).unwrap_or("")
    }

    /// The manifest. Runs the module's manifest function the first time.
    pub fn manifest(&self) -> &veles_types::plugin::Manifest {
        &self.manifest
    }

    pub fn construct(&self) -> std::sync::Arc<dyn crate::rustplugins::VelesPlugin> {
        (self.construct)()
    }
}

/// What the host learns from a module file without loading it.
#[derive(Debug, Clone)]
pub struct ModuleFile {
    pub path: PathBuf,
    /// The key section's contents; `None` when the file has no section.
    pub key: Option<String>,
    /// The file's `DT_NEEDED` entries.
    pub needed: Vec<String>,
    /// The module's undefined dynamic symbols that name veles_core: what
    /// the host's runtime must define for this module.
    pub core_imports: Vec<String>,
}

/// Why a module file may not be loaded into this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The file cannot be read or is not an ELF shared object.
    Unreadable(String),
    /// No key section: not built with [`export!`].
    NotAModule,
    /// Built against other core sources, lock file, toolchain or flags.
    KeyMismatch { module: String, host: String },
    /// Does not link the runtime, so it carries its own copy of core.
    NoRuntime,
    /// Same sources, but another cargo build: `missing` of its core
    /// references are not defined by this process's runtime.
    OtherBuild { missing: usize, example: String },
    /// This process does not link the runtime, so it has no core to give
    /// a module.
    HostWithoutRuntime(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Unreadable(e) => write!(f, "not a loadable module: {e}"),
            Refusal::NotAModule => write!(
                f,
                "not a veles module (no {KEY_SECTION} section; a module declares itself with veles_core::export!)"
            ),
            Refusal::KeyMismatch { module, host } => write!(
                f,
                "{KEY_MISMATCH_MESSAGE} (module key {module}, gateway key {host})"
            ),
            Refusal::NoRuntime => write!(
                f,
                "the module does not link {RUNTIME_SONAME}, so it carries its own copy of core: make it a dylib that depends on veles-runtime"
            ),
            Refusal::OtherBuild { missing, example } => write!(
                f,
                "the module comes from another cargo build of this source (its dependencies were compiled with other features), so {missing} of its references into core do not exist here, e.g. {example}: build it together with the gateway, `cargo build -p veles-cli -p <module crate>`"
            ),
            Refusal::HostWithoutRuntime(e) => {
                write!(f, "this binary cannot load modules: {e}")
            }
        }
    }
}

/// Read a module file's key section, the libraries it needs and its
/// core imports. Loads nothing. ELF and PE both read, on every host:
/// a Linux test can inspect a Windows module and the other way round.
pub fn inspect(path: &Path) -> Result<ModuleFile, Refusal> {
    use object::{Object, ObjectSection, ObjectSymbol};
    let data = std::fs::read(path).map_err(|e| Refusal::Unreadable(format!("{e}")))?;
    let file = object::File::parse(&*data).map_err(|e| Refusal::Unreadable(format!("{e}")))?;
    let key = file
        .section_by_name(KEY_SECTION)
        .and_then(|s| s.data().ok())
        .map(|d| String::from_utf8_lossy(d).into_owned());
    let (needed, core_imports) = match file.format() {
        object::BinaryFormat::Pe => {
            // The import directory names both the DLL and the symbol;
            // PE has no dynamic symbol table to ask.
            let imports = file
                .imports()
                .map_err(|e| Refusal::Unreadable(format!("{e}")))?;
            let mut needed: Vec<String> = Vec::new();
            let mut core = Vec::new();
            for imp in imports {
                let lib = String::from_utf8_lossy(imp.library()).into_owned();
                if !needed.iter().any(|n| n.eq_ignore_ascii_case(&lib)) {
                    needed.push(lib);
                }
                let name = String::from_utf8_lossy(imp.name());
                if names_core(&name) {
                    core.push(name.into_owned());
                }
            }
            (needed, core)
        }
        _ => (
            needed_libraries(&file),
            file.dynamic_symbols()
                .filter(|s| s.is_undefined())
                .filter_map(|s| s.name().ok())
                .filter(|n| names_core(n))
                .map(str::to_string)
                .collect(),
        ),
    };
    Ok(ModuleFile {
        path: path.to_path_buf(),
        key,
        needed,
        core_imports,
    })
}

/// May `file` be loaded into this process? Nothing is loaded either way.
pub fn check(file: &ModuleFile) -> Result<(), Refusal> {
    match &file.key {
        None => return Err(Refusal::NotAModule),
        Some(k) if k != BUILD_KEY => {
            return Err(Refusal::KeyMismatch {
                module: k.clone(),
                host: BUILD_KEY.to_string(),
            })
        }
        Some(_) => {}
    }
    if !file
        .needed
        .iter()
        .any(|n| n.eq_ignore_ascii_case(RUNTIME_SONAME))
    {
        return Err(Refusal::NoRuntime);
    }
    let host = host_core_exports().map_err(|e| Refusal::HostWithoutRuntime(e.clone()))?;
    let missing: Vec<&String> = file
        .core_imports
        .iter()
        .filter(|s| !host.contains(s.as_str()))
        .collect();
    if let Some(first) = missing.first() {
        return Err(Refusal::OtherBuild {
            missing: missing.len(),
            example: (*first).clone(),
        });
    }
    Ok(())
}

/// A mangled symbol from the veles_core crate (v0 `…10veles_core…` or
/// legacy `_ZN10veles_core…`).
fn names_core(sym: &str) -> bool {
    sym.contains("10veles_core")
}

/// `DT_NEEDED`, read from `.dynamic` and `.dynstr`. Both targets that
/// build modules (x86_64, aarch64) are 64-bit little-endian ELF.
fn needed_libraries(file: &object::File) -> Vec<String> {
    use object::{Object, ObjectSection};
    let (Some(dynamic), Some(dynstr)) = (
        file.section_by_name(".dynamic").and_then(|s| s.data().ok()),
        file.section_by_name(".dynstr").and_then(|s| s.data().ok()),
    ) else {
        return Vec::new();
    };
    const DT_NULL: u64 = 0;
    const DT_NEEDED: u64 = 1;
    let mut out = Vec::new();
    for entry in dynamic.as_chunks::<16>().0 {
        let tag = u64::from_le_bytes(entry[..8].try_into().unwrap_or_default());
        let val = u64::from_le_bytes(entry[8..].try_into().unwrap_or_default()) as usize;
        if tag == DT_NULL {
            break;
        }
        if tag == DT_NEEDED {
            if let Some(rest) = dynstr.get(val..) {
                let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
                out.push(String::from_utf8_lossy(&rest[..end]).into_owned());
            }
        }
    }
    out
}

/// The veles_core symbols this process's runtime defines, read once from
/// the runtime file the process actually mapped (found with `dladdr` on
/// a core function). `Err` when core is not in a shared runtime here (a
/// statically linked test binary).
fn host_core_exports() -> Result<&'static HashSet<String>, &'static String> {
    static EXPORTS: std::sync::OnceLock<Result<HashSet<String>, String>> =
        std::sync::OnceLock::new();
    EXPORTS
        .get_or_init(|| {
            use object::{Object, ObjectSymbol};
            let path = runtime_path()?;
            let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let file =
                object::File::parse(&*data).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(match file.format() {
                object::BinaryFormat::Pe => file
                    .exports()
                    .map_err(|e| format!("{}: {e}", path.display()))?
                    .into_iter()
                    .map(|e| String::from_utf8_lossy(e.name()).into_owned())
                    .filter(|n| names_core(n))
                    .collect(),
                _ => file
                    .dynamic_symbols()
                    .filter(|s| !s.is_undefined())
                    .filter_map(|s| s.name().ok())
                    .filter(|n| names_core(n))
                    .map(str::to_string)
                    .collect(),
            })
        })
        .as_ref()
}

/// The file core's code was mapped from, when that file is the runtime.
pub fn runtime_path() -> Result<PathBuf, String> {
    let addr = build_key as fn() -> &'static str as usize;
    let path = crate::platform::image_of(addr)
        .ok_or_else(|| "the loader names no image for core's code".to_string())?;
    let is_runtime = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(RUNTIME_SONAME));
    if !is_runtime {
        return Err(format!(
            "core is linked into {} itself, not {RUNTIME_SONAME}",
            path.display()
        ));
    }
    Ok(path)
}

/// Log a line on behalf of a module from CORE's callsites.
///
/// A `tracing` macro expanded in module code puts a callsite static in
/// the module's mapping and links it into tracing-core's global callsite
/// list, which has no removal. After the module is unmapped, the next
/// `Dispatch::new` (a scoped subscriber, a reload) walks that list into
/// unmapped memory. Logging through here leaves the module no callsite of
/// its own. The line carries the calling thread's owner tag.
pub fn log(level: tracing::Level, message: &str) {
    let owner = crate::lifecycle::thread_owner().unwrap_or("module");
    match level {
        tracing::Level::ERROR => tracing::error!(target: "veles::module", owner, "{message}"),
        tracing::Level::WARN => tracing::warn!(target: "veles::module", owner, "{message}"),
        tracing::Level::INFO => tracing::info!(target: "veles::module", owner, "{message}"),
        tracing::Level::DEBUG => tracing::debug!(target: "veles::module", owner, "{message}"),
        tracing::Level::TRACE => tracing::trace!(target: "veles::module", owner, "{message}"),
    }
}

/// Every tracing callsite this process registered, by the address of
/// its metadata static. The other half of [`log`]'s hazard: a module
/// whose OWN callsite was registered (an `info!` compiled into it — its
/// code or a dependency's, like most of veles-irc) must never be
/// unmapped, and the host's quiescence check asks [`callsites_within`]
/// the module's mapping. Filled by [`CallsiteRecorder`], which the
/// process's subscriber carries (`logbuf::init`).
static CALLSITES: std::sync::Mutex<Option<HashSet<usize>>> = std::sync::Mutex::new(None);

/// A layer that records each callsite as tracing-core registers it, and
/// changes nothing else: it enables everything and leaves the interest
/// to the layers it wraps.
pub struct CallsiteRecorder;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CallsiteRecorder {
    fn register_callsite(
        &self,
        meta: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        let mut set = CALLSITES.lock().unwrap_or_else(|p| p.into_inner());
        set.get_or_insert_with(HashSet::new)
            .insert(meta as *const _ as usize);
        tracing::subscriber::Interest::always()
    }
}

/// How many callsites the recorder has seen in all, from any object.
/// A diagnostic: 0 with a module that logs means its `tracing` macros
/// never registered here (a level that filters them before the
/// callsite, or a tracing-core that is not this process's).
pub fn callsites_recorded() -> usize {
    CALLSITES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map_or(0, HashSet::len)
}

/// How many recorded callsites lie in `ranges` (`[start, end)` address
/// ranges: a module's mappings, from /proc/self/maps).
pub fn callsites_within(ranges: &[(usize, usize)]) -> usize {
    let set = CALLSITES.lock().unwrap_or_else(|p| p.into_inner());
    set.as_ref().map_or(0, |s| {
        s.iter()
            .filter(|a| ranges.iter().any(|(lo, hi)| (*lo..*hi).contains(*a)))
            .count()
    })
}

/// Faulted module instances (M5). The owner-aware panic hook
/// (`lifecycle::install_fatal_panic_hook`) records a module panic here;
/// the supervisor TAKES the entry (`take_faulted`) when it acts on it
/// and restarts the instance with backoff. A plain map owner → (panic
/// location, count).
static FAULTED: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<String, (String, u64)>>,
> = std::sync::OnceLock::new();

fn faulted() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, (String, u64)>> {
    FAULTED.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

/// Mark an instance faulted. Called ONLY from the panic hook on a
/// module thread panicking outside core's crates (M5).
pub fn mark_faulted(owner: &str, location: &str) {
    let mut m = match faulted().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let e = m
        .entry(owner.to_string())
        .or_insert_with(|| (location.to_string(), 0));
    e.0 = location.to_string();
    e.1 += 1;
}

/// Take an owner's fault out of the registry: a fault is acted on ONCE.
/// Left in, it ended every later instance of that owner at its first
/// poll, and the restarts backed off and doubled until the gateway
/// restarted (VPS telegram, 2026-10-10).
pub fn take_faulted(owner: &str) -> Option<(String, u64)> {
    let mut m = match faulted().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    m.remove(owner)
}

/// Snapshot of the faulted registry (owner → (location, count)).
pub fn faulted_instances() -> std::collections::BTreeMap<String, (String, u64)> {
    match faulted().lock() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    }
}

/// Panic, located in THIS core file. Exists ONLY so the M5 panic-policy
/// test (crates/veles-cli/tests/m5_panic_policy.rs) can drive a panic
/// whose `location()` reports a `crates/veles-core/` path on a tagged
/// (module) thread — the `in_core` boundary, where the hook must ABORT
/// even though the thread is a module's. A panic written in the test file
/// would report a non-core location and test the wrong branch. Not for
/// any other caller.
#[doc(hidden)]
pub fn panic_in_core_for_m5_test() -> ! {
    panic!("M5 test: a panic located inside veles-core");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M5: a marked instance shows up in the snapshot.
    #[test]
    fn mark_faulted_records_owner_and_location() {
        mark_faulted("module:echo-test", "some.rs:1:2");
        let f = faulted_instances();
        assert!(f.contains_key("module:echo-test"), "got {f:?}");
    }

    /// A fault is taken once: the second take finds nothing, and the
    /// snapshot no longer lists it.
    #[test]
    fn a_taken_fault_is_gone() {
        mark_faulted("module:take-test", "some.rs:3:4");
        mark_faulted("module:take-test", "some.rs:5:6");
        assert_eq!(
            take_faulted("module:take-test"),
            Some(("some.rs:5:6".to_string(), 2))
        );
        assert_eq!(take_faulted("module:take-test"), None);
        assert!(!faulted_instances().contains_key("module:take-test"));
    }

    /// The key is what build.rs baked: 64 hex characters, and the
    /// section bytes a module carries spell the same string.
    #[test]
    fn the_build_key_is_blake3_hex_and_survives_the_section_form() {
        assert_eq!(BUILD_KEY.len(), 64, "{BUILD_KEY}");
        assert!(BUILD_KEY.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(&key_bytes(BUILD_KEY)[..], BUILD_KEY.as_bytes());
    }

    fn file(key: Option<&str>, needed: &[&str]) -> ModuleFile {
        ModuleFile {
            path: PathBuf::from("/x/libm.so"),
            key: key.map(str::to_string),
            needed: needed.iter().map(|s| s.to_string()).collect(),
            core_imports: vec!["_RNvNtCsX_10veles_core6module9build_key".into()],
        }
    }

    /// The refusals that need only the file: no section, another core,
    /// no runtime. Each must fire before the host looks at itself.
    #[test]
    fn a_module_without_a_key_another_key_or_no_runtime_is_refused() {
        assert_eq!(
            check(&file(None, &[RUNTIME_SONAME])),
            Err(Refusal::NotAModule)
        );
        let other = "0".repeat(64);
        let r = check(&file(Some(&other), &[RUNTIME_SONAME])).unwrap_err();
        assert!(matches!(r, Refusal::KeyMismatch { .. }), "{r:?}");
        assert!(r.to_string().starts_with(KEY_MISMATCH_MESSAGE), "{r}");
        assert_eq!(
            check(&file(Some(BUILD_KEY), &["libstd-x.so"])),
            Err(Refusal::NoRuntime)
        );
    }

    /// A unit-test binary links core statically, so it has no runtime to
    /// give a module: a module with the right key is still refused.
    #[test]
    fn a_host_without_the_runtime_refuses_every_module() {
        let r = check(&file(Some(BUILD_KEY), &[RUNTIME_SONAME])).unwrap_err();
        assert!(matches!(r, Refusal::HostWithoutRuntime(_)), "{r:?}");
    }
}
