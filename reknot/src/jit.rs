//! Experimental x64 object loaders: Rust COFF (Windows MSVC), Rust ELF (Linux
//! GNU), and Windows LLVM JITLink with a separately built bridge. Enabling this feature
//! never downloads or links LLVM into Cargo.
//! The Windows LLVM runtime profile supports emulated TLS. Layered Rust loaders
//! add explicit resident std/support libraries and reject compiler TLS in logic.
//! Panics must be caught inside logic/support; arbitrary startup is rejected.
//! Code and unwind registrations default to process residency. The checked Rust
//! backend additionally supports explicitly trusted managed artifacts and collection.
use crate::generation::GenerationOwner;
use crate::{LoadedHandler, RouteError};
use std::ffi::{c_char, c_void, CStr};
use std::fmt;
use std::io::Read;
use std::marker::PhantomData;
use std::path::Path;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

const BRIDGE_ABI: u32 = 3;

#[path = "jit_artifact.rs"]
mod artifact;
#[path = "coff.rs"]
mod coff;
pub(crate) use coff::retention::CoffTracking;
pub use coff::{
    coff_retention_snapshot, collect_retired_coff, CoffImageId, CoffImageRecord, CoffImageState,
    CoffPolicy, CoffReclaimOptions, CoffReclaimReport, CoffRetentionReason, CoffRetentionSnapshot,
};
pub use coff::{CoffEngine, CoffLimits};
pub use coff::{LayeredArtifact, LayeredEngine, ManagedLayeredArtifact, TrustedLayeredArtifact};
#[path = "elf.rs"]
mod elf;
pub use elf::{ElfEngine, ElfLimits};
/// Limits for identity-checked COFF/ELF sessions.
pub type LayeredLimits = CoffLimits;
pub use coff::retention::{
    collect_retired_elf, collect_retired_jit, elf_retention_snapshot, jit_retention_snapshot,
    CoffImageId as JitImageId, CoffImageRecord as JitImageRecord, CoffImageState as JitImageState,
    CoffPolicy as JitPolicy, CoffReclaimOptions as JitReclaimOptions,
    CoffReclaimReport as JitReclaimReport, CoffRetentionReason as JitRetentionReason,
    CoffRetentionSnapshot as JitRetentionSnapshot,
};
#[cfg(feature = "jit-dev")]
#[path = "jit_development.rs"]
pub mod development;

/// Conservative bounds before code publication. LLVM and KeepLoaded COFF loads
/// consume an attempt even on failure or abandonment; Drop never refunds it.
/// KeepLoaded ELF uses the same cumulative attempt contract.
/// Managed COFF/ELF loads use live image/byte limits instead of max_generations.
#[derive(Clone, Copy, Debug)]
pub struct JitLimits {
    pub max_generations: usize,
    pub max_object_bytes: usize,
}
impl Default for JitLimits {
    fn default() -> Self {
        Self {
            max_generations: 32,
            max_object_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub enum JitError {
    UnsupportedTarget,
    InvalidLimits,
    GenerationLimit { limit: usize },
    RetentionLimit,
    ObjectTooLarge { limit: usize },
    Io(std::io::Error),
    Bridge(libloading::Error),
    BridgeAbi { expected: u32, found: u32 },
    Link(String),
    Manifest(String),
    Identity(String),
    Handler(RouteError),
}
impl fmt::Display for JitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedTarget => write!(f, "unsupported target for the selected JIT backend"),
            Self::InvalidLimits => write!(f, "JIT limits must be positive and representable"),
            Self::GenerationLimit { limit } => {
                write!(f, "JIT load attempt budget reached ({limit})")
            }
            Self::RetentionLimit => write!(f, "COFF retained image/byte budget reached"),
            Self::ObjectTooLarge { limit } => write!(f, "JIT object exceeds {limit} bytes"),
            Self::Io(e) => write!(f, "JIT input: {e}"),
            Self::Bridge(e) => write!(f, "JIT bridge: {e}"),
            Self::BridgeAbi { expected, found } => {
                write!(f, "JIT bridge ABI: expected {expected}, found {found}")
            }
            Self::Link(e) => write!(f, "JIT link: {e}"),
            Self::Manifest(e) => write!(f, "layered build manifest: {e}"),
            Self::Identity(e) => write!(f, "layered build identity: {e}"),
            Self::Handler(e) => write!(f, "JIT handler: {e}"),
        }
    }
}
impl std::error::Error for JitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Bridge(e) => Some(e),
            Self::Handler(e) => Some(e),
            _ => None,
        }
    }
}

/// Link timings exclude the caller's compilation and router activation.
#[derive(Clone, Copy, Debug, Default)]
pub struct JitLoadMetrics {
    pub read_ms: f64,
    /// Input inspection (and, for LLVM, its initial input copy).
    pub copy_ms: f64,
    pub add_ms: f64,
    pub materialize_ms: f64,
    /// Included in materialize_ms, not an additional phase.
    pub unwind_registration_ms: f64,
    pub total_ms: f64,
    /// Retained section/stub content, excluding page padding and loader metadata.
    /// LLVM can retain debug graph content that the Rust backend discards.
    pub linked_bytes: u64,
    pub unwind_entries: u64,
}
#[repr(C)]
#[derive(Default)]
struct BridgeMetrics {
    copy_ms: f64,
    add_ms: f64,
    materialize_ms: f64,
    unwind_registration_ms: f64,
    linked_bytes: u64,
    unwind_entries: u64,
}
type LoadFn = unsafe extern "C" fn(
    *mut c_void,
    *const u8,
    usize,
    *mut u64,
    *mut BridgeMetrics,
    *mut c_char,
    usize,
) -> i32;
struct ResidentEngine {
    // The bridge is intentionally pinned even on validation/initialization
    // failure. C++ owns the process-resident images and RUNTIME_FUNCTION tables.
    _bridge: &'static libloading::Library,
}

/// Owner-thread JIT session. `load` needs &mut self; the session cannot be sent
/// across threads. Loaded handlers may outlive this controller. Dropping it never
/// removes code or unregisters unwind tables, including after the last Waker.
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<reknot::jit::JitEngine>();
/// ```
pub struct JitEngine {
    engine: NonNull<c_void>,
    load: LoadFn,
    collect_tls: unsafe extern "C" fn() -> usize,
    resident: Arc<ResidentEngine>,
    limits: JitLimits,
    attempts: usize,
    last_metrics: Option<JitLoadMetrics>,
    _owner_thread: PhantomData<Rc<()>>,
}
impl JitEngine {
    /// Load a matching Reknot JIT bridge. Invalid limits are rejected before
    /// loading any DLL. The bridge is process-resident once successfully opened.
    ///
    /// # Safety
    /// The bridge and its initializers must be trusted native code implementing
    /// the versioned bridge ABI and keep-loaded contract. This is not a sandbox.
    pub unsafe fn open(path: impl AsRef<Path>, limits: JitLimits) -> Result<Self, JitError> {
        Self::open_inner(path.as_ref(), None, None, limits)
    }

    /// Open with the exact compiler's Rust std DLL and resident Windows runtime
    /// dependencies. Compile the COFF with prefer-dynamic and emulated TLS.
    /// The std DLL is loaded once per OS module identity and is never unloaded.
    ///
    /// # Safety
    /// In addition to `open`'s contract, the runtime and its dependencies must be
    /// trusted and ABI-compatible with every object loaded in this engine.
    /// Rust's internal ABI is not stable across compilers. Panics must be caught
    /// inside the generation, before returning through any Reknot C callback.
    pub unsafe fn open_with_runtime(
        path: impl AsRef<Path>,
        rust_std_dll: impl AsRef<Path>,
        limits: JitLimits,
    ) -> Result<Self, JitError> {
        Self::open_inner(path.as_ref(), Some(rust_std_dll.as_ref()), None, limits)
    }

    /// Open the two-layer profile: a resident std/support DLL pair and one COFF
    /// per logic generation. Only these DLLs and the fixed Windows runtime set
    /// supply external symbols. Both DLLs stay loaded even after later errors.
    /// Logic must use GenerationLocal instead of native or emulated TLS; its
    /// sections and symbols are checked before linking. Support may use OS TLS.
    ///
    /// # Safety
    /// The bridge, both DLLs, their initializers and dependencies must be trusted.
    /// Every logic object must match the resident support build, compiler, std,
    /// target, features and panic strategy. These identities are not yet checked
    /// automatically. A Rust dylib support boundary is compiler-specific, not a
    /// stable ABI. Support changes require a new process. C callbacks must contain
    /// panics; all escaped logic values/callbacks must retain their generation.
    /// COFF inspection cannot rule out dynamic TLS or unmanaged registrations.
    pub unsafe fn open_layered(
        path: impl AsRef<Path>,
        rust_std_dll: impl AsRef<Path>,
        support_dll: impl AsRef<Path>,
        limits: JitLimits,
    ) -> Result<Self, JitError> {
        Self::open_inner(
            path.as_ref(),
            Some(rust_std_dll.as_ref()),
            Some(support_dll.as_ref()),
            limits,
        )
    }

    unsafe fn open_inner(
        path: &Path,
        runtime: Option<&Path>,
        support: Option<&Path>,
        limits: JitLimits,
    ) -> Result<Self, JitError> {
        if !cfg!(all(windows, target_arch = "x86_64")) {
            return Err(JitError::UnsupportedTarget);
        }
        if limits.max_generations == 0
            || limits.max_object_bytes == 0
            || limits.max_object_bytes >= isize::MAX as usize
        {
            return Err(JitError::InvalidLimits);
        }
        #[cfg(windows)]
        let (runtime, support) = {
            use std::os::windows::ffi::OsStrExt;
            let wide_path = |path: Option<&Path>| {
                path.map(|path| {
                    let path = std::fs::canonicalize(path).map_err(JitError::Io)?;
                    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
                    if wide.contains(&0) {
                        return Err(JitError::Link("NUL in resident DLL path".into()));
                    }
                    wide.push(0);
                    Ok(wide)
                })
                .transpose()
            };
            // Validate both paths before loading any DLL or running initializers.
            (wide_path(runtime)?, wide_path(support)?)
        };
        #[cfg(not(windows))]
        let (runtime, support): (Option<Vec<u16>>, Option<Vec<u16>>) = {
            let _ = (runtime, support);
            (None, None)
        };
        let bridge = Box::leak(Box::new(
            libloading::Library::new(path).map_err(JitError::Bridge)?,
        ));
        let version = bridge
            .get::<unsafe extern "C" fn() -> u32>(b"reknot_jit_bridge_abi\0")
            .map_err(JitError::Bridge)?;
        let found = version();
        if found != BRIDGE_ABI {
            return Err(JitError::BridgeAbi {
                expected: BRIDGE_ABI,
                found,
            });
        }
        let create = bridge
            .get::<unsafe extern "C" fn(*const u16, *const u16, *mut c_char, usize) -> *mut c_void>(
                b"reknot_jit_create\0",
            )
            .map_err(JitError::Bridge)?;
        let load = *bridge
            .get::<LoadFn>(b"reknot_jit_load\0")
            .map_err(JitError::Bridge)?;
        let collect_tls = *bridge
            .get::<unsafe extern "C" fn() -> usize>(b"reknot_jit_collect_tls\0")
            .map_err(JitError::Bridge)?;
        let mut error = [0 as c_char; 4096];
        let engine = NonNull::new(create(
            runtime
                .as_ref()
                .map_or(std::ptr::null(), |path| path.as_ptr()),
            support
                .as_ref()
                .map_or(std::ptr::null(), |path| path.as_ptr()),
            error.as_mut_ptr(),
            error.len(),
        ))
        .ok_or_else(|| JitError::Link(message(&error)))?;
        Ok(Self {
            engine,
            load,
            collect_tls,
            resident: Arc::new(ResidentEngine { _bridge: bridge }),
            limits,
            attempts: 0,
            last_metrics: None,
            _owner_thread: PhantomData,
        })
    }

    pub fn load_attempts(&self) -> usize {
        self.attempts
    }
    pub fn limits(&self) -> JitLimits {
        self.limits
    }
    pub fn last_load_metrics(&self) -> Option<JitLoadMetrics> {
        self.last_metrics
    }

    /// Reclaim emulated TLS backing allocations for fully terminated threads.
    /// Runs automatically on load and when a new thread first accesses JIT TLS.
    /// Returns the number of reclaimed threads across this resident bridge,
    /// including other engines. Live-thread storage and code stay resident.
    /// Rust TLS destructors run at normal thread exit, before this collection.
    pub fn collect_finished_threads(&mut self) -> usize {
        unsafe { (self.collect_tls)() }
    }

    /// Link one COFF object and return its owned handler table.
    /// No safe API exposes a raw code address. Unresolved imports, native TLS or startup
    /// sections fail before this table is published. Errors consume the budget.
    ///
    /// # Safety
    /// The object must be trusted x86-64 MSVC COFF implementing Handler ABI 5.
    /// Its code must obey RequestRouter::load's owner-thread, factory, poll/drop
    /// and callback contracts. It must not require unsupported runtime startup,
    /// destructors, native TLS, or exception propagation across the C ABI. With
    /// `open_with_runtime`, emulated TLS and the matching dynamic std are allowed.
    /// With `open_layered`, logic TLS is forbidden and the exact resident support
    /// build must be used. Keep std/support changes outside a live reload session.
    /// Custom allocators and arbitrary native dependencies need their own proven
    /// linkage contract and are not supplied by this loader. A successful
    /// link does not establish these semantic properties. Executor Wakers must
    /// belong to resident host code. Future loads cannot mutate existing images.
    pub unsafe fn load(&mut self, path: impl AsRef<Path>) -> Result<LoadedHandler, JitError> {
        self.last_metrics = None;
        if self.attempts >= self.limits.max_generations {
            return Err(JitError::GenerationLimit {
                limit: self.limits.max_generations,
            });
        }
        self.attempts += 1;
        let start = Instant::now();
        let file = std::fs::File::open(path).map_err(JitError::Io)?;
        let mut bytes = Vec::new();
        file.take(self.limits.max_object_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(JitError::Io)?;
        if bytes.len() > self.limits.max_object_bytes {
            return Err(JitError::ObjectTooLarge {
                limit: self.limits.max_object_bytes,
            });
        }
        let read_ms = start.elapsed().as_secs_f64() * 1000.0;
        let mut metrics = BridgeMetrics::default();
        let mut entry = 0u64;
        let mut error = [0 as c_char; 4096];
        // Form the owner BEFORE any object callback may be invoked, including
        // the API entry and initialization validator. Every later task receives
        // a lease through the normal router machinery.
        let owner = GenerationOwner::resident(self.resident.clone());
        if (self.load)(
            self.engine.as_ptr(),
            bytes.as_ptr(),
            bytes.len(),
            &mut entry,
            &mut metrics,
            error.as_mut_ptr(),
            error.len(),
        ) != 1
        {
            return Err(JitError::Link(message(&error)));
        }
        if entry == 0 {
            return Err(JitError::Link("null handler entry".into()));
        }
        let get_api: reknot_abi::services::HandlerApiFn = std::mem::transmute(entry as usize);
        let handler = LoadedHandler::from_entry(get_api, owner).map_err(JitError::Handler)?;
        self.last_metrics = Some(JitLoadMetrics {
            read_ms,
            copy_ms: metrics.copy_ms,
            add_ms: metrics.add_ms,
            materialize_ms: metrics.materialize_ms,
            unwind_registration_ms: metrics.unwind_registration_ms,
            total_ms: start.elapsed().as_secs_f64() * 1000.0,
            linked_bytes: metrics.linked_bytes,
            unwind_entries: metrics.unwind_entries,
        });
        Ok(handler)
    }
}
fn message(buffer: &[c_char; 4096]) -> String {
    // The versioned bridge always writes a terminating NUL. Keep the final byte
    // zero independently, so even a truncated diagnostic remains bounded.
    let mut copy = *buffer;
    copy[4095] = 0;
    unsafe { CStr::from_ptr(copy.as_ptr()).to_string_lossy().into_owned() }
}

#[cfg(all(test, windows, target_arch = "x86_64"))]
#[path = "jit_tests.rs"]
mod tests;
