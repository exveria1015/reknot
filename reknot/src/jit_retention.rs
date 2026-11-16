// Shared owner-thread registry. Each platform supplies Image::release().
use super::{Image, JitError};
use crate::generation::{CodeLease, GenerationOwner};
use crate::jit::coff::retention::*;
use crate::jit::CoffLimits;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Instant;

struct Record {
    // TLS teardown and controller/last-lease Drop must NEVER free OS state.
    image: ManuallyDrop<Image>,
    owners: Option<Arc<()>>,
    executions: Arc<AtomicUsize>,
    tracking: Arc<CoffTracking>,
    policy: CoffPolicy,
    foundation: Option<String>,
    hash: String,
}
#[derive(Default)]
struct Registry {
    records: VecDeque<Record>,
    bytes: usize,
    released_images: u64,
    released_bytes: u64,
}
thread_local! { static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default()); }
static NEXT: AtomicU64 = AtomicU64::new(1);
fn next_id() -> Result<CoffImageId, JitError> {
    #[allow(deprecated)] // available at the workspace MSRV.
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map(CoffImageId)
        .map_err(|_| JitError::RetentionLimit)
}
pub(super) fn retain_failed_rollback(image: Image, reason: CoffRetentionReason) {
    // No executable ownership was exposed. Even TLS teardown must preserve the
    // OS-visible table if the diagnostics registry is already unavailable.
    let image = ManuallyDrop::new(image);
    let Ok(id) = next_id() else {
        return;
    };
    let state = if reason == CoffRetentionReason::MemoryReleaseFailed {
        CoffImageState::MemoryRetained
    } else {
        CoffImageState::Pinned
    };
    let record = Record {
        image,
        owners: None,
        executions: Arc::new(AtomicUsize::new(0)),
        tracking: Arc::new(CoffTracking {
            id,
            policy: CoffPolicy::KeepLoaded,
            details: Mutex::new(Details {
                state,
                locals: 0,
                activities: 0,
                failures: vec![reason],
            }),
        }),
        policy: CoffPolicy::KeepLoaded,
        foundation: None,
        hash: String::new(),
    };
    let _ = REGISTRY.try_with(|r| {
        let mut r = r.borrow_mut();
        r.bytes += record.image.size;
        r.records.push_back(record);
    });
}

pub(super) fn check_budget(limits: CoffLimits, bytes: usize) -> Result<(), JitError> {
    REGISTRY.with(|r| {
        let r = r.borrow();
        if r.records.len() >= limits.max_retained_images
            || bytes > limits.max_retained_bytes.saturating_sub(r.bytes)
        {
            Err(JitError::RetentionLimit)
        } else {
            Ok(())
        }
    })
}
pub(super) fn publish(
    image: Image,
    limits: CoffLimits,
    policy: CoffPolicy,
    foundation: Option<&str>,
    hash: String,
) -> Result<(GenerationOwner, Arc<CoffTracking>), JitError> {
    // Do not hold the registry borrow while rejecting/dropping an unpublished
    // image: its rollback may itself need to record an OS release failure.
    check_budget(limits, image.size)?;
    let id = next_id()?;
    REGISTRY.with(|registry| {
        let mut r = registry.borrow_mut();
        let tracking = Arc::new(CoffTracking {
            id,
            policy,
            details: Mutex::new(Details {
                state: CoffImageState::Published,
                locals: 0,
                activities: 0,
                failures: Vec::new(),
            }),
        });
        let owners = Arc::new(());
        let executions = Arc::new(AtomicUsize::new(0));
        let code = CodeLease::new(owners.clone(), executions.clone()).with_jit(tracking.clone());
        r.bytes += image.size;
        r.records.push_back(Record {
            image: ManuallyDrop::new(image),
            owners: Some(owners),
            executions,
            tracking: tracking.clone(),
            policy,
            foundation: foundation.map(str::to_owned),
            hash,
        });
        Ok((
            GenerationOwner {
                code,
                #[cfg(feature = "native")]
                id: None,
            },
            tracking,
        ))
    })
}
pub(crate) fn snapshot() -> CoffRetentionSnapshot {
    REGISTRY.with(|registry| {
        let r = registry.borrow();
        let images = r
            .records
            .iter()
            .map(|r| {
                let d = r.tracking.details.lock().unwrap_or_else(|e| e.into_inner());
                let code_references = r.owners.as_ref().map_or(0, |a| Arc::strong_count(a) - 1);
                let executions = r.executions.load(Ordering::Acquire);
                let mut reasons = d.failures.clone();
                if r.policy == CoffPolicy::KeepLoaded {
                    reasons.push(CoffRetentionReason::KeepLoaded);
                }
                if code_references > 0 {
                    reasons.push(CoffRetentionReason::CodeReferences);
                }
                if executions > 0 {
                    reasons.push(CoffRetentionReason::Executing);
                }
                if d.locals > 0 {
                    reasons.push(CoffRetentionReason::GenerationLocals);
                }
                if d.activities > 0 {
                    reasons.push(CoffRetentionReason::ManagedActivities);
                }
                CoffImageRecord {
                    id: r.tracking.id,
                    policy: r.policy,
                    state: d.state,
                    foundation_id: r.foundation.clone(),
                    object_sha256: r.hash.clone(),
                    allocated_bytes: r.image.size,
                    code_references,
                    executions,
                    registered_locals: d.locals,
                    managed_activities: d.activities,
                    reasons,
                }
            })
            .collect();
        CoffRetentionSnapshot {
            images,
            retained_bytes: r.bytes,
            released_images: r.released_images,
            released_bytes: r.released_bytes,
        }
    })
}
pub(crate) fn collect(options: CoffReclaimOptions) -> CoffReclaimReport {
    collect_selected(options, None)
}
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn fail_release(id: CoffImageId, failure: u8) {
    REGISTRY.with(|r| {
        r.borrow_mut()
            .records
            .iter_mut()
            .find(|r| r.tracking.id == id)
            .unwrap()
            .image
            .fail_release = failure
    });
}
pub(crate) fn collect_selected(
    options: CoffReclaimOptions,
    selected: Option<&[CoffImageId]>,
) -> CoffReclaimReport {
    let start = Instant::now();
    let mut report = CoffReclaimReport::default();
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        // Rotate examined records so a small budget cannot starve later retirees.
        for _ in 0..options.max_candidates.min(registry.records.len()) {
            let mut r = registry.records.pop_front().unwrap();
            report.examined += 1;
            let id = r.tracking.id;
            if selected.is_some_and(|ids| !ids.contains(&id)) {
                registry.records.push_back(r);
                continue;
            }
            let mut d = r.tracking.details.lock().unwrap_or_else(|e| e.into_inner());
            if !d.failures.is_empty()
                || matches!(
                    d.state,
                    CoffImageState::Pinned | CoffImageState::MemoryRetained
                )
            {
                report.pinned.push(id);
            } else if r.policy == CoffPolicy::KeepLoaded {
                report.kept_loaded.push(id);
            } else if d.state != CoffImageState::Retired
                || d.locals != 0
                || d.activities != 0
                || r.executions.load(Ordering::Acquire) != 0
            {
                report.waiting.push(id);
            } else {
                // Exclusive ownership closes the clone path; no ID/Weak API can reopen it.
                match Arc::try_unwrap(r.owners.take().unwrap()) {
                    Err(owners) => {
                        r.owners = Some(owners);
                        report.waiting.push(id);
                    }
                    Ok(()) => match r.image.release() {
                        Ok(()) => {
                            registry.bytes -= r.image.size;
                            registry.released_images += 1;
                            registry.released_bytes += r.image.size as u64;
                            report.released.push(id);
                            // Explicit release already succeeded. Drop only host metadata.
                            unsafe {
                                ManuallyDrop::drop(&mut r.image);
                            }
                            continue;
                        }
                        Err((reason, message)) => {
                            d.state = if reason == CoffRetentionReason::MemoryReleaseFailed {
                                CoffImageState::MemoryRetained
                            } else {
                                CoffImageState::Pinned
                            };
                            d.failures.push(reason);
                            report.errors.push((id, message));
                        }
                    },
                }
            }
            drop(d);
            registry.records.push_back(r);
        }
    });
    report.elapsed = start.elapsed();
    report
}
