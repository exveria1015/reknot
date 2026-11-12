#![cfg(all(not(miri), feature = "typed", feature = "dev-tools"))]
use reknot::typed::{BridgeOptions, DevModule, DevOptions, Error, Handler, Module, ReloadEvent};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

const ECHO: Handler<String, String> = Handler::new("native.echo", 1);
const LENGTH: Handler<String, usize> = Handler::new("native.length", 1);
fn module() -> Module {
    Module::new("native.typed", 1).with(ECHO).with(LENGTH)
}
fn ready<T: Future + Unpin>(mut future: T) -> T::Output {
    for _ in 0..10 {
        if let Poll::Ready(output) =
            Pin::new(&mut future).poll(&mut Context::from_waker(Waker::noop()))
        {
            return output;
        }
    }
    panic!("request did not complete");
}
fn wait(dev: &mut DevModule, accept: impl Fn(&ReloadEvent) -> bool) -> ReloadEvent {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(event) = dev.update() {
            eprintln!("{event}");
            if accept(&event) {
                return event;
            }
            assert!(
                !matches!(
                    event,
                    ReloadEvent::BuildFailed { .. }
                        | ReloadEvent::Rejected { .. }
                        | ReloadEvent::WatchFailed { .. }
                ),
                "unexpected update: {event}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for source/build event"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn watch_fixture(label: &str, source: &str) -> (PathBuf, DevModule) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "typed-watch-{label}-{}-{nonce:x}",
        std::process::id()
    ));
    std::fs::create_dir_all(project.join("src")).unwrap();
    let sdk = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
    std::fs::write(
        project.join("Cargo.toml"),
        format!(
            "[package]\nname = \"typed_watch_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\ncrate-type = [\"cdylib\"]\n\
             [dependencies]\nreknot = {{ path = {}, default-features = false, features = [\"typed\"] }}\n\
             [workspace]\n",
            serde_json::to_string(&sdk).unwrap()
        ),
    )
    .unwrap();
    std::fs::write(project.join("src/lib.rs"), source).unwrap();
    let output = Command::new("cargo")
        .current_dir(&project)
        .args(["generate-lockfile", "--offline"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut options = DevOptions::new(project.join("Cargo.toml"), "typed_watch_fixture");
    options.debounce = Duration::from_millis(30);
    let dev = unsafe { DevModule::start(module().builder(), options) }.unwrap();
    (project, dev)
}

#[test]
#[ignore = "builds native DLLs while creating and replacing Cargo configuration directories"]
fn native_watch_new_and_recreated_cargo_config_keeps_reloading() {
    let source = include_str!("fixtures/typed_logic.rs").replace(
        "typed_helper::label()",
        "option_env!(\"REKNOT_NATIVE_WATCH_CONFIG\").unwrap_or(\"unset\")",
    );
    let (project, mut dev) = watch_fixture("config", &source);
    let echo = dev.host().handler(ECHO).unwrap();
    assert_eq!(ready(echo.call("x".into())).unwrap(), "unset:x");
    let directory = project.join(".cargo");
    let config = directory.join("config.toml");
    let configuration = |value: &str| {
        format!("[env]\nREKNOT_NATIVE_WATCH_CONFIG = {{ value = \"{value}\", force = true }}\n")
    };
    assert!(!directory.exists());
    std::fs::create_dir(&directory).unwrap();
    for value in ["one", "two"] {
        std::fs::write(&config, configuration(value)).unwrap();
        wait(&mut dev, |event| {
            matches!(event, ReloadEvent::Reloaded { .. })
        });
        assert_eq!(ready(echo.call("x".into())).unwrap(), format!("{value}:x"));
    }

    // Metadata failure must not prevent re-registration of the new directory.
    // Only files created by this fixture are removed, and remove_dir is empty-only.
    assert!(directory
        .canonicalize()
        .unwrap()
        .starts_with(project.canonicalize().unwrap()));
    std::fs::remove_file(&config).unwrap();
    std::fs::remove_dir(&directory).unwrap();
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(&config, "[invalid TOML").unwrap();
    wait(&mut dev, |event| {
        matches!(event, ReloadEvent::BuildFailed { .. })
    });
    assert_eq!(ready(echo.call("x".into())).unwrap(), "two:x");
    for value in ["three", "four"] {
        std::fs::write(&config, configuration(value)).unwrap();
        wait(&mut dev, |event| {
            matches!(event, ReloadEvent::Reloaded { .. })
        });
        assert_eq!(ready(echo.call("x".into())).unwrap(), format!("{value}:x"));
    }

    std::fs::create_dir_all(project.join("target/generated")).unwrap();
    std::fs::write(project.join("target/generated/output.rs"), "output").unwrap();
    std::fs::write(directory.join("unrelated.log"), "log").unwrap();
    let until = Instant::now() + Duration::from_millis(650);
    while Instant::now() < until {
        assert!(
            dev.update().is_none(),
            "non-input files retriggered the build"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "builds native DLLs while removing, recreating and moving source directories"]
fn native_watch_replaced_source_directory_keeps_reloading() {
    let source = |value: &str| {
        include_str!("fixtures/typed_logic.rs")
            .replace("typed_helper::label()", &format!("\"{value}\""))
    };
    let (project, mut dev) = watch_fixture("source", &source("initial"));
    let echo = dev.host().handler(ECHO).unwrap();
    assert_eq!(ready(echo.call("x".into())).unwrap(), "initial:x");
    let directory = project.join("src");
    let logic = directory.join("lib.rs");
    assert!(directory
        .canonicalize()
        .unwrap()
        .starts_with(project.canonicalize().unwrap()));
    std::fs::remove_file(&logic).unwrap();
    std::fs::remove_dir(&directory).unwrap();
    wait(&mut dev, |event| {
        matches!(event, ReloadEvent::BuildFailed { .. })
    });
    assert_eq!(ready(echo.call("x".into())).unwrap(), "initial:x");
    std::fs::create_dir(&directory).unwrap();
    for value in ["replacement", "later-edit"] {
        std::fs::write(&logic, source(value)).unwrap();
        wait(&mut dev, |event| {
            matches!(event, ReloadEvent::Reloaded { .. })
        });
        assert_eq!(ready(echo.call("x".into())).unwrap(), format!("{value}:x"));
    }

    // Also replace the directory without waiting for a failed build in between.
    std::fs::rename(&directory, project.join("old-src")).unwrap();
    std::fs::create_dir(&directory).unwrap();
    for value in ["moved", "later-moved-edit"] {
        std::fs::write(&logic, source(value)).unwrap();
        wait(&mut dev, |event| {
            matches!(event, ReloadEvent::Reloaded { .. })
        });
        assert_eq!(ready(echo.call("x".into())).unwrap(), format!("{value}:x"));
    }
}

#[test]
#[ignore = "builds custom-layout DLLs and reloads modules and project Cargo configuration"]
fn custom_modules_and_project_cargo_config_reload_from_an_unrelated_cwd() {
    let project = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("typed-layout-{}", std::process::id()));
    for directory in ["implementation", "helper/nested", ".cargo"] {
        std::fs::create_dir_all(project.join(directory)).unwrap();
    }
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .to_string_lossy()
        .replace('\\', "/");
    std::fs::write(
        project.join("Cargo.toml"),
        format!(
            r#"
[package]
name = "typed_layout_fixture"
version = "0.0.0"
edition = "2021"
[lib]
path = "implementation/lib.rs"
crate-type = ["cdylib"]
[dependencies]
reknot = {{ path = "{sdk}", default-features = false, features = ["typed"] }}
typed_helper = {{ path = "helper" }}
[workspace]
"#
        ),
    )
    .unwrap();
    std::fs::write(project.join("helper/Cargo.toml"), "[package]\nname = \"typed_helper\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[lib]\npath = \"lib.rs\"\n").unwrap();
    std::fs::write(
        project.join("helper/lib.rs"),
        "mod nested; pub use nested::label;\n",
    )
    .unwrap();
    let label = |value: &str| format!("pub fn label() -> &'static str {{ \"{value}\" }}\n");
    let child = project.join("implementation/value.rs");
    let helper = project.join("helper/nested/mod.rs");
    std::fs::write(&child, label("v1")).unwrap();
    std::fs::write(&helper, label("h1")).unwrap();
    let source = include_str!("fixtures/typed_logic.rs").replace(
        "typed_helper::label()",
        "format!(\"{}:{}:{}\", value::label(), typed_helper::label(), env!(\"REKNOT_LAYOUT_CONFIG\"))",
    );
    std::fs::write(
        project.join("implementation/lib.rs"),
        format!("mod value;\n{source}"),
    )
    .unwrap();
    let config = project.join(".cargo/config.toml");
    let configuration = |value: &str| {
        format!("[build]\ntarget-dir = \"build-output\"\n[env]\nREKNOT_LAYOUT_CONFIG = {{ value = \"{value}\", force = true }}\n")
    };
    std::fs::write(&config, configuration("c1")).unwrap();
    let output = Command::new("cargo")
        .current_dir(&project)
        .args(["generate-lockfile", "--offline"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        std::env::current_dir().unwrap().canonicalize().unwrap(),
        project.canonicalize().unwrap()
    );
    let mut options = DevOptions::new(project.join("Cargo.toml"), "typed_layout_fixture");
    options.debounce = Duration::from_millis(30);
    let mut dev = unsafe { DevModule::start(module().builder(), options) }.unwrap();
    let echo = dev.host().handler(ECHO).unwrap();
    assert_eq!(ready(echo.call("x".into())).unwrap(), "v1:h1:c1:x");
    assert!(project
        .join("build-output/reknot-typed/typed_layout_fixture")
        .is_dir());
    for (path, contents, expected) in [
        (&child, label("v2"), "v2:h1:c1:x"),
        (&helper, label("h2"), "v2:h2:c1:x"),
        (&config, configuration("c2"), "v2:h2:c2:x"),
    ] {
        std::fs::write(path, contents).unwrap();
        wait(&mut dev, |event| {
            matches!(event, ReloadEvent::Reloaded { .. })
        });
        assert_eq!(ready(echo.call("x".into())).unwrap(), expected);
    }
    // The root-level helper lib makes its crate a recursive watch root. Generated
    // Rust output and unrelated logs must not feed back into the build loop.
    std::fs::write(project.join("build-output/generated.rs"), "output").unwrap();
    std::fs::write(project.join("helper/notes.log"), "log").unwrap();
    std::fs::create_dir_all(project.join("helper/target")).unwrap();
    std::fs::write(project.join("helper/target/generated.rs"), "output").unwrap();
    let until = Instant::now() + Duration::from_millis(650);
    while Instant::now() < until {
        assert!(
            dev.update().is_none(),
            "non-input files retriggered the build"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "builds real SDK DLLs and exercises filesystem notifications"]
fn native_typed_save_reload_failure_recovery_and_old_generation() {
    let project = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("typed-sdk-{}", std::process::id()));
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::create_dir_all(project.join("helper/src")).unwrap();
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .to_string_lossy()
        .replace('\\', "/");
    std::fs::write(
        project.join("Cargo.toml"),
        format!(
            r#"
[package]
name = "typed_fixture"
version = "0.0.0"
edition = "2021"
[lib]
crate-type = ["cdylib"]
[dependencies]
reknot = {{ path = "{sdk}", default-features = false, features = ["typed"] }}
typed_helper = {{ path = "helper" }}
[workspace]
"#
        ),
    )
    .unwrap();
    std::fs::write(
        project.join("helper/Cargo.toml"),
        "[package]\nname = \"typed_helper\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let helper = project.join("helper/src/lib.rs");
    let label = |value: &str| format!("pub fn label() -> &'static str {{ \"{value}\" }}\n");
    std::fs::write(&helper, label("v1")).unwrap();
    let source = include_str!("fixtures/typed_logic.rs");
    let logic = project.join("src/lib.rs");
    std::fs::write(&logic, source).unwrap();
    let output = Command::new("cargo")
        .args(["generate-lockfile", "--offline", "--manifest-path"])
        .arg(project.join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut options = DevOptions::new(project.join("Cargo.toml"), "typed_fixture");
    options.debounce = Duration::from_millis(30);
    let mut dev = unsafe { DevModule::start(module().builder(), options) }.unwrap();
    let host = dev.host().clone();
    let client = dev.bridge(BridgeOptions::default()).unwrap();
    let remote = client.handler(ECHO).unwrap();
    let remote_worker = remote.clone();
    let mut waiting = std::thread::spawn(move || remote_worker.call("__wait_for_cancel".into()))
        .join()
        .unwrap();
    assert_eq!(waiting.generation(), None);
    assert!(dev.update().is_none());
    assert_eq!(waiting.generation(), Some(1));
    let echo = host.handler(ECHO).unwrap();
    let length = host.handler(LENGTH).unwrap();
    let old_image = host.artifact_id().unwrap();
    let old = echo.call("old".into());
    assert_eq!(old.generation(), Some(1));

    // A local dependency edit triggers the same module update; the handle stays put.
    std::fs::write(&helper, label("v2")).unwrap();
    wait(&mut dev, |event| {
        matches!(event, ReloadEvent::Reloaded { .. })
    });
    assert_eq!(host.generation(), 2);
    assert_eq!(ready(echo.call("new".into())).unwrap(), "v2:new");
    assert_eq!(ready(length.call("abc".into())).unwrap(), 3);
    assert_eq!(ready(old).unwrap(), "v1:old");
    assert_eq!(
        host.in_flight(),
        1,
        "old bridged request still owns its first DLL generation"
    );
    waiting.cancel();
    let mut joined = false;
    for _ in 0..10 {
        let _ = dev.update();
        if let Poll::Ready(result) =
            Pin::new(&mut waiting).poll(&mut Context::from_waker(Waker::noop()))
        {
            assert!(matches!(result, Err(Error::Cancelled)));
            joined = true;
            break;
        }
    }
    assert!(joined);
    assert_eq!(host.in_flight(), 0);
    let old_record = reknot::retention_snapshot()
        .images
        .into_iter()
        .find(|image| image.id == old_image);
    assert!(
        old_record.is_some(),
        "typed layer must not silently unload old DLLs"
    );

    // Failed builds keep the running module; fixing and saving is sufficient.
    std::fs::write(&logic, "this is not valid Rust").unwrap();
    wait(&mut dev, |event| {
        matches!(event, ReloadEvent::BuildFailed { .. })
    });
    assert_eq!(host.generation(), 2);
    assert_eq!(ready(echo.call("live".into())).unwrap(), "v2:live");
    std::fs::write(&logic, source).unwrap();
    wait(&mut dev, |event| {
        matches!(event, ReloadEvent::Reloaded { .. })
    });
    assert_eq!(host.generation(), 3);

    // Host/logic contract changes are rejected before admission changes.
    std::fs::write(
        &logic,
        source.replace("\"native.echo\", 1", "\"native.echo\", 2"),
    )
    .unwrap();
    wait(&mut dev, |event| {
        matches!(event, ReloadEvent::Rejected { .. })
    });
    assert_eq!(host.generation(), 3);
    assert_eq!(
        ready(echo.call("compatible".into())).unwrap(),
        "v2:compatible"
    );

    // A newer edit supersedes an in-progress build, without installing its result.
    std::fs::write(&logic, source).unwrap();
    wait(&mut dev, |event| matches!(event, ReloadEvent::Building));
    std::fs::write(&helper, label("v3")).unwrap();
    wait(&mut dev, |event| matches!(event, ReloadEvent::Superseded));
    assert_eq!(host.generation(), 3);
    wait(&mut dev, |event| {
        matches!(event, ReloadEvent::Reloaded { .. })
    });
    assert_eq!(host.generation(), 4);
    assert_eq!(ready(echo.call("latest".into())).unwrap(), "v3:latest");

    // Cargo output and copied DLLs must not cause a self-triggered build loop.
    let until = Instant::now() + Duration::from_millis(650);
    while Instant::now() < until {
        assert!(
            dev.update().is_none(),
            "build artifacts retriggered the source watcher"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Adding a path dependency refreshes the watch set. The build made before
    // registration is discarded, then later edits to the new crate are observed.
    std::fs::create_dir_all(project.join("second/src")).unwrap();
    std::fs::write(
        project.join("second/Cargo.toml"),
        "[package]\nname = \"typed_second\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let second = project.join("second/src/lib.rs");
    std::fs::write(&second, label("v4")).unwrap();
    let manifest_path = project.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path).unwrap();
    std::fs::write(
        &manifest_path,
        manifest.replace(
            "[dependencies]",
            "[dependencies]\ntyped_second = { path = \"second\" }",
        ),
    )
    .unwrap();
    std::fs::write(
        &logic,
        source.replace("typed_helper::label", "typed_second::label"),
    )
    .unwrap();
    let output = Command::new("cargo")
        .args(["generate-lockfile", "--offline", "--manifest-path"])
        .arg(&manifest_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait(&mut dev, |event| matches!(event, ReloadEvent::Superseded));
    wait(&mut dev, |event| {
        matches!(event, ReloadEvent::Reloaded { .. })
    });
    assert_eq!(host.generation(), 5);
    assert_eq!(
        ready(echo.call("new dependency".into())).unwrap(),
        "v4:new dependency"
    );
    std::fs::write(&second, label("v5")).unwrap();
    wait(&mut dev, |event| {
        matches!(event, ReloadEvent::Reloaded { .. })
    });
    assert_eq!(host.generation(), 6);
    assert_eq!(ready(echo.call("watched".into())).unwrap(), "v5:watched");
    let mut bridged = remote.call("remote".into());
    for _ in 0..3 {
        let _ = dev.update();
    }
    assert_eq!(bridged.generation(), Some(6));
    assert_eq!(
        Pin::new(&mut bridged)
            .poll(&mut Context::from_waker(Waker::noop()))
            .map(Result::unwrap),
        Poll::Ready("v5:remote".to_owned())
    );
    let shutdown = client.shutdown();
    let _ = dev.update();
    ready(shutdown).unwrap();
}

#[test]
#[ignore = "builds native DLLs with changed same-name schemas and restartable workers"]
fn native_structural_diagnostics_and_typed_restart_with_migration() {
    use reknot::typed::{ExportedModule, ReloadMode, WorkerState};
    use reknot::WorkerId;
    const WORK: Handler<(), Result<(), String>> = Handler::new("evolution.worker", 1);
    const ECHO: Handler<String, String> = Handler::new("evolution.echo", 1);
    const STATE: WorkerState<u64> = WorkerState::new("evolution.state", 1);
    const NEXT: WorkerState<u64> = WorkerState::new("evolution.state", 2);
    let project = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("typed-evolution-{}", std::process::id()));
    std::fs::create_dir_all(project.join("src")).unwrap();
    let sdk = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
    std::fs::write(project.join("Cargo.toml"), format!("[package]\nname = \"typed_evolution\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n[dependencies]\nreknot = {{ path = {}, default-features = false, features = [\"typed\"] }}\nserde = {{ version = \"1\", features = [\"derive\"] }}\n[workspace]\n", serde_json::to_string(&sdk).unwrap())).unwrap();
    let source = include_str!("fixtures/typed_evolution.rs");
    let logic = project.join("src/lib.rs");
    std::fs::write(&logic, source).unwrap();
    let build = || {
        let output = Command::new("cargo")
            .current_dir(&project)
            .args(["build", "--offline"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    build();
    let library = project.join("target/debug").join(format!(
        "{}typed_evolution{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    // Capture the first DLL's owned contract, exactly as an old host would keep
    // it. Rebuilding the same crate preserves Rust type names while fields change.
    let manifest = unsafe {
        let library = libloading::Library::new(&library).unwrap();
        let entry = library
            .get::<unsafe extern "C" fn() -> *const reknot::HandlerApi>(b"hot_handler_api")
            .unwrap();
        let api = reknot::HandlerApi::read_compatible(entry());
        let mut bytes = vec![0; reknot::MAX_INIT_BYTES];
        let length = api.describe.unwrap()(bytes.as_mut_ptr(), bytes.len());
        serde_json::from_slice::<ExportedModule>(&bytes[..length])
            .unwrap()
            .module
    };
    let mut options = DevOptions::new(project.join("Cargo.toml"), "typed_evolution")
        .with_reload_mode(ReloadMode::Restart);
    options.debounce = Duration::from_millis(30);
    let mut dev = unsafe { DevModule::start(manifest.builder(), options) }.unwrap();
    let host = dev.host().clone();
    host.start_worker(WorkerId(1), WORK, (), STATE, &0).unwrap();
    assert!(host
        .poll_workers(&mut Context::from_waker(Waker::noop()))
        .is_pending());
    std::fs::write(&logic, source.replace("value: u32", "value: String")).unwrap();
    let event = wait(&mut dev, |event| {
        matches!(event, ReloadEvent::Rejected { .. })
    });
    let ReloadEvent::Rejected {
        error: Error::Contract { report, .. },
    } = event
    else {
        panic!("expected structured contract error")
    };
    assert!(report
        .differences
        .iter()
        .any(|d| d.handler.as_deref() == Some("evolution.packet")
            && d.path.contains("/properties/value/type")
            && d.source.is_some()));
    assert_eq!(
        report.expected.handlers()[1].input,
        report.candidate.as_ref().unwrap().module.handlers()[1].input
    );
    assert_eq!(host.generation(), 1);
    assert!(host.is_accepting());
    assert_eq!(host.worker_checkpoint(WorkerId(1), STATE).unwrap(), Some(1));

    let second = source.replace("logic-v1", "logic-v2");
    std::fs::write(&logic, &second).unwrap();
    wait(&mut dev, |event| matches!(event, ReloadEvent::Restarting));
    wait(&mut dev, |event| {
        matches!(event, ReloadEvent::Reloaded { .. })
    });
    assert_eq!(host.generation(), 2);
    assert_eq!(host.worker_checkpoint(WorkerId(1), STATE).unwrap(), Some(2));
    let _ = dev.update();
    assert_eq!(host.worker_checkpoint(WorkerId(1), STATE).unwrap(), Some(3));
    assert_eq!(
        ready(host.handler(ECHO).unwrap().call("hello".into())).unwrap(),
        "logic-v2:hello"
    );

    std::fs::write(
        &logic,
        second.replace("\"evolution.state\", 1", "\"evolution.state\", 2"),
    )
    .unwrap();
    build();
    let restart =
        unsafe { host.restart_with(&library, NEXT.migrate(|_, old| Ok(STATE.decode(old)? + 10))) }
            .unwrap();
    assert_eq!(ready(restart).unwrap(), 3);
    assert_eq!(host.worker_checkpoint(WorkerId(1), NEXT).unwrap(), Some(14));
    host.remove_worker(WorkerId(1)).unwrap();
    for _ in 0..20 {
        if host
            .poll_cleanup(&mut Context::from_waker(Waker::noop()))
            .is_ready()
        {
            return;
        }
    }
    panic!("worker cleanup did not finish");
}
