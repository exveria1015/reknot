#![cfg(all(not(miri), feature = "native"))]
use reknot::*;
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

fn library_name(name: &str) -> String {
    format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        name,
        std::env::consts::DLL_SUFFIX
    )
}
fn build(name: &str, fixture: &str, flags: &[&str]) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("native-reclamation-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(library_name(name));
    let mut command = Command::new("rustc");
    command
        .args([
            "--crate-type=cdylib",
            "--edition=2021",
            "--crate-name",
            name,
        ])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture),
        )
        .arg("-o")
        .arg(&path);
    for flag in flags {
        command.args(["--cfg", flag]);
    }
    if !cfg!(debug_assertions) {
        command.args(["-C", "opt-level=3"]);
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    path
}
fn library() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| build("reclamation", "reclamation.rs", &[]))
}
fn record(id: ArtifactId) -> ImageRecord {
    retention_snapshot()
        .images
        .into_iter()
        .find(|image| image.id == id)
        .unwrap()
}
fn released(id: ArtifactId) {
    assert!(matches!(
        unsafe { reclaim_native_image(id) },
        ReclaimResult::Released {
            copy_removal_error: None
        }
    ));
    assert!(!retention_snapshot()
        .images
        .iter()
        .any(|image| image.id == id));
    assert!(matches!(
        unsafe { reclaim_native_image(id) },
        ReclaimResult::NotFound
    ));
}
fn retained(id: ArtifactId) -> Vec<RetentionReason> {
    match unsafe { reclaim_native_image(id) } {
        ReclaimResult::Retained { reasons } => reasons,
        other => panic!("unexpected reclaim result: {other:?}"),
    }
}
fn poll(task: &mut RequestTask) -> Poll<RunStatus> {
    Pin::new(task).poll(&mut Context::from_waker(Waker::noop()))
}
struct Noop;
impl HostResource for Noop {
    fn call(&mut self, _: u32, _: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        Ok(0)
    }
}

#[test]
fn active_candidates_and_old_drain_requests_keep_their_code_until_explicit_release() {
    let resources = HostResources::new();
    let resource = resources.register(ServiceKey::new(7, 1), Noop);
    let mut router = unsafe { RequestRouter::load(library(), resources) }.unwrap();
    let old_id = router.artifact_id().unwrap();
    let copy = record(old_id).copy;
    assert!(retained(old_id).contains(&RetentionReason::NotRetired));
    let candidate = unsafe { router.prepare(library()) }.unwrap();
    let new_id = candidate.artifact_id().unwrap();
    assert!(retained(new_id).contains(&RetentionReason::NotRetired));
    let mut task = router.dispatch(resource.id(), &[]).unwrap();
    assert!(poll(&mut task).is_pending());
    router.activate(candidate).unwrap();
    assert_eq!(router.release_retired_generations(), 0);
    assert!(retained(old_id)
        .iter()
        .any(|r| matches!(r, RetentionReason::CodeLeases { .. })));
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Done));
    assert_eq!(router.release_retired_generations(), 1);
    assert!(
        !retained(old_id).is_empty(),
        "completed handle still owns its generation"
    );
    drop(task);
    let snapshot = record(old_id); // A diagnostic snapshot must not pin the DLL.
    assert_eq!(snapshot.code_leases, 0);
    assert_eq!(snapshot.active_calls, 0);
    assert_eq!(
        snapshot.retention_reasons,
        [RetentionReason::ExternalQuiescenceUnconfirmed]
    );
    assert!(
        copy.exists(),
        "Drop must never trigger physical reclamation"
    );
    assert_resident(&copy, true);
    released(old_id);
    assert!(!copy.exists());
    assert_resident(&copy, false);
    assert!(router.is_accepting());
    let mut new = router.dispatch(resource.id(), &[]).unwrap();
    assert!(poll(&mut new).is_pending());
    drop(new);
    drop(router);
    released(new_id);
}

#[test]
fn abandoned_validated_candidate_can_be_released_but_rejected_candidate_is_retained() {
    let router = unsafe { RequestRouter::load(library(), HostResources::new()) }.unwrap();
    let active = router.artifact_id().unwrap();
    let candidate = unsafe { router.prepare(library()) }.unwrap();
    let id = candidate.artifact_id().unwrap();
    drop(candidate);
    assert_eq!(record(id).outcome, ImageOutcome::Abandoned);
    released(id);
    let bad = build("reclaim_bad", "handler.rs", &["bad_abi"]);
    assert!(unsafe { router.prepare(&bad) }.is_err());
    let rejected = retention_snapshot()
        .images
        .into_iter()
        .find(|r| r.source == bad)
        .unwrap();
    assert_eq!(rejected.code_leases, 0);
    assert!(retained(rejected.id).contains(&RetentionReason::UnvalidatedCandidate));
    assert!(rejected.copy.exists());
    drop(router);
    released(active);
}

#[test]
fn reclamation_is_confined_to_the_loading_thread() {
    let router = unsafe { RequestRouter::load(library(), HostResources::new()) }.unwrap();
    let id = router.artifact_id().unwrap();
    drop(router);
    let blockers = std::thread::spawn(move || retained(id)).join().unwrap();
    assert!(blockers.contains(&RetentionReason::OwnerThreadRequired));
    released(id);
}

struct Reentrant {
    id: ArtifactId,
    phases: Vec<u32>,
}
impl HostResource for Reentrant {
    fn call(&mut self, phase: u32, _: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        let image = record(self.id);
        assert!(image.active_calls > 0);
        assert!(retained(self.id)
            .iter()
            .any(|r| matches!(r, RetentionReason::HostCalls { .. })));
        self.phases.push(phase);
        Ok(0)
    }
}
#[test]
fn reentrant_host_callbacks_observe_factory_poll_and_destructor_as_executing() {
    let resources = HostResources::new();
    let router = unsafe { RequestRouter::load(library(), resources.clone()) }.unwrap();
    let id = router.artifact_id().unwrap();
    let resource = resources.register(
        ServiceKey::new(7, 1),
        Reentrant {
            id,
            phases: Vec::new(),
        },
    );
    let mut task = router.dispatch(resource.id(), &[2]).unwrap();
    drop(router);
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Done));
    assert_eq!(resource.try_borrow().unwrap().phases, [1, 2, 3]);
    assert_eq!(record(id).active_calls, 0);
    drop(task);
    released(id);
}

#[derive(Default)]
struct Barrier {
    entered: AtomicBool,
    release: AtomicBool,
    timed_out: AtomicBool,
}

struct CleanupGate {
    id: ArtifactId,
    release: bool,
    phases: Vec<u32>,
}
impl HostResource for CleanupGate {
    fn call(&mut self, phase: u32, _: &[u8], output: &mut [u8]) -> Result<usize, ServiceError> {
        assert!(
            record(self.id).active_calls > 0,
            "unguarded activity callback"
        );
        self.phases.push(phase);
        if phase == 5 {
            output[0] = u8::from(self.release);
            Ok(1)
        } else {
            Ok(0)
        }
    }
}

#[test]
fn cancelled_registered_cleanup_keeps_code_until_join_and_foreign_destruction_return() {
    let resources = HostResources::new();
    let router = unsafe { RequestRouter::load(library(), resources.clone()) }.unwrap();
    let id = router.artifact_id().unwrap();
    let resource = resources.register(
        ServiceKey::new(7, 1),
        CleanupGate {
            id,
            release: false,
            phases: Vec::new(),
        },
    );
    let mut task = router.dispatch(resource.id(), &[3]).unwrap();
    assert!(poll(&mut task).is_pending());
    assert!(poll(&mut task).is_pending());
    assert_eq!(task.activity_count(), 1);
    drop(task);
    drop(router);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(poll_cancelled_requests(&mut cx).is_pending());
    assert!(record(id).code_leases > 0);
    assert!(!resource.try_borrow().unwrap().phases.contains(&6));
    resource.try_borrow_mut().unwrap().release = true;
    assert!(poll_cancelled_requests(&mut cx).is_ready());
    assert_eq!(resource.try_borrow().unwrap().phases.last(), Some(&6));
    assert_eq!(record(id).code_leases, 0);
    assert_eq!(record(id).active_calls, 0);
    released(id);
}
unsafe extern "C" fn after_release(context: *mut c_void) {
    let barrier = &*context.cast::<Barrier>();
    barrier.entered.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !barrier.release.load(Ordering::Acquire) {
        if Instant::now() >= deadline {
            barrier.timed_out.store(true, Ordering::Release);
            break;
        }
        std::thread::yield_now();
    }
}
#[test]
fn last_external_waker_release_does_not_unload_a_dll_before_its_caller_returns() {
    let resources = HostResources::new();
    let resource = resources.register(ServiceKey::new(7, 1), Noop);
    let router = unsafe { RequestRouter::load(library(), resources) }.unwrap();
    let id = router.artifact_id().unwrap();
    let copy = record(id).copy;
    let mut task = router.dispatch(resource.id(), &[1]).unwrap();
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Done));
    drop(task);
    drop(router);
    assert_eq!(record(id).code_leases, 1, "escaped Waker must own a lease");
    type Release = unsafe extern "C" fn(unsafe extern "C" fn(*mut c_void), *mut c_void);
    let release: Release = unsafe {
        let handle = libloading::Library::new(&copy).unwrap();
        *handle.get::<Release>(b"hot_release_waker").unwrap()
    }; // The extra OS reference is already closed; only SDK retention remains.
    let barrier = Arc::new(Barrier::default());
    let external = barrier.clone();
    let thread = std::thread::spawn(move || unsafe {
        release(after_release, Arc::as_ptr(&external).cast_mut().cast());
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !barrier.entered.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "external Waker timeout");
        std::thread::yield_now();
    }
    let image = record(id);
    assert_eq!(image.code_leases, 0);
    assert_eq!(
        image.active_calls, 0,
        "external entry is outside SDK call tracking"
    );
    assert_eq!(
        image.retention_reasons,
        [RetentionReason::ExternalQuiescenceUnconfirmed]
    );
    assert_resident(&copy, true);
    // No unsafe reclaim here: the DLL still has a return frame. Neither zero
    // leases nor polling again supplies the required external quiescence proof.
    barrier.release.store(true, Ordering::Release);
    thread.join().unwrap();
    assert!(!barrier.timed_out.load(Ordering::Acquire));
    released(id);
    assert_resident(&copy, false);
}

#[test]
fn forty_cutovers_release_loader_references_without_refunding_attempt_budget() {
    let budget = GenerationBudget::new(41).unwrap();
    let mut router = unsafe {
        RequestRouter::load_with_options(
            library(),
            HostResources::new(),
            RouterOptions {
                generations: budget.clone(),
                ..RouterOptions::default()
            },
        )
    }
    .unwrap();
    let mut ids = BTreeSet::from([router.artifact_id().unwrap()]);
    for _ in 0..40 {
        let old = router.artifact_id().unwrap();
        let candidate = unsafe { router.prepare(library()) }.unwrap();
        ids.insert(candidate.artifact_id().unwrap());
        router.activate(candidate).unwrap();
        assert_eq!(router.release_retired_generations(), 1);
        released(old);
        assert_eq!(
            retention_snapshot()
                .images
                .iter()
                .filter(|image| ids.contains(&image.id))
                .count(),
            1
        );
    }
    assert_eq!(budget.used(), 41);
    assert!(matches!(
        unsafe { router.prepare(library()) },
        Err(RouteError::GenerationLimit { limit: 41 })
    ));
    let id = router.artifact_id().unwrap();
    drop(router);
    released(id);
    assert_eq!(budget.used(), 41);
}

#[cfg(windows)]
fn assert_resident(path: &Path, expected: bool) {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(path: *const u16) -> *mut c_void;
    }
    let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    assert_eq!(
        !unsafe { GetModuleHandleW(wide.as_ptr()) }.is_null(),
        expected
    );
}
#[cfg(target_os = "linux")]
fn assert_resident(path: &Path, expected: bool) {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOLOAD | libc::RTLD_NOW) };
    let resident = !handle.is_null();
    if resident {
        assert_eq!(unsafe { libc::dlclose(handle) }, 0);
    }
    assert_eq!(resident, expected);
}
#[cfg(not(any(windows, target_os = "linux")))]
fn assert_resident(_: &Path, _: bool) {}

#[cfg(windows)]
#[test]
fn successful_close_and_copy_removal_failure_are_reported_separately() {
    use std::os::windows::fs::OpenOptionsExt;
    let router = unsafe { RequestRouter::load(library(), HostResources::new()) }.unwrap();
    let id = router.artifact_id().unwrap();
    let copy = record(id).copy;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1 | 2)
        .open(&copy)
        .unwrap();
    drop(router);
    let before = retention_snapshot().removal_error_count;
    let ReclaimResult::Released {
        copy_removal_error: Some(error),
    } = (unsafe { reclaim_native_image(id) })
    else {
        panic!("expected separate copy-removal error")
    };
    assert_eq!(error.id, id);
    assert!(retention_snapshot().removal_error_count > before);
    assert!(!retention_snapshot()
        .images
        .iter()
        .any(|image| image.id == id));
    assert!(copy.exists());
    assert_resident(&copy, false);
    drop(held);
    std::fs::remove_file(copy).unwrap();
}
