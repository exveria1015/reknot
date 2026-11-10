//! Application-specific SQLite migration. The generic SDK has no SQL dependency.
pub mod demo;
use reknot::{HostResource, ServiceError};
use rusqlite::{Connection, ErrorCode, TransactionBehavior};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub schema: u32,
    pub value: i64,
    pub scale: i64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryState {
    V1 { value: i64 },
    V2 { minor: i64, scale: i64 },
}
impl MemoryState {
    fn snapshot(self) -> Snapshot {
        match self {
            Self::V1 { value } => Snapshot {
                schema: 1,
                value,
                scale: 1,
            },
            Self::V2 { minor, scale } => Snapshot {
                schema: 2,
                value: minor,
                scale,
            },
        }
    }
    fn from_snapshot(snapshot: Snapshot) -> Result<Self, DbError> {
        match snapshot.schema {
            1 => Ok(Self::V1 {
                value: snapshot.value,
            }),
            2 => Ok(Self::V2 {
                minor: snapshot.value,
                scale: 100,
            }),
            _ => Err(DbError::invalid("unsupported durable schema")),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ready,
    Migrating,
    RecoveryRequired,
}
#[derive(Debug)]
pub struct DbError {
    pub service: ServiceError,
    pub detail: String,
    pub recovery_required: bool,
}
impl DbError {
    fn invalid(message: &str) -> Self {
        Self {
            service: ServiceError::FAILED,
            detail: message.into(),
            recovery_required: false,
        }
    }
    fn closed(message: &str) -> Self {
        Self {
            service: ServiceError::CLOSED,
            detail: message.into(),
            recovery_required: true,
        }
    }
}
impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (recovery_required={})",
            self.detail, self.recovery_required
        )
    }
}
impl std::error::Error for DbError {}
impl From<rusqlite::Error> for DbError {
    fn from(error: rusqlite::Error) -> Self {
        let service = match &error {
            rusqlite::Error::SqliteFailure(code, _)
                if matches!(
                    code.code,
                    ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
                ) =>
            {
                ServiceError::BUSY
            }
            _ => ServiceError::FAILED,
        };
        Self {
            service,
            detail: error.to_string(),
            recovery_required: false,
        }
    }
}
fn read_snapshot(connection: &Connection) -> Result<Snapshot, DbError> {
    let schema: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let sql = match schema {
        1 => "SELECT value FROM ledger WHERE id=1",
        2 => "SELECT value_minor FROM ledger WHERE id=1",
        _ => return Err(DbError::invalid("unsupported durable schema")),
    };
    let value: i64 = connection.query_row(sql, [], |row| row.get(0))?;
    let count: i64 = connection.query_row("SELECT COUNT(*) FROM ledger", [], |row| row.get(0))?;
    if value < 0 || count != 1 {
        return Err(DbError::invalid("invalid ledger contents"));
    }
    Ok(Snapshot {
        schema,
        value,
        scale: if schema == 2 { 100 } else { 1 },
    })
}
struct Store {
    connection: Connection,
    memory: MemoryState,
    status: Status,
}
impl Store {
    fn validate(&mut self) -> Result<Snapshot, DbError> {
        if self.status != Status::Ready {
            return Err(DbError::closed("resource is fenced"));
        }
        match read_snapshot(&self.connection) {
            Ok(snapshot) if snapshot == self.memory.snapshot() => Ok(snapshot),
            _ => {
                self.status = Status::RecoveryRequired;
                Err(DbError::closed(
                    "database and host cache cannot be confirmed consistent",
                ))
            }
        }
    }
    fn transactionally(
        &mut self,
        change: impl FnOnce(&mut Connection, Snapshot) -> Result<MemoryState, DbError>,
    ) -> Result<(), DbError> {
        let before = self.validate()?;
        self.status = Status::Migrating;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            change(&mut self.connection, before)
        }))
        .unwrap_or_else(|_| {
            Err(DbError {
                service: ServiceError::PANICKED,
                detail: "application transaction panic".into(),
                recovery_required: false,
            })
        });
        match result {
            Ok(next) => {
                self.memory = next;
                self.status = Status::Ready;
                Ok(())
            }
            Err(mut error) => {
                // Transaction::Drop attempts rollback, but a failed attempt is not
                // proof. Explicitly finish any remaining transaction and inspect
                // durable schema/data before reopening access to the old memory.
                if !self.connection.is_autocommit() {
                    let _ = self.connection.execute_batch("ROLLBACK");
                }
                let confirmed = self.connection.is_autocommit()
                    && read_snapshot(&self.connection).is_ok_and(|snapshot| snapshot == before);
                self.status = if confirmed {
                    Status::Ready
                } else {
                    Status::RecoveryRequired
                };
                error.recovery_required = !confirmed;
                if !confirmed {
                    error.service = ServiceError::CLOSED;
                    error
                        .detail
                        .push_str("; rollback outcome unconfirmed, resource remains fenced");
                }
                Err(error)
            }
        }
    }
}
/// Shared application store, not an SDK/FFI handle. Every adapter uses this same
/// Mutex; external connections are additionally serialized by SQLite transactions.
#[derive(Clone)]
pub struct Database(Arc<Mutex<Store>>);
impl Database {
    pub fn create(path: &Path, value: i64) -> Result<Self, DbError> {
        if path.exists() || value < 0 {
            return Err(DbError::invalid(
                "create requires a new path and nonnegative value",
            ));
        }
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::ZERO)?;
        connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE ledger(id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL CHECK(value>=0)); PRAGMA user_version=1;")?;
        tx.execute("INSERT INTO ledger(id,value) VALUES(1,?1)", [value])?;
        tx.commit()?;
        Ok(Self(Arc::new(Mutex::new(Store {
            connection,
            memory: MemoryState::V1 { value },
            status: Status::Ready,
        }))))
    }
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::ZERO)?;
        let memory = MemoryState::from_snapshot(read_snapshot(&connection)?)?;
        Ok(Self(Arc::new(Mutex::new(Store {
            connection,
            memory,
            status: Status::Ready,
        }))))
    }
    fn lock(&self) -> Result<MutexGuard<'_, Store>, DbError> {
        match self.0.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::WouldBlock) => Err(DbError {
                service: ServiceError::BUSY,
                detail: "resource update in progress".into(),
                recovery_required: false,
            }),
            Err(TryLockError::Poisoned(_)) => Err(DbError::closed(
                "resource mutex poisoned; explicit recovery required",
            )),
        }
    }
    pub fn snapshot(&self) -> Result<Snapshot, DbError> {
        self.lock()?.validate()
    }
    pub fn status(&self) -> Result<Status, DbError> {
        Ok(self.lock()?.status)
    }
    /// Reconstruct memory only from a recognized durable schema. It does not
    /// choose or restart logic; the application must select matching code.
    pub fn recover(&self) -> Result<Snapshot, DbError> {
        let mut store = match self.0.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(DbError {
                    service: ServiceError::BUSY,
                    detail: "cannot recover during active resource access".into(),
                    recovery_required: false,
                })
            }
        };
        store.status = Status::RecoveryRequired;
        let recovered = (|| {
            if !store.connection.is_autocommit() {
                store.connection.execute_batch("ROLLBACK")?;
            }
            let snapshot = read_snapshot(&store.connection)?;
            Ok((snapshot, MemoryState::from_snapshot(snapshot)?))
        })();
        let (snapshot, memory) = recovered.map_err(|mut error: DbError| {
            error.service = ServiceError::CLOSED;
            error.recovery_required = true;
            error
        })?;
        store.memory = memory;
        store.status = Status::Ready;
        drop(store);
        self.0.clear_poison();
        Ok(snapshot)
    }
    pub fn migrate_to_v2(
        &self,
        mut at: impl FnMut(Stage) -> Result<(), DbError>,
    ) -> Result<(), DbError> {
        let mut store = self.lock()?;
        let before = store.validate()?;
        if before.schema == 2 {
            return Ok(());
        } // verified idempotent retry; never multiply twice
        store.transactionally(|connection, before| {
            let minor = before
                .value
                .checked_mul(100)
                .ok_or_else(|| DbError::invalid("value conversion overflow"))?;
            let next = MemoryState::V2 { minor, scale: 100 }; // stage memory; no publication yet
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if read_snapshot(&tx)? != before {
                return Err(DbError::invalid("database changed before migration lock"));
            }
            tx.execute_batch("ALTER TABLE ledger RENAME COLUMN value TO value_minor;")?;
            at(Stage::AfterSchema)?;
            tx.execute("UPDATE ledger SET value_minor=?1 WHERE id=1", [minor])?;
            tx.execute_batch("PRAGMA user_version=2;")?;
            at(Stage::AfterData)?;
            if read_snapshot(&tx)? != next.snapshot() {
                return Err(DbError::invalid("staged data does not match staged memory"));
            }
            at(Stage::BeforeCommit)?;
            tx.commit()?;
            // No fallible application work between durable commit and publishing memory.
            Ok(next)
        })
    }
    pub fn service(&self, version: u32) -> DatabaseService {
        DatabaseService {
            database: self.clone(),
            version,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    AfterSchema,
    AfterData,
    BeforeCommit,
}
pub fn injected_failure() -> DbError {
    DbError::invalid("injected migration failure")
}
pub struct DatabaseService {
    database: Database,
    version: u32,
}
impl HostResource for DatabaseService {
    fn call(
        &mut self,
        operation: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, ServiceError> {
        if output.len() < 8 {
            return Err(ServiceError::BUFFER_TOO_SMALL);
        }
        let mut store = self.database.lock().map_err(|error| error.service)?;
        let snapshot = store.validate().map_err(|error| error.service)?;
        if snapshot.schema != self.version {
            return Err(ServiceError::CONTRACT_MISMATCH);
        }
        let value = match operation {
            0 => snapshot.value,
            1 => {
                let delta = i64::from_le_bytes(
                    input
                        .try_into()
                        .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
                );
                let next_value = snapshot
                    .value
                    .checked_add(delta)
                    .filter(|value| *value >= 0)
                    .ok_or(ServiceError::INVALID_ARGUMENT)?;
                store
                    .transactionally(|connection, before| {
                        let next = MemoryState::from_snapshot(Snapshot {
                            value: next_value,
                            ..before
                        })?;
                        let tx =
                            connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                        if read_snapshot(&tx)? != before {
                            return Err(DbError::invalid("database changed before write lock"));
                        }
                        let sql = if before.schema == 1 {
                            "UPDATE ledger SET value=?1 WHERE id=1"
                        } else {
                            "UPDATE ledger SET value_minor=?1 WHERE id=1"
                        };
                        tx.execute(sql, [next_value])?;
                        tx.commit()?;
                        Ok(next)
                    })
                    .map_err(|error| error.service)?;
                next_value
            }
            _ => return Err(ServiceError::UNSUPPORTED_OPERATION),
        };
        output[..8].copy_from_slice(&value.to_le_bytes());
        Ok(8)
    }
}
