# Managing child tasks and external callbacks

English | [日本語](../docs_ja/MANAGED_ACTIVITIES.md)

Work explicitly registered with the message API is included in root-request completion.
The current versions are Handler ABI **5**, Services ABI **2**, task-control ABI **2** and Activity ABI **1**.
Rebuild the host and logic DLL together. DLLs using Handler ABI 2–4 are rejected.

## Register and drive work

```rust
ctx.spawn_child(|stop| async move {
    // The child owns its data and is polled on the owner thread.
    stop.cancelled().await;
    // Await any asynchronous cleanup here.
    Ok(())
})?;
```

`spawn_child` does not spawn onto a new thread or an external runtime.
Each time the host polls `RequestTask`, it advances both the root and registered work.
The creation closure is not called until the first poll after successful registration. Children must be `'static`;
they cannot borrow the root's stack or Future.
Run the example with `cargo run -p reknot --example managed_children --locked`.

Both `RequestContext::cancelled()` and `ChildStop::cancelled()` register each
waiting Future independently. They work inside selective combinators such as
`FuturesUnordered`, including multiple waits on the same context or cloned child
token. Repolling updates that wait's Waker; dropping it releases its registration.
Cancellation waits do not consume the 64 managed-activity slots or spawn work.

Request cancellation notifications use an optional cancellation ABI 1 tail after
the existing task-control and generation-services prefixes. Rebuild host and logic
together. On an older host without that tail, an outstanding `cancelled()` wait
panics explicitly (generated handlers report Error); it does not read missing
fields or silently stall. The synchronous `stop_requested()` query remains available.

For a JoinHandle from an independent runtime or a callback registration, use
`ctx.register_activity(adapter)` and `unsafe impl ManagedActivity`.
The adapter implements two operations:

| Operation | Contract |
| --- | --- |
| `request_stop(Pin<&mut Self>)` | Requests stop or unsubscription. `Ok` means the request was accepted, not that work has finished. `Err` is recorded and retried on the next poll |
| `poll_join(Pin<&mut Self>, cx)` | Confirms join or unsubscription and the return of in-flight callbacks. Returns `Pending` until confirmed. Both `Ready(Ok)` and `Ready(Err)` confirm completion |

If unsubscription fails and callbacks may still run, do not return `Ready(Err)`.
Report the stop-request error and keep the join side `Pending`.
Use `Ready(Err)`, for example, when child work failed but joining it has completed.
Falsely reporting completion can violate safety, so implementing this trait is unsafe.

The adapter owns the data used by external work and keeps it valid after the root is destroyed.
Rejected registration drops the adapter before its first poll, so Drop must also be safe at that point.
Deferring external registration until the first poll makes this easier to arrange.
Registration, stop, join and destruction run on the owner thread. External threads must not directly access a thread-bound
`RequestContext` or service table. Provide separate synchronization for external execution.

## What counts as completion

- Normal completion or root error/panic: destroy the root Future, then wait for registered work to stop and join.
- Restart: request stop, including work in earlier Drain generations, then migrate after roots and registered work finish.
- Child failure: request stop for the root and other registered work, then report the request as Error after all work finishes.
- `cancel(self)`, Drop or `remove_worker`: destroy the root Future synchronously and move remaining registered work to the owner-thread cleanup queue.

`RequestTask::status()` reaches a terminal value, and `in_flight()` decreases,
only after registered work joins or unsubscribes and its adapters are destroyed.
This is later than the root async function returning.
Inspect progress through `activity_count()` / `activity_error()`, or
`worker_activity_count(id)` / `worker_activity_error(id)` for workers.
The first stop-error code is retained. Terminal join errors are recorded as the common `FAILED` code.
The application chooses checkpoint contents and timing. If a child's final result is needed,
receive it after requesting stop, then save the checkpoint. Waiting for completion does not automatically capture state.

Each request allows 64 concurrent registrations. A full set returns `BUSY`; registration after a stop request or root completion returns `REVOKED`.
Completed work is destroyed and its slot becomes available again. Custom unregistered spawn tasks are not discovered automatically.

## Drive cleanup after Drop

```rust,ignore
drop(task); // Destroys the root; registered cleanup may still be pending.
// Call repeatedly from the frame loop or host executor on the same owner thread.
let pending = reknot::poll_cancelled_requests(&mut cx).is_pending();
```

`poll_workers` and Restart also drive this queue. The queue belongs to the owner thread,
and `poll_cancelled_requests` can keep driving it after Router Drop.
Each pass polls the requests present at its start. If their cleanup drops more
requests, it schedules another pass after restoring queue ownership. Existing
I/O waits use their own notifications; pending cleanup alone does not self-wake.
These calls may advance unfinished work from other Routers, but the return value of `poll_workers` describes only the target Router's registered workers.
Keep polling while work remains after a stop request.
The DLL generation, adapters and required owned data remain alive throughout cleanup.

If cleanup is not driven, or the owner thread exits before completion is confirmed, unfinished adapters and generation references
are deliberately retained without freeing their memory. Drop does not stand in for an asynchronous join.
Like KeepLoaded, this avoids premature release; it does not guarantee that work stops.
A panic in an unsafe external adapter method also leaves completion unconfirmed, retaining the adapter in a poisoned state.
A panic in a safe local child Future becomes a terminal Error after that Future is destroyed.
Recovery from destructor panics or panic=abort is outside the scope.

## Restart after failure

Even after join completes, a failure while stopping or finishing registered work rejects cutover with
`RouteError::ActivityFailed { generation }` and leaves admission paused.
Failures remain recorded for old Drain generations and dropped requests too.
If completion is still unconfirmed, Restart remains Pending. Set any required timeout in the host.

After reviewing the failure and its effects, the application may acknowledge the record through
`acknowledge_activity_failure(generation)`. This is rejected unless the generation's `in_flight` count is zero.
Acknowledgment does not resume admission, register failed workers again or roll back external effects.
Remove and register failed workers as needed, then explicitly retry Restart and call `resume()`.
Ordinary root errors without a registered-activity failure retain the existing Restart behavior.

## Verification

```powershell
cargo test -p reknot --test managed_activities --test native_routing --locked
cargo run -p reknot --example managed_children --locked
./scripts/check-miri.ps1 -Toolchain nightly-2026-11-02
```

`managed_activities` checks delayed child completion, normal completion and panics, capacity limits, rejection after stop,
cleanup after Drop, unsubscription failure, terminal errors and retry after acknowledgment.
Miri checks the same managed paths on the Linux interpreter target with both Stacked and Tree borrow models.
Real DLL tests run separately on native targets. See [testing](TESTING.md) for toolchains and log handling.

`native_routing` uses a real DLL independent of the macros and runs its callback from another thread.
The callback stays active until the host releases it. The test checks that state survives root and Router Drop,
and verifies callback return, thread join and adapter destruction in that order.
It also checks that a failed unsubscription request is not mistaken for completion or successful cutover, and that Handler ABI 2–4 are rejected.

The SDK does not provide ready-made adapters for every runtime or OS unsubscription API, forced termination,
external I/O rollback or automatic unloading.
Each `ManagedActivity` implementation is responsible for unsubscribing its callback source and synchronizing in-flight calls.
