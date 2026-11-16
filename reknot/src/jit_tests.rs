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
