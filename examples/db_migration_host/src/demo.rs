use crate::{injected_failure, Database, Snapshot, Stage, Status};
use reknot::*;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::task::{Context, Poll, Waker};
use std::time::{SystemTime, UNIX_EPOCH};

const SESSION: ServiceKey = ServiceKey::new(0x4442_5345, 1);
const STATE: ServiceKey = ServiceKey::new(0x4442_5354, 1);
const DB_V1: ServiceKey = ServiceKey::new(0x4442_4d47, 1);
const DB_V2: ServiceKey = ServiceKey::new(0x4442_4d47, 2);
#[derive(Default)]
struct Log(Vec<Vec<u8>>);
impl HostResource for Log {
    fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0.push(input.to_vec());
        Ok(0)
    }
}
fn environment(database: ResourceId) -> HandlerEnvironment {
    HandlerEnvironment::default().bind(1, database)
}
fn cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut cx())
}
fn done(task: &mut RequestTask) -> RunStatus {
    for _ in 0..8 {
        if let Poll::Ready(status) = poll(task) {
            return status;
        }
    }
    panic!("unexpected pending database request")
}

fn libraries() -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = root
        .join("target/db-migration-build")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&target)?;
    let mut result = Vec::new();
    for (name, features) in [
        ("db_v1", None),
        ("db_compatible_v2", Some("version_two,compatible")),
        ("db_breaking_v2", Some("version_two")),
    ] {
        let mut command = Command::new(option_env!("CARGO").unwrap_or("cargo"));
        command
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "db_migration_logic",
                "--lib",
                "--no-default-features",
            ])
            .arg("--target-dir")
            .arg(&target);
        if let Some(features) = features {
            command.args(["--features", features]);
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
            development::library_path_in(&target, "db_migration_logic"),
            &path,
        )?;
        result.push(path);
    }
    Ok(result)
}

pub fn verify() -> Result<(), Box<dyn std::error::Error>> {
    let modules = libraries()?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/db-migration-demo")
        .join(format!("{}_{nonce}", std::process::id()));
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("application.sqlite");
    let db = Database::create(&path, 10)?;
    let host = HostResources::new();
    let log = host.register(SESSION, Log::default());
    let v1 = host.register(DB_V1, db.service(1));
    let v2 = host.register(DB_V2, db.service(2));
    // The three modules are trusted workspace code using the same current SDK.
    let mut router = unsafe {
        RequestRouter::load_with_environment(
            &modules[0],
            host.clone(),
            environment(v1.id()),
            RouterOptions::default(),
        )
    }?;

    // Application-reviewed compatibility: both code versions use the same v1 data API.
    let mut old = router.dispatch(log.id(), b"increment")?;
    assert!(poll(&mut old).is_pending());
    let compatible = unsafe { router.prepare_with_environment(&modules[1], environment(v1.id())) }?;
    assert_eq!(router.activate(compatible)?, 2);
    let mut new = router.dispatch(log.id(), b"increment")?;
    assert_eq!(done(&mut old), RunStatus::Done);
    assert_eq!(done(&mut new), RunStatus::Done);
    assert_eq!(
        db.snapshot()?,
        Snapshot {
            schema: 1,
            value: 12,
            scale: 1
        }
    );

    // Breaking units/layout require quiescence and a database transaction, not Drain.
    router.start_worker(
        WorkerId(1),
        log.id(),
        b"worker",
        Checkpoint::new(STATE, vec![0]),
    )?;
    let _ = router.poll_workers(&mut cx());
    let mut delayed = router.dispatch(log.id(), b"increment")?;
    assert!(poll(&mut delayed).is_pending());
    let candidate = unsafe { router.prepare_with_environment(&modules[2], environment(v2.id())) }?;
    let mut commit_called = false;
    let mut failure = None;
    let mut restart = router.restart_with_commit(
        candidate,
        |_, state, _| Ok(state.clone()),
        || {
            commit_called = true;
            db.migrate_to_v2(|stage| {
                if stage == Stage::AfterSchema {
                    Err(injected_failure())
                } else {
                    Ok(())
                }
            })
            .map_err(|error| {
                let service = error.service;
                failure = Some(error);
                service
            })
        },
    )?;
    assert!(poll(&mut restart).is_pending()); // ordinary root has not acknowledged stop
    assert_eq!(
        db.snapshot()?,
        Snapshot {
            schema: 1,
            value: 12,
            scale: 1
        }
    );
    assert_eq!(poll(&mut delayed), Poll::Ready(RunStatus::Done)); // no ordinary request replay/write
    assert!(matches!(
        poll(&mut restart),
        Poll::Ready(Err(RouteError::HostCommit(ServiceError::FAILED)))
    ));
    drop(restart);
    assert!(commit_called);
    assert!(!failure.unwrap().recovery_required);
    assert_eq!(router.generation(), 2);
    assert!(!router.is_accepting());
    assert_eq!(router.worker_checkpoint(WorkerId(1)).unwrap().bytes, [1]);
    assert_eq!(db.status()?, Status::Ready);
    assert_eq!(
        db.snapshot()?,
        Snapshot {
            schema: 1,
            value: 12,
            scale: 1
        }
    );
    assert_eq!(
        Database::open(&path)?.snapshot()?,
        Snapshot {
            schema: 1,
            value: 12,
            scale: 1
        }
    );

    // Retry from the last committed checkpoint; old workers and ordinary requests
    // are not executed a second time. The transaction is the final fallible step.
    let candidate = unsafe { router.prepare_with_environment(&modules[2], environment(v2.id())) }?;
    let mut restart = router.restart_with_commit(
        candidate,
        |_, state, _| {
            assert_eq!(state.bytes, [1]);
            Ok(state.clone())
        },
        || db.migrate_to_v2(|_| Ok(())).map_err(|error| error.service),
    )?;
    assert!(matches!(poll(&mut restart), Poll::Ready(Ok(3))));
    drop(restart);
    assert!(!router.is_accepting()); // failure paused admission; no implicit recovery/reopen
    assert_eq!(
        db.snapshot()?,
        Snapshot {
            schema: 2,
            value: 1200,
            scale: 100
        }
    );
    let services = host.services();
    assert_eq!(
        services.call(v1.id(), DB_V1, 1, &1i64.to_le_bytes(), &mut [0; 8]),
        Err(ServiceError::CONTRACT_MISMATCH)
    );
    assert!(host.revoke(v1.id()));
    assert_eq!(
        services.call(v1.id(), DB_V1, 0, &[], &mut [0; 8]),
        Err(ServiceError::REVOKED)
    );
    router.resume()?;
    let _ = router.poll_workers(&mut cx());
    let mut task = router.dispatch(log.id(), b"increment")?;
    assert_eq!(done(&mut task), RunStatus::Done);
    assert_eq!(
        db.snapshot()?,
        Snapshot {
            schema: 2,
            value: 1300,
            scale: 100
        }
    );
    assert_eq!(
        log.try_borrow()?.0,
        [
            b"v1:g1:11".to_vec(),
            b"v2:g2:12".to_vec(),
            b"v2:g3:1300".to_vec()
        ]
    );
    router.stop_worker(WorkerId(1));
    let _ = router.poll_workers(&mut cx());
    assert_eq!(router.worker_status(WorkerId(1)), Some(RunStatus::Done));
    assert_eq!(router.worker_checkpoint(WorkerId(1)).unwrap().bytes, [2]);
    drop(task);
    drop(old);
    drop(new);
    drop(delayed);
    drop(router);
    drop(services);
    drop(host);
    drop(log);
    drop(v1);
    drop(v2);
    drop(db);

    // Cold-start recovery reconstructs the host cache from durable metadata.
    // Selecting the matching DLL remains the application's responsibility.
    assert_eq!(
        Database::open(&path)?.snapshot()?,
        Snapshot {
            schema: 2,
            value: 1300,
            scale: 100
        }
    );
    println!("PASS: compatible Drain, breaking Restart, SQL rollback and retry, host cache publication, old-adapter rejection, checkpoint continuity and durable reopen");
    println!("Database: {}", path.canonicalize()?.display());
    Ok(())
}
