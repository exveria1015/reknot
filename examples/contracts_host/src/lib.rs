//! cargo run -p contracts_host --locked
use contracts_protocol::*;
use reknot::*;
use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct Log(Vec<Vec<u8>>);
impl HostResource for Log {
    fn call(&mut self, _: u32, bytes: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0.push(bytes.to_vec());
        Ok(0)
    }
}
struct Counter(Rc<RefCell<u32>>);
impl HostResource for Counter {
    fn call(&mut self, _: u32, bytes: &[u8], output: &mut [u8]) -> Result<usize, ServiceError> {
        if output.len() < 4 {
            return Err(ServiceError::BUFFER_TOO_SMALL);
        }
        let step = u32::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
        );
        *self.0.borrow_mut() += step;
        output[..4].copy_from_slice(&self.0.borrow().to_le_bytes());
        Ok(4)
    }
}
struct Format;
impl HostResource for Format {
    fn start_async(
        &mut self,
        _: u32,
        bytes: &[u8],
        _: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        let value = u32::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
        );
        Ok(Box::pin(async move { Ok(value.to_string().into_bytes()) }))
    }
}
fn environment(counter: ResourceId, formatter: ResourceId, step: u32) -> HandlerEnvironment {
    HandlerEnvironment::default()
        .bind(1, counter)
        .bind(2, formatter)
        .with_initial(InitData::from_value(&Config { step }).unwrap())
}
fn libraries() -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = root
        .join("target/contracts-build")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&target)?;
    let mut paths = Vec::new();
    for (name, version_two) in [("contracts_v1", false), ("contracts_v2", true)] {
        let mut command = Command::new(option_env!("CARGO").unwrap_or("cargo"));
        command
            .current_dir(&root)
            .args([
                "build",
                "--locked",
                "-p",
                "contracts_logic",
                "--lib",
                "--no-default-features",
            ])
            .arg("--target-dir")
            .arg(&target);
        if version_two {
            command.args(["--features", "version_two"]);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
        }
        let path = target.join(development::library_file_name(name));
        std::fs::copy(
            development::library_path_in(&target, "contracts_logic"),
            &path,
        )?;
        paths.push(path);
    }
    Ok((paths.remove(0), paths.remove(0)))
}
fn done(task: &mut RequestTask) -> RunStatus {
    for _ in 0..10 {
        if let Poll::Ready(status) =
            Pin::new(&mut *task).poll(&mut Context::from_waker(Waker::noop()))
        {
            return status;
        }
    }
    panic!("contract sample did not complete");
}
pub fn verify() -> Result<(), Box<dyn std::error::Error>> {
    let (old, new) = libraries()?;
    let host = HostResources::new();
    let log = host.register(PRIMARY, Log::default());
    let a = Rc::new(RefCell::new(0));
    let b = Rc::new(RefCell::new(0));
    let first = host.register(COUNTER, Counter(a.clone()));
    let second = host.register(COUNTER, Counter(b.clone()));
    let wrong_version = host.register(ServiceKey::new(COUNTER.id, 2), Counter(Rc::default()));
    let formatter = host.register(FORMAT, Format);
    // Both native modules are trusted workspace code built with the matching SDK.
    let mut router = unsafe {
        RequestRouter::load_with_environment(
            old,
            host.clone(),
            environment(first.id(), formatter.id(), 2),
            RouterOptions::default(),
        )
    }?;
    let missing = HandlerEnvironment::default()
        .bind(1, second.id())
        .with_initial(InitData::from_value(&Config { step: 5 })?);
    assert!(matches!(
        unsafe { router.prepare_with_environment(&new, missing) },
        Err(RouteError::MissingService { binding: 2, .. })
    ));
    assert!(matches!(
        unsafe {
            router
                .prepare_with_environment(&new, environment(wrong_version.id(), formatter.id(), 5))
        },
        Err(RouteError::ContractMismatch { .. })
    ));
    let mut wrong_schema = environment(second.id(), formatter.id(), 5);
    wrong_schema.initial.as_mut().unwrap().schema.version += 1;
    assert!(matches!(
        unsafe { router.prepare_with_environment(&new, wrong_schema) },
        Err(RouteError::InitSchemaMismatch { .. })
    ));
    let mut malformed = environment(second.id(), formatter.id(), 5);
    malformed.initial.as_mut().unwrap().bytes = b"not CFG1".to_vec();
    assert!(matches!(
        unsafe { router.prepare_with_environment(&new, malformed) },
        Err(RouteError::InitData(ServiceError::INVALID_ARGUMENT))
    ));
    assert_eq!(*a.borrow(), 0);
    assert_eq!(*b.borrow(), 0);
    assert!(log.try_borrow()?.0.is_empty());
    assert_eq!(router.generation(), 1);
    assert!(router.is_accepting());
    let mut first_task = router.dispatch(log.id(), b"old")?;
    assert!(Pin::new(&mut first_task)
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_pending());
    let candidate = unsafe {
        router.prepare_with_environment(&new, environment(second.id(), formatter.id(), 5))
    }?;
    assert_eq!(router.activate(candidate)?, 2);
    let mut second_task = router.dispatch(log.id(), b"new")?;
    drop(router);
    drop(host);
    drop(first);
    drop(second);
    drop(formatter);
    drop(wrong_version);
    assert_eq!(done(&mut first_task), RunStatus::Done);
    assert_eq!(done(&mut second_task), RunStatus::Done);
    assert_eq!(*a.borrow(), 2);
    assert_eq!(*b.borrow(), 5);
    assert_eq!(
        log.try_borrow()?.0,
        [b"v1:g1:2".to_vec(), b"v2:g2:5".to_vec()]
    );
    println!("PASS: two required services, sync/async calls, typed init rejection before execution, old/new DLL environments retained after router drop");
    Ok(())
}
