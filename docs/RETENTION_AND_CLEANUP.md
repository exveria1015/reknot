# Observe retention and clean up copies

English | [日本語](../docs_ja/RETENTION_AND_CLEANUP.md)

The SDK provides in-process DLL reference observation, process-lifetime sessions and explicit copy cleanup.
Native images remain loaded until process exit by default. The load-budget and Handler ABI 5 contracts remain unchanged.
Hosts that can establish shutdown conditions may explicitly reclaim images; see [DLL lifetime](UNLOAD_SAFETY.md).

## Observe from the host

```rust,ignore
let snapshot = reknot::retention_snapshot();
println!("native_images={} recorded_copy_bytes={}",
    snapshot.tracked_native_images, snapshot.recorded_copy_bytes);
for image in snapshot.images {
    println!("{:?} {:?} {:?}: {} {:?}", image.id, image.state,
        image.outcome, image.copy.display(), image.retention_reasons);
}
println!("attempts={}/{}", router.options().generations.used(),
    router.options().generations.limit());
```

`retention_snapshot()` counts native references retained by this SDK instance in the host.
It distinguishes active, preparing and retired images, and tracks validation rejection and candidate Drop. Multiple candidates
prepared from the same Router generation each receive a separate `ArtifactId`. Static APIs do not count as native images.
A snapshot owns only values; it does not extend the lifetime of images, Futures or Wakers.

`recorded_copy_bytes` is the sum of retained images' file lengths recorded when their copies were created.
It is separate from actual memory usage, the OS module count and current disk usage.
`GenerationBudget::used()` is also a cumulative attempt count; cleanup and image release do not reduce it.
`released_native_images` counts successful releases of SDK loader references. `close_failure_count` counts failed closes.
Failed images retain a `close_error` and remain included in the image count and recorded copy size as uncertain retentions.
These counters do not measure disappearance from the OS or memory reclaimed.
Copy and native-open failures have cumulative counts. Copy-removal errors include the most recent 32 entries and a cumulative count.

`registered_locals` reports remaining registered TLS values; `local_cleanup_failures` reports destruction failures.
Both can prevent reclamation, and a failed destructor is never retried.
See [registered TLS and host-owned notifications](MANAGED_LOCALS_AND_NOTIFICATIONS.md) for the scope of managed notifications and values.

## Inspect copies with the CLI

Run these commands from the repository root:

```powershell
cargo run -p reknot --bin reknot-cache --locked -- inspect
cargo run -p reknot --bin reknot-cache --locked -- clean
cargo run -p reknot --bin reknot-cache --locked -- clean --apply
```

`inspect` and `clean` without `--apply` are read-only. They neither create missing management files nor delete files.
Apply rechecks the current state instead of trusting a previously displayed plan.
The external CLI does not inspect another running process's memory usage or retained-image count.

The default root is `<temp>/reknot/`. Before starting the host, set `REKNOT_CACHE_ROOT`
to a dedicated, local absolute path if needed. The SDK instance fixes its root on the first native load.
Use the same root through CLI `--root` or API `CacheOptions::root`.
Relative paths, `..` components, and roots reached through links or junctions are rejected.

```powershell
cargo run -p reknot --bin reknot-cache --locked -- inspect --root C:\scratch\reknot-cache
cargo run -p reknot --bin reknot-cache --locked -- clean --apply --max-sessions 100 --max-entries 10000
```

Default scan limits are 1,024 sessions and 65,536 directory entries. Unknown and legacy entries also count toward the entry limit.
Truncation or unreadable entries produce `partial=true`; those totals are incomplete.
Counts and bytes describe the selected copy files. They do not guarantee an equivalent increase in OS-reported free space.

| Status | Meaning |
| --- | --- |
| InUse | The current SDK's session, or a session whose lease cannot be acquired. Cleanup does not infer whether another process is alive and delete on that basis |
| Eligible | A candidate that passed new-format, structure and permission checks, with its lease acquired |
| Trash | Confirmed unused and moved to the deletion-only area; deletion can be retried |
| Legacy | An old PID-only format. Paths and sizes are reported, but even apply does not delete it |
| Unknown | Unknown or incomplete management data, unknown files, links, permission issues or similar problems. Not deleted |
| Deleted | Cleanup of the session completed |
| PartialFailure | Processing trash failed. Reports deleted counts/bytes and errors; the next apply can retry |

CLI exit codes are 0 for a complete scan or operation, 1 for argument, root, lock or similar errors, and 2 for partial results or failures.
Normally skipped InUse or Legacy entries are not errors.

## Migrating from flow-reloader

The default cache root is now `<temp>/reknot/`. Existing files under
`<temp>/flow-reloader/` are not moved or deleted automatically. Inspect that
directory explicitly with `reknot-cache inspect --root <old-absolute-root>`.
If you choose to clean it, use the same root with `clean --apply`; only eligible
managed sessions are removed, under the same lease and permission checks.

The stored `managed-v1` format, including its original marker, is unchanged.
`FLOW_RELOAD_CACHE_ROOT` remains a fallback alias when `REKNOT_CACHE_ROOT` is not
set. If both are set, `REKNOT_CACHE_ROOT` takes precedence.

## Sessions and exclusive access

New copies use `managed-v1/sessions/<SessionId>/copies/<ArtifactId>.<ext>`.
Each process/SDK instance uses one session.
Unique IDs and create-new operations prevent overwrites. A PID or timestamp alone never authorizes deletion.

The host holds an exclusive lock on the session's `lease.lock` until process exit.
Dropping the last Router, finishing all tasks or failing to prepare a candidate does not release it.
Child processes do not inherit the handle. Unloading the SDK itself mid-process or passing a copy to another process as a load source is unsupported.

Cleanup attempts to acquire `namespace.lock` and the target lease without blocking.
If either is unavailable, it skips the operation. Only after acquiring them does it move the session to `trash/<SessionId>` and delete known regular files individually.
Trash left after interruption is never restored as a load source. Metadata stays until the end; remaining empty directories are removed last.
Cleanup does not recursively delete whole trees or remove original build artifacts, project `target/` directories, databases or Cargo caches.

The namespace lock serializes session creation and cleanup. Contention returns WouldBlock or a CLI error;
the host may retry loading later. Ordinary loading and execution in an existing session do not use this lock.
There is no automatic unload, automatic restart or reuse of consumed load-budget slots.

## Permissions and supported environments

Use a dedicated local directory belonging to the same user, with hosts that follow the same protocol.
New Unix directories use mode 0700 and files use 0600.
On Windows, only newly created directories receive a protected, inheritable DACL allowing the user, SYSTEM and Administrators.
Existing permissions are not changed. Ownership and write access by other principals are checked, and unsuitable directories are rejected.
The implementation uses the Windows [DACL configuration API](https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-setnamedsecurityinfow).

Unverifiable permissions, unknown ACLs, unsupported locking and I/O errors are not treated as evidence that a session has exited.
A root under shared `/tmp` is also rejected if it belongs to another user; specify your own dedicated root when needed.
Unmanaged concurrent modification, attacker-controlled roots and network-filesystem locking are outside the supported scope.
This mechanism does not detect arbitrary users directly loading a DLL.

## Verification

```powershell
cargo test -p reknot --test cache_lifecycle --locked -- --test-threads=1
```

Tests use separate processes for the parent host, child host and cleanup CLI to check:

- Active images, multiple candidates from one generation, rejection, Drop and retirement with real DLLs; distinction from static APIs and attempt budgets.
- No deletion while a host is alive, even after all Routers are dropped; eligibility only after normal or forced process exit.
- A surviving child process does not inherit its exited parent host's lease.
- Namespace contention, concurrent apply from two CLIs, lease acquisition after inspection, corrupt or oversized metadata, missing leases, unknown files, legacy entries and scan limits.
- Junctions and sentinel files at link targets remain untouched; partial trash states, actual file-deletion failure and retry.
- Read-only CLI defaults and deletion only through explicit apply.

Miri also checks pure Rust ownership and execution paths, but does not replace file-lock, DACL or real-DLL tests.
The two ignored functions are child-process entry points launched by the parent tests.
Parent-test setup is serialized internally; each test explicitly creates the process contention it needs to verify.

See [testing](TESTING.md) for commands and the full verification scope.
