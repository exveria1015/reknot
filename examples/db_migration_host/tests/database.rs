use db_migration_host::*;
use reknot::{HostResource, ServiceError};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn path() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/db-migration-tests");
    std::fs::create_dir_all(&root).unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    root.join(format!(
        "{}_{}_{}.sqlite",
        std::process::id(),
        nonce,
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
fn expected(schema: u32, value: i64) -> Snapshot {
    Snapshot {
        schema,
        value,
        scale: if schema == 1 { 1 } else { 100 },
    }
}

#[test]
fn schema_and_data_failures_and_panic_rollback_database_and_leave_memory_unchanged() {
    for (stage, panic) in [
        (Stage::AfterSchema, false),
        (Stage::AfterData, false),
        (Stage::BeforeCommit, false),
        (Stage::AfterData, true),
    ] {
        let file = path();
        let db = Database::create(&file, 12).unwrap();
        let error = db
            .migrate_to_v2(|at| {
                if at == stage {
                    if panic {
                        panic!("migration panic");
                    }
                    return Err(injected_failure());
                }
                Ok(())
            })
            .unwrap_err();
        assert!(!error.recovery_required, "{error}");
        assert_eq!(db.status().unwrap(), Status::Ready);
        assert_eq!(db.snapshot().unwrap(), expected(1, 12));
        assert_eq!(
            Database::open(&file).unwrap().snapshot().unwrap(),
            expected(1, 12)
        );
        db.migrate_to_v2(|_| Ok(())).unwrap();
        assert_eq!(db.snapshot().unwrap(), expected(2, 1200));
        db.migrate_to_v2(|_| panic!("idempotent migration should not execute SQL stages"))
            .unwrap();
        assert_eq!(db.snapshot().unwrap(), expected(2, 1200));
    }
}

#[test]
fn another_writer_prevents_begin_and_another_reader_can_prevent_actual_commit() {
    for writer in [true, false] {
        let file = path();
        let db = Database::create(&file, 7).unwrap();
        let other = Connection::open(&file).unwrap();
        other.busy_timeout(Duration::ZERO).unwrap();
        if writer {
            other.execute_batch("BEGIN IMMEDIATE;").unwrap();
        } else {
            other
                .execute_batch("BEGIN; SELECT value FROM ledger WHERE id=1;")
                .unwrap();
        }
        let mut reached_commit = false;
        let error = db
            .migrate_to_v2(|stage| {
                reached_commit |= stage == Stage::BeforeCommit;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.service, ServiceError::BUSY, "{error}");
        assert_eq!(reached_commit, !writer); // distinguish COMMIT failure from BEGIN failure
        assert!(!error.recovery_required, "{error}");
        other.execute_batch("ROLLBACK").unwrap();
        assert_eq!(db.snapshot().unwrap(), expected(1, 7));
        db.migrate_to_v2(|_| Ok(())).unwrap();
        assert_eq!(
            Database::open(&file).unwrap().snapshot().unwrap(),
            expected(2, 700)
        );
    }
}

#[test]
fn resource_access_on_another_thread_is_refused_while_db_and_memory_change() {
    let file = path();
    let db = Database::create(&file, 5).unwrap();
    let (go, ready) = std::sync::mpsc::channel();
    let (report, received) = std::sync::mpsc::channel();
    let mut peer = db.service(1);
    let observer = std::thread::spawn(move || {
        ready.recv().unwrap();
        report.send(peer.call(0, &[], &mut [0; 8])).unwrap();
    });
    db.migrate_to_v2(|stage| {
        if stage == Stage::AfterSchema {
            go.send(()).unwrap();
            assert_eq!(
                received.recv_timeout(Duration::from_secs(2)).unwrap(),
                Err(ServiceError::BUSY)
            );
        }
        Ok(())
    })
    .unwrap();
    observer.join().unwrap();
    let mut old = db.service(1);
    let mut new = db.service(2);
    assert_eq!(
        old.call(1, &1i64.to_le_bytes(), &mut [0; 8]),
        Err(ServiceError::CONTRACT_MISMATCH)
    );
    let mut output = [0; 8];
    new.call(1, &100i64.to_le_bytes(), &mut output).unwrap();
    assert_eq!(i64::from_le_bytes(output), 600);
}

#[test]
fn unrecognized_or_divergent_durable_state_stays_fenced_until_verified_recovery() {
    let file = path();
    let db = Database::create(&file, 4).unwrap();
    let other = Connection::open(&file).unwrap();
    other.execute_batch("PRAGMA user_version=99;").unwrap();
    assert!(db.snapshot().unwrap_err().recovery_required);
    assert_eq!(db.status().unwrap(), Status::RecoveryRequired);
    let error = db.recover().unwrap_err();
    assert!(error.recovery_required);
    assert_eq!(error.service, ServiceError::CLOSED);
    assert_eq!(
        db.service(1).call(0, &[], &mut [0; 8]),
        Err(ServiceError::CLOSED)
    );
    other
        .execute_batch("PRAGMA user_version=1; UPDATE ledger SET value=8 WHERE id=1;")
        .unwrap();
    assert_eq!(
        db.service(1).call(0, &[], &mut [0; 8]),
        Err(ServiceError::CLOSED)
    ); // no blind reopen
    assert_eq!(db.recover().unwrap(), expected(1, 8));
    assert_eq!(db.snapshot().unwrap(), expected(1, 8));
}

#[test]
fn conversion_overflow_and_small_reply_buffer_cannot_commit_partial_changes() {
    let file = path();
    let db = Database::create(&file, i64::MAX).unwrap();
    assert!(db.migrate_to_v2(|_| Ok(())).is_err());
    assert_eq!(db.snapshot().unwrap(), expected(1, i64::MAX));
    assert_eq!(
        db.service(1).call(1, &1i64.to_le_bytes(), &mut [0; 1]),
        Err(ServiceError::BUFFER_TOO_SMALL)
    );
    assert_eq!(db.snapshot().unwrap(), expected(1, i64::MAX));
}
