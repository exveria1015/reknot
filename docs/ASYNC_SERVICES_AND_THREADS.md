# Asynchronous host services and multiple threads

English | [日本語](../docs_ja/ASYNC_SERVICES_AND_THREADS.md)

The host owns I/O and execution threads. Logic DLLs send copied bytes and await results.
The current versions are **Handler ABI 5 / Services ABI 2 / task-control ABI 2 / Activity ABI 1**.
Rebuild the host and DLL together. Handler ABI 2–4 are rejected at load time.

## Call from logic

```rust
let reply = ctx.call_async(1, b"ping", 64)?.await?;
ctx.call(2, &reply, &mut [])?;
```

`call_async(operation, input, max_output_bytes)` copies the input and returns a `ServiceCall`.
The operation starts only when polled on the owner thread after successful registration.
The host returns an owned `HostServiceFuture` from `HostResource::start_async`.
Synchronous `HostResource::call` remains available. Each method's default implementation returns `UNSUPPORTED_OPERATION`.

Keep the start function short and synchronous. Its returned Future owns the I/O and required data.
A failed start must leave no operation running, and polling the Future must not block.
`Ready` means the I/O has finished. Do not return `Ready` merely because cancellation was requested,
or detach I/O that still needs to be joined.

`call_async` participates in [registered-work management](MANAGED_ACTIVITIES.md).
If the root returns, the `ServiceCall` is dropped, or the root is cancelled or dropped,
it requests stop and retains the registered work and unfinished count until the actual I/O confirms completion.
After Drop, keep polling through `poll_cancelled_requests`, `poll_workers` or Restart.
Revoking a resource requests stop for existing operations but does not prematurely invalidate the IDs needed to join them.
Their eventual results become `REVOKED`, so stale results are not applied to a new resource.

Cancellation returns `CANCELLED`, resource revocation returns `REVOKED`, a full service returns `BUSY`,
and admission after service shutdown returns `CLOSED`. Ordinary I/O errors are returned as call results.
An I/O error is separate from an activity failure caused by inability to confirm completion.
If a persistent worker should treat `CANCELLED` during shutdown as a normal stop, it must handle that case and return `Ok(())`.
Propagating the error with `?` makes the root end in Error.

## Limits and result transfer

| Constraint | Default / behavior |
| --- | --- |
| Registry operation slots | `AsyncLimits::default()` allows 64 operations, including completed but uncollected results |
| Input and output | 64 KiB each by default; configurable with `HostResources::with_async_limits`. Each buffer is capped at 1 MiB |
| Per-call output length | Set by the third argument to `call_async`. Values above the host limit are rejected before starting |
| Oversized data | Input or output beyond the limit returns `INVALID_ARGUMENT`. Results are never truncated and reported as successful |
| Backpressure | Full capacity returns `BUSY`. No automatic waiting for admission, retry or replay of ordinary requests |
| Operation IDs | Registry ID plus a sequence number that is never reused. IDs from another registry or already released operations return `REVOKED` |

The DLL boundary uses C ABI IDs, status values and buffers; it does not share Rust `Vec`s, Futures or connections.
Results are copied from the host and the operation is collected. If a low-level ABI output buffer is too small,
the call reports the required size and retains the completed result. It does not execute the operation again.
The ID is released after its terminal result is collected. Releasing a pending ID early requests stop and retains the operation.

If polling a host Future panics, completion remains unconfirmed: the Future is retained and subsequent polls stay Pending.
There is no general automatic recovery from this state. Limits apply to data retained by the SDK and queues;
they do not bound separately allocated application memory or peak usage inside an I/O handler.

## The host I/O thread

`ThreadService::spawn(limits, handler)` returns a `ServiceClient` and `ServiceWorker`.
It uses standard-library threads, Mutex and Condvar without adding a required runtime dependency.

- `ServiceClient` is a safe `Send + Sync` entry point within the host. It transfers only copied input, an operation number and synchronized reply state.
- `ServiceWorker` belongs to the creating thread. `shutdown()` requests stop for queued and running work.
- Awaiting `ServiceWorker` actually joins the thread. Drop requests shutdown; it does not confirm a completed join.
- The I/O handler closure and I/O resources run on the worker thread. Keep the closure in trusted host code.
- The handler must check `CancellationToken::is_cancelled()` and bound waits through mechanisms such as socket timeouts.

ThreadService capacity includes queued work, running work and uncollected replies, shared across all clients.
Slots remain occupied until completed results are consumed and the necessary owners release them.
Input and output sizes have independent limits.
A panic in the I/O handler becomes `PANICKED` for that operation. Recovery is not guaranteed for destructor panics,
unwinding across FFI, panic=abort or failures in arbitrary host Futures.

## Fairness and thread ownership

Each ThreadService has one I/O thread. Accepted requests run one at a time,
in FIFO queue-registration order. There is no guarantee of ordering between simultaneous arrivals on different threads,
reserved capacity per client or a maximum execution time. If the first I/O operation ignores stop requests, later work must wait.
The application chooses how to separate long and short operations, for example by using different ThreadServices.

On each logic thread, create, poll and destroy its HostResources, RequestRouter and RequestTask on that same thread.
SDK handles and raw pointers cannot be passed to another thread and treated as Send.
Register a clone of ServiceClient as a local host resource on each thread.
Resource IDs and operation IDs are independent across registries.

On supporting hosts, notifications use a host-owned C notification handle and host-side Rust Waker to wake the original owner thread.
Once the request and its registered work finish, the target is detached. Notification clone, wake and drop left in host I/O then do not call the DLL.
Older hosts fall back to the previous FFI bridge. See [registered locals and notifications](MANAGED_LOCALS_AND_NOTIFICATIONS.md).
Being able to wake from another thread does not make the task itself movable.
Input moves to the I/O thread as host-owned data; results are collected on the logic's owner thread.
Dropping a Waker on an external thread still follows generation-retention contracts. There is no automatic unloading.

## Run and verify

```powershell
cargo run -p threaded_io_host --locked
cargo test -p reknot --test async_services --test threaded_services --locked
cargo test -p threaded_io_host --test native_threaded_io --locked -- --ignored --test-threads=1 --nocapture
./scripts/check-miri.ps1 -Toolchain nightly-2026-11-02
```

The example builds separate v1 and v2 logic DLLs without editing sources, performs a finite set of checks, then exits.
Build output is isolated under `target/threaded-io-build/<PID>/`.
A host I/O thread receives real TCP responses. Two logic owner threads Drain while old requests remain unfinished,
then receive results from both generations.
Finally, each owner thread destroys its logic resources, and the creating thread joins the I/O worker.

Native tests use real DLLs and TCP to check cancellation during a read, socket release, no repeated sends,
full queues, capacity held by uncollected results, FIFO ordering, no I/O for cancelled queued requests, and owner-thread behavior.
Miri checks asynchronous services and synchronization, notifications and destruction across multiple owner threads, without real TCP, under both borrow models.
Success on Miri's Linux interpreter target is not evidence of real Linux DLL or network execution.
See [testing](TESTING.md) for commands and the distinction between verification scopes.

The current implementation does not move arbitrary Futures between threads, transfer DLL tasks to a thread pool,
roll back I/O or force work to stop. For explicit checks of multiple service contracts, see
[service contracts and initialization](SERVICE_CONTRACTS_AND_INIT.md).
