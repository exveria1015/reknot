//! Real Linux objects, isolated resident foundations and owner-thread collection.
use super::*;
use crate::jit::{
    collect_retired_elf, elf_retention_snapshot, CoffLimits, JitReclaimOptions, JitRetentionReason,
    LayeredArtifact, LayeredEngine, TrustedLayeredArtifact,
};
use std::time::{Duration, Instant};

fn trusted(path: &Path) -> TrustedLayeredArtifact {
    unsafe { LayeredArtifact::read(path).unwrap().into_trusted() }
}
fn collect() -> crate::jit::JitReclaimReport {
    collect_retired_elf(JitReclaimOptions::default())
}
fn mapped(address: usize, memory: bool, unwind: bool) {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    assert_eq!(
        maps.lines().any(|line| {
            let (a, b) = line
                .split_whitespace()
                .next()
                .unwrap()
                .split_once('-')
                .unwrap();
            (usize::from_str_radix(a, 16).unwrap()..usize::from_str_radix(b, 16).unwrap())
                .contains(&address)
        }),
        memory,
        "mapping at {address:x}"
    );
    let gcc = unsafe { libloading::Library::new("libgcc_s.so.1") }.unwrap();
    let find = unsafe {
        gcc.get::<unsafe extern "C" fn(usize, *mut [usize; 3]) -> *const u8>(b"_Unwind_Find_FDE")
    }
    .unwrap();
    assert_eq!(
        !unsafe { find(address, &mut [0; 3]) }.is_null(),
        unwind,
        "FDE at {address:x}"
    );
}
fn library(name: &str) -> libloading::Library {
    let name = std::ffi::CString::new(name).unwrap();
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
    assert!(!handle.is_null());
    unsafe { libloading::os::unix::Library::from_raw(handle).into() }
}

fn managed(manifest: &Path, mode: &str) {
    let limits = CoffLimits {
        load: crate::jit::JitLimits {
            max_generations: 1,
            ..Default::default()
        },
        max_retained_images: 3,
        ..Default::default()
    };
    let mut engine = LayeredEngine::open(&trusted(manifest), limits).unwrap();
    let first = if mode == "keep" {
        engine.load(trusted(manifest))
    } else {
        engine.load_managed(unsafe { trusted(manifest).into_managed() })
    }
    .unwrap();
    let id = first.elf_image_id().unwrap();
    assert!(first.coff_image_id().is_none());
    let address = first.api.make_request as usize;
    mapped(address, true, true);
    let resources = HostResources::new();
    let events = resources.register(KEY, Events::default());
    let mut router = RequestRouter::from_loaded_with_options(
        first,
        resources.clone(),
        crate::RouterOptions {
            generations: crate::GenerationBudget::new(1).unwrap(),
            ..Default::default()
        },
    )
    .unwrap();
    if mode == "external" || mode == "adapter-panic" {
        let mut task = router.dispatch(events.id(), mode.as_bytes()).unwrap();
        assert_eq!(poll(&mut task), Poll::Pending);
        let support = library("libjit_layered_support.so");
        let entered =
            unsafe { support.get::<unsafe extern "C" fn() -> bool>(b"layered_tail_entered") }
                .unwrap();
        let release =
            unsafe { support.get::<unsafe extern "C" fn()>(b"layered_release_tail") }.unwrap();
        let start = Instant::now();
        if mode == "external" {
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
        assert!(collect().released.is_empty());
        mapped(address, true, true);
        assert_eq!(elf_retention_snapshot().images[0].managed_activities, 1);
        if mode == "adapter-panic" {
            return;
        }
        unsafe { release() };
        while crate::poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_pending() {
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::yield_now();
        }
        assert_eq!(collect().released, [id]);
        mapped(address, false, false);
        return;
    }
    if mode == "local-failure" || mode == "keep" || mode.starts_with("release-") {
        request(
            &router,
            &events,
            if mode == "local-failure" {
                b"fail-drop"
            } else {
                b"local"
            },
            RunStatus::Done,
        );
        if mode.starts_with("release-") {
            super::super::super::platform::retention::fail_release(
                id,
                if mode == "release-unwind" { 1 } else { 2 },
            );
        }
        drop(router);
        drop(engine);
        let report = collect();
        assert!(report.released.is_empty());
        mapped(address, true, mode != "release-memory");
        let expected = match mode {
            "keep" => JitRetentionReason::KeepLoaded,
            "local-failure" => JitRetentionReason::LocalCleanupFailed,
            "release-unwind" => JitRetentionReason::UnwindRemovalFailed,
            _ => JitRetentionReason::MemoryReleaseFailed,
        };
        assert!(elf_retention_snapshot().images[0]
            .reasons
            .contains(&expected));
        assert!(
            collect().errors.is_empty(),
            "uncertain release is never retried"
        );
        return;
    }
    let mut old = router.dispatch(events.id(), b"wait").unwrap();
    assert_eq!(poll(&mut old), Poll::Pending);
    let second = engine
        .load_managed(unsafe { trusted(manifest).into_managed() })
        .unwrap();
    router
        .activate(router.prepare_loaded(second).unwrap())
        .unwrap();
    router.release_retired_generations();
    assert!(collect().released.is_empty());
    assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
    assert!(
        collect().released.is_empty(),
        "completed handle still retains code"
    );
    drop(old);
    router.release_retired_generations();
    assert_eq!(collect().released, [id]);
    mapped(address, false, false);
    assert_eq!(values(&events, 9), [1]);
    request(&router, &events, b"checks", RunStatus::Done);
    request(&router, &events, b"panic", RunStatus::Error);
    // Collection from a host callback must see the active call-return guard.
    struct Reenter;
    impl HostResource for Reenter {
        fn call(&mut self, _: u32, _: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
            assert!(elf_retention_snapshot()
                .images
                .iter()
                .any(|i| i.executions > 0));
            assert!(collect().released.is_empty());
            Ok(0)
        }
    }
    let resource = resources.register(KEY, Reenter);
    let mut reenter = router.dispatch(resource.id(), b"reenter").unwrap();
    assert_eq!(poll(&mut reenter), Poll::Ready(RunStatus::Done));
    drop(reenter);
    for _ in 0..128 {
        let next = engine
            .load_managed(unsafe { trusted(manifest).into_managed() })
            .unwrap();
        router
            .activate(router.prepare_loaded(next).unwrap())
            .unwrap();
        request(&router, &events, b"local", RunStatus::Done);
        router.release_retired_generations();
        assert_eq!(collect().released.len(), 1);
        assert_eq!(elf_retention_snapshot().images.len(), 1);
    }
    let abandoned = engine
        .load_managed(unsafe { trusted(manifest).into_managed() })
        .unwrap();
    let abandoned_address = abandoned.api.make_request as usize;
    drop(router.prepare_loaded(abandoned).unwrap());
    assert_eq!(collect().released.len(), 1);
    mapped(abandoned_address, false, false);
    assert!(engine.load_attempts() > 128);
    assert_eq!(engine.keep_loaded_attempts(), 0);
    drop(router);
    drop(engine);
    assert_eq!(collect().released.len(), 1);
    let snapshot = elf_retention_snapshot();
    assert!(snapshot.images.is_empty());
    assert_eq!(snapshot.retained_bytes, 0);
    eprintln!(
        "ELF managed: {} images physically released",
        snapshot.released_images
    );
}

fn identity(manifest: &Path, preload: bool) {
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    if preload {
        let _std = unsafe {
            libloading::Library::new(value["foundation"]["std_library"]["path"].as_str().unwrap())
        }
        .unwrap();
        assert!(
            matches!(LayeredEngine::open(&trusted(manifest), CoffLimits::default()), Err(JitError::Identity(s)) if s.contains("already loaded"))
        );
        return;
    }
    let dir = manifest.parent().unwrap().join("identity-copies");
    std::fs::create_dir(&dir).unwrap();
    for key in ["std_library", "support_library"] {
        let path = Path::new(value["foundation"][key]["path"].as_str().unwrap());
        let copy = dir.join(path.file_name().unwrap());
        std::fs::copy(path, &copy).unwrap();
        value["foundation"][key]["path"] = serde_json::json!(copy);
    }
    let path = dir.join("logic.o.json");
    let save =
        |v: &serde_json::Value| std::fs::write(&path, serde_json::to_vec(v).unwrap()).unwrap();
    let mut wrong = value.clone();
    wrong["foundation"]["support_library"]["sha256"] = "0".repeat(64).into();
    save(&wrong);
    assert!(matches!(
        LayeredEngine::open(&trusted(&path), CoffLimits::default()),
        Err(JitError::Identity(_))
    ));
    save(&value);
    let captured = trusted(&path);
    let mut engine = LayeredEngine::open(&captured, CoffLimits::default()).unwrap();
    let mut count = 0;
    for fd in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
        if std::fs::read_link(fd.path())
            .is_ok_and(|p| p.to_string_lossy().starts_with("/memfd:reknot-"))
        {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(fd.path())
                .unwrap();
            use std::os::fd::AsRawFd;
            let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
            assert_eq!(
                seals,
                libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL
            );
            assert_eq!(unsafe { libc::ftruncate(file.as_raw_fd(), 0) }, -1);
            count += 1;
        }
    }
    assert_eq!(count, 2);
    // Existing loaded bytes and repeated identical opens survive locator mutation.
    for key in ["std_library", "support_library"] {
        std::fs::write(
            value["foundation"][key]["path"].as_str().unwrap(),
            b"replaced",
        )
        .unwrap();
    }
    LayeredEngine::open(&trusted(&path), CoffLimits::default()).unwrap();
    let loaded = engine
        .load_managed(unsafe { captured.into_managed() })
        .unwrap();
    let resources = HostResources::new();
    let events = resources.register(KEY, Events::default());
    let router = RequestRouter::from_loaded(loaded, resources).unwrap();
    request(&router, &events, b"checks", RunStatus::Done);
    wrong = value.clone();
    wrong["foundation"]["cargo_lock_sha256"] = "1".repeat(64).into();
    save(&wrong);
    assert!(
        matches!(LayeredEngine::open(&trusted(&path), CoffLimits::default()), Err(JitError::Identity(s)) if s.contains("restart required"))
    );
    assert!(matches!(
        engine.load(trusted(&path)),
        Err(JitError::Identity(_))
    ));
    drop(router);
    assert_eq!(collect().released.len(), 1);
}

#[test]
#[ignore = "real Python/Cargo producer, sealed foundations and Linux managed reclamation"]
fn real_elf_checked_contracts() {
    if let Ok(mode) = std::env::var("REKNOT_ELF_CHECKED_MODE") {
        let manifest = PathBuf::from(std::env::var("REKNOT_ELF_CHECKED_MANIFEST").unwrap());
        match mode.as_str() {
            "identity" | "preloaded" => identity(&manifest, mode == "preloaded"),
            #[cfg(feature = "jit-dev")]
            "dev-wait" | "dev-cancel" | "dev-pinned" => {
                development(manifest.parent().unwrap(), &mode)
            }
            _ => managed(&manifest, &mode),
        }
        return;
    }
    let f = fixtures();
    let builder = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/build-layered-elf.py");
    run_command(
        Command::new("python3")
            .arg(builder)
            .arg("-ManifestPath")
            .arg(f.dir.join("Cargo.toml"))
            .args([
                "-Package",
                "jit_layered_logic",
                "-Offline",
                "-TargetDirectory",
            ])
            .arg(f.dir.join("checked-cargo"))
            .arg("-SupportLibrary")
            .arg(
                f.dir
                    .join("checked-cargo/x86_64-unknown-linux-gnu/debug/libjit_layered_support.so"),
            )
            .arg("-CacheDirectory")
            .arg(f.dir.join("checked-cache"))
            .arg("-Output")
            .arg(f.dir.join("checked.o")),
    );
    let modes = [
        "identity",
        "preloaded",
        "roundtrip",
        "external",
        "adapter-panic",
        "local-failure",
        "keep",
        "release-unwind",
        "release-memory",
    ];
    for mode in modes.into_iter().chain(if cfg!(feature = "jit-dev") {
        vec!["dev-wait", "dev-cancel", "dev-pinned"]
    } else {
        vec![]
    }) {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jit::elf::tests::linux::checked::real_elf_checked_contracts",
                "--ignored",
                "--nocapture",
            ])
            .env("REKNOT_ELF_CHECKED_MODE", mode)
            .env("REKNOT_ELF_CHECKED_MANIFEST", f.dir.join("checked.o.json"))
            .output()
            .unwrap();
        std::fs::write(f.dir.join(format!("{mode}.stdout")), &output.stdout).unwrap();
        std::fs::write(f.dir.join(format!("{mode}.stderr")), &output.stderr).unwrap();
        assert!(
            output.status.success(),
            "{mode}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("ELF {mode}: passed");
    }
    eprintln!("ELF checked evidence: {}", f.dir.display());
}

#[cfg(feature = "jit-dev")]
fn development(dir: &Path, mode: &str) {
    use crate::jit::development::{
        LayeredDevOptions, LayeredDevSession, LayeredDevState, ManagedPressure,
    };
    fn until(session: &mut LayeredDevSession, test: impl Fn(&LayeredDevState) -> bool) {
        let start = Instant::now();
        loop {
            if let Some(event) = session.poll_update() {
                if test(&event.state) {
                    return;
                }
                assert!(
                    !matches!(
                        event.state,
                        LayeredDevState::Rejected
                            | LayeredDevState::WatchUnavailable
                            | LayeredDevState::CacheFull
                            | LayeredDevState::BudgetExhausted
                    ),
                    "{event:?}"
                );
            }
            assert!(
                start.elapsed() < Duration::from_secs(90),
                "{:?}",
                session.snapshot()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let source = dir.join("logic/src/lib.rs");
    let original = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jit_layered_logic.rs"),
    )
    .unwrap();
    let save = |v: u64| {
        std::fs::write(
            &source,
            original.replace("if cfg!(version_two) { 2 } else { 1 }", &v.to_string()),
        )
        .unwrap()
    };
    save(1);
    let mut options = LayeredDevOptions::new(
        dir.join("Cargo.toml"),
        "jit_layered_logic",
        "jit_layered_support",
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/build-layered-elf.py"),
        dir.join("dev-cache"),
    );
    options.offline = true;
    options.declared_inputs_complete = true;
    options.limits.load.max_generations = 1;
    options.max_router_generations = 1;
    options.debounce = Duration::from_millis(10);
    if mode == "dev-cancel" {
        options.managed_reload.pressure = ManagedPressure::CancelOldest;
    }
    let resources = HostResources::new();
    let events = resources.register(KEY, Events::default());
    let mut session = unsafe { LayeredDevSession::start_managed(options, resources) }.unwrap();
    until(&mut session, |s| {
        matches!(s, LayeredDevState::Activated { .. })
    });
    let mut old = if mode == "dev-pinned" {
        request(
            session.router().unwrap(),
            &events,
            b"fail-drop",
            RunStatus::Done,
        );
        None
    } else {
        let mut old = session
            .router()
            .unwrap()
            .dispatch(events.id(), b"cancel")
            .unwrap();
        assert_eq!(poll(&mut old), Poll::Pending);
        Some(old)
    };
    save(2);
    until(&mut session, |s| {
        matches!(s, LayeredDevState::Activated { .. })
    });
    request(
        session.router().unwrap(),
        &events,
        b"local",
        RunStatus::Done,
    );
    assert_eq!(*values(&events, 1).last().unwrap(), 207);
    save(3);
    if mode == "dev-pinned" {
        until(&mut session, |s| {
            matches!(s, LayeredDevState::RecoveryRequired { .. })
        });
        assert_eq!(session.snapshot().load_attempts, 2);
    } else {
        until(&mut session, |s| {
            matches!(s, LayeredDevState::WaitingForRetirement { .. })
        });
        assert_eq!(session.snapshot().load_attempts, 2);
        save(4);
        for _ in 0..30 {
            session.poll_update();
            std::thread::sleep(Duration::from_millis(5));
        }
        if mode == "dev-cancel" {
            assert_eq!(poll(old.as_mut().unwrap()), Poll::Ready(RunStatus::Error));
            session.poll_update();
            assert_eq!(session.snapshot().load_attempts, 2);
            drop(old.take());
        } else {
            assert_eq!(poll(old.as_mut().unwrap()), Poll::Pending);
            old.take().unwrap().cancel();
        }
        until(&mut session, |s| {
            matches!(s, LayeredDevState::Activated { .. })
        });
        assert_eq!(session.snapshot().keep_loaded_attempts, 0);
        request(
            session.router().unwrap(),
            &events,
            b"local",
            RunStatus::Done,
        );
        assert_eq!(*values(&events, 1).last().unwrap(), 407);
        std::fs::write(&source, "invalid Rust code").unwrap();
        until(&mut session, |s| matches!(s, LayeredDevState::BuildFailed));
        request(
            session.router().unwrap(),
            &events,
            b"local",
            RunStatus::Done,
        );
        assert_eq!(*values(&events, 1).last().unwrap(), 407);
        save(5);
        until(&mut session, |s| {
            matches!(s, LayeredDevState::Activated { .. })
        });
        let support_path = dir.join("support/src/lib.rs");
        let support = std::fs::read_to_string(&support_path).unwrap();
        std::fs::write(&support_path, format!("{support}\n#[no_mangle] pub extern \"C\" fn changed_foundation() -> u32 {{ 42 }}\n")).unwrap();
        until(&mut session, |s| {
            matches!(s, LayeredDevState::RestartRequired { .. })
        });
        request(
            session.router().unwrap(),
            &events,
            b"local",
            RunStatus::Done,
        );
        assert_eq!(*values(&events, 1).last().unwrap(), 507);
        session.shutdown().unwrap();
        std::fs::write(support_path, support).unwrap();
    }
    session.shutdown().unwrap();
    drop(session);
    collect();
    assert_eq!(
        elf_retention_snapshot().images.len(),
        usize::from(mode == "dev-pinned")
    );
    std::fs::write(source, original).unwrap();
}
