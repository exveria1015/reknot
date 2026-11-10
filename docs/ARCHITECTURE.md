# Architecture and requirements

English | [日本語](../docs_ja/ARCHITECTURE.md)

## Scope

The priority is servers and persistent services, followed by games, then desktop applications.
The initial release focuses on reliable updates during development. Production deployment, crash isolation and automatic rollback of side effects are outside its required scope.
The API centers on ownership and shutdown contracts.

Execution uses `RequestRouter` and versioned host services.
Native loading and static linking share the same handler, resource and shutdown contracts.

## Ownership and execution boundaries

| Owner | Holds | During reload |
| --- | --- | --- |
| Host | Executor, connections, input/output queues, UI and GPU resources | Keeps resources alive and exposes them to logic through IDs and service operations |
| Logic generation | DLL, function tables, generation-specific data | Stays alive while execution refers to it; no physical unload by default |
| Managed root | Future, copied input, owned references to resources and task control, generation reference | Retains them until completion or explicit cancellation |
| Registered worker | Stable WorkerId, creation input, last checkpoint | Managed by the host for recreation during Restart |

The DLL boundary uses C ABI function tables, opaque handles and byte slices valid for the duration of a call.
Each side frees the Rust containers it allocates. The layout of a host resource's `T` never crosses into the DLL.
Matching service IDs, versions and schemas does not prove that data has the same meaning or that an implementation is correct.

Create, poll and destroy tasks and resource registries on their owner thread.
Waking from another thread and making the Future itself Send are separate contracts.
Low-level FFI safety depends on trusted logic honoring ownership, retention and destruction contracts.

## Two cutover modes

| Mode | Existing execution | New work | Suitable for |
| --- | --- | --- | --- |
| Drain | Continues on the old generation until completion | Uses the new generation after activation | Finite requests and jobs |
| Restart | Stop request → join → checkpoint migration | Recreates registered workers with new code and restores admission state | Persistent services and game work that can be recreated |

To update code at the next message while keeping a connection open, let the host receive messages and choose a generation for each request.
Draining a persistent connection loop inside a DLL does not change the code running that loop.
The SDK also does not prevent old and new generations from reading the same stream at once. Define ownership rules in the host.

Restart transfers explicit checkpoints. It does not move an arbitrary Future's stack or resume it at the same instruction.
Ordinary requests are not replayed, to avoid repeating effects such as external sends.
Stopping and joining explicitly registered children is part of root completion. Unmanaged child tasks and callbacks are not discovered automatically; the application must confirm they have finished.

## State and failure

- Keep shared state that must survive updates in the host. Maintain service contracts that allow old and new generations to coexist.
- Save only the generation-local state that needs migration in a checkpoint with a schema ID and version.
- A Future's local state belongs to its execution generation. Partial updates and external effects are not rolled back automatically.

Build and candidate-validation failures leave existing admission in place.
Restart waits for all roots to finish and all migrations and factories to succeed before installing the new worker list.
A failure after stopping, or dropping the Restart Future, leaves admission closed and preserves old checkpoints.
This does not restore old workers or migrate host resources and databases.

Candidate loading and unpolled factories must not start external I/O or mutate shared resources.
This contract includes OS-level DLL initialization; metadata checks cannot verify it.

## Admission cutover and native reclamation

The initial release must control admission and the lifetime of managed work.
The message API uses KeepLoaded and does not automatically reclaim old DLLs.
The shared load budget defaults to 32 cumulative attempts. It bounds the number of attempts, not memory usage.
Restart the development process when the budget is exhausted.

Physical reclamation requires more than a reference count: DLL calls must have returned, and no TLS destructor, callback or other cleanup may execute later.
Hosts that can establish these conditions may use `unsafe reclaim_native_image`.
The SDK rechecks retirement, owned references and host-initiated call returns. External Waker returns, TLS and dependencies remain the host's responsibility.
See [DLL lifetime](UNLOAD_SAFETY.md) for the procedure and retention reasons.

## Integration design

For games, define the frame boundary, owner thread and conditions for applying an old asynchronous job's result to the current world.
The [Bevy example](BEVY_GAME.md) shows a minimal integration of these boundaries.
Discarding a result with an old generation ID does not undo effects that have already happened.
Child work is managed only when explicitly registered. Asynchronous host services and host I/O queues support requests from multiple owner threads.
Keep SDK handles on their owner thread, and confirm completion of unregistered work in the application.
Host resource migration uses an application-defined commit hook.
See [compatibility and scope](COMPATIBILITY.md) for integration requirements, and
[host resources and routing](HOST_RESOURCES_AND_ROUTING.md) and [Restart](RESTART_AND_MIGRATION.md) for the contracts.
