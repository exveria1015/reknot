//! End-to-end producer -> immutable artifact -> safe checked session.
use super::*;
use serde_json::{json, Value};

fn manifest(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn trusted(path: &Path) -> TrustedLayeredArtifact {
    // These fixtures and their producer are controlled native test code. The
    // session, not this unsafe block, checks which resident foundation they match.
    unsafe { LayeredArtifact::read(path).unwrap().into_trusted() }
}
pub(super) fn build(dir: &Path, name: &str, reuse: Option<&Path>, success: bool) -> PathBuf {
    let script = std::env::var_os("REKNOT_LAYERED_BUILDER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/build-layered-jit.ps1")
        });
    assert!(
        script.is_file(),
        "requires repository builder or REKNOT_LAYERED_BUILDER"
    );
    let output = dir.join(format!("artifacts/{name}.obj"));
    let mut command = Command::new("pwsh");
    command
        .args(["-NoProfile", "-File"])
        .arg(script)
        .arg("-ManifestPath")
        .arg(dir.join("Cargo.toml"))
        .args(["-Package", "jit_layered_logic", "-Offline"])
        .arg("-TargetDirectory")
        .arg(dir.join("build"))
        .arg("-SupportDll")
        .arg(dir.join("build/x86_64-pc-windows-msvc/debug/jit_layered_support.dll"))
        .arg("-Output")
        .arg(&output);
    if let Some(manifest) = reuse {
        command.arg("-FoundationManifest").arg(manifest);
    }
    let result = command.output().unwrap();
    std::fs::write(dir.join(format!("{name}-producer.stdout")), &result.stdout).unwrap();
    std::fs::write(dir.join(format!("{name}-producer.stderr")), &result.stderr).unwrap();
    assert_eq!(
        result.status.success(),
        success,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    if !success {
        assert!(!output.with_extension("obj.json").exists());
    }
    output.with_extension("obj.json")
}
fn copied_manifest(dir: &Path, original: &Path, field: &str) -> (PathBuf, PathBuf) {
    let mut value = manifest(original);
    let record = value.pointer_mut(field).unwrap();
    let source = PathBuf::from(record["path"].as_str().unwrap());
    let copy = dir.join(source.file_name().unwrap());
    std::fs::copy(&source, &copy).unwrap();
    record["path"] = json!(copy);
    let manifest = dir.join("copied.json");
    std::fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    (manifest, copy)
}
fn child(dir: &Path, mode: &str) {
    let v1 = dir.join("artifacts/v1.obj.json");
    let v2 = dir.join("artifacts/v2.obj.json");
    let changed = dir.join("artifacts/changed.obj.json");
    let work = dir.join(mode);
    std::fs::create_dir_all(&work).unwrap();
    if mode == "std-tamper" || mode == "support-tamper" {
        let field = if mode == "std-tamper" {
            "/foundation/std_dll"
        } else {
            "/foundation/support_dll"
        };
        let (copied, dll) = copied_manifest(&work, &v1, field);
        let artifact = trusted(&copied);
        // Change only a test-owned copy after provenance was accepted.
        let mut bytes = std::fs::read(&dll).unwrap();
        let end = bytes.len() - 1;
        bytes[end] ^= 0x01;
        std::fs::write(&dll, bytes).unwrap();
        assert!(
            matches!(LayeredEngine::open(&artifact, CoffLimits::default()), Err(JitError::Identity(e)) if e.contains("SHA-256"))
        );
        // Neither DLL was initialized when either identity failed. A subsequent
        // correct open in this same process must therefore still succeed.
        LayeredEngine::open(&trusted(&v1), CoffLimits::default()).unwrap();
        return;
    }
    if mode == "preloaded" {
        let value = manifest(&changed);
        let _raw = unsafe {
            CoffEngine::open_layered(
                value["foundation"]["std_dll"]["path"].as_str().unwrap(),
                value["foundation"]["support_dll"]["path"].as_str().unwrap(),
                CoffLimits::default(),
            )
        }
        .unwrap();
        assert!(
            matches!(LayeredEngine::open(&trusted(&v1), CoffLimits::default()), Err(JitError::Identity(e)) if e.contains("already loaded"))
        );
        return;
    }
    assert_eq!(mode, "roundtrip");
    let (snapshot_manifest, snapshot_file) = copied_manifest(&work, &v1, "/logic/artifact");
    let snapshot = trusted(&snapshot_manifest);
    std::fs::write(&snapshot_file, b"changed after snapshot").unwrap();
    std::fs::write(&snapshot_manifest, b"changed after snapshot").unwrap();
    let mut engine = LayeredEngine::open(&snapshot, CoffLimits::default()).unwrap();
    let first = engine.load(snapshot).unwrap();
    let resources = HostResources::new();
    let events = resources.register(KEY, Events::default());
    let mut router = RequestRouter::from_loaded(first, resources).unwrap();
    let mut old = router.dispatch(events.id(), b"wait").unwrap();
    assert_eq!(poll(&mut old), Poll::Pending);
    let incompatible = trusted(&changed);
    assert_ne!(engine.foundation_id(), incompatible.foundation_id());
    assert!(matches!(
        LayeredEngine::open(&incompatible, CoffLimits::default()),
        Err(JitError::Identity(_))
    ));
    assert!(matches!(
        engine.load(incompatible),
        Err(JitError::Identity(_))
    ));
    assert!(engine.last_load_metrics().is_none());
    assert_eq!(engine.load_attempts(), 2);
    assert_eq!(router.generation(), 1);
    assert!(values(&events, 1).is_empty());
    let next = trusted(&v2);
    assert_eq!(next.foundation_id(), engine.foundation_id());
    let second_engine = LayeredEngine::open(&next, CoffLimits::default()).unwrap();
    drop(second_engine); // Reuses known DLL identities, without relaxing the checks.
    let next = engine.load(next).unwrap();
    let candidate = router.prepare_loaded(next).unwrap();
    router.activate(candidate).unwrap();
    run(&router, &events, b"local", RunStatus::Done);
    assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
    drop(old);
    assert_eq!(router.release_retired_generations(), 1);
    assert_eq!(values(&events, 1), [207, 107]);
    assert_eq!(values(&events, 9), [1]);
    run(&router, &events, b"checks", RunStatus::Done);
    run(&router, &events, b"panic", RunStatus::Error);
    let mut cancelled = router.dispatch(events.id(), b"cancel").unwrap();
    assert_eq!(poll(&mut cancelled), Poll::Pending);
    drop(cancelled);
    assert!(crate::poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_ready());
    assert_eq!(values(&events, 6), [2]);
    let value = manifest(&v1);
    for field in ["std_dll", "support_dll"] {
        let dll = PathBuf::from(value["foundation"][field]["path"].as_str().unwrap());
        assert!(std::fs::OpenOptions::new().write(true).open(&dll).is_err());
        assert!(std::fs::rename(&dll, work.join(field)).is_err());
    }
    let foundation_dir =
        PathBuf::from(value["foundation"]["support_dll"]["path"].as_str().unwrap())
            .parent()
            .unwrap()
            .to_owned();
    assert!(std::fs::rename(&foundation_dir, work.join("renamed-foundation")).is_err());
    // Pinning locator directories must not prevent ordinary source edits or
    // writing new build artifacts elsewhere under the same workspace.
    std::fs::create_dir(work.join("new-artifacts")).unwrap();
    std::fs::write(work.join("new-artifacts/source.rs"), b"a new edit").unwrap();
    drop(engine);
    drop(router);
    assert_eq!(values(&events, 9), [1, 2]);
    let support = value["foundation"]["support_dll"]["path"].as_str().unwrap();
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .open(support)
            .is_err(),
        "KeepLoaded also retains identity guards"
    );
}

#[test]
#[ignore = "requires repository PowerShell 7 builder, stable Rust and cached dependencies; no LLVM"]
fn real_coff_verified_layered_identity() {
    if let Ok(mode) = std::env::var("REKNOT_IDENTITY_CHILD") {
        child(
            Path::new(&std::env::var("REKNOT_IDENTITY_FIXTURES").unwrap()),
            &mode,
        );
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../target/layered-identity-fixtures")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(dir).unwrap();
    project(&dir);
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
    let source = dir.join("logic/src/lib.rs");
    let template = std::fs::read_to_string(&source).unwrap();
    let marker = "if cfg!(version_two) { 2 } else { 1 }";
    assert_eq!(template.matches(marker).count(), 1);
    std::fs::write(&source, template.replace(marker, "1")).unwrap();
    let v1 = build(&dir, "v1", None, true);
    std::fs::write(&source, template.replace(marker, "2")).unwrap();
    let v2 = build(&dir, "v2", Some(&v1), true);
    assert_eq!(
        LayeredArtifact::read(&v1).unwrap().foundation_id(),
        LayeredArtifact::read(&v2).unwrap().foundation_id()
    );
    let support = dir.join("support/src/lib.rs");
    let mut changed = std::fs::read_to_string(&support).unwrap();
    changed
        .push_str("\n#[no_mangle] pub extern \"C\" fn changed_foundation_marker() -> u32 { 2 }\n");
    std::fs::write(support, changed).unwrap();
    build(&dir, "refused", Some(&v1), false);
    // Full build creates a new immutable foundation, leaving the old receipt and
    // its DLL/object copies readable for the still-running host.
    build(&dir, "changed", None, true);
    LayeredArtifact::read(&v1).unwrap();
    // Each case gets a fresh Windows loader namespace, independent of legacy
    // LLVM/raw COFF tests that may execute concurrently in this parent process.
    for mode in ["roundtrip", "std-tamper", "support-tamper", "preloaded"] {
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jit::tests::layered::identity::real_coff_verified_layered_identity",
                "--ignored",
                "--nocapture",
            ])
            .env("REKNOT_IDENTITY_CHILD", mode)
            .env("REKNOT_IDENTITY_FIXTURES", &dir)
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
    }
}
