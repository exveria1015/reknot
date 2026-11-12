# Registered TLS and host-owned notifications

English | [日本語](../docs_ja/MANAGED_LOCALS_AND_NOTIFICATIONS.md)

The framework manages notifications routed through the SDK and the lifetime of explicitly registered generation-local values.
Applications or dedicated adapters implement dependency-specific shutdown.
Completion of that shutdown remains a condition for DLL reclamation.

| Area | Managed by the SDK | Confirmed by the application |
| --- | --- | --- |
| External notifications | Host I/O notifications from `call_async`, C notification handles from `HostNotification` | DLL-side Rust Wakers passed directly to external runtimes, custom callbacks |
| TLS | Values registered through `GenerationLocal<T>`, tied to an owner thread and generation | Arbitrary `std::thread_local!` values, unregistered TLS in unmanaged threads or dependencies |
| Dependency shutdown | The common stop/join contract of `ManagedActivity` | Dependency-specific stop APIs, unsubscription, required destruction order and OS deregistration |

## Values tied to a generation and owner thread

`GenerationLocal<T>` is a static key for registered TLS. Its value is registered with the host's generation management
and shared within the same generation of the same Router. Other Routers and new generations have independent values.
Routers, requests and registered work remain on their owner thread, and values are accessed only on that thread.

```rust
use reknot::{GenerationLocal, RequestContext, ServiceError};

static BUFFER: GenerationLocal<Vec<u8>> = GenerationLocal::new(Vec::new);

fn append(ctx: &RequestContext) -> Result<usize, ServiceError> {
    BUFFER.with_mut(ctx, |buffer| {
        buffer.extend_from_slice(ctx.input());
        buffer.len()
    })
}
```

Values initialize on first access. `with` takes a shared borrow; `with_mut` takes an exclusive borrow.
Borrows stay inside the closure: references to a value cannot escape. Conflicting borrows and reentry into the same key during initialization return
`BUSY`. A panic during initialization or access returns `PANICKED`. Neither values nor external effects are rolled back.
`MAX_GENERATION_LOCALS` allows 64 keys per generation. It does not bound the number of bytes stored.

Releasing the generation's last owning reference destroys values in reverse initialization order.
Old requests and completed `RequestTask` handles also retain the generation. Drop those tasks and,
when needed, call `release_retired_generations()`. Values are destroyed while the DLL is valid,
and code stays protected until the destructor returns to the host. Access to that generation's locals is rejected once destruction begins.

If a destructor panics, the state is treated as uncertain. Code stays loaded and the same destructor is never retried.
Observe this through the native image's `registered_locals` and `local_cleanup_failures`, and the retention reasons
`RegisteredLocals` and `LocalCleanupFailed`. The registered count includes uncertain residue.
If other DLL-owned objects remain after work has finished, their ownership must also be released.

This mechanism does not enumerate arbitrary TLS. It does not create or join dedicated generation threads,
or add dependency-specific TLS cleanup. Keep using KeepLoaded when a dependency's shutdown state is unknown.

## External notifications that do not call the DLL

The SDK's `ctx.call_async` / `call_service_async` pass host-owned notifications to host I/O.
Creation, clone, wake and drop of the I/O-side Waker run entirely in host code.
The host executor's Waker must also remain valid independently of any reloadable generation.

1. When polling a request, update the notification target to its owner-thread executor.
2. External I/O wakes that executor. DLL execution advances through ordinary managed polling.
3. Keep the notification active while registered work remains, even after the root ends. The host continues driving cleanup after cancellation.
4. Detach the target once registered work also finishes. Delayed notifications, clones and drops then run entirely in host code, even after DLL reclamation.

When notification races with completion, a host notification acquired before detachment may still arrive.
It cannot reenter the DLL to run a completed request. Detaching a notification also does not mark unfinished registered work as complete.
The host gate contains executor notification and final Waker destructor panics
separately, including when completion detaches the target during notification.
This applies to unwinding panics; it cannot recover from `panic=abort`.

Use `ctx.host_notification()?` for a custom C notification path.
`HostNotification::as_raw()` returns a borrowed `HostWaker` descriptor.
If external code needs to retain it, acquire an owned handle through its vtable's `clone`. For each owned handle,
call exactly one `drop` or ownership-consuming `wake`. `wake_by_ref` does not consume ownership.
Keep the Rust wrapper on the owner thread and pass the C notification handle to external code.
If you wrap it in a Rust Waker inside the DLL before passing it outside, you must separately guarantee that DLL code's lifetime.

## ABI and reclamation contract

The existing layouts of Handler ABI 5 and task-control ABI 2 remain unchanged.
Notifications and registered values use the optional Generation services extension ABI 1 at the end of the task-control table.
The SDK checks the table size and extension header before reading extension callbacks.
New APIs return `UNSUPPORTED_OPERATION` for an old host or unknown extension.
On an old host, `call_async` falls back to the notification path with the previous retention contract.

Completing this managed cleanup does not prove that dependency-specific work, unregistered TLS or unmanaged callbacks
have finished. The caller of `unsafe reclaim_native_image` must establish those conditions.
KeepLoaded remains the default, and successful reclamation does not refund the cumulative load budget.

## Verification

```powershell
cargo test -p reknot --test generation_locals --locked
cargo test -p reknot --test native_managed_generation --locked -- --test-threads=1
```

Static tests cover shared and separate values, borrowing, panics, and rejection of old tables and unknown extensions.
DLLs built with the actual SDK cover cancellation cleanup, destruction of registered values, delayed notifications after DLL reclamation,
and retention with explicit reasons after destructor failure. These run in Windows and Linux CI.
Both Miri borrow models also check Rust ownership, borrowing and notification races, separately from real-loader tests.
