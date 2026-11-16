//! Experimental Windows x64 COFF/JITLink loader. Build the versioned bridge
//! separately; enabling this feature never downloads or links LLVM into Cargo.
//! Only self-contained objects are supported: no CRT, TLS or platform startup.
//! Code and unwind registrations stay resident until process exit.
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

const BRIDGE_ABI: u32 = 1;

/// Conservative bounds before code publication. Failed and abandoned loads
/// consume an attempt too; neither router recreation nor Drop refunds it.
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
    ObjectTooLarge { limit: usize },
    Io(std::io::Error),
    Bridge(libloading::Error),
    BridgeAbi { expected: u32, found: u32 },
    Link(String),
    Handler(RouteError),
}
impl fmt::Display for JitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedTarget => write!(f, "JITLink currently requires Windows x64"),
            Self::InvalidLimits => write!(f, "JIT limits must be positive and representable"),
            Self::GenerationLimit { limit } => {
                write!(f, "JIT load attempt budget reached ({limit})")
            }
            Self::ObjectTooLarge { limit } => write!(f, "COFF object exceeds {limit} bytes"),
            Self::Io(e) => write!(f, "COFF input: {e}"),
            Self::Bridge(e) => write!(f, "JIT bridge: {e}"),
            Self::BridgeAbi { expected, found } => {
                write!(f, "JIT bridge ABI: expected {expected}, found {found}")
            }
            Self::Link(e) => write!(f, "JIT link: {e}"),
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
    /// Input copy and COFF format/section checks inside the bridge.
    pub copy_ms: f64,
    pub add_ms: f64,
    pub materialize_ms: f64,
    /// Included in materialize_ms, not an additional phase.
    pub unwind_registration_ms: f64,
    pub total_ms: f64,
    /// Live graph content, excluding page padding and LLVM bookkeeping.
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
        if !cfg!(all(windows, target_arch = "x86_64")) {
            return Err(JitError::UnsupportedTarget);
        }
        if limits.max_generations == 0
            || limits.max_object_bytes == 0
            || limits.max_object_bytes >= isize::MAX as usize
        {
            return Err(JitError::InvalidLimits);
        }
        let bridge = Box::leak(Box::new(
            libloading::Library::new(path.as_ref()).map_err(JitError::Bridge)?,
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
            .get::<unsafe extern "C" fn(*mut c_char, usize) -> *mut c_void>(b"reknot_jit_create\0")
            .map_err(JitError::Bridge)?;
        let load = *bridge
            .get::<LoadFn>(b"reknot_jit_load\0")
            .map_err(JitError::Bridge)?;
        let mut error = [0 as c_char; 4096];
        let engine = NonNull::new(create(error.as_mut_ptr(), error.len()))
            .ok_or_else(|| JitError::Link(message(&error)))?;
        Ok(Self {
            engine,
            load,
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

    /// Link one self-contained COFF object and return its owned handler table.
    /// No safe API exposes a raw code address. Unresolved imports, TLS or startup
    /// sections fail before this table is published. Errors consume the budget.
    ///
    /// # Safety
    /// The object must be trusted x86-64 MSVC COFF implementing Handler ABI 5.
    /// Its code must obey RequestRouter::load's owner-thread, factory, poll/drop
    /// and callback contracts. It must not require unsupported runtime startup,
    /// destructors, TLS, or exception propagation across the C ABI. A successful
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
