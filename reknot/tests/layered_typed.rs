#![cfg(all(
    feature = "typed",
    feature = "jit-dev",
    not(miri),
    any(
        all(windows, target_arch = "x86_64", target_env = "msvc"),
        all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
    )
))]
use reknot::jit::development::{LayeredDevOptions, LayeredDevState};
use reknot::typed::{
    BridgeOptions, DevBackend, DevModule, Error, Handler, Module, ReloadEvent, ReloadMode,
    WireFormat, WorkerState,
};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Command,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

const ECHO: Handler<String, String> = Handler::new("jit.typed.echo", 1).with_format(format());
const WORK: Handler<(), Result<(), String>> =
    Handler::new("jit.typed.worker", 1).with_format(format());
const STATE: WorkerState<u64> = WorkerState::new("jit.typed.state", 1);
const SAMPLES: Handler<Vec<f32>, Vec<f32>> =
    Handler::new("jit.typed.samples", 1).with_format(format());
const SOURCE: &str = include_str!("fixtures/jit_typed_logic.rs");
const fn format() -> WireFormat {
    #[cfg(feature = "typed-binary")]
    {
        WireFormat::MessagePack
    }
    #[cfg(not(feature = "typed-binary"))]
    {
        WireFormat::Json
    }
}
fn source() -> String {
    #[cfg(feature = "typed-binary")]
    {
        let mut source = SOURCE.to_owned();
        for name in ["echo", "worker", "samples"] {
            let declaration = format!("Handler::new(\"jit.typed.{name}\", 1)");
            source = source.replace(
                &declaration,
                &format!("{declaration}.with_format(reknot::typed::WireFormat::MessagePack)"),
            );
        }
        source
    }
    #[cfg(not(feature = "typed-binary"))]
    {
        SOURCE.into()
    }
}
fn module() -> Module {
    Module::new("jit.typed", 1)
        .with(ECHO)
        .with(WORK)
        .with(SAMPLES)
        .with_state(STATE)
}
fn cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut cx())
}
fn ready<F: Future + Unpin>(mut future: F) -> F::Output {
    for _ in 0..100 {
        if let Poll::Ready(value) = poll(&mut future) {
            return value;
        }
    }
    panic!("future did not finish");
}
fn fixture(mode: &str) -> PathBuf {
    let mut suffix = 0;
    let dir = loop {
        let path =
            std::env::temp_dir().join(format!("rk-tjit-{}-{mode}-{suffix}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => suffix += 1,
            Err(error) => panic!("cannot create fixture: {error}"),
        }
    };
    for name in ["logic", "support"] {
        std::fs::create_dir_all(dir.join(name).join("src")).unwrap();
    }
    let typed_feature = if cfg!(feature = "typed-binary") {
        "typed-binary"
    } else {
        "typed"
    };
    let sdk = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers=[\"logic\",\"support\"]\nresolver=\"2\"\n[profile.dev]\nopt-level=0\ndebug=0\ncodegen-units=1\nlto=false\npanic=\"unwind\"\n").unwrap();
    std::fs::write(dir.join("support/Cargo.toml"), format!("[package]\nname=\"typed_jit_support\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\ncrate-type=[\"dylib\"]\n[dependencies]\nreknot={{path=\"{sdk}\",default-features=false,features=[\"{typed_feature}\"]}}\n")).unwrap();
    std::fs::write(
        dir.join("support/src/lib.rs"),
        "pub use reknot::*;\npub fn foundation_version() -> u64 { 1 }\n",
    )
    .unwrap();
    std::fs::write(dir.join("logic/Cargo.toml"), "[package]\nname=\"typed_jit_logic\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\ncrate-type=[\"cdylib\"]\n[dependencies]\nreknot={package=\"typed_jit_support\",path=\"../support\"}\n").unwrap();
    edit(&dir, &source());
    let output = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    dir
}
fn edit(dir: &Path, source: &str) {
    std::fs::write(dir.join("logic/src/lib.rs"), source).unwrap();
}
fn options(dir: &Path) -> LayeredDevOptions {
    let script = if cfg!(windows) {
        "build-layered-jit.ps1"
    } else {
        "build-layered-elf.py"
    };
    let mut options = if std::env::var("REKNOT_TYPED_JIT_PRODUCER").as_deref() == Ok("script") {
        LayeredDevOptions::new(
            dir.join("Cargo.toml"),
            "typed_jit_logic",
            "typed_jit_support",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../scripts")
                .join(script),
            dir.join("cache"),
        )
    } else {
        LayeredDevOptions::builtin(
            dir.join("Cargo.toml"),
            "typed_jit_logic",
            "typed_jit_support",
            dir.join("cache"),
        )
    };
    options.offline = true;
    options.declared_inputs_complete = true;
    options.debounce = Duration::from_millis(15);
    options
}
fn wait(dev: &mut DevModule, mut expected: impl FnMut(&ReloadEvent) -> bool) -> ReloadEvent {
    let until = Instant::now() + Duration::from_secs(180);
    while Instant::now() < until {
        if let Some(event) = dev.update() {
            eprintln!("{event}");
            if expected(&event) {
                return event;
            }
            assert!(
                matches!(event, ReloadEvent::Building | ReloadEvent::Superseded),
                "unexpected {event:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timeout: {:?}", dev.layered_snapshot());
}
fn drain(mode: &str) {
    let dir = fixture(mode);
    let managed = mode == "managed";
    let mut opts = options(&dir);
    if managed {
        opts.limits.load.max_generations = 1;
    }
    let mut builder = module().builder();
    if managed {
        builder = builder.router_options(reknot::RouterOptions {
            generations: reknot::GenerationBudget::new(1).unwrap(),
            ..Default::default()
        });
    }
    let mut dev = unsafe {
        if managed {
            DevModule::start_managed(builder, opts)
        } else {
            DevModule::start(builder, opts)
        }
    }
    .unwrap();
    let echo = dev.host().handler(ECHO).unwrap();
    let samples = dev.host().handler(SAMPLES).unwrap();
    let frame: Vec<f32> = (0..4096).map(|n| n as f32 * 0.25 + 0.125).collect();
    assert_eq!(ready(samples.call(frame.clone())).unwrap(), frame);
    assert_eq!(ready(echo.call("first".into())).unwrap(), "v1:first");
    let mut old = echo.call("old".into());
    assert!(poll(&mut old).is_pending());
    let client = dev.bridge(BridgeOptions::default()).unwrap();
    let remote = client.handler(ECHO).unwrap();
    edit(&dir, &source().replace("\"v1\"", "\"v2\""));
    wait(&mut dev, |e| matches!(e, ReloadEvent::Reloaded { .. }));
    assert_eq!(ready(echo.call("new".into())).unwrap(), "v2:new");
    assert_eq!(ready(samples.call(frame.clone())).unwrap(), frame);
    if managed {
        assert!(dev.retry_layered());
        wait(&mut dev, |e| {
            matches!(e, ReloadEvent::Layered { event }
            if matches!(event.state, LayeredDevState::WaitingForRetirement { .. }))
        });
        assert_eq!(dev.layered_snapshot().unwrap().load_attempts, 2);
    }
    assert_eq!(ready(old).unwrap(), "v1:old");
    if managed {
        wait(&mut dev, |e| matches!(e, ReloadEvent::Reloaded { .. }));
    }
    let mut queued = remote.call("bridge".into());
    let mut delivered = false;
    for _ in 0..100 {
        dev.update();
        if let Poll::Ready(result) = poll(&mut queued) {
            assert_eq!(result.unwrap(), "v2:bridge");
            delivered = true;
            break;
        }
    }
    assert!(delivered, "bridge did not deliver the typed result");
    // Failed builds and contract rejection preserve the same host and handles.
    edit(
        &dir,
        "compile_error!(\"intentional typed JIT build failure\");",
    );
    wait(&mut dev, |e| matches!(e, ReloadEvent::BuildFailed { .. }));
    assert_eq!(
        ready(echo.call("after failure".into())).unwrap(),
        "v2:after failure"
    );
    edit(
        &dir,
        &source().replace(
            "Handler::new(\"jit.typed.echo\", 1)",
            "Handler::new(\"jit.typed.echo\", 2)",
        ),
    );
    let rejection = wait(&mut dev, |e| matches!(e, ReloadEvent::Rejected { .. }));
    assert!(matches!(
        rejection,
        ReloadEvent::Rejected {
            error: Error::Contract { .. }
        }
    ));
    assert_eq!(
        ready(echo.call("after rejection".into())).unwrap(),
        "v2:after rejection"
    );
    edit(&dir, &source().replace("\"v1\"", "\"v3\""));
    wait(&mut dev, |e| matches!(e, ReloadEvent::Reloaded { .. }));
    assert_eq!(
        ready(echo.call("recovered".into())).unwrap(),
        "v3:recovered"
    );
    if managed {
        dev.update();
        assert_eq!(dev.layered_snapshot().unwrap().keep_loaded_attempts, 0);
        assert_eq!(dev.layered_snapshot().unwrap().live_images.len(), 1);
    }
    std::fs::write(
        dir.join("support/src/lib.rs"),
        "pub use reknot::*;\npub fn foundation_version() -> u64 { 2 }\n",
    )
    .unwrap();
    wait(
        &mut dev,
        |e| matches!(e, ReloadEvent::Layered { event } if matches!(event.state, LayeredDevState::RestartRequired { .. })),
    );
    assert_eq!(
        ready(echo.call("foundation".into())).unwrap(),
        "v3:foundation"
    );
    dev.shutdown().unwrap();
    assert!(!dev.retry_layered());
    drop(dev);
    assert_eq!(ready(echo.call("retained".into())).unwrap(), "v3:retained");
}
fn restart(interrupt: bool) {
    let dir = fixture(if interrupt { "interrupted" } else { "restart" });
    let opts = DevBackend::from(options(&dir)).with_reload_mode(ReloadMode::Restart);
    let mut dev = unsafe { DevModule::start_managed(module().builder(), opts) }.unwrap();
    let echo = dev.host().handler(ECHO).unwrap();
    let client = dev.bridge(BridgeOptions::default()).unwrap();
    let remote = client.handler(ECHO).unwrap();
    let worker = reknot::WorkerId(7);
    dev.host()
        .start_worker(worker, WORK, (), STATE, &0)
        .unwrap();
    dev.update();
    assert_eq!(
        dev.host().worker_checkpoint(worker, STATE).unwrap(),
        Some(1)
    );
    let mut call = echo.call("cancel".into());
    assert!(poll(&mut call).is_pending());
    edit(&dir, &source().replace("\"v1\"", "\"v2\""));
    wait(&mut dev, |e| matches!(e, ReloadEvent::Restarting));
    assert!(!dev.host().is_accepting());
    if interrupt {
        dev.shutdown().unwrap();
        assert_eq!(ready(&mut call).unwrap(), "v1:cancel");
        for _ in 0..10 {
            dev.update();
        }
        assert_eq!(dev.host().generation(), 1);
        assert!(!dev.host().is_accepting());
        dev.host().remove_worker(worker).unwrap();
        dev.host().resume().unwrap();
        assert_eq!(ready(echo.call("resumed".into())).unwrap(), "v1:resumed");
        return;
    }
    let mut queued = remote.call("queued".into());
    dev.update();
    assert!(poll(&mut queued).is_pending());
    // Cooperative Restart notifies the old call; it can finish normally on the
    // old code. Only an explicit Call::cancel promises Error::Cancelled.
    assert_eq!(ready(&mut call).unwrap(), "v1:cancel");
    wait(&mut dev, |e| matches!(e, ReloadEvent::Reloaded { .. }));
    assert!(dev.host().is_accepting());
    dev.update();
    assert_eq!(
        dev.host().worker_checkpoint(worker, STATE).unwrap(),
        Some(3)
    );
    assert_eq!(ready(echo.call("restart".into())).unwrap(), "v2:restart");
    let mut delivered = false;
    for _ in 0..100 {
        dev.update();
        if let Poll::Ready(result) = poll(&mut queued) {
            assert_eq!(result.unwrap(), "v2:queued");
            delivered = true;
            break;
        }
    }
    assert!(delivered, "bridge admission did not resume after Restart");
    dev.host().remove_worker(worker).unwrap();
    for _ in 0..10 {
        dev.update();
    }
    dev.shutdown().unwrap();
}
fn initial_mismatch() {
    let dir = fixture("initial-mismatch");
    edit(
        &dir,
        &source().replace(
            "Handler::new(\"jit.typed.echo\", 1)",
            "Handler::new(\"jit.typed.echo\", 2)",
        ),
    );
    let result = unsafe { DevModule::start(module().builder(), options(&dir)) };
    assert!(matches!(result, Err(Error::Contract { .. })));
}

fn prepared_recovery() {
    let dir = fixture("prepared-recovery");
    let support = dir.join("support/Cargo.toml");
    let text = std::fs::read_to_string(&support).unwrap();
    std::fs::write(
        &support,
        format!("{text}\n[features]\ndefault=['default_flag']\ndefault_flag=[]\nselected=[]\n"),
    )
    .unwrap();
    let logic = dir.join("logic/Cargo.toml");
    let text = std::fs::read_to_string(&logic).unwrap().replace(
        "path=\"../support\"}",
        "path=\"../support\",default-features=false}",
    );
    std::fs::write(
        &logic,
        format!(
            "{text}\n[features]\ndefault=['reknot/default_flag']\nselected=['reknot/selected']\n"
        ),
    )
    .unwrap();
    std::fs::create_dir(dir.join(".cargo")).unwrap();
    std::fs::write(
        dir.join(".cargo/config.toml"),
        "[build]\nbuild-dir='{workspace-root}/intermediate-output'\n",
    )
    .unwrap();
    let mut opts = options(&dir);
    opts.features = vec!["selected".into()];
    opts.no_default_features = true;
    let support_opt = if opts.builder.as_os_str().is_empty() {
        "1"
    } else {
        "0"
    };
    let mut dev = unsafe { DevModule::start(module().builder(), opts) }.unwrap();
    let echo = dev.host().handler(ECHO).unwrap();
    let manifest = dev.layered_snapshot().unwrap().manifest.unwrap();
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(
        record["foundation"]["support_build"]["features"],
        serde_json::json!(["selected"])
    );
    assert_eq!(
        record["foundation"]["support_build"]["opt_level"],
        support_opt
    );
    assert_eq!(record["logic"]["opt_level"], 0);
    assert!(dir.join("intermediate-output").is_dir());
    assert!(!dir.join("target").exists());

    // A failed reuse must release its lock, prepare again, and still activate a
    // valid edit under the same non-default features and output configuration.
    record["foundation"]["cargo_lock_sha256"] = "0".repeat(64).into();
    std::fs::write(&manifest, serde_json::to_vec(&record).unwrap()).unwrap();
    edit(&dir, &source().replace("\"v1\"", "\"v2\""));
    wait(&mut dev, |e| matches!(e, ReloadEvent::Reloaded { .. }));
    assert!(dev
        .layered_snapshot()
        .unwrap()
        .manifest
        .unwrap()
        .components()
        .any(|c| c.as_os_str() == "recheck"));
    assert_eq!(ready(echo.call("retry".into())).unwrap(), "v2:retry");

    // Writes under Cargo's custom intermediate directory must not schedule edits.
    std::fs::write(dir.join("intermediate-output/ignored.rs"), b"ignored").unwrap();
    let idle = Instant::now();
    while idle.elapsed() < Duration::from_millis(150) {
        assert!(dev.update().is_none());
        std::thread::sleep(Duration::from_millis(5));
    }

    // A new dependency is outside the initial recursive watches. Compilation
    // fails, but the newly discovered watch must survive that failure.
    std::fs::create_dir_all(dir.join("extra/src")).unwrap();
    std::fs::write(
        dir.join("extra/Cargo.toml"),
        "[package]\nname='extra'\nversion='0.0.0'\nedition='2021'\n",
    )
    .unwrap();
    let extra = dir.join("extra/src/lib.rs");
    std::fs::write(&extra, "compile_error!(\"broken new dependency\");").unwrap();
    let text = std::fs::read_to_string(&logic)
        .unwrap()
        .replace("[dependencies]", "[dependencies]\nextra={path='../extra'}");
    std::fs::write(logic, text).unwrap();
    let result = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let failure = wait(&mut dev, |e| matches!(e, ReloadEvent::BuildFailed { .. }));
    assert!(failure.to_string().contains("broken new dependency"));
    assert_eq!(ready(echo.call("failed".into())).unwrap(), "v2:failed");
    let idle = Instant::now();
    while idle.elapsed() < Duration::from_millis(150) {
        assert!(dev.update().is_none());
        std::thread::sleep(Duration::from_millis(5));
    }
    std::fs::write(extra, "pub fn value() -> u32 { 1 }").unwrap();
    // The changed lockfile changes the foundation identity; recovery must build
    // automatically but keep the old generation and require a restart.
    wait(
        &mut dev,
        |e| matches!(e, ReloadEvent::Layered { event } if matches!(event.state, LayeredDevState::RestartRequired { .. })),
    );
    assert_eq!(
        ready(echo.call("recovered".into())).unwrap(),
        "v2:recovered"
    );
    dev.shutdown().unwrap();
}

fn superseded_builds() {
    let dir = fixture("superseded");
    let mut dev = unsafe { DevModule::start(module().builder(), options(&dir)) }.unwrap();
    let echo = dev.host().handler(ECHO).unwrap();
    let before = dev.host().generation();
    let attempts = dev.layered_snapshot().unwrap().load_attempts;
    edit(&dir, &source().replace("\"v1\"", "\"obsolete\""));
    wait(&mut dev, |e| matches!(e, ReloadEvent::Building));
    edit(&dir, &source().replace("\"v1\"", "\"latest\""));
    wait(&mut dev, |e| matches!(e, ReloadEvent::Superseded));
    assert_eq!(dev.host().generation(), before);
    assert_eq!(ready(echo.call("during".into())).unwrap(), "v1:during");
    wait(&mut dev, |e| matches!(e, ReloadEvent::Reloaded { .. }));
    assert_eq!(ready(echo.call("after".into())).unwrap(), "latest:after");
    assert_eq!(dev.layered_snapshot().unwrap().load_attempts, attempts + 1);
    edit(&dir, "compile_error!(\"current compilation failed\");");
    wait(&mut dev, |e| matches!(e, ReloadEvent::BuildFailed { .. }));
    assert_eq!(ready(echo.call("failed".into())).unwrap(), "latest:failed");
    edit(&dir, &source().replace("\"v1\"", "\"recovered\""));
    wait(&mut dev, |e| matches!(e, ReloadEvent::Reloaded { .. }));
    assert_eq!(ready(echo.call("fixed".into())).unwrap(), "recovered:fixed");
    dev.shutdown().unwrap();
}

fn producer() {
    use reknot::jit::{
        builder::{build, BuildOptions},
        LayeredArtifact,
    };
    let dir = fixture("producer");
    let mut opts = BuildOptions::new(
        dir.join("Cargo.toml"),
        "typed_jit_logic",
        "typed_jit_support",
        dir.join("cache"),
        dir.join("first.object"),
    );
    opts.offline = true;
    let first = build(&opts).unwrap();
    LayeredArtifact::read(&first.manifest).unwrap();
    let first_profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&first.manifest).unwrap()).unwrap();
    assert_eq!(
        first_profile["foundation"]["support_build"]["opt_level"],
        "1"
    );
    assert_eq!(first_profile["logic"]["opt_level"], 0);
    assert!(!first.reused);
    // Reusing a result path must leave its bytes intact.
    let original = std::fs::read(&opts.output).unwrap();
    assert!(build(&opts).is_err());
    assert_eq!(std::fs::read(&opts.output).unwrap(), original);
    opts.output = dir.join("second.object");
    opts.foundation_manifest = Some(first.manifest.clone());
    let second = build(&opts).unwrap();
    assert!(second.reused);
    LayeredArtifact::read(&second.manifest).unwrap();
    // Profile changes cannot reuse the previous resident foundation silently.
    opts.support_optimization = Some(0);
    opts.output = dir.join("wrong-profile.object");
    assert!(build(&opts).is_err());
    assert!(!opts.output.exists());
    opts.foundation_manifest = None;
    opts.support_optimization = None; // Fixture's Cargo dev profile is opt=0.
    opts.output = dir.join("inherited-profile.object");
    let inherited = build(&opts).unwrap();
    let profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&inherited.manifest).unwrap()).unwrap();
    assert_eq!(profile["foundation"]["support_build"]["opt_level"], "0");
    assert_eq!(profile["logic"]["opt_level"], 0);
    opts.support_optimization = Some(1);
    // A mismatched foundation must not publish an output or silently rebuild it.
    let mut stale: serde_json::Value =
        serde_json::from_slice(&std::fs::read(first.manifest).unwrap()).unwrap();
    stale["foundation"]["cargo_lock_sha256"] = "0".repeat(64).into();
    let stale_path = dir.join("stale.json");
    std::fs::write(&stale_path, serde_json::to_vec(&stale).unwrap()).unwrap();
    opts.output = dir.join("rejected.object");
    opts.foundation_manifest = Some(stale_path);
    assert!(build(&opts).is_err());
    assert!(!opts.output.exists());
    opts.foundation_manifest = None;
    opts.max_cache_bytes = 1;
    assert!(build(&opts).unwrap_err().cache_full);
    assert!(!opts.output.exists());
}

#[test]
#[ignore = "real COFF/ELF producer, typed Drain/Restart, managed ownership and recovery"]
fn real_layered_typed_development() {
    if let Ok(mode) = std::env::var("REKNOT_TYPED_JIT_CHILD") {
        match mode.as_str() {
            "producer" => producer(),
            "restart" => restart(false),
            "interrupted" => restart(true),
            "initial-mismatch" => initial_mismatch(),
            "prepared-recovery" => prepared_recovery(),
            "superseded" => superseded_builds(),
            _ => drain(&mode),
        }
        return;
    }
    // Each child has one process-resident foundation; never reuse a process for
    // independently compiled support libraries.
    for mode in [
        "keep",
        "managed",
        "restart",
        "interrupted",
        "initial-mismatch",
        "producer",
        "prepared-recovery",
        "superseded",
    ] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "real_layered_typed_development",
                "--ignored",
                "--nocapture",
            ])
            .env("REKNOT_TYPED_JIT_CHILD", mode)
            .output()
            .unwrap();
        eprintln!("{mode}: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "typed JIT {mode} failed");
    }
}
