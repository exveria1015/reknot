# DLL lifetime and unmanaged work

English | [日本語](../docs_ja/UNLOAD_SAFETY.md)

## Native image retention

By default, DLLs opened by `RequestRouter` remain loaded until process exit.
This includes unused candidates and candidates rejected by ABI, symbol or service validation.
Dropping a Router, completing a root request or dropping a Waker does not physically unload a DLL.
Hosts that can establish shutdown conditions, including external work and TLS, may use the low-level
`unsafe reclaim_native_image(ArtifactId)`. Handler ABI remains at version 5.

The host registry stores an owning reference immediately after loading, so code remains available after validation failure or a later unwind.
`RequestTask`, registered work and Wakers passed outside the DLL also retain references to their execution generation.
SDK host I/O uses host-owned notifications that can be invalidated when a request ends.
External clones of these notifications do not retain the DLL; wake and drop after completion run entirely in host code.
TLS values registered through `GenerationLocal<T>` are destroyed when generation ownership ends.
See [registered locals and notifications](MANAGED_LOCALS_AND_NOTIFICATIONS.md) for their scope and dependency obligations.
With static linking, code is part of the executable and no DLL copy is created.

## Explicit reclamation from the host

Get an image ID through `RequestRouter::artifact_id()` or `PreparedHandler::artifact_id()`.
The ID and the result of `retention_snapshot()` do not retain the image.
Follow these steps, calling reclamation on the thread that loaded the image:

1. Switch to a new generation or drop the Router to end admission to the image.
2. Finish old requests and registered work. Drive cancellation cleanup to completion with `poll_cancelled_requests`.
3. Drop old tasks and candidates, then call `release_retired_generations()` to release completed old generations held by the Router.
   Generations with a recorded activity failure also require confirmation through `acknowledge_activity_failure`.
4. Join external threads, unsubscribe callbacks and confirm their return, and establish shutdown conditions for TLS and dependencies.
5. Accept those obligations as the `unsafe` contract and call `reclaim_native_image(id)`.

The SDK checks that the image is retired, no code leases remain, and all host-initiated DLL calls have returned.
It rechecks these conditions and takes the owning reference while holding the registry lock, preventing references from being reacquired.
The actual `Library::close` runs after releasing SDK locks.

An external-thread DLL Waker may still be destroying objects or returning through DLL code after releasing the last host reference.
A zero reference count or a subsequent poll therefore does not confirm completion.
Use a separate mechanism, such as joining the thread, to establish that each DLL-side wake, drop and clone has fully returned and cannot run again.
Keep an image loaded if you cannot establish the state of TLS on shared threads, OS registrations or dependency cleanup.
The SDK provides neither a DLL shutdown hook that automatically establishes reclamation safety nor a dedicated thread per generation.

Each image in `retention_snapshot().images` reports `code_leases`, `active_calls` and `retention_reasons`.
The reasons are listed below. Because external completion is the host's responsibility,
`ExternalQuiescenceUnconfirmed` always appears in the snapshot.

| Reason | Required action |
| --- | --- |
| `NotRetired` | End use of the active or preparing image |
| `CodeLeases { count }` | Release tasks, candidates, Wakers and registered work. The count measures code references, not requests |
| `HostCalls { count }` | Wait for factory, validator, poll, drop and activity calls to return to the host |
| `OwnerThreadRequired` | Perform the operation on the thread that loaded the image |
| `UnvalidatedCandidate` | Retain an image whose validation did not complete. Rejection is not proof of shutdown |
| `ExternalQuiescenceUnconfirmed` | Establish external Waker returns, TLS and dependency shutdown, and satisfy the explicit reclamation contract |
| `Reclaiming` | Wait for close and its finalization to return before attempting reclamation again |
| `CloseUncertain` | Retain the image and copy after a close failure; do not reuse the consumed handle |
| `RegisteredLocals { count }` | Wait for registered generation-local ownership to end and destruction to return. The count includes uncertain residue |
| `LocalCleanupFailed { count }` | Retain an image whose registered-local destruction failed. Do not run the same destructor again |

If SDK conditions are not met, reclamation returns `ReclaimResult::Retained { reasons }`.
On success it returns `Released { copy_removal_error }`, deleting the copy after releasing the loader reference.
A copy-removal failure is reported separately and does not undo a successful close. `CloseFailed` means the release state is uncertain;
the same handle will not be closed again. An unknown or already reclaimed ID returns `NotFound`.

`Released` means the SDK released its loader reference. OS image residency, other loader references and actual memory usage must be checked separately.
`released_native_images` and `close_failure_count` expose cumulative counts.

## Code retention and work completion

Keeping code loaded does not stop tasks or extend the lifetime of borrowed data.
Work explicitly registered with `spawn_child` and `register_activity` is included in root completion.
Adapters for external work must confirm join, unsubscription and the return of in-flight callbacks, as well as request a stop.
See [managed children and callbacks](MANAGED_ACTIVITIES.md) for the full contract.

Threads, runtime tasks and external callbacks created independently by logic are not discovered automatically.
The application must ensure that they cannot continue accessing host resources or borrowed data beyond their lifetime.
Low-level `unsafe FfiFuture::new` likewise leaves code and data lifetime guarantees to its caller.

## Retention and cleanup

Repeated updates accumulate code, static data and copies.
The shared `GenerationBudget` limits cumulative load attempts, while `retention_snapshot()` reports retained images.
Successful explicit reclamation does not refund attempts.
Restart the development process when the budget is exhausted. This mechanism does not guarantee a memory ceiling.

Copies live under `reknot/managed-v1/sessions/<SessionId>/copies/` in the user's temporary directory.
After a session exits, `reknot-cache clean --apply` removes eligible copies after acquiring its exclusive session lease.
This operates on files from exited sessions; it does not unload running code.
Legacy PID directories, links and unknown entries are not deleted automatically. See [retention and cleanup](RETENTION_AND_CLEANUP.md) for the conditions.

## Verification

`native_routing` checks that old requests can still be polled and dropped after Router destruction, and that external callbacks are joined.
`cache_lifecycle` checks retention of unused and rejected candidates, retention after all Routers are dropped, cross-process leases and cleanup after exit.
`native_reclamation` uses real DLLs to check default retention, references held by retired generations, completed handles and candidates,
active-call records visible during host reentry, the interval between an external Waker's last host release and its DLL return, and 40 consecutive cutovers.
OS residency checks are implemented for Windows and Linux. Windows tests also exercise an actual copy-removal failure.
Close-failure handling is tested by injecting a return value in unit tests, without creating invalid OS handles.
The `task` unit tests cover owned Waker references and wake/drop from another thread.
Miri checks ownership and borrowing paths without real DLLs. See [testing](TESTING.md) for commands and limits.
