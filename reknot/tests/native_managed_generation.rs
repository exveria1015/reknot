#![cfg(all(not(miri), feature = "native"))]
use reknot::*;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::rc::Rc;
use std::sync::OnceLock;
use std::task::{Context, Poll, Waker};

const KEY: ServiceKey = ServiceKey::new(0x4d41_4e41, 1);
fn library() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let project = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("managed-sdk-{}", std::process::id()));
        std::fs::create_dir_all(project.join("src")).unwrap();
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let dependency = manifest_dir.to_string_lossy().replace('\\', "/");
        std::fs::write(project.join("Cargo.toml"), format!("[package]\nname = \"managed_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n[dependencies]\nreknot = {{ path = \"{dependency}\", default-features = false }}\n[workspace]\n")).unwrap();
        std::fs::copy(manifest_dir.join("tests/fixtures/managed_logic.rs"), project.join("src/lib.rs")).unwrap();
        let target = project.join("build");
        let mut command = Command::new("cargo");
        command.current_dir(&project).args(["build", "--offline", "--manifest-path"]).arg(project.join("Cargo.toml"))
            .arg("--target-dir").arg(&target).env("CARGO_INCREMENTAL", "0");
        if !cfg!(debug_assertions) { command.arg("--release"); }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        target.join(if cfg!(debug_assertions) { "debug" } else { "release" })
            .join(format!("{}managed_fixture{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX))
    })
}
#[derive(Default)]
struct Probe {
    id: Cell<Option<ArtifactId>>,
    ready: Cell<bool>,
    cancelled: Cell<bool>,
    late: RefCell<Vec<Waker>>,
    events: RefCell<Vec<(u32, Vec<u8>)>>,
}
struct Session(Rc<Probe>);
struct Io {
    probe: Rc<Probe>,
    cancel: CancellationToken,
    input: Vec<u8>,
    first: bool,
}
impl Future for Io {
    type Output = Result<Vec<u8>, ServiceError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.first {
            self.first = false;
            self.probe.late.borrow_mut().push(cx.waker().clone());
        }
        self.probe.cancelled.set(self.cancel.is_cancelled());
        if !self.probe.ready.get() {
            Poll::Pending
        } else if self.cancel.is_cancelled() {
            Poll::Ready(Err(ServiceError::CANCELLED))
        } else {
            Poll::Ready(Ok(self.input.clone()))
        }
    }
}
impl Drop for Io {
    fn drop(&mut self) {
        self.probe.events.borrow_mut().push((8, Vec::new()));
    }
}
impl HostResource for Session {
    fn call(&mut self, op: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        if op == 9 {
            let record = image(self.0.id.get().unwrap());
            assert!(
                record.active_calls > 0,
                "registered destructor must return through a host guard"
            );
            assert_eq!(record.registered_locals, 1);
        }
        self.0.events.borrow_mut().push((op, input.to_vec()));
        Ok(0)
    }
    fn start_async(
        &mut self,
        _: u32,
        input: &[u8],
        cancel: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        Ok(Box::pin(Io {
            probe: self.0.clone(),
            cancel,
            input: input.to_vec(),
            first: true,
        }))
    }
}
fn image(id: ArtifactId) -> ImageRecord {
    retention_snapshot()
        .images
        .into_iter()
        .find(|r| r.id == id)
        .unwrap()
}
fn poll(task: &mut RequestTask) -> Poll<RunStatus> {
    Pin::new(task).poll(&mut Context::from_waker(Waker::noop()))
}
fn setup() -> (RequestRouter, Resource<Session>, Rc<Probe>) {
    let probe = Rc::new(Probe::default());
    let resources = HostResources::new();
    let session = resources.register(KEY, Session(probe.clone()));
    let router = unsafe { RequestRouter::load(library(), resources) }.unwrap();
    probe.id.set(router.artifact_id());
    (router, session, probe)
}
#[test]
fn host_io_waker_can_be_woken_cloned_and_dropped_after_registered_tls_and_dll_are_reclaimed() {
    let (router, session, probe) = setup();
    let id = router.artifact_id().unwrap();
    let copy = image(id).copy;
    let mut task = router.dispatch(session.id(), b"hello").unwrap();
    assert!(poll(&mut task).is_pending());
    assert_eq!(image(id).registered_locals, 1);
    assert_eq!(probe.late.borrow().len(), 1);
    probe.ready.set(true);
    assert!(poll(&mut task).is_pending());
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Done));
    drop(task);
    drop(router);
    assert_eq!(image(id).registered_locals, 0);
    assert_eq!(
        image(id).code_leases,
        0,
        "host-only late notification must not pin the DLL"
    );
    let operations: Vec<_> = probe.events.borrow().iter().map(|(op, _)| *op).collect();
    assert!(
        operations.iter().position(|op| *op == 8).unwrap()
            < operations.iter().position(|op| *op == 9).unwrap()
    );
    assert!(matches!(
        unsafe { reclaim_native_image(id) },
        ReclaimResult::Released {
            copy_removal_error: None
        }
    ));
    assert!(!copy.exists());
    assert_unmapped(&copy);
    let external = probe.late.borrow_mut().pop().unwrap();
    std::thread::spawn(move || {
        let cloned = external.clone();
        external.wake_by_ref();
        cloned.wake();
        drop(external);
    })
    .join()
    .unwrap();
}

#[cfg(windows)]
fn assert_unmapped(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(path: *const u16) -> *mut std::ffi::c_void;
    }
    let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    assert!(unsafe { GetModuleHandleW(wide.as_ptr()) }.is_null());
}
#[cfg(target_os = "linux")]
fn assert_unmapped(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOLOAD | libc::RTLD_NOW) };
    if !handle.is_null() {
        unsafe {
            libc::dlclose(handle);
        }
    }
    assert!(
        handle.is_null(),
        "SDK fixture is still mapped after releasing its loader reference"
    );
}
#[cfg(not(any(windows, target_os = "linux")))]
fn assert_unmapped(_: &Path) {}
#[test]
fn cancelled_host_io_keeps_request_and_registered_tls_until_join_then_late_waker_is_inert() {
    let (router, session, probe) = setup();
    let id = router.artifact_id().unwrap();
    let mut task = router.dispatch(session.id(), b"wait").unwrap();
    assert!(poll(&mut task).is_pending());
    drop(task);
    drop(router);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(poll_cancelled_requests(&mut cx).is_pending());
    assert!(probe.cancelled.get());
    assert_eq!(image(id).registered_locals, 1);
    probe.ready.set(true);
    assert!(poll_cancelled_requests(&mut cx).is_ready());
    assert_eq!(image(id).registered_locals, 0);
    assert!(matches!(
        unsafe { reclaim_native_image(id) },
        ReclaimResult::Released { .. }
    ));
    let external = probe.late.borrow_mut().pop().unwrap();
    std::thread::spawn(move || external.wake()).join().unwrap();
}
#[test]
fn registered_local_destructor_panic_retains_image_with_reason_and_is_never_retried() {
    let (router, session, probe) = setup();
    let id = router.artifact_id().unwrap();
    let mut task = router.dispatch(session.id(), b"fail-drop").unwrap();
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Done));
    drop(task);
    drop(router);
    let record = image(id);
    assert_eq!(record.local_cleanup_failures, 1);
    assert_eq!(record.registered_locals, 1);
    assert!(record
        .retention_reasons
        .contains(&RetentionReason::LocalCleanupFailed { count: 1 }));
    for _ in 0..2 {
        assert!(matches!(
            unsafe { reclaim_native_image(id) },
            ReclaimResult::Retained { .. }
        ));
    }
    assert_eq!(
        probe
            .events
            .borrow()
            .iter()
            .filter(|(op, _)| *op == 9)
            .count(),
        1
    );
    assert!(record.copy.exists());
}
