# Host resource and database migration

English | [日本語](../docs_ja/HOST_RESOURCE_MIGRATION.md)

The SDK provides `restart_with_commit(candidate, migrate_checkpoint, commit)` and a real-DLL example
that switches SQLite and a host-side cache together. The application owns SQL, exclusion and recovery policy.
The core adds no database dependency or general migration framework. Handler ABI remains at version 5.

## Conditions for old and new code to coexist

| Change | Policy in this example |
| --- | --- |
| Code changes while DB operations, value units and schema stay the same | Bind the same DB v1 service and Drain. Old and new requests share the resource |
| Column names, value units or host-side state change | Prepare a DB v2 service, finish old work with Restart, then migrate |
| Return to old code | Select code compatible with the persistent schema. No automatic conversion back to v1 after v2 updates |

The primary service remains a host-owned Session. Binding 1 supplies the additional DB service.
The compatible DLL requires DB v1; the incompatible DLL requires DB v2.
`prepare_with_environment` can select the new DB adapter, allowing cutover within the same Router.
This does not change the primary service's own ID or version.

A matching service version does not prove data compatibility. The application must confirm that old and new reads/writes have consistent meaning.
After migration to DB v2, the old v1 adapter returns CONTRACT_MISMATCH.
Revoking its old ResourceId then makes calls return REVOKED.

## Commit-hook order

```rust,ignore
let candidate = unsafe {
    router.prepare_with_environment(new_dll, next_environment)
}?;
let generation = router.restart_with_commit(
    candidate,
    |_, checkpoint, _| Ok(checkpoint.clone()),
    || database.migrate_to_v2(|_| Ok(())).map_err(|error| error.service),
)?.await?;
```

1. Validate the candidate and pause admission.
2. Request stop for managed roots in the current and old Drain generations, and join child and external work.
3. Convert and validate all worker checkpoints, then create every new Future.
4. Recheck service bindings and call `commit` once.
5. On success, switch the generation and workers, restoring the original admission state. New handler bodies start on the next poll.

Use `migrate_checkpoint` for pure transformations of owned bytes. Put resource mutations in `commit`.
Unpolled factories must not mutate application state or start I/O.
`commit` must not revoke prepared services. Revoke old DB IDs after success.

Ordinary `restart` uses an empty successful hook and keeps its existing behavior.
The hook is not called if checkpoints, factories or joins fail.
Dropping a Pending Restart also leaves the hook uncalled and admission paused.
The host executor must concurrently poll ordinary requests so they can respond to stop. They are not replayed automatically.

A hook error becomes `RouteError::HostCommit(error)`; unwinding becomes HostCommit(PANICKED).
The old generation and shutdown checkpoints remain, with admission paused. The core does not roll back hook effects.
A retry starts while paused, so call `resume()?` explicitly after it succeeds.

## Exclusion for SQLite and host resources

The example's `Database` holds its connection, `MemoryState` and state flags in one `Arc<Mutex<Store>>`.
DB v1/v2 adapters and application administration use this same lock.
`try_lock` contention returns BUSY; unresolved inconsistency returns CLOSED, preventing observation of partially migrated state.
This does not move SDK Services or Routers across threads.
Adapters registered on different owner threads share only the host-side `Database`.

Migration follows this sequence:

1. Compare the known `PRAGMA user_version` and target rows with the host cache.
2. Prepare a new `MemoryState::V2` and check integer conversion for overflow.
3. Start `BEGIN IMMEDIATE`, then recheck pre-migration data after acquiring the write lock.
4. Rename `value` to `value_minor`, multiply values by 100 and set `user_version` to 2.
5. Verify the transaction's results and COMMIT.
6. While still holding the lock, install the prepared host state and resume access.

Columns, values and the persistent version change in one SQL transaction.
Publishing the cache after durable COMMIT involves no further SQL, allocations or user callbacks.
If the database is already v2, migration validates it and succeeds without multiplying values again.
Ordinary updates use the same lock and a transaction, and check reply capacity before performing effects.

SQLite locks also affect other connections. Another writer can make BEGIN IMMEDIATE return BUSY;
another reader can do the same for COMMIT. This example uses a rollback journal and busy timeout 0, returning failure instead of waiting indefinitely.
See [SQLite transactions](https://sqlite.org/lang_transaction.html) for details.
This does not stop unrelated external writers throughout an entire hot update.
The application coordinates shutdown and versions across all sharing Routers, service users and external connections.

SQL calls and the commit hook are synchronous in this example. Busy timeout 0 does not bound file-I/O latency.
The example demonstrates exclusion and recovery; it does not guarantee short execution times for frames or busy servers.
For applications whose ordinary DB operations may wait, consider the existing asynchronous services or host I/O thread.
The commit hook itself is not asynchronous.

## Failure and recovery

| Situation | Database / cache | Router / next action |
| --- | --- | --- |
| Error/panic during SQL changes, or BEGIN/COMMIT BUSY | Attempts ROLLBACK. Returns to Ready only after confirming autocommit and restoration of the previous version and target values | Paused on the old generation. After contention clears, retry from the same checkpoints |
| Restoration of old state cannot be confirmed | Sets RecoveryRequired and returns CLOSED. Does not reopen access using the old cache | Keep paused and inspect persistent state |
| External changes make the version or values disagree with the cache | Sets RecoveryRequired and returns CLOSED | Restore or select a supported schema, then call `recover()` explicitly |
| `recover()` succeeds | Rebuilds the cache from a known persistent version and target values | Select a compatible DLL and service version. Recovery itself does not resume the Router |

rusqlite Transaction attempts rollback on Drop by default, but the example does not assume that it succeeds.
It explicitly rolls back a remaining transaction, then checks connection state and this application's target data.
This check is specific to the small ledger schema; it is not a complete consistency check for arbitrary databases.
See the [rusqlite Transaction contract](https://docs.rs/rusqlite/0.41.1/rusqlite/struct.Transaction.html).

A durable database COMMIT and the in-memory Router cutover are not one crash-atomic operation.
If the process exits between them, the next startup must choose cache state and compatible code from the persistent version.
The example's `open` / `recover` restore the cache for known versions but do not select a DLL automatically.
Unknown versions are rejected. Arbitrary effects after successful COMMIT and automatic downgrade after new-version updates are also outside the scope.

## Run and understand the verification scope

```powershell
cargo run -p db_migration_host --locked
cargo test -p db_migration_host --test database --locked
cargo test -p db_migration_host --test native_migration --locked -- --ignored --test-threads=1
```

The example builds three DLL variants and creates `target/db-migration-demo/<run-id>/application.sqlite`.
It does not use an existing application database. Output files remain available for inspection.
Only `db_migration_host` depends on rusqlite 0.41.1 with bundled SQLite, which also requires a C compiler.

Native tests check:

- Old/new requests during compatible Drain with real DLLs; shutdown waits for incompatible Restart; retry after failure and continuity of worker state.
- Rollback and cache preservation after errors or panics immediately after column changes, after data changes and just before COMMIT.
- Actual BEGIN/COMMIT BUSY from another connection, and BUSY for calls from another thread during migration.
- Rejection of old adapters and revoked IDs, blocking unknown versions, explicit recovery, and preflight checks for overflow and reply capacity.
- Persistence of the v2 version and values after closing and reopening the connection.

The core `host_commit` tests check hook ordering, single invocation, skipped invocation, failure and panic.
Miri covers this core portion. SQLite C code, file I/O and real DLLs are checked separately in native tests.
Verification does not include forced-exit or disk-failure injection, other databases, distributed transactions,
long-running asynchronous migrations or frame-time guarantees.
