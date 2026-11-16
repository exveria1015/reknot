use super::*;

#[path = "jit_identity_tests.rs"]
mod identity;
#[path = "jit_stage_de_tests.rs"]
mod stage_de;

const KEY: ServiceKey = ServiceKey::new(0x4c41_5945, 1);

enum LayeredBackend {
    Llvm(JitEngine),
    Rust(CoffEngine),
}
impl LayeredBackend {
    unsafe fn load(&mut self, path: impl AsRef<Path>) -> Result<LoadedHandler, JitError> {
        match self {
            Self::Llvm(e) => e.load(path),
            Self::Rust(e) => e.load(path),
        }
    }
    fn last_load_metrics(&self) -> Option<JitLoadMetrics> {
        match self {
            Self::Llvm(e) => e.last_load_metrics(),
            Self::Rust(e) => e.last_load_metrics(),
        }
    }
    unsafe fn symbol(&self, name: &CStr) -> usize {
        match self {
            Self::Llvm(e) => runtime_symbol(e, name) as usize,
            Self::Rust(e) => *e.last_symbols.get(name.to_bytes()).unwrap(),
        }
    }
}

struct LayeredFixtures {
    dir: PathBuf,
    runtime: PathBuf,
    support: PathBuf,
    v1: PathBuf,
    v2: PathBuf,
    tls: PathBuf,
    external_tls: PathBuf,
    emulated: PathBuf,
    bad: PathBuf,
    missing: PathBuf,
}
fn layered_fixtures() -> &'static LayeredFixtures {
    static FIXTURES: std::sync::OnceLock<LayeredFixtures> = std::sync::OnceLock::new();
    FIXTURES.get_or_init(|| {
        let toolchain =
            std::env::var("REKNOT_JIT_LAYERED_TOOLCHAIN").unwrap_or_else(|_| "stable".into());
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/jit-layered-fixtures")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        project(&dir);
        let v1 = build_layered(&dir, &toolchain, "v1", None);
        let v2 = build_layered(&dir, &toolchain, "v2", Some("version_two"));
        let tls = build_layered(&dir, &toolchain, "native-tls", Some("native_tls"));
        let external_tls = build_layered(&dir, &toolchain, "external-tls", Some("external_tls"));
        let emulated = build_std(&dir, "emulated-tls", &[], true);
        let bad = build(&dir, "bad-abi", &["bad_abi"]);
        let missing = build(&dir, "unresolved", &["unresolved_import"]);
        let support = dir.join("build/x86_64-pc-windows-msvc/debug/jit_layered_support.dll");
        let runtime = compiler_runtime(&toolchain);
        LayeredFixtures {
            dir,
            runtime,
            support,
            v1,
            v2,
            tls,
            external_tls,
            emulated,
            bad,
            missing,
        }
    })
}

fn project(dir: &Path) {
    for name in ["support", "logic"] {
        std::fs::create_dir_all(dir.join(name).join("src")).unwrap();
    }
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR"))
        .to_string_lossy()
        .replace('\\', "/");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"support\", \"logic\"]\nresolver = \"2\"\n\
         [profile.dev]\nopt-level = 1\ndebug = 2\ncodegen-units = 1\nlto = false\npanic = \"unwind\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("support/Cargo.toml"),
        format!(
            "[package]\nname = \"jit_layered_support\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\ncrate-type = [\"dylib\"]\n\
             [dependencies]\nreknot = {{ path = \"{sdk}\", default-features = false }}\nserde_json = \"1\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("logic/Cargo.toml"),
        "[package]\nname = \"jit_layered_logic\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
         [lib]\ncrate-type = [\"cdylib\"]\n\
         [dependencies]\njit_layered_support = { path = \"../support\" }\n",
    )
    .unwrap();
    for (target, source) in [
        ("support", "jit_layered_support.rs"),
        ("logic", "jit_layered_logic.rs"),
    ] {
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(source),
            dir.join(target).join("src/lib.rs"),
        )
        .unwrap();
    }
}

fn build_layered(dir: &Path, toolchain: &str, name: &str, cfg: Option<&str>) -> PathBuf {
    let object = dir.join(format!("{name}.obj"));
    let mut command = Command::new("cargo");
    let crate_type =
        std::env::var("REKNOT_JIT_LAYERED_CRATE_TYPE").unwrap_or_else(|_| "cdylib".into());
    command
        .current_dir(dir)
        .arg(format!("+{toolchain}"))
        .args(["rustc", "--crate-type", &crate_type])
        .args([
            "--offline",
            "--target",
            "x86_64-pc-windows-msvc",
            "-p",
            "jit_layered_logic",
        ])
        .arg("--target-dir")
        .arg(dir.join("build"))
        .env("CARGO_INCREMENTAL", "0")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env(
            "RUSTFLAGS",
            "-C prefer-dynamic=yes -C force-unwind-tables=yes",
        )
        .arg("--")
        .args(["--check-cfg", "cfg(version_two,native_tls,external_tls)"])
        .arg(format!("--emit=obj={}", object.display()));
    if let Ok(opt) = std::env::var("REKNOT_JIT_LAYERED_OPT") {
        command.args(["-C", &format!("opt-level={opt}")]);
    }
    if let Ok(debug) = std::env::var("REKNOT_JIT_LAYERED_DEBUG") {
        command.args(["-C", &format!("debuginfo={debug}")]);
    }
    if let Some(cfg) = cfg {
        command.args(["--cfg", cfg]);
    }
    let result = command.output().unwrap();
    std::fs::write(dir.join(format!("{name}-build.log")), &result.stderr).unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(object.is_file());
    object
}

#[derive(Default)]
struct Events(Vec<(u32, u64)>);
impl HostResource for Events {
    fn call(&mut self, op: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0
            .push((op, u64::from_le_bytes(input.try_into().unwrap())));
        Ok(0)
    }
}

fn values(resource: &crate::Resource<Events>, op: u32) -> Vec<u64> {
    resource
        .try_borrow()
        .unwrap()
        .0
        .iter()
        .filter_map(|(kind, value)| (*kind == op).then_some(*value))
        .collect()
}

fn run(
    router: &RequestRouter,
    resource: &crate::Resource<Events>,
    input: &[u8],
    status: RunStatus,
) {
    let mut task = router.dispatch(resource.id(), input).unwrap();
    assert_eq!(poll(&mut task), Poll::Ready(status));
}

#[test]
#[ignore = "requires -Testing LLVM bridge, REKNOT_JIT_BRIDGE, stable/nightly Rust and cached SDK dependencies"]
fn real_coff_layered_sdk_cutover_locals_panic_and_tls_rejection() {
    layered_case(false);
}

#[test]
#[ignore = "requires stable/nightly Rust and cached SDK dependencies; no LLVM needed"]
fn real_coff_rust_layered_sdk_cutover_locals_panic_and_tls_rejection() {
    layered_case(true);
}

fn layered_case(rust: bool) {
    let LayeredFixtures {
        dir,
        runtime,
        support,
        v1,
        v2,
        tls,
        external_tls,
        emulated,
        bad,
        missing,
    } = layered_fixtures();
    let mut engine = if rust {
        LayeredBackend::Rust(
            unsafe { CoffEngine::open_layered(runtime, support, CoffLimits::default()) }.unwrap(),
        )
    } else {
        let bridge = std::env::var_os("REKNOT_JIT_BRIDGE").expect("set REKNOT_JIT_BRIDGE");
        LayeredBackend::Llvm(
            unsafe { JitEngine::open_layered(bridge, runtime, support, JitLimits::default()) }
                .unwrap(),
        )
    };
    let first = unsafe { engine.load(v1) }.unwrap();
    assert!(engine.last_load_metrics().unwrap().unwind_entries > 0);
    let resources = HostResources::new();
    let events = resources.register(KEY, Events::default());
    let mut router = RequestRouter::from_loaded(first, resources).unwrap();
    let mut old = router.dispatch(events.id(), b"wait").unwrap();
    assert_eq!(poll(&mut old), Poll::Pending);

    // Rejection cannot change the active handler. No process-wide lookup may
    // acquire support exports even though another engine already loaded it.
    let mut without_support = if rust {
        // Supply std twice: the actual support is already process-resident but
        // deliberately absent from this engine's explicit import set.
        LayeredBackend::Rust(
            unsafe { CoffEngine::open_layered(runtime, runtime, CoffLimits::default()) }.unwrap(),
        )
    } else {
        let bridge = std::env::var_os("REKNOT_JIT_BRIDGE").unwrap();
        LayeredBackend::Llvm(
            unsafe { JitEngine::open_with_runtime(bridge, runtime, JitLimits::default()) }.unwrap(),
        )
    };
    assert!(matches!(
        unsafe { without_support.load(v1) },
        Err(JitError::Link(_))
    ));
    for object in [&tls, &external_tls] {
        assert!(
            matches!(unsafe { engine.load(object) }, Err(JitError::Link(e)) if e.contains("native TLS"))
        );
        assert!(engine.last_load_metrics().is_none());
        assert_eq!(router.generation(), 1);
    }
    assert!(
        matches!(unsafe { engine.load(emulated) }, Err(JitError::Link(e)) if e.contains("emulated TLS"))
    );
    assert!(matches!(
        unsafe { engine.load(bad) },
        Err(JitError::Handler(RouteError::Abi { .. }))
    ));
    assert!(matches!(
        unsafe { engine.load(missing) },
        Err(JitError::Link(_))
    ));
    let malformed = dir.join(if rust {
        "malformed-rust.obj"
    } else {
        "malformed-llvm.obj"
    });
    std::fs::write(&malformed, b"not COFF").unwrap();
    assert!(matches!(
        unsafe { engine.load(&malformed) },
        Err(JitError::Link(_))
    ));
    assert_eq!(router.generation(), 1);

    let next = unsafe { engine.load(v2) }.unwrap();
    let candidate = router.prepare_loaded(next).unwrap();
    assert_eq!(router.activate(candidate).unwrap(), 2);
    let mut new = router.dispatch(events.id(), b"wait").unwrap();
    assert_eq!(poll(&mut new), Poll::Pending);
    run(&router, &events, b"local", RunStatus::Done);
    assert_eq!(values(&events, 0), [1, 1, 2]);
    assert_eq!(
        values(&events, 3),
        [1, 2, 3],
        "support TLS must stay resident across generations"
    );
    assert_eq!(router.release_retired_generations(), 0);
    assert!(values(&events, 9).is_empty());
    assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
    drop(old);
    assert_eq!(router.release_retired_generations(), 1);
    assert_eq!(values(&events, 9), [1]);
    assert_eq!(values(&events, 1), [207, 107]);

    run(&router, &events, b"checks", RunStatus::Done);
    let payload_drops: unsafe extern "C" fn() -> u64 =
        unsafe { std::mem::transmute(engine.symbol(c"layered_payload_drops")) };
    assert_eq!(unsafe { payload_drops() }, 0);
    run(&router, &events, b"panic", RunStatus::Error);
    assert_eq!(
        unsafe { payload_drops() },
        1,
        "panic payload must be destroyed before returning to the host"
    );
    run(&router, &events, b"local", RunStatus::Done);
    let mut cancelled = router.dispatch(events.id(), b"cancel").unwrap();
    assert_eq!(poll(&mut cancelled), Poll::Pending);
    drop(cancelled);
    assert!(crate::poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_ready());
    assert_eq!(values(&events, 6), [2]);

    // A failed local destructor is caught in logic and never retried. KeepLoaded
    // also keeps the support-owned Future wrapper and its logic vtable valid.
    run(&router, &events, b"fail-drop", RunStatus::Done);
    drop(engine);
    drop(router);
    assert_eq!(values(&events, 9), [1]);
    assert_eq!(poll(&mut new), Poll::Ready(RunStatus::Done));
    drop(new);
    assert_eq!(values(&events, 9), [1, 2]);
    for _ in 0..2 {
        assert!(crate::poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_ready());
    }
    assert_eq!(values(&events, 9), [1, 2]);
    assert_eq!(values(&events, 1).last(), Some(&207));
}
