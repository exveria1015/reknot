//! Value-only diagnostics for this host SDK instance. No library leases are held here.
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactId(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageState {
    Preparing,
    Prepared,
    Active,
    Retired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageOutcome {
    Pending,
    Candidate,
    Adopted,
    Rejected,
    Abandoned,
}
/// Reasons a native image is still retained. Counts cover this SDK instance,
/// not arbitrary foreign pointers or threads. Quiescence needs caller evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetentionReason {
    NotRetired,
    CodeLeases { count: usize },
    HostCalls { count: usize },
    OwnerThreadRequired,
    UnvalidatedCandidate,
    ExternalQuiescenceUnconfirmed,
    Reclaiming,
    CloseUncertain,
    RegisteredLocals { count: usize },
    LocalCleanupFailed { count: u64 },
}
#[derive(Clone, Debug)]
pub struct ImageRecord {
    pub id: ArtifactId,
    pub source: PathBuf,
    pub copy: PathBuf,
    pub copy_bytes: u64,
    pub loaded: bool,
    pub state: ImageState,
    pub outcome: ImageOutcome,
    /// Owning code references other than the host retention list itself.
    pub code_leases: usize,
    /// SDK-initiated foreign calls that have not returned to their host frame.
    pub active_calls: usize,
    pub retention_reasons: Vec<RetentionReason>,
    /// close consumed its handle but did not confirm release. Never retry it.
    pub close_error: Option<String>,
    pub registered_locals: usize,
    pub local_cleanup_failures: u64,
    pub(crate) reclaiming: bool,
}
#[derive(Clone, Debug)]
pub struct CopyRemovalError {
    pub id: ArtifactId,
    pub path: PathBuf,
    pub message: String,
}
#[derive(Clone, Debug)]
pub struct RetentionSnapshot {
    pub tracked_native_images: usize,
    /// File lengths at copy completion, for live or uncertain SDK loader references.
    pub recorded_copy_bytes: u64,
    pub images: Vec<ImageRecord>,
    pub open_failures: u64,
    pub copy_failures: u64,
    /// The latest 32 failures; the cumulative counter is not truncated.
    pub removal_errors: Vec<CopyRemovalError>,
    pub removal_error_count: u64,
    /// Successful SDK loader-reference releases, not proof of OS unmapping/RAM freed.
    pub released_native_images: u64,
    pub close_failure_count: u64,
}
struct Registry {
    images: BTreeMap<ArtifactId, ImageRecord>,
    open_failed: u64,
    copy_failed: u64,
    errors: VecDeque<CopyRemovalError>,
    error_count: u64,
    released: u64,
    close_failed: u64,
}
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    images: BTreeMap::new(),
    open_failed: 0,
    copy_failed: 0,
    errors: VecDeque::new(),
    error_count: 0,
    released: 0,
    close_failed: 0,
});
static NEXT: AtomicU64 = AtomicU64::new(1);
fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}
/// Reports this SDK instance, not other SDKs/processes, OS module residency or RAM.
pub fn retention_snapshot() -> RetentionSnapshot {
    let r = registry();
    let images: Vec<_> = r.images.values().cloned().collect();
    let mut snapshot = RetentionSnapshot {
        tracked_native_images: images.iter().filter(|v| v.loaded).count(),
        recorded_copy_bytes: images
            .iter()
            .filter(|v| v.loaded)
            .fold(0u64, |n, v| n.saturating_add(v.copy_bytes)),
        images,
        open_failures: r.open_failed,
        copy_failures: r.copy_failed,
        removal_errors: r.errors.iter().cloned().collect(),
        removal_error_count: r.error_count,
        released_native_images: r.released,
        close_failure_count: r.close_failed,
    };
    // Never take the native-image mutex while holding the diagnostic registry.
    drop(r);
    crate::native::annotate(&mut snapshot.images);
    snapshot
}
pub(crate) fn next_id() -> std::io::Result<ArtifactId> {
    NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map(ArtifactId)
        .map_err(|_| std::io::Error::other("artifact IDs exhausted"))
}
pub(crate) fn copied(id: ArtifactId, source: PathBuf, copy: PathBuf, bytes: u64) {
    registry().images.insert(
        id,
        ImageRecord {
            id,
            source,
            copy,
            copy_bytes: bytes,
            loaded: false,
            state: ImageState::Preparing,
            outcome: ImageOutcome::Pending,
            code_leases: 0,
            active_calls: 0,
            retention_reasons: Vec::new(),
            close_error: None,
            registered_locals: 0,
            local_cleanup_failures: 0,
            reclaiming: false,
        },
    );
}
pub(crate) fn copy_failed() {
    let mut r = registry();
    r.copy_failed = r.copy_failed.saturating_add(1);
}
pub(crate) fn open_failed() {
    let mut r = registry();
    r.open_failed = r.open_failed.saturating_add(1);
}
pub(crate) fn update(id: ArtifactId, f: impl FnOnce(&mut ImageRecord)) {
    if let Some(v) = registry().images.get_mut(&id) {
        f(v);
    }
}
pub(crate) fn record(id: ArtifactId) -> Option<ImageRecord> {
    registry().images.get(&id).cloned()
}
pub(crate) fn released() {
    let mut r = registry();
    r.released = r.released.saturating_add(1);
}
pub(crate) fn close_failed(id: ArtifactId, message: String) {
    let mut r = registry();
    r.close_failed = r.close_failed.saturating_add(1);
    if let Some(image) = r.images.get_mut(&id) {
        image.close_error = Some(message);
        image.reclaiming = false;
    }
}
pub(crate) fn removed(id: ArtifactId, error: Option<CopyRemovalError>) {
    let mut r = registry();
    r.images.remove(&id);
    if let Some(error) = error {
        r.error_count = r.error_count.saturating_add(1);
        if r.errors.len() == 32 {
            r.errors.pop_front();
        }
        r.errors.push_back(error);
    }
}
