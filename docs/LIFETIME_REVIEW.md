# Ownership, borrowing and destruction in async execution

English | [日本語](../docs_ja/LIFETIME_REVIEW.md)

Handler ABI 5 uses `RequestTask` for both native loading and static linking.

## Message execution

`RequestTask` holds the Future, code generation and host-side task control.
The DLL's RequestContext owns its input and initial checkpoint, and retains/releases references to services and task control.
Execution keeps the references it needs even after the Router or original registry handle is dropped.
A RequestTask decrements the unfinished count only after the foreign root Future and registered work have been joined and destroyed.
Cancellation and Drop destroy the root Future and move registered cleanup to an owner-thread queue.
`poll_cancelled_requests`, registered-worker polling and Restart drive that cleanup while retaining the generation and required data.
Restart waits for managed roots and registered work to finish before migrating checkpoints.

Keep these operations on the owner thread. Explicitly registered work and adapters must be `'static` and own their data;
they must not leave external borrows into the root Future. Custom spawn tasks and callbacks are not managed unless registered.
Requesting a stop, confirming completion and reclaiming code are separate contracts. See [registered work](MANAGED_ACTIVITIES.md) and [Restart](RESTART_AND_MIGRATION.md).

## Code responsibilities

| Module | Responsibility |
| --- | --- |
| `routing.rs`, `routing/control.rs` | Admission, generations, task control, unfinished counts and cleanup waits |
| `routing/restart.rs` | Cooperative stop, checkpoint migration, preparation of all factories, commit hooks |
| `task/future.rs` | Owning and polling `TaskHandle`; destroying it exactly once on completion, failure or cancellation |
| `task/waker.rs` | Converting between owned Wakers and retaining code references |
| `activity.rs` | Stopping and joining registered work; confirming external callback returns |
| `generation.rs`, `native.rs` | Code references, image observation and default process-lifetime retention of opened DLLs |
| `request.rs`, `reknot_macros/src/handler.rs` | Copying input, retaining services and creating independent Futures |

## Future and Waker lifetimes

`FfiFuture` destroys the foreign Future when poll returns completion or failure, then exposes the result.
Polling again does not call the destroyed handle. Cancellation also destroys that handle exactly once.
Waker clones retain an owned host handle and code reference, so they may outlive the poll call.
They support wake from another thread, but the Future, Router and resource registry stay on their owner thread.

Low-level `unsafe FfiFuture::new` does not automatically retain a DLL reference.
The caller must guarantee the lifetime of code, vtables and borrowed data, and honor the poll/drop contract.
For ordinary host execution, use `RequestTask`, which also manages the generation and waits for registered work to finish.

## Destruction and registered work

Root Future completion alone does not mean external I/O or registered work has finished.
If registered work remains after a `RequestTask` is dropped, the host must drive its cleanup on the owner thread.
Until completion is confirmed, the generation's unfinished count stays unchanged and Restart cannot commit.
Unmanaged work is not discovered automatically; the application must confirm its stop and return.

See [testing and Miri](TESTING.md) for verification, and
[DLL lifetime](UNLOAD_SAFETY.md) for the native-image retention contract.
