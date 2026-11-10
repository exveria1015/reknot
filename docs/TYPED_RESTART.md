# Typed Restart and state migration

English | [日本語](../docs_ja/TYPED_RESTART.md)

`ModuleHost` can cooperatively stop registered workers, wait for managed work,
migrate typed checkpoints and recreate the workers on a new generation. Drain is
still the default for ordinary requests. Restart only recreates explicitly
registered workers; ordinary calls are never replayed.

## Declare and register a worker

Define the handler and a single module-wide state contract in the shared protocol:

```rust
use reknot::typed::{Handler, Module, WorkerState};

pub const RUN: Handler<(), Result<(), String>> = Handler::new("app.worker", 1);
pub const STATE: WorkerState<u64> = WorkerState::new("app.counter", 1);

pub fn module() -> Module {
    Module::new("app", 1).with(RUN).with_state(STATE)
}
```

Use a struct/enum with `#[reknot::schema]` and Serde derives for richer state.
All workers in this module use this state contract; an enum can represent several
worker kinds. IDs are application-selected `WorkerId` values. The worker output
is `Result<(), String>`: an application error blocks Restart and is available
through `worker_error(id)`.

In the logic crate, import the protocol and explicitly restore/save its state:

```rust
use app_protocol::{module, RUN, STATE};

#[reknot::handler(RUN)]
async fn run(context: reknot::RequestContext, _: ()) -> Result<(), String> {
    let count = STATE.restore(&context).map_err(|e| e.to_string())?;
    context.cancelled().await;
    STATE.save(&context, &count).map_err(|e| e.to_string())?;
    Ok(())
}
reknot::export_handlers!(module(), [run]);
```

The example simply waits for stop. Real workers must save updated state and
cooperate with cancellation; register child work through `RequestContext`.
The host registers a root with
`host.start_worker(WorkerId(1), RUN, (), STATE, &0)?` and drives `poll_workers(cx)`.
`DevModule::update()` already drives registered workers. `stop_worker` requests
cooperative stop; `remove_worker` removes the registration and schedules remaining
cleanup. Keep driving cleanup after removal. `worker_status` returns `None` for an
unfinished or absent worker; `worker_checkpoint(id, STATE)` decodes its saved state.

## Restart with unchanged state

Prepare a trusted native candidate with `unsafe { host.restart(path)? }`, or a
resident static table with `unsafe { host.restart_api(api)? }`. Both return an
owned `ModuleRestart` future and close admission after successful preflight.
Await/poll that future on the owner thread while also driving externally owned
calls and bridge drivers. Do not block their executor while waiting.

The sequence is: validate candidate → pause admission → notify all managed roots
and join roots/children → stage checkpoint conversions and new worker factories →
final host commit → install the new generation and restore prior admission state.
New worker bodies are not polled before this commit. Poll workers after cutover.

Existing `HandlerHandle` and `HandlerClient` values remain usable. Already
admitted calls retain their generation and receive cooperative stop. Queued
bridge calls wait while paused and choose a generation when eventually admitted;
successful cutover or explicit `resume()` wakes their drivers. Direct calls made
while paused fail with the underlying `Paused` routing error.

## Migrate the state explicitly

Declare a new `WorkerState<NewState>` with a new version in the new logic. The host
retains the old type/descriptor for decoding and knows the new one for conversion:

```rust
let migration = NEW_STATE.migrate(|_id, old_checkpoint| {
    let old = OLD_STATE.decode(old_checkpoint)?;
    Ok(NewState { total: old })
});
let restart = unsafe { host.restart_with(new_dll_path, migration)? };
// Poll restart concurrently with ordinary calls and bridge drivers.
```

Use `restart_api_with` for static tables. Handler input/output contracts remain
unchanged; their changes still require rebuilding/restarting the host. Migration
closures must be pure: conversion may fail after earlier workers were converted,
and a later retry starts from the retained old checkpoints. No partial migrated
worker list is installed.

`migration.with_commit(|| -> Result<(), ServiceError> { ... })` adds a synchronous
final host commit after join and successful staging, before new bodies run.
Applications own transaction durability and rollback; callback failure does not
undo external effects. See [host resource migration](HOST_RESOURCE_MIGRATION.md).
Typed checkpoints contain both the state descriptor and the value. The 1 MiB
limit includes this envelope. Decode validates the key and structural contract
before invoking the application's value deserializer.

## Recover from failure or interruption

| Result | State and next action |
| --- | --- |
| Preflight rejects the candidate | Old generation and admission remain unchanged; inspect contract diagnostics |
| Worker or registered child remains unfinished | Restart stays Pending; keep driving shutdown, or apply the host's timeout policy |
| Worker error, migration/commit error, or staging failure | Old generation/checkpoints remain, admission stays paused; inspect `Error::Restart`, then retry or recover explicitly |
| Restart future dropped or timed out | Admission stays paused; no forced destruction, request replay or automatic resume |
| Success | New workers are staged and prior admission state is restored; an already paused host stays paused |

`host.resume()?` reopens admission after recovery; it does not recreate workers or
undo stop notifications. Dropping the future also during a reentrant worker poll
defers internal flag cleanup to the next owner maintenance/recovery call. Keep
driving workers, ordinary calls, bridges and cancellation cleanup as appropriate.
The SDK does not impose a shutdown deadline or force an uncooperative worker to end.

## Choose automatic Restart during development

Set `DevOptions::with_reload_mode(ReloadMode::Restart)` before `DevModule::start`.
Save-time builds then emit `ReloadEvent::Restarting`, followed by `Reloaded` or
`Rejected`. `update()` drives the pending Restart and attached bridges/workers.
State must keep the same contract; use the explicit migration API for a changed
state schema. Errors after shutdown starts keep admission paused.

Edits received during a pending Restart are processed after that Restart finishes;
its shutdown is not rolled back. `ReloadMode::Drain` remains the default for the
generated application and existing hosts. This guide covers the typed API itself;
framework-specific event/lifecycle integration needs separate application checks.

[Contract diagnostics](CONTRACTS_AND_SCHEMAS.md) · [Low-level Restart](RESTART_AND_MIGRATION.md) · [Documentation index](INDEX.md)
