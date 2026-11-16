//! Isolated Windows acceptance tests: real saves/builds and OS release observations.
use super::*;
use crate::jit::{
    coff_retention_snapshot, collect_retired_coff, CoffReclaimOptions, CoffRetentionReason,
};
use crate::HostWaker;
use std::time::{Duration, Instant};
use windows_sys::Win32::System::{Diagnostics::Debug::RtlLookupFunctionEntry, Memory::*};

fn trusted(path: &Path) -> TrustedLayeredArtifact {
    unsafe { LayeredArtifact::read(path).unwrap().into_trusted() }
}
fn assert_mapped(address: usize, mapped: bool) {
    let mut base = 0;
    let found = unsafe { RtlLookupFunctionEntry(address as u64, &mut base, std::ptr::null_mut()) };
    assert_eq!(
        !found.is_null(),
        mapped,
        "unwind registration at {address:x}"
    );
    let mut info = MEMORY_BASIC_INFORMATION::default();
    assert_ne!(
        unsafe { VirtualQuery(address as _, &mut info, std::mem::size_of_val(&info)) },
        0
    );
    assert_eq!(info.State, if mapped { MEM_COMMIT } else { MEM_FREE });
}
#[derive(Default)]
struct Probe {
    notifications: Vec<HostWaker>,
    reentries: usize,
}
impl HostResource for Probe {
    fn call(&mut self, op: u32, bytes: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        if op == 20 {
            self.reentries += 1;
            assert!(coff_retention_snapshot()
                .images
                .iter()
                .any(|i| i.executions > 0));
            assert!(collect_retired_coff(CoffReclaimOptions::default())
                .released
                .is_empty());
        }
        if op == 21 {
            assert_eq!(bytes.len(), std::mem::size_of::<HostWaker>());
            let raw = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<HostWaker>()) };
            self.notifications.push(HostWaker {
                data: unsafe { ((*raw.vtable).clone)(raw.data) },
                vtable: raw.vtable,
            });
        }
        Ok(0)
    }
}
fn run_probe(
    router: &RequestRouter,
    probe: &crate::Resource<Probe>,
    input: &[u8],
    expected: RunStatus,
) {
    let mut task = router.dispatch(probe.id(), input).unwrap();
    assert_eq!(poll(&mut task), Poll::Ready(expected));
}
fn managed_child(manifest: &Path, mode: &str) {
    let mut limits = CoffLimits::default();
    if mode == "keep-baseline" {
        limits.load.max_generations = 160;
    }
    limits.max_retained_images = if mode == "keep-baseline" { 160 } else { 4 };
    let mut engine = LayeredEngine::open(&trusted(manifest), limits).unwrap();
    let first = if mode == "keep-baseline" {
        engine.load(trusted(manifest))
    } else {
        engine.load_managed(unsafe { trusted(manifest).into_managed() })
    }
    .unwrap();
    let id = first.coff_image_id().unwrap();
    let address = engine.test_unwind_address();
    assert_mapped(address, true);
    let resources = HostResources::new();
    let probe = resources.register(KEY, Probe::default());
    let mut router = RequestRouter::from_loaded_with_options(
        first,
        resources,
        crate::RouterOptions {
            generations: crate::GenerationBudget::new(if mode == "keep-baseline" {
                160
            } else {
                32
            })
            .unwrap(),
            ..Default::default()
        },
    )
    .unwrap();
    if mode == "keep-baseline" {
        let mut times = Vec::new();
        for _ in 0..128 {
            let start = Instant::now();
            let next = engine.load(trusted(manifest)).unwrap();
            router
                .activate(router.prepare_loaded(next).unwrap())
                .unwrap();
            times.push(start.elapsed().as_secs_f64() * 1000.0);
            run_probe(&router, &probe, b"local", RunStatus::Done);
            router.release_retired_generations();
        }
        times.sort_by(f64::total_cmp);
        drop(router);
        drop(engine);
        assert!(collect_retired_coff(CoffReclaimOptions::default())
            .released
            .is_empty());
        let snapshot = coff_retention_snapshot();
        assert_eq!(snapshot.images.len(), 129);
        eprintln!("KeepLoaded verified reload n=128 median={:.3} ms p95={:.3} ms; retained_images={} retained_bytes={}",times[64],times[121],snapshot.images.len(),snapshot.retained_bytes);
        return;
    }
    if mode == "local-failure" {
        run_probe(&router, &probe, b"fail-drop", RunStatus::Done);
        drop(router);
        drop(engine);
        assert!(collect_retired_coff(CoffReclaimOptions::default())
            .released
            .is_empty());
        let snapshot = coff_retention_snapshot();
        assert!(snapshot.images[0]
            .reasons
            .contains(&CoffRetentionReason::LocalCleanupFailed));
        assert_mapped(address, true);
        return;
    }
    if mode == "forgotten" {
        let task = router.dispatch(probe.id(), b"wait").unwrap();
        std::mem::forget(task);
        drop(router);
        assert!(collect_retired_coff(CoffReclaimOptions::default())
            .released
            .is_empty());
        assert_mapped(address, true);
        return;
    }
    if mode == "external" || mode == "adapter-panic" {
        let input = if mode == "external" {
            b"external".as_slice()
        } else {
            b"adapter-panic".as_slice()
        };
        let mut task = router.dispatch(probe.id(), input).unwrap();
        assert_eq!(poll(&mut task), Poll::Pending);
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        let support = unsafe {
            libloading::Library::new(value["foundation"]["support_dll"]["path"].as_str().unwrap())
        }
        .unwrap();
        let entered: libloading::Symbol<unsafe extern "C" fn() -> bool> =
            unsafe { support.get(b"layered_tail_entered") }.unwrap();
        let release: libloading::Symbol<unsafe extern "C" fn()> =
            unsafe { support.get(b"layered_release_tail") }.unwrap();
        if mode == "external" {
            let start = Instant::now();
            while !unsafe { entered() } {
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::yield_now();
            }
        }
        drop(router);
        drop(task);
        assert!(
            crate::poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_pending()
        );
        assert!(collect_retired_coff(CoffReclaimOptions::default())
            .released
            .is_empty());
        assert_mapped(address, true);
        assert_eq!(coff_retention_snapshot().images[0].managed_activities, 1);
        if mode == "adapter-panic" {
            return;
        }
        unsafe { release() };
        let start = Instant::now();
        while crate::poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_pending() {
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::yield_now();
        }
        assert_eq!(
            collect_retired_coff(CoffReclaimOptions::default()).released,
            [id]
        );
        assert_mapped(address, false);
        return;
    }
    assert_eq!(mode, "roundtrip");
    let other = engine
        .load_managed(unsafe { trusted(manifest).into_managed() })
        .unwrap();
    let other_id = other.coff_image_id().unwrap();
    let other_router = RequestRouter::from_loaded(other, HostResources::new()).unwrap();
    assert_eq!(router.generation(), other_router.generation());
    assert_ne!(router.coff_image_id(), other_router.coff_image_id());
    drop(other_router);
    assert_eq!(
        collect_retired_coff(CoffReclaimOptions::default()).released,
        [other_id]
    );
    assert_mapped(address, true);
    let mut old = router.dispatch(probe.id(), b"wait").unwrap();
    assert_eq!(poll(&mut old), Poll::Pending);
    let second = engine
        .load_managed(unsafe { trusted(manifest).into_managed() })
        .unwrap();
    assert_ne!(id, second.coff_image_id().unwrap());
    router
        .activate(router.prepare_loaded(second).unwrap())
        .unwrap();
    router.release_retired_generations();
    assert!(collect_retired_coff(CoffReclaimOptions::default())
        .released
        .is_empty());
    assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
    assert!(
        collect_retired_coff(CoffReclaimOptions::default())
            .released
            .is_empty(),
        "completed handle still owns code"
    );
    drop(old);
    router.release_retired_generations();
    assert_eq!(
        collect_retired_coff(CoffReclaimOptions::default()).released,
        [id]
    );
    assert_mapped(address, false);
    run_probe(&router, &probe, b"reenter", RunStatus::Done);
    run_probe(&router, &probe, b"checks", RunStatus::Done);
    run_probe(&router, &probe, b"panic", RunStatus::Error);
    run_probe(&router, &probe, b"notification", RunStatus::Done);
    let mut cancelled = router.dispatch(probe.id(), b"cancel").unwrap();
    assert_eq!(poll(&mut cancelled), Poll::Pending);
    drop(cancelled);
    assert!(crate::poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_ready());
    let mut timings = Vec::new();
    let mut collections = Vec::new();
    for _ in 0..128 {
        let old_address = engine.test_unwind_address();
        let start = Instant::now();
        let next = engine
            .load_managed(unsafe { trusted(manifest).into_managed() })
            .unwrap();
        router
            .activate(router.prepare_loaded(next).unwrap())
            .unwrap();
        timings.push(start.elapsed().as_secs_f64() * 1000.0);
        run_probe(&router, &probe, b"local", RunStatus::Done);
        router.release_retired_generations();
        let report = collect_retired_coff(CoffReclaimOptions::default());
        assert_eq!(report.released.len(), 1);
        collections.push(report.elapsed.as_secs_f64() * 1000.0);
        assert_mapped(old_address, false);
        assert_eq!(coff_retention_snapshot().images.len(), 1);
    }
    let abandoned = engine
        .load_managed(unsafe { trusted(manifest).into_managed() })
        .unwrap();
    let abandoned_id = abandoned.coff_image_id().unwrap();
    let last_address = engine.test_unwind_address();
    drop(router.prepare_loaded(abandoned).unwrap());
    assert_eq!(
        collect_retired_coff(CoffReclaimOptions::default()).released,
        [abandoned_id]
    );
    assert_mapped(last_address, false);
    assert!(engine.load_attempts() > 128);
    assert_eq!(engine.keep_loaded_attempts(), 0);
    assert_eq!(router.options().generations.used(), 0);
    drop(engine);
    drop(router);
    assert_eq!(
        collect_retired_coff(CoffReclaimOptions::default())
            .released
            .len(),
        1
    );
    assert_eq!(coff_retention_snapshot().retained_bytes, 0);
    // The raw notification's callbacks are resident host functions, even after
    // every logic image was actually released.
    {
        let mut p = probe.try_borrow_mut().unwrap();
        assert_eq!(p.reentries, 1);
        for w in p.notifications.drain(..) {
            unsafe {
                let c = ((*w.vtable).clone)(w.data);
                ((*w.vtable).wake_by_ref)(w.data);
                ((*w.vtable).drop)(w.data);
                ((*w.vtable).wake)(c);
            }
        }
    }
    timings.sort_by(f64::total_cmp);
    collections.sort_by(f64::total_cmp);
    eprintln!("managed verified reload n=128 median={:.3} ms p95={:.3} ms; collect median={:.3} ms p95={:.3} ms; retained=0",timings[64],timings[121],collections[64],collections[121]);
    let limits = CoffLimits {
        max_retained_images: 1,
        ..Default::default()
    };
    let mut engine = LayeredEngine::open(&trusted(manifest), limits).unwrap();
    drop(engine.load(trusted(manifest)).unwrap());
    assert!(collect_retired_coff(CoffReclaimOptions::default())
        .released
        .is_empty());
    assert!(matches!(
        engine.load_managed(unsafe { trusted(manifest).into_managed() }),
        Err(JitError::RetentionLimit)
    ));
}
fn fixture_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../target/layered-stage-de")
        .join(format!("{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(dir).unwrap();
    project(&dir);
    let result = Command::new("cargo")
        .args([
            "+stable",
            "generate-lockfile",
            "--offline",
            "--manifest-path",
        ])
        .arg(dir.join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    dir
}
#[test]
#[ignore = "real Windows COFF build and isolated managed lifetime/OS tests"]
fn real_coff_managed_reclamation() {
    if let Ok(mode) = std::env::var("REKNOT_MANAGED_CHILD") {
        managed_child(
            Path::new(&std::env::var("REKNOT_MANAGED_MANIFEST").unwrap()),
            &mode,
        );
        return;
    }
    let dir = fixture_dir("managed");
    let manifest = super::identity::build(&dir, "managed", None, true);
    for mode in [
        "keep-baseline",
        "roundtrip",
        "local-failure",
        "forgotten",
        "external",
        "adapter-panic",
    ] {
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jit::tests::layered::stage_de::real_coff_managed_reclamation",
                "--ignored",
                "--nocapture",
            ])
            .env("REKNOT_MANAGED_CHILD", mode)
            .env("REKNOT_MANAGED_MANIFEST", &manifest)
            .output()
            .unwrap();
        std::fs::write(dir.join(format!("{mode}.stdout")), &result.stdout).unwrap();
        std::fs::write(dir.join(format!("{mode}.stderr")), &result.stderr).unwrap();
        assert!(
            result.status.success(),
            "{mode}:\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        eprintln!("{mode}: {}", String::from_utf8_lossy(&result.stderr));
    }
}

#[test]
#[ignore = "two real PowerShell producers, writer lock, corruption and cache quota"]
fn real_coff_layered_builder_jobs() {
    use std::os::windows::fs::OpenOptionsExt;
    use std::process::Stdio;
    let dir = fixture_dir("builders");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/build-layered-jit.ps1");
    let target = dir.join("build");
    std::fs::create_dir_all(&target).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .share_mode(0)
        .open(target.join(".reknot-layered-writer.lock"))
        .unwrap();
    let start = |name: &str, capacity: u64| {
        let mut command = Command::new("pwsh");
        command
            .args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(&script)
            .arg("-ManifestPath")
            .arg(dir.join("Cargo.toml"))
            .args(["-Package", "jit_layered_logic", "-Offline"])
            .arg("-TargetDirectory")
            .arg(&target)
            .arg("-SupportDll")
            .arg(target.join("x86_64-pc-windows-msvc/debug/jit_layered_support.dll"))
            .arg("-Output")
            .arg(dir.join(format!("jobs/{name}/logic.obj")))
            .arg("-ReceiptPath")
            .arg(dir.join(format!("jobs/{name}/result.json")))
            .arg("-CacheDirectory")
            .arg(dir.join("cache"))
            .arg("-MaxCacheBytes")
            .arg(capacity.to_string())
            .args([
                "-SessionId",
                "builder-test",
                "-Revision",
                "7",
                "-JobId",
                "9",
            ])
            .stdout(Stdio::from(
                std::fs::File::create(dir.join(format!("{name}.stdout"))).unwrap(),
            ))
            .stderr(Stdio::from(
                std::fs::File::create(dir.join(format!("{name}.stderr"))).unwrap(),
            ));
        command.spawn().unwrap()
    };
    let mut aborted = start("aborted", u64::MAX);
    std::thread::sleep(Duration::from_millis(200));
    aborted.kill().unwrap();
    aborted.wait().unwrap();
    assert!(!dir.join("jobs/aborted/logic.obj.json").exists());
    let mut a = start("one", u64::MAX);
    let mut b = start("two", u64::MAX);
    std::thread::sleep(Duration::from_millis(800));
    drop(lock);
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    let receipt = |name: &str| -> serde_json::Value {
        serde_json::from_slice(
            &std::fs::read(dir.join(format!("jobs/{name}/result.json"))).unwrap(),
        )
        .unwrap()
    };
    for name in ["one", "two"] {
        let r = receipt(name);
        assert_eq!(r["status"], "Built");
        assert_eq!(r["revision"], 7);
        assert_eq!(r["job"], 9);
        LayeredArtifact::read(dir.join(format!("jobs/{name}/logic.obj.json"))).unwrap();
    }
    assert!(
        receipt("one")["lock_wait_ms"].as_f64().unwrap() > 100.0
            || receipt("two")["lock_wait_ms"].as_f64().unwrap() > 100.0
    );
    let mut full = start("full", 1);
    assert!(!full.wait().unwrap().success());
    assert_eq!(receipt("full")["status"], "CacheFull");
    assert!(!dir.join("jobs/full/logic.obj.json").exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("jobs/one/logic.obj.json")).unwrap())
            .unwrap();
    let dll = Path::new(
        manifest["foundation"]["support_dll"]["path"]
            .as_str()
            .unwrap(),
    );
    let original = std::fs::read(dll).unwrap();
    std::fs::write(dll, b"corrupt immutable cache").unwrap();
    let mut corrupt = start("corrupt", u64::MAX);
    assert!(!corrupt.wait().unwrap().success());
    assert_eq!(receipt("corrupt")["status"], "BuildFailed");
    assert!(!dir.join("jobs/corrupt/logic.obj.json").exists());
    assert_eq!(
        std::fs::read(dll).unwrap(),
        b"corrupt immutable cache",
        "producer must not overwrite a corrupt immutable copy"
    );
    std::fs::write(dll, original).unwrap();
}

#[cfg(feature = "jit-dev")]
mod development {
    use super::*;
    use crate::jit::development::*;

    fn failed_dependency_child(dir: &Path, managed: bool) {
        let mut options = LayeredDevOptions::new(
            dir.join("Cargo.toml"),
            "jit_layered_logic",
            "jit_layered_support",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/build-layered-jit.ps1"),
            dir.join("cache"),
        );
        options.offline = true;
        options.declared_inputs_complete = true;
        options.debounce = Duration::from_millis(10);
        let resources = HostResources::new();
        let events = resources.register(KEY, Events::default());
        let mut session = unsafe {
            if managed {
                LayeredDevSession::start_managed(options, resources)
            } else {
                LayeredDevSession::start(options, resources)
            }
        }
        .unwrap();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::Activated { .. }),
            &events,
        );
        let generation = session.router().unwrap().generation();
        let attempts = session.snapshot().load_attempts;

        // This dependency is outside every existing recursive watch. Discovery
        // succeeds, but its first compilation cannot produce a valid artifact.
        std::fs::create_dir_all(dir.join("extra/src")).unwrap();
        std::fs::write(
            dir.join("extra/Cargo.toml"),
            "[package]\nname='extra'\nversion='0.0.0'\nedition='2021'\n",
        )
        .unwrap();
        let source = dir.join("extra/src/lib.rs");
        std::fs::write(&source, "compile_error!(\"broken new dependency\");").unwrap();
        let workspace = dir.join("Cargo.toml");
        let text = std::fs::read_to_string(&workspace).unwrap().replace(
            "[\"support\", \"logic\"]",
            "[\"support\", \"logic\", \"extra\"]",
        );
        std::fs::write(workspace, text).unwrap();
        let logic_manifest = dir.join("logic/Cargo.toml");
        let mut text = std::fs::read_to_string(&logic_manifest).unwrap();
        text.push_str("extra = { path = '../extra' }\n");
        std::fs::write(logic_manifest, text).unwrap();
        let lock = Command::new("cargo")
            .args([
                "+stable",
                "generate-lockfile",
                "--offline",
                "--manifest-path",
            ])
            .arg(dir.join("Cargo.toml"))
            .output()
            .unwrap();
        assert!(
            lock.status.success(),
            "{}",
            String::from_utf8_lossy(&lock.stderr)
        );
        let failed = until(
            &mut session,
            |s| matches!(s, LayeredDevState::BuildFailed),
            &events,
        );
        assert!(failed.diagnostic.unwrap().contains("broken new dependency"));
        assert_eq!(session.router().unwrap().generation(), generation);
        assert_eq!(session.snapshot().load_attempts, attempts);

        // A failed build must remain idle until an input changes; neither retry
        // nor a touch to a previously watched file may be needed for recovery.
        let idle = Instant::now();
        while idle.elapsed() < Duration::from_millis(200) {
            assert!(session.poll_update().is_none());
            run(
                session.router().unwrap(),
                &events,
                b"local",
                RunStatus::Done,
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let revision = session.snapshot().revision;
        std::fs::write(source, "pub fn value()->u32{1}").unwrap();
        let edited = Instant::now();
        loop {
            if let Some(event) = session.poll_update() {
                eprintln!("dependency fix: {:?} {:?}", event.state, event.diagnostic);
                if matches!(event.state, LayeredDevState::Building { .. }) {
                    break;
                }
            }
            assert!(
                edited.elapsed() < Duration::from_secs(10),
                "fixing only the new dependency did not schedule a build: {:?}",
                session.snapshot()
            );
            run(
                session.router().unwrap(),
                &events,
                b"local",
                RunStatus::Done,
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(session.snapshot().revision.0 > revision.0);
        // Adding a dependency changed Cargo.lock. Re-evaluation must preserve
        // the running foundation and report the existing restart requirement.
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::RestartRequired { .. }),
            &events,
        );
        assert_eq!(session.router().unwrap().generation(), generation);
        assert_eq!(session.snapshot().load_attempts, attempts);
        session.shutdown().unwrap();
    }

    #[test]
    #[ignore = "real source watcher recovery after a newly added dependency fails to compile"]
    fn real_coff_layered_failed_dependency_watch() {
        if let Ok(dir) = std::env::var("REKNOT_FAILED_DEPENDENCY_DIR") {
            let managed = std::env::var("REKNOT_FAILED_DEPENDENCY_POLICY").unwrap() == "managed";
            failed_dependency_child(Path::new(&dir), managed);
            return;
        }
        for policy in ["keep", "managed"] {
            let dir = fixture_dir(&format!("failed-dependency-{policy}"));
            let result = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "jit::tests::layered::stage_de::development::real_coff_layered_failed_dependency_watch",
                    "--ignored",
                    "--nocapture",
                ])
                .env("REKNOT_FAILED_DEPENDENCY_DIR", &dir)
                .env("REKNOT_FAILED_DEPENDENCY_POLICY", policy)
                .output()
                .unwrap();
            std::fs::write(dir.join("dependency.stdout"), &result.stdout).unwrap();
            std::fs::write(dir.join("dependency.stderr"), &result.stderr).unwrap();
            assert!(
                result.status.success(),
                "{policy}:\n{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            eprintln!("{policy}: {}", String::from_utf8_lossy(&result.stderr));
        }
    }

    fn pressure_child(dir: &Path, mode: &str) {
        let source = dir.join("logic/src/lib.rs");
        let original = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jit_layered_logic.rs"),
        )
        .unwrap();
        let marker = "if cfg!(version_two) { 2 } else { 1 }";
        std::fs::write(&source, original.replace(marker, "1")).unwrap();
        let mut options = LayeredDevOptions::new(
            dir.join("Cargo.toml"),
            "jit_layered_logic",
            "jit_layered_support",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/build-layered-jit.ps1"),
            dir.join("cache"),
        );
        options.offline = true;
        options.declared_inputs_complete = true;
        options.debounce = Duration::from_millis(10);
        // These are KeepLoaded allowances; neither may limit managed updates.
        options.limits.load.max_generations = 1;
        options.max_router_generations = 1;
        if mode == "cancel" || mode == "external" {
            options.managed_reload.pressure = ManagedPressure::CancelOldest;
        }
        if mode == "bytes" {
            options.managed_reload.max_live_generations = 8;
            options.limits.max_retained_bytes = 256 * 1024;
        }
        let resources = HostResources::new();
        let events = resources.register(KEY, Events::default());
        let mut session = unsafe { LayeredDevSession::start_managed(options, resources) }.unwrap();
        let first = until(
            &mut session,
            |s| matches!(s, LayeredDevState::Activated { .. }),
            &events,
        );
        let first_id = session.router().unwrap().coff_image_id().unwrap();
        let manifest = first.manifest.unwrap();
        // Maintenance of this session must not reclaim another controller's image.
        let outside = if mode == "wait" {
            let mut engine =
                LayeredEngine::open(&trusted(&manifest), CoffLimits::default()).unwrap();
            let image = engine
                .load_managed(unsafe { trusted(&manifest).into_managed() })
                .unwrap();
            let id = image.coff_image_id().unwrap();
            drop(image);
            Some((engine, id))
        } else {
            None
        };
        let mut old = if mode == "pinned" {
            run(
                session.router().unwrap(),
                &events,
                b"fail-drop",
                RunStatus::Done,
            );
            None
        } else {
            let input = if mode == "external" {
                b"external".as_slice()
            } else {
                b"cancel".as_slice()
            };
            let mut task = session
                .router()
                .unwrap()
                .dispatch(events.id(), input)
                .unwrap();
            assert_eq!(poll(&mut task), Poll::Pending);
            Some(task)
        };
        std::fs::write(&source, original.replace(marker, "2")).unwrap();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::Activated { .. }),
            &events,
        );
        let mut current = session
            .router()
            .unwrap()
            .dispatch(events.id(), b"cancel")
            .unwrap();
        assert_eq!(poll(&mut current), Poll::Pending);
        std::fs::write(&source, original.replace(marker, "3")).unwrap();
        if mode == "pinned" {
            let state = until(
                &mut session,
                |s| matches!(s, LayeredDevState::RecoveryRequired { .. }),
                &events,
            );
            assert_eq!(
                state.state,
                LayeredDevState::RecoveryRequired { image: first_id }
            );
            assert_eq!(state.load_attempts, 2);
        } else {
            let state = until(
                &mut session,
                |s| matches!(s, LayeredDevState::WaitingForRetirement { .. }),
                &events,
            );
            assert_eq!(
                state.state,
                LayeredDevState::WaitingForRetirement { image: first_id }
            );
            assert_eq!(state.load_attempts, 2);
            assert_eq!(state.live_images.len(), 2);
            // More edits while stalled keep only the latest observed revision.
            std::fs::write(&source, original.replace(marker, "4")).unwrap();
            if mode == "bytes" {
                // The exact-size gate holds a built, captured artifact. It is
                // discarded on a new revision, without consuming a load attempt.
                until(
                    &mut session,
                    |s| matches!(s, LayeredDevState::Superseded),
                    &events,
                );
                until(
                    &mut session,
                    |s| matches!(s, LayeredDevState::WaitingForRetirement { .. }),
                    &events,
                );
            }
            for _ in 0..20 {
                session.poll_update();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(session.snapshot().load_attempts, 2);
            assert_eq!(
                poll(&mut current),
                Poll::Pending,
                "active roots are not cancelled"
            );
            if mode == "cancel" {
                assert_eq!(poll(old.as_mut().unwrap()), Poll::Ready(RunStatus::Error));
                session.poll_update();
                assert_eq!(
                    session.snapshot().load_attempts,
                    2,
                    "completed handle still owns old code"
                );
                drop(old.take());
            } else if mode == "external" {
                assert_eq!(poll(old.as_mut().unwrap()), Poll::Pending);
                assert!(coff_retention_snapshot()
                    .images
                    .iter()
                    .any(|i| i.id == first_id && i.managed_activities == 1));
                let value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
                let support = unsafe {
                    libloading::Library::new(
                        value["foundation"]["support_dll"]["path"].as_str().unwrap(),
                    )
                }
                .unwrap();
                let release: libloading::Symbol<unsafe extern "C" fn()> =
                    unsafe { support.get(b"layered_release_tail") }.unwrap();
                unsafe { release() };
                let start = Instant::now();
                while poll(old.as_mut().unwrap()).is_pending() {
                    assert!(start.elapsed() < Duration::from_secs(10));
                    std::thread::yield_now();
                }
                drop(old.take());
            } else {
                assert_eq!(
                    poll(old.as_mut().unwrap()),
                    Poll::Pending,
                    "Wait does not cancel roots"
                );
                old.take().unwrap().cancel();
            }
            let pending_manifest = session.snapshot().manifest;
            let state = until(
                &mut session,
                |s| matches!(s, LayeredDevState::Activated { .. }),
                &events,
            );
            assert_eq!(state.load_attempts, 3);
            assert_eq!(state.keep_loaded_attempts, 0);
            if mode == "bytes" {
                assert_eq!(
                    state.manifest, pending_manifest,
                    "reuse captured candidate without another build"
                );
            }
            run(
                session.router().unwrap(),
                &events,
                b"local",
                RunStatus::Done,
            );
            assert_eq!(*values(&events, 1).last().unwrap(), 407);
            assert!(!coff_retention_snapshot()
                .images
                .iter()
                .any(|i| i.id == first_id));
        }
        current.cancel();
        session.poll_update();
        if let Some((_, id)) = &outside {
            assert!(coff_retention_snapshot().images.iter().any(|i| i.id == *id));
        }
        session.shutdown().unwrap();
        drop(session);
        drop(outside);
        collect_retired_coff(CoffReclaimOptions::default());
        let snapshot = coff_retention_snapshot();
        assert_eq!(snapshot.images.len(), usize::from(mode == "pinned"));
        if mode == "pinned" {
            assert!(snapshot.images[0]
                .reasons
                .contains(&CoffRetentionReason::LocalCleanupFailed));
        }
    }

    #[test]
    #[ignore = "managed retirement waiting, selective cancellation, pending bytes and pinned recovery"]
    fn real_coff_managed_development_pressure() {
        if let Ok(mode) = std::env::var("REKNOT_MANAGED_PRESSURE_MODE") {
            pressure_child(
                Path::new(&std::env::var("REKNOT_MANAGED_PRESSURE_DIR").unwrap()),
                &mode,
            );
            return;
        }
        let dir = fixture_dir("managed-pressure");
        for mode in ["wait", "cancel", "external", "bytes", "pinned"] {
            let result = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "jit::tests::layered::stage_de::development::real_coff_managed_development_pressure", "--ignored", "--nocapture"])
                .env("REKNOT_MANAGED_PRESSURE_MODE", mode)
                .env("REKNOT_MANAGED_PRESSURE_DIR", &dir).output().unwrap();
            std::fs::write(dir.join(format!("{mode}.stdout")), &result.stdout).unwrap();
            std::fs::write(dir.join(format!("{mode}.stderr")), &result.stderr).unwrap();
            assert!(
                result.status.success(),
                "{mode}:\n{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            eprintln!("{mode}: {}", String::from_utf8_lossy(&result.stderr));
        }
    }

    fn latency_child(dir: &Path) {
        let source = dir.join("logic/src/lib.rs");
        let original = std::fs::read_to_string(&source).unwrap();
        let marker = "if cfg!(version_two) { 2 } else { 1 }";
        let options = LayeredDevOptions {
            offline: true,
            declared_inputs_complete: true,
            ..LayeredDevOptions::new(
                dir.join("Cargo.toml"),
                "jit_layered_logic",
                "jit_layered_support",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/build-layered-jit.ps1"),
                dir.join("cache"),
            )
        };
        let resources = HostResources::new();
        let events = resources.register(KEY, Events::default());
        let mut session = unsafe { LayeredDevSession::start(options, resources) }.unwrap();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::Activated { .. }),
            &events,
        );
        // Warm target, default 50 ms debounce, 5 ms owner poll interval. Every
        // pending request is accepted before poll_update and completed after it.
        for version in 2..=4 {
            run(
                session.router().unwrap(),
                &events,
                b"local",
                RunStatus::Done,
            );
            let mut last_response = Instant::now();
            let start = Instant::now();
            std::fs::write(&source, original.replace(marker, &version.to_string())).unwrap();
            let mut max_gap = 0.0f64;
            let mut responses = 0;
            loop {
                let mut old = session
                    .router()
                    .unwrap()
                    .dispatch(events.id(), b"wait")
                    .unwrap();
                assert_eq!(poll(&mut old), Poll::Pending);
                let event = session.poll_update();
                assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
                drop(old);
                let now = Instant::now();
                max_gap = max_gap.max(now.duration_since(last_response).as_secs_f64() * 1000.0);
                last_response = now;
                responses += 1;
                if let Some(event) = event {
                    match event.state {
                        LayeredDevState::Activated { .. } => {
                            run(
                                session.router().unwrap(),
                                &events,
                                b"local",
                                RunStatus::Done,
                            );
                            assert_eq!(*values(&events, 1).last().unwrap(), version * 100 + 7);
                            let m = session.snapshot().metrics;
                            eprintln!("edit_latency mode=worker version={version} edit_ms={:.3} max_old_response_gap_ms={max_gap:.3} old_responses={responses} worker_ms={:.3} build_ms={:.3} artifact_ms={:.3} owner_ms={:.3}", start.elapsed().as_secs_f64()*1000.0,m.worker_ms,m.build_ms,m.artifact_read_ms,m.owner_ms);
                            break;
                        }
                        LayeredDevState::Building { .. } | LayeredDevState::Superseded => {}
                        other => panic!("unexpected update: {other:?} {:?}", event.diagnostic),
                    }
                }
                assert!(start.elapsed() < Duration::from_secs(60));
                std::thread::sleep(Duration::from_millis(5));
            }
            session.router_mut().unwrap().release_retired_generations();
        }
        session.shutdown().unwrap();
        let manifest = session.snapshot().manifest.unwrap();
        let mut engine = LayeredEngine::open(&trusted(&manifest), CoffLimits::default()).unwrap();
        for version in 5..=7 {
            run(
                session.router().unwrap(),
                &events,
                b"local",
                RunStatus::Done,
            );
            let last_response = Instant::now();
            let mut old = session
                .router()
                .unwrap()
                .dispatch(events.id(), b"wait")
                .unwrap();
            assert_eq!(poll(&mut old), Poll::Pending);
            let start = Instant::now();
            std::fs::write(&source, original.replace(marker, &version.to_string())).unwrap();
            let (artifact, m) = session.test_build_blocking().unwrap();
            let owner = Instant::now();
            let handler = engine.load(unsafe { artifact.into_trusted() }).unwrap();
            let router = session.router_mut().unwrap();
            router
                .activate(router.prepare_loaded(handler).unwrap())
                .unwrap();
            let owner_ms = owner.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
            drop(old);
            assert_eq!(*values(&events, 1).last().unwrap(), (version - 1) * 100 + 7);
            let max_gap = last_response.elapsed().as_secs_f64() * 1000.0;
            run(router, &events, b"local", RunStatus::Done);
            assert_eq!(*values(&events, 1).last().unwrap(), version * 100 + 7);
            eprintln!("edit_latency mode=synchronous version={version} edit_ms={:.3} max_old_response_gap_ms={max_gap:.3} old_responses=1 worker_ms={:.3} build_ms={:.3} artifact_ms={:.3} owner_ms={owner_ms:.3}",start.elapsed().as_secs_f64()*1000.0,m.worker_ms,m.build_ms,m.artifact_read_ms);
            router.release_retired_generations();
        }
    }

    #[test]
    #[ignore = "same-pipeline synchronous/worker edit latency and old response intervals"]
    fn real_coff_layered_edit_latency() {
        if let Ok(dir) = std::env::var("REKNOT_DEV_LATENCY_CHILD") {
            latency_child(Path::new(&dir));
            return;
        }
        let dir = fixture_dir("edit-latency");
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jit::tests::layered::stage_de::development::real_coff_layered_edit_latency",
                "--ignored",
                "--nocapture",
            ])
            .env("REKNOT_DEV_LATENCY_CHILD", &dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("latency.stdout"), &result.stdout).unwrap();
        std::fs::write(dir.join("latency.stderr"), &result.stderr).unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        eprintln!("{}", String::from_utf8_lossy(&result.stderr));
    }

    fn keep_child(dir: &Path) {
        let source = dir.join("logic/src/lib.rs");
        let original = std::fs::read_to_string(&source).unwrap();
        let options = LayeredDevOptions {
            offline: true,
            max_router_generations: 2,
            ..LayeredDevOptions::new(
                dir.join("Cargo.toml"),
                "jit_layered_logic",
                "jit_layered_support",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/build-layered-jit.ps1"),
                dir.join("cache"),
            )
        };
        let resources = HostResources::new();
        let events = resources.register(KEY, Events::default());
        let mut session = unsafe { LayeredDevSession::start(options, resources) }.unwrap();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::Activated { .. }),
            &events,
        );
        let mut old = session
            .router()
            .unwrap()
            .dispatch(events.id(), b"wait")
            .unwrap();
        assert_eq!(poll(&mut old), Poll::Pending);
        std::fs::write(
            &source,
            original.replace("if cfg!(version_two) { 2 } else { 1 }", "2"),
        )
        .unwrap();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::Activated { .. }),
            &events,
        );
        assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
        drop(old);
        let manifest = session.snapshot().manifest;
        std::fs::write(
            &source,
            original.replace("if cfg!(version_two) { 2 } else { 1 }", "3"),
        )
        .unwrap();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::BudgetExhausted),
            &events,
        );
        assert_eq!(session.snapshot().load_attempts, 2);
        assert_eq!(session.snapshot().manifest, manifest);
        session.retry();
        for _ in 0..20 {
            assert!(!session
                .poll_update()
                .is_some_and(|e| matches!(e.state, LayeredDevState::Building { .. })));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(session.snapshot().load_attempts, 2);
        assert!(session
            .collect_retired(CoffReclaimOptions::default())
            .is_none());
        session.shutdown().unwrap();
        drop(session);
        let snapshot = coff_retention_snapshot();
        assert_eq!(snapshot.images.len(), 2);
        assert!(snapshot
            .images
            .iter()
            .all(|i| i.policy == crate::jit::CoffPolicy::KeepLoaded));
        assert!(collect_retired_coff(CoffReclaimOptions::default())
            .released
            .is_empty());
    }
    fn until(
        session: &mut LayeredDevSession,
        accept: impl Fn(&LayeredDevState) -> bool,
        resource: &crate::Resource<Events>,
    ) -> LayeredDevSnapshot {
        let start = Instant::now();
        let mut ticks = 0;
        let mut last_tick = Instant::now();
        let mut max_gap_ms = 0.0f64;
        loop {
            let now = Instant::now();
            max_gap_ms = max_gap_ms.max(now.duration_since(last_tick).as_secs_f64() * 1000.0);
            last_tick = now;
            if let Some(event) = session.poll_update() {
                eprintln!("{:?} {:?}", event.state, event.diagnostic);
                if accept(&event.state) {
                    eprintln!(
                        "host polls={ticks} elapsed={:.3} ms max_poll_gap={:.3} ms owner={:.3} ms",
                        start.elapsed().as_secs_f64() * 1000.0,
                        max_gap_ms,
                        session.snapshot().metrics.owner_ms
                    );
                    return session.snapshot();
                }
            }
            if let Some(router) = session.router() {
                run(router, resource, b"local", RunStatus::Done);
                ticks += 1;
            }
            assert!(
                start.elapsed() < Duration::from_secs(90),
                "timeout {:?}, ticks={ticks}",
                session.snapshot()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn child(dir: &Path) {
        let source = dir.join("logic/src/lib.rs");
        let original = std::fs::read_to_string(&source).unwrap();
        let marker = "if cfg!(version_two) { 2 } else { 1 }";
        std::fs::write(&source, original.replace(marker, "1")).unwrap();
        let builder =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/build-layered-jit.ps1");
        let wrapper = dir.join("builder.ps1");
        let script = std::fs::read_to_string(builder).unwrap();
        let quoted = dir
            .display()
            .to_string()
            .trim_start_matches(r"\\?\")
            .replace('\'', "''");
        let gate=format!("$ErrorActionPreference = 'Stop'\nif (Test-Path -LiteralPath '{quoted}/gate') {{ [IO.File]::WriteAllText('{quoted}/entered','ready'); while (!(Test-Path -LiteralPath '{quoted}/release')) {{ Start-Sleep -Milliseconds 10 }} }}");
        std::fs::write(
            &wrapper,
            script.replacen("$ErrorActionPreference = 'Stop'", &gate, 1),
        )
        .unwrap();
        let mut options = LayeredDevOptions::new(
            dir.join("Cargo.toml"),
            "jit_layered_logic",
            "jit_layered_support",
            &wrapper,
            dir.join("cache"),
        );
        options.offline = true;
        options.declared_inputs_complete = true;
        options.debounce = Duration::from_millis(10);
        options.limits.load.max_generations = 2;
        options.max_router_generations = 2;
        let resources = HostResources::new();
        let events = resources.register(KEY, Events::default());
        let mut session = unsafe { LayeredDevSession::start_managed(options, resources) }.unwrap();
        let first = until(
            &mut session,
            |s| matches!(s, LayeredDevState::Activated { .. }),
            &events,
        );
        assert_eq!(first.load_attempts, 1);
        let first_manifest = first.manifest.unwrap();
        let first_foundation = LayeredArtifact::read(&first_manifest)
            .unwrap()
            .foundation_id()
            .to_owned();
        let mut old = session
            .router()
            .unwrap()
            .dispatch(events.id(), b"wait")
            .unwrap();
        assert_eq!(poll(&mut old), Poll::Pending);
        std::fs::write(&source, "compile_error!(\"intentional syntax failure\");").unwrap();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::BuildFailed),
            &events,
        );
        assert_eq!(session.router().unwrap().generation(), 1);
        // A real builder pauses at a deterministic barrier. More source edits
        // occur while old requests continue; its obsolete result must not activate.
        std::fs::write(dir.join("gate"), "").unwrap();
        std::fs::write(&source, original.replace(marker, "2")).unwrap();
        let start = Instant::now();
        let mut ticks = 0;
        while !dir.join("entered").exists() {
            if let Some(event) = session.poll_update() {
                eprintln!("gate: {:?} {:?}", event.state, event.diagnostic);
            }
            run(
                session.router().unwrap(),
                &events,
                b"local",
                RunStatus::Done,
            );
            ticks += 1;
            assert!(start.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::write(&source, original.replace(marker, "3")).unwrap();
        std::thread::sleep(Duration::from_millis(40));
        std::fs::write(dir.join("release"), "").unwrap();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::Superseded),
            &events,
        );
        let second = until(
            &mut session,
            |s| matches!(s, LayeredDevState::Activated { .. }),
            &events,
        );
        assert!(ticks > 1);
        assert_eq!(second.load_attempts, 2);
        assert_eq!(
            LayeredArtifact::read(second.manifest.unwrap())
                .unwrap()
                .foundation_id(),
            first_foundation
        );
        run(
            session.router().unwrap(),
            &events,
            b"local",
            RunStatus::Done,
        );
        assert_eq!(*values(&events, 1).last().unwrap(), 307);
        assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
        drop(old);
        session.router_mut().unwrap().release_retired_generations();
        assert_eq!(
            session
                .collect_retired(CoffReclaimOptions::default())
                .unwrap()
                .released
                .len(),
            1
        );
        // Directory instance replacement forces fresh watches and a rebuild.
        let src = dir.join("logic/src");
        let saved = dir.join("logic/old-src");
        std::fs::rename(&src, &saved).unwrap();
        std::fs::create_dir(&src).unwrap();
        std::fs::write(&source, original.replace(marker, "4")).unwrap();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::Activated { .. }),
            &events,
        );
        // Output writes cannot trigger another job. Explicit retry alone consumes
        // further load attempts and never replaces the engine to reset its budget.
        let attempts = session.snapshot().load_attempts;
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {
            session.poll_update();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(session.snapshot().load_attempts, attempts);
        // Managed updates exceed both nominal lifetime limits without resetting
        // counters. Session maintenance releases retired images automatically.
        for _ in 0..5 {
            session.retry();
            until(
                &mut session,
                |s| matches!(s, LayeredDevState::Activated { .. }),
                &events,
            );
        }
        assert!(session.snapshot().load_attempts > 6);
        assert_eq!(session.snapshot().keep_loaded_attempts, 0);
        assert_eq!(session.router().unwrap().options().generations.used(), 0);
        let support = dir.join("support/src/lib.rs");
        let support_original = std::fs::read_to_string(&support).unwrap();
        std::fs::write(&support,format!("{support_original}\n#[no_mangle] pub extern \"C\" fn different_foundation()->u32{{9}}\n")).unwrap();
        let current_generation = session.router().unwrap().generation();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::RestartRequired { .. }),
            &events,
        );
        assert_eq!(session.router().unwrap().generation(), current_generation);
        std::fs::write(&support, support_original).unwrap();
        // Rebuilding identical source can change PE bytes. Only actual identity
        // equality permits returning to ordinary load.
        until(
            &mut session,
            |s| {
                matches!(
                    s,
                    LayeredDevState::RestartRequired { .. } | LayeredDevState::Activated { .. }
                )
            },
            &events,
        );
        let attempts = session.snapshot().load_attempts;
        // Add a local dependency outside the old watch graph, then edit its
        // contents under the newly registered watch set.
        std::fs::create_dir_all(dir.join("extra/src")).unwrap();
        std::fs::write(
            dir.join("extra/Cargo.toml"),
            "[package]\nname='extra'\nversion='0.0.0'\nedition='2021'\n",
        )
        .unwrap();
        std::fs::write(dir.join("extra/src/lib.rs"), "pub fn value()->u32{1}").unwrap();
        let workspace = dir.join("Cargo.toml");
        let text = std::fs::read_to_string(&workspace).unwrap().replace(
            "[\"support\", \"logic\"]",
            "[\"support\", \"logic\", \"extra\"]",
        );
        std::fs::write(workspace, text).unwrap();
        let logic_manifest = dir.join("logic/Cargo.toml");
        let mut text = std::fs::read_to_string(&logic_manifest).unwrap();
        text.push_str("extra = { path = '../extra' }\n");
        std::fs::write(&logic_manifest, text).unwrap();
        let lock = Command::new("cargo")
            .args([
                "+stable",
                "generate-lockfile",
                "--offline",
                "--manifest-path",
            ])
            .arg(dir.join("Cargo.toml"))
            .output()
            .unwrap();
        assert!(
            lock.status.success(),
            "{}",
            String::from_utf8_lossy(&lock.stderr)
        );
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::Superseded),
            &events,
        );
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::RestartRequired { .. }),
            &events,
        );
        std::fs::write(dir.join("extra/src/lib.rs"), "pub fn value()->u32{2}").unwrap();
        until(
            &mut session,
            |s| matches!(s, LayeredDevState::RestartRequired { .. }),
            &events,
        );
        assert_eq!(session.snapshot().load_attempts, attempts);
        // Shutdown while the producer is actually running: the owner waits only
        // at this explicit API and must never publish the delayed result.
        std::fs::remove_file(dir.join("release")).unwrap();
        std::fs::remove_file(dir.join("entered")).unwrap();
        session.retry();
        let start = Instant::now();
        while !dir.join("entered").exists() {
            session.poll_update();
            assert!(start.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(5));
        }
        let release = dir.join("release");
        let unblock = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            std::fs::write(release, "").unwrap();
        });
        session.shutdown().unwrap();
        unblock.join().unwrap();
        assert!(session.poll_update().is_none());
        assert_eq!(session.snapshot().load_attempts, attempts);
        drop(session);
        collect_retired_coff(CoffReclaimOptions::default());
        assert_eq!(coff_retention_snapshot().retained_bytes, 0);
    }
    #[test]
    #[ignore = "PowerShell builder, real source watcher and Windows COFF development flow"]
    fn real_coff_layered_development() {
        if let Ok(dir) = std::env::var("REKNOT_DEV_KEEP_CHILD") {
            keep_child(Path::new(&dir));
            return;
        }
        if let Ok(dir) = std::env::var("REKNOT_DEV_CHILD") {
            child(Path::new(&dir));
            return;
        }
        let dir = fixture_dir("development");
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jit::tests::layered::stage_de::development::real_coff_layered_development",
                "--ignored",
                "--nocapture",
            ])
            .env("REKNOT_DEV_CHILD", &dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("development.stdout"), &result.stdout).unwrap();
        std::fs::write(dir.join("development.stderr"), &result.stderr).unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        eprintln!("{}", String::from_utf8_lossy(&result.stderr));
        let keep_dir = fixture_dir("development-keep");
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jit::tests::layered::stage_de::development::real_coff_layered_development",
                "--ignored",
                "--nocapture",
            ])
            .env("REKNOT_DEV_KEEP_CHILD", &keep_dir)
            .output()
            .unwrap();
        std::fs::write(keep_dir.join("development.stdout"), &result.stdout).unwrap();
        std::fs::write(keep_dir.join("development.stderr"), &result.stderr).unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        eprintln!(
            "KeepLoaded session: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
