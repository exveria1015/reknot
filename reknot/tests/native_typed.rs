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
