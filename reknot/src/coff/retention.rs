//! Diagnostic identity is deliberately not a capability to reacquire code.
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CoffImageId(pub(crate) u64);
impl CoffImageId {
    pub fn get(self) -> u64 {
        self.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoffPolicy {
    KeepLoaded,
    Managed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoffImageState {
    Published,
    Prepared,
    Active,
    Retired,
    Pinned,
    MemoryRetained,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoffRetentionReason {
    KeepLoaded,
    CodeReferences,
    Executing,
    GenerationLocals,
    ManagedActivities,
    LocalCleanupFailed,
    ActivityAbandoned,
    EntryRejected,
    UnwindRemovalFailed,
    MemoryReleaseFailed,
}
#[derive(Clone, Debug)]
pub struct CoffImageRecord {
    pub id: CoffImageId,
    pub policy: CoffPolicy,
    pub state: CoffImageState,
    pub foundation_id: Option<String>,
    /// Empty only for a failed, unexecuted rollback before publication metadata exists.
    pub object_sha256: String,
    pub allocated_bytes: usize,
    pub code_references: usize,
    pub executions: usize,
    pub registered_locals: usize,
    pub managed_activities: usize,
    pub reasons: Vec<CoffRetentionReason>,
}
#[derive(Clone, Debug, Default)]
pub struct CoffRetentionSnapshot {
    pub images: Vec<CoffImageRecord>,
    pub retained_bytes: usize,
    pub released_images: u64,
    pub released_bytes: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct CoffReclaimOptions {
    pub max_candidates: usize,
}
impl Default for CoffReclaimOptions {
    fn default() -> Self {
        Self { max_candidates: 64 }
    }
}
#[derive(Clone, Debug, Default)]
pub struct CoffReclaimReport {
    pub examined: usize,
    pub released: Vec<CoffImageId>,
    pub waiting: Vec<CoffImageId>,
    pub pinned: Vec<CoffImageId>,
    pub kept_loaded: Vec<CoffImageId>,
    pub errors: Vec<(CoffImageId, String)>,
    pub elapsed: Duration,
}
pub(crate) struct CoffTracking {
    pub id: CoffImageId,
    pub policy: CoffPolicy,
    pub details: Mutex<Details>,
}
pub(crate) struct Details {
    pub state: CoffImageState,
    pub locals: usize,
    pub activities: usize,
    pub failures: Vec<CoffRetentionReason>,
}
impl CoffTracking {
    pub(crate) fn state(&self, next: CoffImageState) {
        let mut d = self.details.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(
            d.state,
            CoffImageState::Pinned | CoffImageState::MemoryRetained
        ) {
            d.state = next;
        }
    }
    pub(crate) fn local(&self, delta: isize) {
        let mut d = self.details.lock().unwrap_or_else(|e| e.into_inner());
        d.locals = d.locals.saturating_add_signed(delta);
    }
    pub(crate) fn activity(&self, delta: isize) {
        let mut d = self.details.lock().unwrap_or_else(|e| e.into_inner());
        d.activities = d.activities.saturating_add_signed(delta);
    }
    pub(crate) fn pin(&self, reason: CoffRetentionReason) {
        let mut d = self.details.lock().unwrap_or_else(|e| e.into_inner());
        d.state = CoffImageState::Pinned;
        if !d.failures.contains(&reason) {
            d.failures.push(reason);
        }
    }
}

/// Inspect this owner thread's images. IDs/snapshots cannot create a code lease.
pub fn coff_retention_snapshot() -> CoffRetentionSnapshot {
    #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
    {
        super::platform::retention::snapshot()
    }
    #[cfg(not(all(windows, target_arch = "x86_64", target_env = "msvc")))]
    {
        CoffRetentionSnapshot::default()
    }
}
/// Explicit owner-thread maintenance. Never cancels requests or drops user tasks.
/// Only managed images with closed ownership and returned calls can be released.
/// KeepLoaded and uncertain/failed releases remain resident; no automatic retry.
pub fn collect_retired_coff(options: CoffReclaimOptions) -> CoffReclaimReport {
    #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
    {
        super::platform::retention::collect(options)
    }
    #[cfg(not(all(windows, target_arch = "x86_64", target_env = "msvc")))]
    {
        let _ = options;
        CoffReclaimReport::default()
    }
}

#[cfg(all(
    feature = "jit-dev",
    not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))
))]
pub(crate) fn collect_selected_coff(
    options: CoffReclaimOptions,
    images: &[CoffImageId],
) -> CoffReclaimReport {
    #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
    {
        super::platform::retention::collect_selected(options, Some(images))
    }
    #[cfg(not(all(windows, target_arch = "x86_64", target_env = "msvc")))]
    {
        let _ = (options, images);
        CoffReclaimReport::default()
    }
}

/// Inspect this owner thread's ELF images; identities do not reacquire code.
pub fn elf_retention_snapshot() -> CoffRetentionSnapshot {
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    {
        crate::jit::elf::platform::retention::snapshot()
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
    {
        CoffRetentionSnapshot::default()
    }
}
/// Collect eligible ELF images under the explicitly trusted managed contract.
/// First remove unwind registration, then release pages. Uncertain failures pin.
pub fn collect_retired_elf(options: CoffReclaimOptions) -> CoffReclaimReport {
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    {
        crate::jit::elf::platform::retention::collect(options)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
    {
        let _ = options;
        CoffReclaimReport::default()
    }
}
/// Inspect the native Rust object backend (COFF on Windows, ELF on Linux GNU).
pub fn jit_retention_snapshot() -> CoffRetentionSnapshot {
    if cfg!(target_os = "linux") {
        elf_retention_snapshot()
    } else {
        coff_retention_snapshot()
    }
}
/// Explicit owner-thread maintenance for the native Rust object backend.
pub fn collect_retired_jit(options: CoffReclaimOptions) -> CoffReclaimReport {
    if cfg!(target_os = "linux") {
        collect_retired_elf(options)
    } else {
        collect_retired_coff(options)
    }
}
#[cfg(feature = "jit-dev")]
pub(crate) fn collect_selected_jit(
    options: CoffReclaimOptions,
    images: &[CoffImageId],
) -> CoffReclaimReport {
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    {
        crate::jit::elf::platform::retention::collect_selected(options, Some(images))
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
    {
        collect_selected_coff(options, images)
    }
}
