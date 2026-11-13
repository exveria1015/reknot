#![cfg(all(not(miri), feature = "native"))]
use reknot::*;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};
use std::task::{Context, Poll, Wake, Waker};

const KEY: ServiceKey = ServiceKey::new(0x5245_5052, 1);
const STATE: ServiceKey = ServiceKey::new(0x5245_5053, 1);
fn library() -> &'static Path {
    static LIBRARY: OnceLock<PathBuf> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        let project = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cancellation-sdk-{}", std::process::id()));
        std::fs::create_dir_all(project.join("src")).unwrap();
        let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let dependency = sdk.to_string_lossy().replace('\\', "/");
        std::fs::write(project.join("Cargo.toml"), format!(
            "[package]\nname = \"cancellation_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n[dependencies]\nreknot = {{ path = \"{dependency}\", default-features = false }}\nfutures-util = {{ version = \"0.3\", default-features = false, features = [\"std\"] }}\n[workspace]\n"
        )).unwrap();
        std::fs::copy(sdk.join("tests/fixtures/cancellation_logic.rs"), project.join("src/lib.rs")).unwrap();
        let target = project.join("build");
        let mut command = Command::new("cargo");
        command.current_dir(&project).args(["build", "--offline", "--target-dir"]).arg(&target).env("CARGO_INCREMENTAL", "0");
        if !cfg!(debug_assertions) { command.arg("--release"); }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        target.join(if cfg!(debug_assertions) { "debug" } else { "release" })
            .join(format!("{}cancellation_fixture{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX))
    })
}
struct Notified(AtomicBool);
impl Wake for Notified {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}
struct Session;
impl HostResource for Session {}

#[test]
fn dll_cancellation_waits_restart_and_release_all_registrations_before_reclamation() {
    for mode in [
        b"request-one".as_slice(),
        b"request-many",
        b"child-many",
        b"full-capacity",
        b"registration-lifetime",
        b"panic-wake",
    ] {
        let resources = HostResources::new();
        let session = resources.register(KEY, Session);
        let mut router = unsafe { RequestRouter::load(library(), resources) }.unwrap();
        let old_image = router.artifact_id().unwrap();
        router
            .start_worker(
                WorkerId(1),
                session.id(),
                mode,
                Checkpoint::new(STATE, vec![]),
            )
            .unwrap();
        let notified = Arc::new(Notified(AtomicBool::new(false)));
        let waker = Waker::from(notified.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(router.poll_workers(&mut cx).is_pending());
        let candidate = unsafe { router.prepare(library()) }.unwrap();
        let mut restart = router
            .restart(candidate, |_, state, _| Ok(state.clone()))
            .unwrap();
        let mut finished = false;
        for _ in 0..8 {
            assert!(
                notified.0.swap(false, Ordering::SeqCst),
                "missing wake for {mode:?}"
            );
            if let Poll::Ready(result) = Pin::new(&mut restart).poll(&mut cx) {
                assert_eq!(result.unwrap(), 2);
                finished = true;
                break;
            }
        }
        drop(restart);
        assert!(finished, "{mode:?}");
        assert!(router.is_accepting());
        let next_image = router.artifact_id().unwrap();
        drop(router);
        for id in [old_image, next_image] {
            let image = retention_snapshot()
                .images
                .into_iter()
                .find(|v| v.id == id)
                .unwrap();
            assert_eq!(
                image.code_leases, 0,
                "cancellation wait retained DLL {id:?}"
            );
            // This fixture uses no external tasks, callbacks, threads or TLS.
            // All request tasks and their cancellation waits have been destroyed.
            assert!(matches!(
                unsafe { reclaim_native_image(id) },
                ReclaimResult::Released {
                    copy_removal_error: None
                }
            ));
        }
    }
}
