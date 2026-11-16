use super::*;
use crate::{
    HostResource, HostResources, RequestRouter, RequestTask, RunStatus, ServiceError, ServiceKey,
};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::task::{Context, Poll, Waker};

const KEY: ServiceKey = ServiceKey::new(0x4c41_5945, 1);
#[path = "checked_tests.rs"]
mod checked;
struct Fixtures {
    dir: PathBuf,
    runtime: PathBuf,
    support: PathBuf,
    v1: PathBuf,
    v2: PathBuf,
    tls: PathBuf,
    bad: PathBuf,
    missing: PathBuf,
}
fn run_command(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn fixtures() -> &'static Fixtures {
    static FIXTURES: std::sync::OnceLock<Fixtures> = std::sync::OnceLock::new();
    FIXTURES.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("reknot-elf-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let sdk = Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = [\"support\", \"logic\"]\nresolver = \"2\"\n[profile.dev]\nopt-level = 1\ndebug = 1\ncodegen-units = 1\nlto = false\npanic = \"unwind\"\n").unwrap();
        for (name, source) in [("support", "jit_layered_support.rs"), ("logic", "jit_layered_logic.rs")] {
            std::fs::create_dir_all(dir.join(name).join("src")).unwrap();
            std::fs::copy(sdk.join("tests/fixtures").join(source), dir.join(name).join("src/lib.rs")).unwrap();
        }
        std::fs::write(dir.join("support/Cargo.toml"), format!(
            "[package]\nname = \"jit_layered_support\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"dylib\"]\n[dependencies]\nreknot = {{ path = {:?}, default-features = false }}\nserde_json = \"1\"\n", sdk.to_str().unwrap())).unwrap();
        std::fs::write(dir.join("logic/Cargo.toml"), "[package]\nname = \"jit_layered_logic\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n[dependencies]\njit_layered_support = { path = \"../support\" }\n").unwrap();
        let build = |name: &str, cfg: Option<&str>| {
            let object = dir.join(format!("{name}.o"));
            let mut command = Command::new("cargo");
            command.current_dir(&dir).args(["rustc", "--offline", "--lib", "-p", "jit_layered_logic", "--target", "x86_64-unknown-linux-gnu"])
                .arg("--target-dir").arg(dir.join("build"))
                .env("CARGO_INCREMENTAL", "0").env_remove("CARGO_ENCODED_RUSTFLAGS")
                .env("RUSTFLAGS", "-C prefer-dynamic=yes -C relocation-model=pic -C force-unwind-tables=yes -C panic=unwind")
                .args(["--", "--check-cfg", "cfg(version_two,native_tls,external_tls)"])
                .arg(format!("--emit=obj={}", object.display()));
            if let Some(cfg) = cfg { command.args(["--cfg", cfg]); }
            run_command(&mut command);
            object
        };
        let v1 = build("v1", None);
        let v2 = build("v2", Some("version_two"));
        let tls = build("tls", Some("native_tls"));
        let build_raw = |name: &str, cfg: &str| {
            let object = dir.join(format!("{name}.o"));
            run_command(Command::new("rustc").arg(sdk.join("tests/fixtures/jit_handler.rs"))
                .args(["--crate-type=lib", "--edition=2021", "--emit=obj", "-C", "panic=abort", "-C", "force-unwind-tables=yes",
                    "-C", "relocation-model=pic", "-C", "codegen-units=1", "-C", "opt-level=1", "--cfg", cfg])
                .arg("-o").arg(&object));
            object
        };
        let bad = build_raw("bad", "bad_abi");
        let missing = build_raw("missing", "unresolved_import");
        let libdir = run_command(Command::new("rustc").args(["--print", "target-libdir"]));
        let runtimes: Vec<_> = std::fs::read_dir(libdir.trim()).unwrap().map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("libstd-") && p.extension().is_some_and(|ext| ext == "so")).collect();
        assert_eq!(runtimes.len(), 1);
        Fixtures { support: dir.join("build/x86_64-unknown-linux-gnu/debug/libjit_layered_support.so"), dir, runtime: runtimes[0].clone(), v1, v2, tls, bad, missing }
    })
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
fn values(events: &crate::Resource<Events>, op: u32) -> Vec<u64> {
    events
        .try_borrow()
        .unwrap()
        .0
        .iter()
        .filter_map(|(kind, value)| (*kind == op).then_some(*value))
        .collect()
}
fn poll(task: &mut RequestTask) -> Poll<RunStatus> {
    Pin::new(task).poll(&mut Context::from_waker(Waker::noop()))
}
fn code_is_read_execute(address: usize) {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let mapping = maps
        .lines()
        .find(|line| {
            let range = line.split_whitespace().next().unwrap();
            let (begin, end) = range.split_once('-').unwrap();
            (usize::from_str_radix(begin, 16).unwrap()..usize::from_str_radix(end, 16).unwrap())
                .contains(&address)
        })
        .expect("ELF callback must remain mapped");
    assert_eq!(mapping.split_whitespace().nth(1), Some("r-xp"), "{mapping}");
}
fn request(
    router: &RequestRouter,
    events: &crate::Resource<Events>,
    input: &[u8],
    expected: RunStatus,
) {
    let mut task = router.dispatch(events.id(), input).unwrap();
    assert_eq!(poll(&mut task), Poll::Ready(expected));
}

#[test]
#[ignore = "requires Linux GNU Rust compiler and cached SDK dependencies; no LLVM bridge"]
fn real_elf_layered_cutover_locals_panic_tls_and_residency() {
    let f = fixtures();
    let mut engine =
        unsafe { ElfEngine::open_layered(&f.runtime, &f.support, ElfLimits::default()) }.unwrap();
    let first = unsafe { engine.load(&f.v1) }.unwrap();
    let first_callback = first.api.make_request as usize;
    code_is_read_execute(first_callback);
    assert!(engine.last_load_metrics().unwrap().unwind_entries > 0);
    let resources = HostResources::new();
    let events = resources.register(KEY, Events::default());
    let mut router = RequestRouter::from_loaded(first, resources).unwrap();
    let mut old = router.dispatch(events.id(), b"wait").unwrap();
    assert_eq!(poll(&mut old), Poll::Pending);
    let retained = engine.retained_bytes();
    assert!(
        matches!(unsafe { engine.load(&f.tls) }, Err(JitError::Link(message)) if message.contains("TLS"))
    );
    assert!(
        matches!(unsafe { engine.load(&f.missing) }, Err(JitError::Link(message)) if message.contains("unresolved"))
    );
    assert_eq!(
        engine.retained_bytes(),
        retained,
        "pre-execution failures must release mappings"
    );
    assert!(matches!(
        unsafe { engine.load(&f.bad) },
        Err(JitError::Handler(crate::RouteError::Abi { .. }))
    ));
    assert!(
        engine.retained_bytes() > retained,
        "ABI entry has executed, keep its code"
    );
    let mut without_support = unsafe { ElfEngine::open(ElfLimits::default()) }.unwrap();
    assert!(
        matches!(unsafe { without_support.load(&f.v1) }, Err(JitError::Link(message)) if message.contains("unresolved"))
    );
    assert_eq!(without_support.retained_bytes(), 0);
    assert_eq!(router.generation(), 1);
    let next = unsafe { engine.load(&f.v2) }.unwrap();
    let candidate = router.prepare_loaded(next).unwrap();
    assert_eq!(router.activate(candidate).unwrap(), 2);
    let mut new = router.dispatch(events.id(), b"wait").unwrap();
    assert_eq!(poll(&mut new), Poll::Pending);
    request(&router, &events, b"local", RunStatus::Done);
    assert_eq!(values(&events, 0), [1, 1, 2]);
    assert_eq!(
        values(&events, 3),
        [1, 2, 3],
        "support TLS persists across generations"
    );
    assert_eq!(router.release_retired_generations(), 0);
    assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
    drop(old);
    assert_eq!(router.release_retired_generations(), 1);
    assert_eq!(values(&events, 9), [1]);
    assert_eq!(values(&events, 1), [207, 107]);
    request(&router, &events, b"checks", RunStatus::Done);
    request(&router, &events, b"panic", RunStatus::Error);
    request(&router, &events, b"local", RunStatus::Done);
    let mut cancelled = router.dispatch(events.id(), b"cancel").unwrap();
    assert_eq!(poll(&mut cancelled), Poll::Pending);
    drop(cancelled);
    assert!(crate::poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_ready());
    assert_eq!(values(&events, 6), [2]);
    request(&router, &events, b"fail-drop", RunStatus::Done);
    drop(engine);
    drop(router);
    code_is_read_execute(first_callback);
    assert_eq!(poll(&mut new), Poll::Ready(RunStatus::Done));
    drop(new);
    assert_eq!(values(&events, 9), [1, 2]);
    assert!(crate::poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_ready());
    assert_eq!(values(&events, 9), [1, 2]);
}

#[test]
#[ignore = "requires Linux GNU Rust compiler and cached SDK dependencies"]
fn real_elf_attempt_input_image_and_session_limits() {
    let f = fixtures();
    let mut limits = ElfLimits {
        load: JitLimits {
            max_generations: 1,
            max_object_bytes: 1,
        },
        ..Default::default()
    };
    let mut engine = unsafe { ElfEngine::open(limits) }.unwrap();
    assert!(matches!(
        unsafe { engine.load(&f.bad) },
        Err(JitError::ObjectTooLarge { .. })
    ));
    assert!(matches!(
        unsafe { engine.load(&f.bad) },
        Err(JitError::GenerationLimit { limit: 1 })
    ));
    limits.load = JitLimits::default();
    limits.max_image_bytes = 64;
    let mut engine = unsafe { ElfEngine::open(limits) }.unwrap();
    assert!(unsafe { engine.load(&f.bad) }.is_err());
    assert_eq!(engine.retained_bytes(), 0);
    limits.max_image_bytes = ElfLimits::default().max_image_bytes;
    limits.max_session_bytes = 1;
    let mut engine = unsafe { ElfEngine::open(limits) }.unwrap();
    assert!(
        matches!(unsafe { engine.load(&f.bad) }, Err(JitError::Link(message)) if message.contains("budget"))
    );
    assert_eq!(engine.retained_bytes(), 0);
    let malformed = f.dir.join("malformed.o");
    std::fs::write(&malformed, b"not ELF").unwrap();
    assert!(unsafe { engine.load(malformed) }.is_err());
}
