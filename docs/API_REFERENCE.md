# API reference

English | [日本語](../docs_ja/API_REFERENCE.md)

## Typed application API

This API turns ordinary functions into reloadable entry points. Start with the [typed-handler guide](TYPED_HANDLERS.md) for a walkthrough.
This section collects contracts and limits. The typed API uses the existing ABI and is enabled by the `typed` feature.

### Definitions and setup

| API | Role |
| --- | --- |
| `Handler<Input, Output>::new(name, version)` | Define a stable handler identity and input/output contract version |
| `Module::new(name, version).with(handler)` | Declare the handlers in one crate/DLL |
| `#[reknot::handler(CONTRACT)]` | Generate an entry point for a sync or async function, optionally taking `RequestContext` first |
| `export_handlers!(module(), [handlers])` | Export the crate's handlers together; use once per crate |
| `module().builder()` | Configure host resources, service bindings, request capacity and router options |
| `builder.load(path)` / `builder.from_api(api)` | Create a `ModuleHost` from a native DLL or resident callback table; requires `unsafe` |
| `DevModule::start(builder, options)` | Begin the initial build/load and source watching; requires `unsafe`, including trust in future edits and dependencies |
| `DevModule::update()` | Detect edits, apply background build results, drive attached bridges and run cancellation cleanup |
| `reknot-new PATH` | Generate host, logic and protocol crates in a new directory; requires feature `typed` |

Protocol and logic dependencies use `default-features = false, features = ["typed"]`.
Set the logic's `[lib] crate-type` to `["cdylib"]` for native loading, or `["cdylib", "rlib"]` to support static linking too.
Development hosts enable the default features, or `typed`, `native` and `dev-tools` explicitly.

### Contract checks and input/output

Candidate validation compares module names/versions, handler names/versions, Rust input/output type names, generated schemas in both Serde directions, state contracts and the actual export list.
List order does not matter. Duplicate keys and detectable hash collisions are registration errors.
Identities are not generated from Rust function names and do not grant permissions.

Use `#[reknot::schema]` on protocol types. Structural differences produce an `Error::Contract { report, source }` with expected/actual values, JSON pointers and handler source locations.
Equal schemas do not establish semantic compatibility. See [contract diagnostics](CONTRACTS_AND_SCHEMAS.md).
When these change, increment the corresponding contract version and rebuild/restart the host too.
Adding or removing a handler also requires updating the declaration shared by host and logic.

Inputs and outputs are owned values satisfying `Serialize + DeserializeOwned + JsonSchema + 'static`.
Group several arguments into a struct or tuple; use `()` when no value is needed.
Borrowed SDK objects, `&self` methods and generic functions are not supported.

Only copied JSON bytes cross the boundary. Rust objects and their destructors are not shared across DLLs.
Values that JSON cannot represent, or failures during conversion, produce errors.
For an output of `Result<Reply, AppError>`, the outer call result reports SDK failure and the inner result reports application behavior.
Build failure or candidate rejection keeps admission unchanged. Runtime panics and application errors do not trigger event replay.

### Calls and threads

| API | Thread and admission contract |
| --- | --- |
| `ModuleHost::handler(contract)` | Return an owner-thread `HandlerHandle`; clone and keep it across updates without re-registering |
| `HandlerHandle::call(input)` | Attempt admission synchronously and return `Call<Output>`; successful admission selects the generation before the first poll |
| `ModuleHost::bridge(options)` | Return a `ModuleClient` and `BridgeDriver`; poll the driver on the owner thread |
| `DevModule::bridge(options)` | Attach the driver to `update()` and return the client |
| `ModuleClient::handler(contract)` | Return a `Send + Sync` `HandlerClient` that can be cloned into event handlers on other threads |
| `HandlerClient::call(input)` | Enqueue immediately and return a `Send` `RemoteCall<Output>`; select the generation at owner admission |
| `RemoteCall::generation()` | `None` before admission; the selected generation afterward while the call remains pending |

`ModuleClient` is `Send + Sync`. `ModuleHost`, `HandlerHandle`, `Call` and `BridgeDriver` are not `Send`.
With Tokio, run the owner on `current_thread` or a `LocalSet`.
The host converts framework events and sends replies; no framework-specific adapter is included.

### Capacity and admission limits

| Item | Limit or default | Setting |
| --- | --- | --- |
| JSON input and output | 1 MiB each | `MAX_MESSAGE_BYTES` |
| Handlers per module | 1–64 | `MAX_HANDLERS` |
| Concurrent module requests | Default 256 | `module().builder().max_in_flight(n)` |
| Bridge request slots | Default 256; queued, running and completed-but-unconsumed requests combined | `BridgeOptions::capacity` |
| Running bridge requests | Default 64 | `BridgeOptions::max_in_flight` |

Bridge limits must be positive, with `max_in_flight <= capacity`. The module limit applies separately.
Capacity exhaustion or contention returns `Error::Busy`. There is no automatic retry.
Admission is FIFO; completion order may differ. Cancelled running requests keep their slots until registered child cleanup finishes.

`outstanding()` counts occupied bridge request slots. Zero does not prove that a DLL can be reclaimed.

### Cancellation and shutdown

After `Call::cancel` or `RemoteCall::cancel`, keep awaiting the request to wait for registered child cleanup.
A result that completed before cancellation may still be returned. Dropping the future also requests cancellation.

| Operation | Shutdown contract |
| --- | --- |
| `client.close().await` | Close admission immediately and wait for accepted work to finish; dropping the last client, including `HandlerClient` clones, also closes admission |
| `client.shutdown().await` | Close admission, cancel work and wait for registered child cleanup; covers only that bridge's requests |
| Dropping `BridgeDriver` | Report `Error::BridgeClosed` to remaining calls and shutdown waiters; this is not a successful join, and cleanup moves to `host.poll_cleanup` |
| Dropping a local `Call` | Continue registered child cleanup through `poll_cleanup` on the owner thread |

Keep polling the driver during shutdown. `DevModule::update()` drives attached bridges and cleanup.
Custom/static hosts can await cleanup after Drop with `std::future::poll_fn(|cx| host.poll_cleanup(cx)).await`.
This `poll_cleanup` drives all cancelled requests on the same thread.

An uncooperative child can keep shutdown pending. The SDK does not force unmanaged work to stop or roll back external effects.

### Host services from typed handlers

Add `RequestContext` as the first argument and call services through `ctx.call_service` or `ctx.call_service_async`.
The host binds them with `builder.resources(resources).bind(binding, resource.id())`;
the logic declares them with `export_handlers!(..., requires = REQUIRED_SERVICES)`.
See the [typed service tests](../reknot/tests/typed_services.rs) for the combination.

`ctx.resource()` and `ctx.call()` are reserved for typed reply delivery; do not use them for application I/O.
The initialization-data slot also holds the module contract. Pass configuration through inputs or host services.
Register child work under the existing `spawn_child` and `register_activity` contracts.

See [development operation](DEVELOPMENT.md) for watching/building and [DLL lifetime](UNLOAD_SAFETY.md) for native code ownership.
The typed API covers single-module Drain and [typed Restart](TYPED_RESTART.md). `WorkerState<S>` defines checkpoints;
`start_worker`, `poll_workers`, `stop_worker`, `remove_worker`, `worker_status`, `worker_error` and `worker_checkpoint` manage registered roots.
`restart` / `restart_api` preserve state; `restart_with` / `restart_api_with` accept an explicit `StateMigration`.
`ModuleRestart` is an owner-thread future. Failure or interruption keeps admission paused; `resume()` is explicit recovery.
`DevOptions::with_reload_mode(ReloadMode::Restart)` opts into automatic Restart for unchanged state contracts.
`Module::schema_json()` exports the full contract. Multi-DLL atomic updates remain outside this API.

## Message API

The boundaries use Handler ABI 5, Services ABI 2 and task-control ABI 2.

`retention_snapshot()` reports SDK-held native references, copied file sizes, lifecycle/validation state and the last copy-removal error,
without retaining new image references. These are separate from memory use and budget consumption.
`inspect_cache(&CacheOptions)` and `clean_cache(&CacheOptions)` inspect the dedicated copy root;
clean removes eligible exited sessions only when `apply=true`. The CLI is `reknot-cache`.
See [session ownership, locking and limits](RETENTION_AND_CLEANUP.md).

Alongside the default KeepLoaded policy, hosts that can establish shutdown conditions may explicitly reclaim retired images with `unsafe reclaim_native_image(ArtifactId)`.
`artifact_id()` returns a router/candidate's image ID; `release_retired_generations()` releases the router's completed retired generations.
The SDK rechecks retirement, code references, return from calls and the owner thread.
Complete return from external DLL-side Waker execution, TLS cleanup and dependency shutdown remain the caller's `unsafe` obligations.
See [DLL lifetime](UNLOAD_SAFETY.md) for retention reasons, `ReclaimResult`, and the distinction between close and copy removal.
Handler ABI and cumulative load-budget semantics are unchanged.

`GenerationLocal<T>` provides registered TLS per generation and owner thread through `with` and `with_mut`.
`RequestContext::host_notification()` provides a notification whose C-handle clone/wake/drop stays in resident host code after request completion.
`call_async` uses it automatically on supporting hosts.
Arbitrary TLS and dependency-specific shutdown are outside this mechanism. See [scope and usage](MANAGED_LOCALS_AND_NOTIFICATIONS.md).

The message API separates resource ownership, request admission and execution bound to a generation.

### Resources and logic

| API | Contract |
| --- | --- |
| `HostResource::call(operation, input, output)` | Short synchronous operation returning bytes written or ServiceError; never retain borrowed buffers |
| `HostResources::register(key, value)` | Register a nonzero service ID/version and return a typed `Resource<T>` |
| `Resource<T>::id()` / `try_borrow*()` | An ID for DLL access; typed access within the host only |
| `HostResources::revoke(id)` | Revoke later access; an old ID never identifies a reused slot |
| `HostResources::services()` | Create an owning `Services` client that retains the registry |
| `Services::call(...)` | Check the registry, ID generation, service version and output length |

Dropping `Resource<T>` alone does not unregister it. A typed owner can keep the value alive after revocation, but DLL operations return REVOKED.
`Services` retains ownership even if the original registry or router handle is dropped.
Break resource-to-registry ownership cycles with revocation or another explicit lifetime rule.
These handles connect trusted code on one owner thread; they do not isolate permissions from malicious DLLs.

```rust
use reknot::{RequestContext, ServiceError, ServiceKey};
const SERVICE: ServiceKey = ServiceKey::new(1, 1);
const STATE: ServiceKey = ServiceKey::new(2, 1);

#[reknot::reload_handler(SERVICE, state = STATE)]
async fn worker(ctx: RequestContext) -> Result<(), ServiceError> {
    let saved = ctx.checkpoint().ok_or(ServiceError::INVALID_ARGUMENT)?;
    ctx.cancelled().await;
    ctx.save_checkpoint(STATE, &saved.bytes)?;
    Ok(())
}
```

`reload_handler(SERVICE)` also works without worker state.
The macro generates `hot_handler_api` and a separate opaque future for each execution from `async fn(RequestContext) -> Result<(), ServiceError>`.
The factory copies input/seed data and retains the service and control tables.
The body starts at first poll. Creating or dropping an unpolled future must not start I/O or mutate shared state.

`RequestContext` exposes `input`, `generation`, `resource`, `service`, `services`, `call`,
`stop_requested`, `cancelled`, `checkpoint`, `save_checkpoint`, `spawn_child` and `register_activity`.
`checkpoint()` reads the initial seed; `save_checkpoint` copies committed data into the host.
The macro also accepts `requires = &[ServiceRequirement::new(binding, key), ...]` and `init = Config`.
`Config` implements `InitPayload` (`SCHEMA`, `encode` and `decode`).
Build the environment with `HandlerEnvironment::bind` and `with_initial(InitData::from_value(&config)?)`,
then pass it with `RouterOptions` to `load_with_environment` or `from_api_with_environment`.
Use `prepare_with_environment` / `prepare_api_with_environment` to change a candidate's environment; ordinary prepare reuses the current one.
Additional services and pure initialization decoding are checked before factories or request bodies run. Resource revocation is rechecked at admission and cutover.
Use `initial::<Config>()`, `binding`, `call_service` and `call_service_async` for per-generation copies and bindings.
Limits are 32 additional requirements and 1 MiB of initialization bytes, separate from ordinary input and checkpoints.
See [service and initialization contracts](SERVICE_CONTRACTS_AND_INIT.md).
`call_async(operation, input, max_output_bytes)` returns an owner-thread `ServiceCall`.
It copies input and includes host I/O cancellation cleanup in registered work.
The host implements `HostResource::start_async` with an owned `HostServiceFuture` and observes shutdown through `CancellationToken`.
`HostResources::with_async_limits` sets operation and buffer limits.
Share only the `ServiceClient` from `ThreadService::spawn` between host threads; do not move SDK handles out of their logic owner threads.
Call `ServiceWorker::shutdown()` and await it to stop and join the I/O thread.
See [async services and threads](ASYNC_SERVICES_AND_THREADS.md).
A handler's `ServiceError` or caught unwind becomes `RunStatus::Error`; `RequestTask` currently does not expose the original detailed error.
Destructor panics and `panic=abort` are outside recovery guarantees.

### Admission and execution

| API | Behavior |
| --- | --- |
| `unsafe RequestRouter::load(path, resources)` | Load trusted SDK-native code |
| `unsafe load_with_options(path, resources, options)` | Set a shared budget and checkpoint limit |
| `unsafe from_api` / `from_api_with_options` | Use static/custom callbacks; the caller guarantees code and ABI lifetimes |
| `dispatch(resource_id, input)` | Copy input and return an owned `RequestTask` implementing `Future<Output = RunStatus>` |
| `unsafe prepare(path)` / `prepare_api(api)` | Prepare an inert candidate without changing admission |
| `activate(candidate)` | Drain; requires matching service and state schemas |
| `pause()` / `resume() -> Result` | Close/reopen admission, without queuing or replay |
| `in_flight()` | Count unfinished managed roots, including workers and old Drain generations |
| `generation()` / `is_accepting()` / `options()` | Inspect the admission generation, state and limits |

The `unsafe` native-loading contract includes initialization, finalization and callbacks in future replacement candidates too.
Metadata checks do not prove native-code safety. Wrong-router and stale-base candidates are rejected. Drain preserves a paused state.

`RequestTask` owns its future, generation and control and may outlive the router.
The host polls it on its owner thread; `generation`, `resource` and `status` expose its state.
`cancel(self)` and Drop synchronously destroy the root future. Registered stop/join work continues in the owner thread's cleanup queue,
and the unfinished count remains until that work joins.
The host drives `poll_cancelled_requests(cx)`. `poll_workers` and Restart also drive this queue.
Cancellation does not undo effects or automatically stop independent unregistered child work.
External Wakers retain code references; the native default is KeepLoaded.

### Managed children and external callbacks

`spawn_child(|stop| async move { ... })` registers a `'static` child future to be polled on the owner thread.
Observe shutdown through `ChildStop::cancelled()` / `is_requested()`. The creation closure runs at first poll after successful registration.
This does not spawn onto an OS thread or external runtime.
`register_activity(adapter)` accepts an `unsafe impl ManagedActivity` owning an external join handle or subscription.
`request_stop` errors are recorded and retried. `poll_join` may return `Ready(Ok/Err)` only after join, unregistration and return of active callbacks are confirmed.
If shutdown is unconfirmed, return `Pending`. A rejected adapter must also be safe to drop before its first poll.

`activity_count()` / `activity_error()`, and `worker_activity_count(id)` / `worker_activity_error(id)` for workers, report state.
There can be 64 simultaneous registrations per request. Full capacity returns `BUSY`; registration after stopping or closing returns `REVOKED`.
After cleanup joins, any activity failure blocks Restart with `ActivityFailed`. The application may call `acknowledge_activity_failure(generation)`
only once that generation has no unfinished roots.
Resuming admission, registering workers again and recovering effects are separate operations.
See [managed work contracts and verification](MANAGED_ACTIVITIES.md).

### Registered workers and Restart

- `start_worker(WorkerId, resource, input, Checkpoint)`: explicitly register work that can be recreated.
- `poll_workers(cx)`, `worker_status(id)`, `worker_checkpoint(id)`: drive and observe workers.
- `stop_worker(id)`: request cooperative stop. `remove_worker(id)`: cancel and unregister.
- `restart(candidate, migrate) -> Result<Restart<'_>, RouteError>`: close admission, wait for all managed roots including old Drain generations,
  then switch after migrating all checkpoints and preparing all factories. Restart drives registered workers; the host concurrently drives externally owned `RequestTask`s.
- `restart_with_commit(candidate, migrate, commit)`: after the same shutdown and staging, call one synchronous `FnOnce() -> Result<(), ServiceError>` immediately before the generation switch.
  Earlier failure or interruption never calls the hook. An error or unwind becomes `RouteError::HostCommit` and leaves admission closed.
  The application owns exclusion, transactions and recovery; the SDK does not roll back effects. Do not invalidate staged bindings.
  See the [SQLite and host resource example](HOST_RESOURCE_MIGRATION.md).

Only registered workers are recreated. Failure or interruption preserves old checkpoints and leaves admission closed.
`resume()?` does not recreate workers. Restart may change the state schema, but the service contract must match.
See the [Restart contract](RESTART_AND_MIGRATION.md).

A `Checkpoint` contains a `ServiceKey` schema and owned `Vec<u8>` bytes.
`RouterOptions` defaults to a shared `GenerationBudget` of 32 cumulative load attempts and a 1 MiB checkpoint limit.
Native attempts are reserved before loading and never refunded, including missing files, failed checks and abandoned candidates.
Share the budget across router recreation and inspect it through `used()` / `limit()`. It is not a process-wide memory bound.

## Build configurations

Defaults enable `typed`, `native` and `dev-tools`. `typed` provides Serde JSON handlers; `native` provides DLL loading, retention diagnostics and copy cleanup;
`dev-tools` provides source watching and Cargo helpers through `reknot::development`.
A `native`-only configuration does not include watching. `reknot-cache` requires `native`.
Use `default-features = false` in logic crates and statically linked hosts.

| Configuration | Dependency options | Included features |
| --- | --- | --- |
| Development host | Defaults | DLL loading, retention diagnostics, copy cleanup, build/watch helpers |
| Host with an external builder | `default-features = false, features = ["native"]` | DLL loading, retention diagnostics and copy cleanup |
| Logic crate or static host | `default-features = false` | Services, tasks, routing and handler generation |
| Typed logic or static host | `default-features = false, features = ["typed"]` | Typed input/output and module contracts, without native dependencies |

Loading mode is independent of Debug/Release.

`reload_handler` also generates `handler_api() -> HandlerApi` for static linking.
Pass it to `unsafe RequestRouter::from_api`; the caller guarantees callback lifetimes.
Run the static example with the same logic and host resource contracts:

```sh
cargo run -p service_host --release --no-default-features --features static-logic --locked
```

## Low-level task ABI

`TaskHandle` / `TaskVTable` describe an owned foreign future. `FfiFuture` polls it and destroys it exactly once.
With low-level `unsafe FfiFuture::new`, the caller guarantees code and borrowed-data lifetimes.
Use `RequestTask` for ordinary admission, generation ownership and cleanup joins.
When `native` is enabled, I/O, library and symbol lookup failures are returned as `LoadError` and `RouteError::Load`.
The current boundaries are Handler ABI 5, Services ABI 2 and task-control ABI 2.
Build host and logic against the same SDK.

See [compatibility and scope](COMPATIBILITY.md) for runtime contracts and the [changelog](../CHANGELOG.md) for API changes.
