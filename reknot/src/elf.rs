//! Linux ELF64 relocatable objects with explicit resident dependencies.
use super::{CoffImageId, CoffLimits, CoffPolicy, JitError, JitLimits, JitLoadMetrics};
use crate::LoadedHandler;
use std::marker::PhantomData;
use std::path::Path;
use std::rc::Rc;

#[path = "elf/format.rs"]
mod format;
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[path = "elf/platform.rs"]
pub(super) mod platform;

/// Bounds for a process-resident ELF session. Padding, BSS, GOT and branch stubs
/// count toward image/session bytes. A new engine has a separate session budget.
#[derive(Clone, Copy, Debug)]
pub struct ElfLimits {
    pub load: JitLimits,
    pub max_image_bytes: usize,
    pub max_session_bytes: usize,
    /// Live owner-thread retention across controllers, including pinned failures.
    pub max_retained_images: usize,
    pub max_retained_bytes: usize,
}
impl Default for ElfLimits {
    fn default() -> Self {
        Self {
            load: JitLimits::default(),
            max_image_bytes: 64 * 1024 * 1024,
            max_session_bytes: 256 * 1024 * 1024,
            max_retained_images: 256,
            max_retained_bytes: 256 * 1024 * 1024,
        }
    }
}
impl From<CoffLimits> for ElfLimits {
    fn from(limits: CoffLimits) -> Self {
        Self {
            load: limits.load,
            max_image_bytes: limits.max_image_bytes,
            // Checked sessions use live owner-thread capacity, like COFF.
            max_session_bytes: usize::MAX,
            max_retained_images: limits.max_retained_images,
            max_retained_bytes: limits.max_retained_bytes,
        }
    }
}
impl From<ElfLimits> for CoffLimits {
    fn from(limits: ElfLimits) -> Self {
        Self {
            load: limits.load,
            max_image_bytes: limits.max_image_bytes,
            max_retained_images: limits.max_retained_images,
            max_retained_bytes: limits.max_retained_bytes,
        }
    }
}

/// Experimental Linux x86_64 GNU ELF loader, without an LLVM bridge.
/// Raw loads are linked independently and kept until process exit, including
/// after engine Drop, handler rejection and the last task/Waker's destruction.
/// Use LayeredArtifact / LayeredEngine for identity checks and managed collection.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<reknot::jit::ElfEngine>();
/// ```
pub struct ElfEngine {
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    pub(super) runtime: platform::Runtime,
    pub(super) limits: ElfLimits,
    pub(super) attempts: usize,
    pub(super) keep_loaded_attempts: usize,
    pub(super) retained_bytes: usize,
    pub(super) last_image: Option<CoffImageId>,
    pub(super) last_metrics: Option<JitLoadMetrics>,
    pub(super) _owner_thread: PhantomData<Rc<()>>,
}
impl ElfEngine {
    /// Open with the fixed GNU runtime set (libc, libm and libgcc_s).
    ///
    /// # Safety
    /// The OS dynamic loader configuration and runtime libraries must be trusted.
    /// All successfully opened libraries are retained, even on a later error.
    pub unsafe fn open(limits: ElfLimits) -> Result<Self, JitError> {
        Self::open_inner(None, limits)
    }

    /// Open with compiler-matched resident Rust std and support shared libraries.
    /// The support library must depend on this std build. Symbols resolve only
    /// through these explicit handles, their ELF dependencies and the fixed GNU
    /// runtime set; JIT generations are never global symbol providers.
    ///
    /// # Safety
    /// Libraries, dependencies, initializers and loader configuration must be
    /// trusted. The caller must ensure exact compiler/target/features/panic and
    /// support-build compatibility for every object. Rust dylib ABI is unstable.
    /// Foundation changes require a fresh process. The paths are canonicalized
    /// but not locked or authenticated; concurrent replacement is forbidden.
    pub unsafe fn open_layered(
        rust_std_so: impl AsRef<Path>,
        support_so: impl AsRef<Path>,
        limits: ElfLimits,
    ) -> Result<Self, JitError> {
        Self::open_inner(Some((rust_std_so.as_ref(), support_so.as_ref())), limits)
    }

    unsafe fn open_inner(
        paths: Option<(&Path, &Path)>,
        limits: ElfLimits,
    ) -> Result<Self, JitError> {
        if limits.load.max_generations == 0
            || limits.load.max_object_bytes == 0
            || limits.load.max_object_bytes >= isize::MAX as usize
            || limits.max_image_bytes == 0
            || limits.max_image_bytes > i32::MAX as usize
            || limits.max_session_bytes == 0
            || limits.max_retained_images == 0
            || limits.max_retained_bytes == 0
        {
            return Err(JitError::InvalidLimits);
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
        {
            Ok(Self {
                runtime: platform::Runtime::open(paths)?,
                limits,
                attempts: 0,
                keep_loaded_attempts: 0,
                retained_bytes: 0,
                last_image: None,
                last_metrics: None,
                _owner_thread: PhantomData,
            })
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
        {
            let _ = paths;
            Err(JitError::UnsupportedTarget)
        }
    }

    pub fn limits(&self) -> ElfLimits {
        self.limits
    }
    pub fn load_attempts(&self) -> usize {
        self.attempts
    }
    pub fn keep_loaded_attempts(&self) -> usize {
        self.keep_loaded_attempts
    }
    pub fn last_image_id(&self) -> Option<CoffImageId> {
        self.last_image
    }
    /// Published mappings in this session, including later ABI/entry failures.
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
    pub fn last_load_metrics(&self) -> Option<JitLoadMetrics> {
        self.last_metrics
    }

    /// Link one ELF64 little-endian x86-64 ET_REL object, protect its pages and
    /// register .eh_frame before calling hot_handler_api. Failures consume attempts.
    ///
    /// # Safety
    /// The object is trusted native code obeying the Handler ABI and router's
    /// owner-thread, callback and lifetime contracts. Use PIC, one codegen unit,
    /// unwind tables and the exact resident foundation. Catch all panics before
    /// returning through C callbacks. Logic must not use compiler TLS, startup/
    /// destructor hooks, custom allocators or unmanaged dependency registration.
    /// Use GenerationLocal for logic-local state. Inspection is not a sandbox or
    /// proof of these semantic requirements. Executor Wakers belong to resident code.
    pub unsafe fn load(&mut self, path: impl AsRef<Path>) -> Result<LoadedHandler, JitError> {
        self.begin_load(CoffPolicy::KeepLoaded)?;
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
        {
            platform::load(self, path.as_ref())
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
        {
            let _ = path;
            Err(JitError::UnsupportedTarget)
        }
    }
    pub(super) fn begin_load(&mut self, policy: CoffPolicy) -> Result<(), JitError> {
        self.last_metrics = None;
        self.last_image = None;
        if policy == CoffPolicy::KeepLoaded {
            if self.keep_loaded_attempts >= self.limits.load.max_generations {
                return Err(JitError::GenerationLimit {
                    limit: self.limits.load.max_generations,
                });
            }
            self.keep_loaded_attempts += 1;
        }
        self.attempts = self.attempts.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
#[path = "elf/tests.rs"]
mod tests;
