//! Rust COFF backend. Platform operations are confined to platform.rs.
use super::{JitError, JitLimits, JitLoadMetrics};
use crate::LoadedHandler;
use std::marker::PhantomData;
use std::path::Path;
use std::rc::Rc;

use super::artifact;
#[path = "coff/format.rs"]
mod format;
pub use artifact::{
    LayeredArtifact, LayeredEngine, ManagedLayeredArtifact, TrustedLayeredArtifact,
};
#[path = "coff/retention.rs"]
pub(crate) mod retention;
pub use retention::{
    coff_retention_snapshot, collect_retired_coff, CoffImageId, CoffImageRecord, CoffImageState,
    CoffPolicy, CoffReclaimOptions, CoffReclaimReport, CoffRetentionReason, CoffRetentionSnapshot,
};
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
#[path = "coff/platform.rs"]
pub(super) mod platform;

/// Input/attempt limits and a separate bound on the committed image, including
/// BSS, import slots, jump stubs, alignment and page padding. Parser quotas are
/// 8,192 sections, 1,000,000 symbol records and 1,000,000 relocations per object.
#[derive(Clone, Copy, Debug)]
pub struct CoffLimits {
    /// Input bytes apply to both policies; max_generations only to KeepLoaded.
    pub load: JitLimits,
    pub max_image_bytes: usize,
    /// Owner-thread totals across controllers, including KeepLoaded and failed releases.
    pub max_retained_images: usize,
    pub max_retained_bytes: usize,
}
impl Default for CoffLimits {
    fn default() -> Self {
        Self {
            load: JitLimits::default(),
            max_image_bytes: 64 * 1024 * 1024,
            max_retained_images: 256,
            max_retained_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Experimental owner-thread Rust COFF loader for the two-layer Windows x64
/// MSVC profile. No LLVM bridge is needed. Images and runtime DLLs remain loaded
/// until process exit, including when this controller or a handler is dropped.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<reknot::jit::CoffEngine>();
/// ```
pub struct CoffEngine {
    #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
    pub(super) runtime: platform::Runtime,
    pub(super) limits: CoffLimits,
    pub(super) attempts: usize,
    pub(super) keep_loaded_attempts: usize,
    pub(super) last_image: Option<CoffImageId>,
    pub(super) last_metrics: Option<JitLoadMetrics>,
    #[cfg(all(test, windows, target_arch = "x86_64", target_env = "msvc"))]
    pub(super) last_symbols: std::collections::HashMap<Vec<u8>, usize>,
    pub(super) _owner_thread: PhantomData<Rc<()>>,
}
impl CoffEngine {
    /// Load the exact compiler-matched std and support DLLs. Only their exports,
    /// the fixed Windows runtime set and the host CRT's actual type_info vtable
    /// resolve imports. Logic TLS, startup hooks and cross-generation imports
    /// are unsupported. Failed loads also consume the session's attempt budget.
    ///
    /// # Safety
    /// Both DLLs, their dependencies and initializers must be trusted native code.
    /// Every object must match their compiler, target, features and panic strategy.
    /// Rust dylib ABI identities are the caller's responsibility in this raw API;
    /// use `LayeredEngine` for manifest validation and foundation matching.
    /// Support changes require a new process.
    pub unsafe fn open_layered(
        rust_std_dll: impl AsRef<Path>,
        support_dll: impl AsRef<Path>,
        limits: CoffLimits,
    ) -> Result<Self, JitError> {
        if limits.load.max_generations == 0
            || limits.load.max_object_bytes == 0
            || limits.load.max_object_bytes >= isize::MAX as usize
            || limits.max_image_bytes == 0
            || limits.max_image_bytes > i32::MAX as usize
            || limits.max_retained_images == 0
            || limits.max_retained_bytes == 0
        {
            return Err(JitError::InvalidLimits);
        }
        #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
        {
            let runtime = platform::Runtime::open(rust_std_dll.as_ref(), support_dll.as_ref())?;
            Ok(Self {
                runtime,
                limits,
                attempts: 0,
                keep_loaded_attempts: 0,
                last_image: None,
                last_metrics: None,
                #[cfg(test)]
                last_symbols: Default::default(),
                _owner_thread: PhantomData,
            })
        }
        #[cfg(not(all(windows, target_arch = "x86_64", target_env = "msvc")))]
        {
            let _ = (rust_std_dll, support_dll);
            Err(JitError::UnsupportedTarget)
        }
    }

    pub fn limits(&self) -> CoffLimits {
        self.limits
    }
    /// Total attempts across both policies; saturating diagnostic counter.
    pub fn load_attempts(&self) -> usize {
        self.attempts
    }
    /// Cumulative attempts using KeepLoaded. Managed attempts do not consume this limit.
    pub fn keep_loaded_attempts(&self) -> usize {
        self.keep_loaded_attempts
    }
    /// Image published by the last attempt, including an entry/ABI rejection.
    pub fn last_image_id(&self) -> Option<CoffImageId> {
        self.last_image
    }
    pub fn last_load_metrics(&self) -> Option<JitLoadMetrics> {
        self.last_metrics
    }

    /// Load one regular COFF or bigobj logic generation and invoke its handler
    /// entry after protection and unwind registration. The normal LoadedHandler
    /// and router ownership contract applies. No raw address is exposed safely.
    ///
    /// # Safety
    /// The object must be trusted native code satisfying `JitEngine::load`'s
    /// layered profile and all RequestRouter callback/lifetime contracts. Compiler
    /// TLS, arbitrary startup, custom allocators and unlisted native dependencies
    /// are not supported. Panics must be contained before crossing C callbacks.
    /// COFF validation is not a sandbox or proof of those semantic properties.
    pub unsafe fn load(&mut self, path: impl AsRef<Path>) -> Result<LoadedHandler, JitError> {
        self.begin_load(CoffPolicy::KeepLoaded)?;
        #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
        {
            platform::load(self, path.as_ref())
        }
        #[cfg(not(all(windows, target_arch = "x86_64", target_env = "msvc")))]
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
        // Telemetry, not a consumable lifetime allowance for managed images.
        self.attempts = self.attempts.saturating_add(1);
        Ok(())
    }
}
