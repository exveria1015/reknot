# Host resources and Drain admission cutover

English | [日本語](../docs_ja/HOST_RESOURCES_AND_ROUTING.md)

This guide describes the message API contract for Handler ABI 5 and Services ABI 2.
Cooperative shutdown and state migration for persistent workers are covered in [Restart](RESTART_AND_MIGRATION.md).

## Roles and scope

The host owns connections, sessions and other resources. DLLs call versioned service operations.
`RequestRouter` chooses the generation for new requests; versioned host services provide access to state.
This guide covers **Drain** and resource operations. The SDK also supports cooperative Restart and checkpoint migration for registered workers,
and [stop/join for explicitly registered children](MANAGED_ACTIVITIES.md). Unregistered work is not discovered automatically.

| Type | Responsibility |
| --- | --- |
| `HostResources` | Registers, validates IDs for and revokes host resources. Its lifetime is independent of logic generations |
| `Resource<T>` | Host-only typed access through `try_borrow`, `try_borrow_mut` and `id()` |
| `Services` | An owned reference to host services. Even clone/drop in the DLL call back through the host's function table |
| `RequestContext` | DLL-owned input copy and retained services; target resource, service version and execution generation ID |
| `RequestRouter` | Generation for new admission, candidate preparation/activation, pause/resume |
| `RequestTask` | One execution and its old generation's lifetime; tracks completion, error and cancellation separately |

Use all of these on their creating thread. `Services` and tasks are not Send.
A server can run concurrent tasks with a local executor on the owner thread.
See [asynchronous services and multiple threads](ASYNC_SERVICES_AND_THREADS.md) for host-side sharing across owner threads.
Cross-thread Waker notifications follow the host Waker ABI and DLL generation-retention contracts.

## Register and revoke resources

Implement `HostResource::call` in the host and register a value with `register(ServiceKey::new(id, version), value)`.
The application defines service IDs, versions, operation numbers and input/output byte formats.
Only IDs, C ABI function tables and bytes borrowed for the duration of a call cross the DLL boundary. The layout of `T` and Rust containers are not shared.

A `ResourceId` contains a registry, slot and slot generation.
Reusing a slot after `revoke(id)` does not make an old ID refer to the new resource.
Copying an ID does not confer ownership. Dropping `Resource<T>` alone does not remove its registration; revoke it explicitly.
A host-held `Resource<T>` may keep the value alive after revocation, but DLL operations return REVOKED.
These IDs are references between trusted code, not access controls against malicious native code.

Each `Services` retains the host registry. Existing tasks and explicitly cloned service references remain valid after the original `HostResources` or Router is dropped.
Registered resources are destroyed when the last owning reference is released. Break cycles, such as a resource holding Services for its own registry, through revocation or another explicit step.

## Service operations

```rust,ignore
impl HostResource for Session {
    fn call(&mut self, operation: u32, input: &[u8], output: &mut [u8])
        -> Result<usize, ServiceError>
    {
        // Run the versioned operation and return the number of bytes written to output.
        // Queue network writes for the host I/O loop.
        todo!()
    }
}
```

Keep operations **short and synchronous**. Actual socket I/O, receive buffers, TLS and reconnection decisions belong to the host or adapter.
See [asynchronous services](ASYNC_SERVICES_AND_THREADS.md) for `start_async` and `call_async`, which use an owned host Future.
If the host's `Resource<T>` is borrowed, or the same resource is reentered, the call returns BUSY instead of creating overlapping mutable references.
Exclusion covers one operation, not a transaction across multiple operations and awaits. Define consistency between old/new tasks and response ordering in the host protocol.

The main errors are REVOKED, CONTRACT_MISMATCH, BUSY, BUFFER_TOO_SMALL, INVALID_ARGUMENT and PANICKED.
Unwinding from a host operation is caught within FFI. Partial updates already made are not rolled back.
An implementation reporting more output than fits in the buffer is also rejected. There is no automatic retry for insufficient capacity, so the host implementation must check space before starting an irreversible operation.
Recovery is not guaranteed from double panics during destruction, panic=abort or unsafe native code that violates the contract.

## Handlers

```rust
use reknot::{RequestContext, ServiceError, ServiceKey};

#[reknot::reload_handler(ServiceKey::new(0x4c49_4e45, 1))]
async fn handle(request: RequestContext) -> Result<(), ServiceError> {
    request.call(0, request.input(), &mut [])?;
    Ok(())
}
```

The macro generates `hot_handler_api` and an opaque Future per execution.
During the factory call, input and the initial checkpoint are copied into the DLL, and services and task control are retained. The Future does not store host-stack descriptors or input buffers.
The handler body starts on its first poll. Errors and unwinding become RunStatus::Error, and the Future is destroyed exactly once.
The original ServiceError details are currently unavailable through the RequestTask result.

## Prepare and activate a candidate

```rust,ignore
let resources = HostResources::new();
let session = resources.register(LINE_SESSION, host_owned_session);

// Verify trusted SDK code initialization, finalization and FFI ownership contracts.
let mut router = unsafe { RequestRouter::load(old_library, resources.clone()) }?;
let old_task = router.dispatch(session.id(), b"first message")?;

let candidate = unsafe { router.prepare(new_library) }?;
// New requests still use the old generation up to this point.
router.activate(candidate)?;
let new_task = router.dispatch(session.id(), b"next message")?;
// old_task runs old code; new_task runs new code.
```

Prepare copies and loads the DLL, checks its stable header and API table, and verifies the target service ID and version.
Admission stays unchanged during preparation. Candidate DLL initialization must not begin external sends, launch tasks or start similar effects.
Dropping a candidate without activation leaves admission unchanged.

Activate changes admission in one step on the Router's owner thread. Running tasks remain on their original code.
A candidate for another Router, or one made stale by activation of another candidate, is rejected.
Dispatch during `pause()` returns Paused. Activating while paused does not resume admission; call `resume()?` explicitly.
The SDK neither buffers unlimited input internally nor automatically retries failed requests.

`in_flight()` counts unfinished requests managed by the Router. It decreases after the root and explicitly registered work are joined and destroyed.
Registered work remaining after cancellation or Drop still counts as unfinished and advances through the owner-thread cleanup queue.
The count does not measure remaining Wakers, unregistered callbacks, retained DLLs or unregistered tasks launched by the application.

Matching service versions cannot detect implementations that assign different meanings to the same version.
The Router checks the primary service and declared additional requirements. Each operation also checks the target resource's version.
Restart migrates registered-worker checkpoints. See [contracts and initialization](SERVICE_CONTRACTS_AND_INIT.md) for additional service requirements, explicit bindings
and typed initialization-data validation.
The SDK does not validate compatibility of all shared state.
Both Drain and Restart reject candidates that change the primary service version. Additional services may bind a candidate to resources with different versions.
For incompatible host-resource changes, use `restart_with_commit` after stopping work to perform application-specific exclusion and transactions.
See [host resource and database migration](HOST_RESOURCE_MIGRATION.md).

## DLL lifetime and ABI

This path checks Handler ABI **5** and Services ABI **2** through separate headers.
Rust allocators do not need to be shared; each side destroys the containers it allocates.

RequestTask retains the DLL generation that created it and passes code references to Wakers.
DLLs on this path use KeepLoaded. Ordinary cutover does not attempt physical unload, including after candidate load failure or Drop.
A shared GenerationBudget limits cumulative load attempts. It defaults to 32 slots; failed candidates consume slots and slots are not automatically refunded.
There is no automatic reclamation. Restart the development process when the budget is exhausted. See [retention limits](RESTART_AND_MIGRATION.md#retention-limits).
from_api and prepare_api are unsafe extension points for static linking and similar uses. Their caller must guarantee the lifetime of callbacks and code.

## Example and verification

```powershell
cargo run -p service_host --locked
```

The default address is 127.0.0.1:4000; set REKNOT_ADDR to change it.
Send newline-delimited text, then edit the response label in `examples/service_logic/src/lib.rs` and save.
The next message on the same connection uses the new generation. Send `slow` followed by a newline to start a five-second operation; an update during that operation leaves it running on the old code until completion.
The example uses one connection and one thread. Builds are synchronous, and edits made while waiting for input are handled at the next message boundary.

`native_routing` checks connection preservation and Drain, `host_resources` checks ownership and revocation, and `request_routing` checks admission and candidates.
See [testing](TESTING.md) for commands and the scope of Miri versus real-DLL tests.
