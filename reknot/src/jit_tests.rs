use super::*;
use crate::{HostResource, HostResources, RequestRouter, RunStatus, ServiceError, ServiceKey};
use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::task::{Context, Poll, Waker};

fn build(dir: &Path, name: &str, flags: &[&str]) -> PathBuf {
    let output = dir.join(format!("{name}.obj"));
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jit_handler.rs");
    let mut command = Command::new("rustc");
    command.args([
        "--crate-name",
        "jit_fixture",
        "--crate-type",
        "cdylib",
        "--edition=2021",
        "--emit=obj",
        "-C",
        "opt-level=0",
        "-C",
        "debuginfo=2",
        "-C",
        "panic=abort",
        "-C",
        "force-unwind-tables=yes",
        "-C",
        "debug-assertions=no",
        "-C",
        "overflow-checks=no",
    ]);
    for flag in flags {
        command.args(["--cfg", flag]);
    }
    let result = command.arg(source).arg("-o").arg(&output).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    output
}
struct Service(Rc<RefCell<Vec<u64>>>);
impl HostResource for Service {
    fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0
            .borrow_mut()
            .push(u64::from_le_bytes(input.try_into().unwrap()));
        Ok(0)
    }
}
fn expected(mut value: u64, version: u64) -> u64 {
    for i in 0..64 {
        value = value
            .wrapping_mul(6364136223846793005)
            .wrapping_add(i + version)
            .rotate_left(7);
    }
    value
}
fn poll(task: &mut crate::RequestTask) -> Poll<RunStatus> {
    Pin::new(task).poll(&mut Context::from_waker(Waker::noop()))
}
unsafe fn check_stack(engine: &JitEngine) {
    type Lookup =
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut u64, *mut c_char, usize) -> i32;
    let lookup = engine
        .resident
        ._bridge
        .get::<Lookup>(b"reknot_jit_test_symbol\0")
        .unwrap();
    let verify = engine
        .resident
        ._bridge
        .get::<unsafe extern "C" fn(u64) -> u32>(b"reknot_jit_test_stack\0")
        .unwrap();
    let mut entry = 0;
    let mut error = [0; 4096];
    assert_eq!(
        lookup(
            engine.engine.as_ptr(),
            c"bench".as_ptr(),
            &mut entry,
            error.as_mut_ptr(),
            error.len()
        ),
        1,
        "{}",
        message(&error)
    );
    assert!(
        verify(entry) > 0,
        "Windows stack walker did not traverse this generation"
    );
}

unsafe fn runtime_symbol(engine: &JitEngine, name: &CStr) -> u64 {
    type Lookup =
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut u64, *mut c_char, usize) -> i32;
    let lookup = engine
        .resident
        ._bridge
        .get::<Lookup>(b"reknot_jit_test_symbol\0")
        .unwrap();
    let mut entry = 0;
    let mut error = [0; 4096];
    assert_eq!(
        lookup(
            engine.engine.as_ptr(),
            name.as_ptr(),
            &mut entry,
            error.as_mut_ptr(),
            error.len()
        ),
        1,
        "{}",
        message(&error)
    );
    assert_ne!(entry, 0);
    entry
}

fn build_std(dir: &Path, name: &str, flags: &[&str], emulated: bool) -> PathBuf {
    let output = dir.join(format!("{name}.obj"));
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jit_std_handler.rs");
    let toolchain = std::env::var("REKNOT_JIT_TOOLCHAIN").unwrap_or_else(|_| "nightly".into());
    let mut command = Command::new("rustc");
    command.arg(format!("+{toolchain}")).args([
        "--crate-name",
        "jit_std_fixture",
        "--crate-type",
        "cdylib",
        "--edition=2021",
        "--emit=obj",
        "-C",
        "opt-level=1",
        "-C",
        "debuginfo=2",
        "-C",
        "prefer-dynamic=yes",
        "-C",
        "panic=unwind",
        "-C",
        "force-unwind-tables=yes",
    ]);
    if emulated {
        command.args(["-Z", "tls-model=emulated"]);
    }
    for flag in flags {
        command.args(["--cfg", flag]);
    }
    let result = command.arg(source).arg("-o").arg(&output).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    output
}

fn std_runtime() -> PathBuf {
    let toolchain = std::env::var("REKNOT_JIT_TOOLCHAIN").unwrap_or_else(|_| "nightly".into());
    compiler_runtime(&toolchain)
}

fn compiler_runtime(toolchain: &str) -> PathBuf {
    let result = Command::new("rustc")
        .arg(format!("+{toolchain}"))
        .args(["--print", "target-libdir"])
        .output()
        .unwrap();
    assert!(result.status.success());
    let dir = String::from_utf8(result.stdout).unwrap();
    let libraries: Vec<_> = std::fs::read_dir(dir.trim())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name().unwrap().to_string_lossy().starts_with("std-")
                && p.extension().is_some_and(|ext| ext == "dll")
        })
        .collect();
    assert_eq!(
        libraries.len(),
        1,
        "expected exactly one compiler-matched std DLL"
    );
    libraries.into_iter().next().unwrap()
}

#[path = "jit_layered_tests.rs"]
mod layered;

#[test]
#[ignore = "requires LLVM bridge with -Testing, REKNOT_JIT_BRIDGE and nightly Rust"]
fn real_coff_std_tls_panic_and_generation_isolation() {
    let bridge = std::env::var_os("REKNOT_JIT_BRIDGE").expect("set REKNOT_JIT_BRIDGE");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../target/jit-std-fixtures")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let v1 = build_std(&dir, "v1", &[], true);
    let v2 = build_std(&dir, "v2", &["version_two"], true);
    let native_tls = build_std(&dir, "native-tls", &[], false);
    type Call = unsafe extern "C" fn(u64) -> u64;
    type Stat = unsafe extern "C" fn(u32) -> u64;
    type LastError = unsafe extern "C" fn() -> u64;
    // A thread already exists when the runtime and generations are loaded.
    let (send, recv) = std::sync::mpsc::channel::<(Call, Call, LastError)>();
    let (ready_send, ready_recv) = std::sync::mpsc::channel();
    let (release_send, release_recv) = std::sync::mpsc::channel();
    let existing = std::thread::spawn(move || {
        let (old, new, last_error) = recv.recv().unwrap();
        unsafe {
            assert_eq!(last_error(), 1_234_554_321);
            assert_eq!(old(1), 107);
            assert_eq!(new(1), 207);
            assert_eq!(old(1), 108);
            assert_eq!(new(1), 208);
        }
        ready_send.send(()).unwrap();
        release_recv.recv().unwrap();
    });
    let mut engine =
        unsafe { JitEngine::open_with_runtime(&bridge, std_runtime(), JitLimits::default()) }
            .unwrap();
    let first = unsafe { engine.load(&v1) }.unwrap();
    let last_error: LastError =
        unsafe { std::mem::transmute(runtime_symbol(&engine, c"jit_runtime_last_error") as usize) };
    assert!(engine.last_load_metrics().unwrap().unwind_entries > 100);
    let (old_touch, old_probe, old_workers, old_stat): (Call, Call, Call, Stat) = unsafe {
        let quiet: unsafe extern "C" fn() =
            std::mem::transmute(runtime_symbol(&engine, c"jit_runtime_quiet") as usize);
        quiet();
        (
            std::mem::transmute::<usize, Call>(
                runtime_symbol(&engine, c"jit_runtime_touch") as usize
            ),
            std::mem::transmute::<usize, Call>(
                runtime_symbol(&engine, c"jit_runtime_probe") as usize
            ),
            std::mem::transmute::<usize, Call>(
                runtime_symbol(&engine, c"jit_runtime_workers") as usize
            ),
            std::mem::transmute::<usize, Stat>(
                runtime_symbol(&engine, c"jit_runtime_stat") as usize
            ),
        )
    };
    let replies = Rc::new(RefCell::new(Vec::new()));
    let resources = HostResources::new();
    let resource = resources.register(ServiceKey::new(0x54494d45, 1), Service(replies.clone()));
    let mut router = RequestRouter::from_loaded(first, resources).unwrap();
    let mut old = router
        .dispatch(resource.id(), &31u64.to_le_bytes())
        .unwrap();
    assert_eq!(poll(&mut old), Poll::Pending);
    assert!(
        matches!(unsafe { engine.load(&native_tls) }, Err(JitError::Link(e)) if e.contains("native TLS"))
    );
    let second = unsafe { engine.load(&v2) }.unwrap();
    let new_touch: Call =
        unsafe { std::mem::transmute(runtime_symbol(&engine, c"jit_runtime_touch") as usize) };
    let new_stat: Stat =
        unsafe { std::mem::transmute(runtime_symbol(&engine, c"jit_runtime_stat") as usize) };
    let candidate = router.prepare_loaded(second).unwrap();
    router.activate(candidate).unwrap();
    send.send((old_touch, new_touch, last_error)).unwrap();
    ready_recv.recv().unwrap();
    engine.collect_finished_threads();
    let retained_bytes = unsafe {
        engine
            .resident
            ._bridge
            .get::<unsafe extern "C" fn() -> usize>(b"reknot_jit_test_tls_bytes\0")
            .unwrap()
    };
    assert!(unsafe { retained_bytes() } > 0);
    // A live thread may be suspended with initialized old/new TLS. Collection
    // must leave both its data and its not-yet-run destructors intact.
    unsafe {
        assert_eq!(old_stat(0), 0);
        assert_eq!(new_stat(0), 0);
    }
    release_send.send(()).unwrap();
    existing.join().unwrap();
    unsafe {
        assert_eq!(old_stat(0), 1);
        assert_eq!(old_stat(1), 1);
        assert_eq!(new_stat(0), 1);
        assert_eq!(new_stat(1), 1);
        assert_eq!(old_probe(5), 6);
        assert_eq!(old_stat(2), 2);
        assert_eq!(old_stat(3), 1);
        assert_eq!(old_workers(12), 12);
        assert_eq!(old_stat(0), 13);
        assert_eq!(old_stat(1), 13);
        assert_eq!(old_stat(2), 26);
        assert_eq!(old_stat(3), 13);
    }
    // Exercise reclamation concurrently with thread creation, TLS access and
    // exit. Each OS thread handle remains unique even when IDs are recycled.
    let workers: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(move || unsafe {
                assert_eq!(old_workers(8), 8);
            })
        })
        .collect();
    while workers.iter().any(|worker| !worker.is_finished()) {
        engine.collect_finished_threads();
        std::thread::yield_now();
    }
    for worker in workers {
        worker.join().unwrap();
    }
    unsafe {
        assert_eq!(old_stat(0), 45);
        assert_eq!(old_stat(1), 45);
    }
    // The std runtime may have collected finished threads during worker startup.
    engine.collect_finished_threads();
    let retained = unsafe {
        engine
            .resident
            ._bridge
            .get::<unsafe extern "C" fn() -> usize>(b"reknot_jit_test_tls_threads\0")
            .unwrap()
    };
    assert_eq!(unsafe { retained() }, 0);
    assert_eq!(unsafe { retained_bytes() }, 0);
    let mut new = router
        .dispatch(resource.id(), &31u64.to_le_bytes())
        .unwrap();
    assert_eq!(poll(&mut new), Poll::Pending);
    drop(router);
    drop(engine);
    // Global std callbacks, old/new futures and TLS destructors still point at
    // their own retained code after both controlling owners have been dropped.
    assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
    assert_eq!(poll(&mut new), Poll::Ready(RunStatus::Done));
    assert_eq!(*replies.borrow(), [32, 33]);
    std::thread::spawn(move || unsafe {
        assert_eq!(old_touch(9), 107);
    })
    .join()
    .unwrap();
    unsafe {
        assert_eq!(old_stat(0), 46);
        assert_eq!(old_stat(1), 46);
    }
    let mut bare = unsafe { JitEngine::open(&bridge, JitLimits::default()) }.unwrap();
    // A no-runtime engine must not acquire runtime symbols from earlier images.
    assert!(matches!(unsafe { bare.load(&v1) }, Err(JitError::Link(_))));
    bare.collect_finished_threads();
}

#[test]
#[ignore = "requires LLVM bridge built with -Testing and REKNOT_JIT_BRIDGE"]
fn real_coff_owned_cutover_retention_limits_and_rejection() {
    let bridge = std::env::var_os("REKNOT_JIT_BRIDGE").expect("set REKNOT_JIT_BRIDGE");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../target/jit-fixtures")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let v1 = build(&dir, "v1", &[]);
    let v2 = build(&dir, "v2", &["version_two"]);
    let bad = build(&dir, "bad", &["bad_abi"]);
    let tls = build(&dir, "tls", &["unsupported_tls"]);
    let missing = build(&dir, "missing", &["unresolved_import"]);
    let mut engine = unsafe {
        JitEngine::open(
            &bridge,
            JitLimits {
                max_generations: 7,
                ..JitLimits::default()
            },
        )
    }
    .unwrap();
    let first = unsafe { engine.load(&v1) }.unwrap();
    assert!(engine.last_load_metrics().unwrap().unwind_entries > 0);
    unsafe {
        check_stack(&engine);
    }
    let replies = Rc::new(RefCell::new(Vec::new()));
    let resources = HostResources::new();
    let resource = resources.register(ServiceKey::new(0x54494d45, 1), Service(replies.clone()));
    let mut router = RequestRouter::from_loaded(first, resources).unwrap();
    let seed = 123u64;
    let mut old = router.dispatch(resource.id(), &seed.to_le_bytes()).unwrap();
    assert_eq!(poll(&mut old), Poll::Pending);
    assert!(matches!(
        unsafe { engine.load(&bad) },
        Err(JitError::Handler(RouteError::Abi { .. }))
    ));
    assert!(engine.last_load_metrics().is_none());
    assert!(matches!(
        unsafe { engine.load(&tls) },
        Err(JitError::Link(_))
    ));
    assert!(matches!(
        unsafe { engine.load(&missing) },
        Err(JitError::Link(_))
    ));
    let abandoned = unsafe { engine.load(&v2) }.unwrap();
    drop(abandoned);
    assert_eq!(router.generation(), 1);
    let second = unsafe { engine.load(&v2) }.unwrap();
    unsafe {
        check_stack(&engine);
    }
    let prepared = router.prepare_loaded(second).unwrap();
    assert_eq!(router.activate(prepared).unwrap(), 2);
    // A forgotten loaded owner consumes its slot and cannot invalidate code.
    std::mem::forget(unsafe { engine.load(&v1) }.unwrap());
    assert_eq!(engine.load_attempts(), 7);
    assert!(matches!(
        unsafe { engine.load(&v1) },
        Err(JitError::GenerationLimit { limit: 7 })
    ));
    drop(engine);
    let mut new = router.dispatch(resource.id(), &seed.to_le_bytes()).unwrap();
    assert_eq!(poll(&mut new), Poll::Pending);
    drop(router);
    assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
    assert_eq!(poll(&mut new), Poll::Ready(RunStatus::Done));
    assert_eq!(*replies.borrow(), [expected(seed, 1), expected(seed, 3)]);

    let mut limited = unsafe {
        JitEngine::open(
            &bridge,
            JitLimits {
                max_generations: 1,
                max_object_bytes: 8,
            },
        )
    }
    .unwrap();
    assert!(matches!(
        unsafe { limited.load(&v1) },
        Err(JitError::ObjectTooLarge { limit: 8 })
    ));
    assert_eq!(limited.load_attempts(), 1);
    assert!(matches!(
        unsafe { limited.load(&v1) },
        Err(JitError::GenerationLimit { .. })
    ));
    assert!(matches!(
        unsafe {
            JitEngine::open(
                "does-not-exist",
                JitLimits {
                    max_generations: 0,
                    ..JitLimits::default()
                },
            )
        },
        Err(JitError::InvalidLimits)
    ));
}
