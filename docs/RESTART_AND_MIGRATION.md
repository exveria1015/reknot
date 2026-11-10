# Restart, state migration and generation limits

English | [日本語](../docs_ja/RESTART_AND_MIGRATION.md)

Shutdown, state-migration and retention-limit contracts for Handler ABI 5.

## What changes during cutover

Drain uses `activate(candidate)`: existing execution stays on the old generation, and new requests use the new one.
Restart uses `restart(candidate, migrate)` and waits for all execution managed by the module to stop.
Only workers explicitly registered through `start_worker(WorkerId, resource, input, Checkpoint)` are recreated.
Ordinary `dispatch` requests are stopped but never replayed automatically.

The sequence is:

1. Check the candidate's Router ID, base generation and service contracts. Failure here does not affect admission.
2. Close admission and request stop for managed tasks in the current and earlier Drain generations. Notify the Wakers from their most recent polls too.
3. The Restart Future polls registered workers. The host executor must concurrently drive externally owned RequestTasks.
4. Wait for every managed task Future and for registered work to join, unsubscribe and be destroyed. Activity failures produce ActivityFailed; registered-worker Error produces WorkerFailed.
5. Copy each worker's last checkpoint and convert it to the new schema with the migration function.
6. Validate every checkpoint's schema and size, then create all new worker Futures. Handler bodies do not run yet.
7. For `restart_with_commit`, call the final synchronous commit hook once. Ordinary `restart` uses an empty hook.
8. Install the generation and worker list, then restore the original admission state. A Router that was already paused stays paused. New workers begin on their next poll.

Managed work consists of root executions owned or tracked by this Router and work explicitly registered under them.
Stop and join for `spawn_child` / `register_activity` follow the [registered-work contract](MANAGED_ACTIVITIES.md).
Custom unregistered spawn tasks and external callbacks are not tracked automatically; the application must stop and join them.
Restart does not transfer an arbitrary Future's current execution position or external I/O effects into new code.

## Stop and save in the worker

```rust
use reknot::{RequestContext, ServiceError, ServiceKey};
const SERVICE: ServiceKey = ServiceKey::new(1, 1);
const STATE: ServiceKey = ServiceKey::new(2, 1);

#[reknot::reload_handler(SERVICE, state = STATE)]
async fn worker(ctx: RequestContext) -> Result<(), ServiceError> {
    let saved = ctx.checkpoint().ok_or(ServiceError::INVALID_ARGUMENT)?;
    let state = saved.bytes.clone(); // Restore application-specific state here.
    ctx.cancelled().await;
    ctx.save_checkpoint(STATE, &state)?;
    Ok(())
}
```

Use `stop_requested()` for a synchronous check and `cancelled().await` to wait inside a managed root.
A notification requests stop; it does not confirm completion. Owner-thread polling requests stop for explicitly registered children and waits for join.
Restart also drives cleanup from dropped requests. It does not automatically wake waits in unregistered child work.
In a frame-driven application, use `poll_workers(cx)` to advance registered workers and `worker_status(id)` to inspect them.

A `Checkpoint` combines a ServiceKey for its schema ID/version with an owned Vec<u8>.
Size limits apply to initial seeds, saves from the DLL and migrated state. The default is 1 MiB per worker.
`save_checkpoint` copies the DLL buffer into the host during the call. Only the last committed checkpoint is retained.
It neither rolls back partial updates before stopping nor guarantees that a future stop will successfully save state.

## Migrate in the host

```rust,ignore
router.start_worker(WorkerId(1), session.id(), b"worker", initial_checkpoint)?;
let candidate = unsafe { router.prepare(new_library) }?;
let generation = router.restart(candidate, |worker_id, old, target_schema| {
    let bytes = migrate_application_state(worker_id, old)?;
    Ok(Checkpoint::new(target_schema, bytes))
})?.await?;
```

The migration function receives `WorkerId, &Checkpoint, target_schema` and returns newly owned data.
The application defines serialization and conversion. Matching schemas does not prove the meaning of the bytes is correct.
For Restart with an unchanged schema, reuse the state with a closure such as `|_, state, _| Ok(state.clone())`.

Migration functions should transform input data into output data without mutating shared resources or performing external I/O.
Outputs are applied only after all migrations succeed. Old checkpoints survive migration errors, panics and new-factory failures.
A subset of workers is never started on the new generation while the rest remains unprepared.

Drain activation requires matching state schemas as well as service contracts.
Restart can change the state schema, but still requires the target host service ID and version to match.
`restart_with_commit` provides a commit point after all preparation and before cutover.
The application designs exclusion, changes and recovery for host resources, databases and connection protocol state.
The [SQLite and host-cache migration example](HOST_RESOURCE_MIGRATION.md) uses one lock and an SQL transaction.
The core does not roll back effects or provide crash atomicity between the database and Router.

## Failure, interruption and recovery

| Situation | Result |
| --- | --- |
| Candidate loading, ABI, service-contract or retention-limit failure | Preserves old admission and existing execution |
| A worker stays Pending without finishing | Restart stays Pending and waits without destroying its Future |
| A registered child or callback has not confirmed completion | Restart stays Pending. A failed unsubscription request alone does not permit destruction or cutover |
| Registered work confirms completion but has a recorded failure | ActivityFailed; retains the old generation and failure record, with admission paused |
| Migration error/panic, schema mismatch, oversized checkpoint or new-factory failure | Error with the old generation and checkpoints retained, and admission paused |
| Commit hook returns Err or panics | HostCommit; retains the old generation and checkpoints, with admission paused. The application rolls back or blocks resource access |
| Host drops the Restart Future, for example after a timeout | Admission stays paused. Old tasks are not forcibly destroyed or unloaded |
| Success | Registers workers on the new generation and restores the admission state from before Restart |

The library has no deadline clock or forced stop. The host may interrupt the Restart Future with timeout/select or frame-loop control.
A timeout on the same thread cannot be guaranteed to run if code never returns from poll.

After failure, retry Restart from the original checkpoints with another candidate or migration function.
Admission is already paused on retry, so call `resume()?` explicitly after success if needed.
`resume` does not recreate old workers or reverse external effects. `remove_worker(id)` explicitly cancels the target Future and removes its registration.
Leaving a failed worker in Error causes WorkerFailed again on retry; adjust registrations according to the recovery policy.
For a registered-activity failure, wait until all work in the generation finishes, inspect the outcome,
then acknowledge the record with `acknowledge_activity_failure(generation)` before retrying.

## Retention limits

Default RouterOptions allow 32 generation slots and checkpoints of 1 MiB.

```rust,ignore
let budget = GenerationBudget::new(8)?;
let options = RouterOptions {
    generations: budget.clone(),
    max_checkpoint_bytes: 64 * 1024,
};
let router = unsafe { RequestRouter::load_with_options(path, resources, options.clone()) }?;
```

GenerationBudget is a cloneable shared counter. Reuse the same budget/options when recreating a Router to preserve the cumulative limit.
Inspect it with `used()` and `limit()`. When no slot remains, GenerationLimit is returned before Library::new, without affecting running generations.

Native loading reserves one slot before starting. Missing files, ABI mismatches and discarded candidates all consume a slot without refund.
This is a conservative limit because DLL initialization and KeepLoaded retention cannot be precisely undone.
Static API candidates also consume a slot for each valid preparation. Activation and Restart themselves do not consume another slot.
Task completion does not return a slot. Automatic unloading and slot reuse are outside the current design.

This is a **load-attempt limit per shared budget**. It does not bound total memory, the whole process or separate budgets.
KeepLoaded DLLs and copies may remain until process exit. End the development session, for example by restarting the process, when the budget is exhausted.

## ABI and verification

Handler ABI is **5**, Services ABI is 2, task-control ABI is 2 and Activity ABI is 1.
Factories copy input and seeds, and retain services and task control. Factories and destruction of unpolled Futures must not start application mutations or I/O.

Run the example:

```powershell
cargo run -p reknot --example restart --locked
```

Output changes from `generation 1: restored 7` to `generation 2: restored 80`. The old value 7 is saved as 8 on stop, then migrated into the new two-byte format.
This example uses a statically linked API. The native_routing test uses real DLLs: it saves an old one-byte value of 3 as 4 on stop, then migrates it to a new two-byte value of 40.
The restart tests cover multiple workers, waiting for old Drain generations, migration failure and retry, interruption, size limits, panics, staged factory failure and shared budgets.
Miri and real-DLL tests serve different purposes. Neither proves safety of the entire library.

See [testing](TESTING.md) for commands and the distinction between Miri, native DLL and GUI checks.
