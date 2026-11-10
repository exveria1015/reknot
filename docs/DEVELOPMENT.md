# Development operation

English | [日本語](../docs_ja/DEVELOPMENT.md)

The typed API's `DevModule` handles edit detection, builds and candidate activation.
This page explains the usual development workflow and the conditions for a custom host.

## Build and switch

On the first launch, the host builds and loads the logic before accepting requests.
After startup, it calls `DevModule::update()` regularly to drive this sequence:

1. Receive source-change notifications. The default debounce interval is 200 ms.
2. Build in the background. Only one build runs at a time.
3. Check the finished candidate's contracts, then switch new admissions if validation succeeds.

The current generation can accept work during a build. Already admitted requests finish on their selected generation.
If more edit notifications arrive, the controller discards the superseded build result and rebuilds.

`update()` also drives bridge requests and cancellation cleanup.
Keep calling it while waiting for input, running requests and awaiting shutdown.
Dropping `DevModule` stops watching and applying updates, but an already started Cargo process runs to completion.

## Watched source inputs

Cargo's dependency graph identifies inputs for the logic and its local dependencies.

| Watched input | Coverage |
| --- | --- |
| Rust source | `src/` in the logic and local dependencies, plus Cargo target source files |
| Crate configuration | Each `Cargo.toml` and `build.rs` |
| Workspace configuration | `Cargo.toml`, `Cargo.lock`, and `config` / `config.toml` inside an existing `.cargo` directory |

Build inputs exclude `target/`, copied DLLs and logs.
Overlapping source and build-output paths are rejected. Avoid writing generated files under source directories in custom build scripts too.

After changing an include file or generated input outside the watch set, also save a watched source file or restart the host.
Restart after creating a new `.cargo` directory as well.

Dependency declaration changes refresh the watch set. A build under that new watch set must complete before activation.
Builds use `--locked`, so update `Cargo.lock` with ordinary Cargo commands first when adding dependencies requires it.
After SDK or ABI changes, rebuild both host and logic and restart the host.

Watching is based on notifications delivered by the OS.
It does not guarantee delivery without loss or delay, or an atomic snapshot of the entire filesystem.

## Recover from failure

Failures during an update after startup are handled as follows:

| Situation | Behavior and action |
| --- | --- |
| Compilation error | Keep the running generation. Read the log, fix the code and save |
| Candidate contract mismatch | Keep the current admission target. Check the shared protocol; for an intended contract change, increment its version and restart the host too |
| Watch error | Check the reported path. Restart the host if saving no longer triggers updates |
| Load budget exhausted | Stop loading and restart the development host |

Failed requests and completed events are not automatically replayed.
Runtime panics and application errors are separate from keeping the old generation after a failed build.
For checks by symptom, see [troubleshooting](TROUBLESHOOTING.md).

## Limits and observation

Record the admission generation, active request count, build/load errors, pending shutdowns, load budget and DLL copy paths to follow cutovers.
When using Restart, also record worker states and checkpoint sizes.

`in_flight()` counts root requests, including requests waiting for registered child cleanup.
Dropping a request does not reduce the count while registered work remains. It does not count unmanaged tasks, unregistered callbacks or DLLs.

The default load budget is 32 attempts. It counts cumulative load attempts, including failed and abandoned candidates; it does not bound memory use.
Share the same `GenerationBudget` when recreating routers to keep one budget for the session.
Explicit DLL reclamation does not refund consumed attempts.

See the [API reference](API_REFERENCE.md#capacity-and-admission-limits) for typed input/output and queue limits.
Lower-level hosts must set their own input, send-queue and I/O limits.
The default Restart checkpoint limit is 1 MiB per worker.

### DLLs and copy files

With the default `KeepLoaded` policy, DLLs and their copies accumulate.
The typed host releases completed retired router references, but does not automatically reclaim DLLs.
To implement explicit reclamation, record `host.artifact_id()` before switching and follow the [reclamation contract](UNLOAD_SAFETY.md).
A deferred reclamation attempt is separate from switching new admissions.

Copies live in the user's temporary directory under `reknot/managed-v1/sessions/<SessionId>/copies/`.
Use `retention_snapshot()` to inspect SDK retention and `reknot-cache inspect` to inspect files.
`reknot-cache clean --apply` removes eligible exited sessions.
Legacy `<PID>/` entries and unknown entries are not deleted, even with apply enabled.
See [retention and copy cleanup](RETENTION_AND_CLEANUP.md) for the procedure.

## Control updates through the lower-level API

`RequestRouter` itself does not start Cargo or a source watcher.
`service_host` builds synchronously; the Tauri sample builds in a background child process.
The `dev-tools` feature provides build, path and watch helpers in `reknot::development`.
You can also supply artifacts produced by your own build process.

After building, use `unsafe prepare(path)` to load and check a candidate. Use `activate` for Drain,
or `restart` to stop, migrate and recreate registered workers.
Prepare again from the target router's current generation if a candidate is stale or belongs to another router.

Candidate preparation must not change shared resources or start external sends. DLL initialization code has the same obligation.
Keep service contracts usable by coexisting old and new generations.
ABI, service and schema checks do not prove data semantics or native-code safety.

### Restart failure and interruption

While Restart waits for shutdown, keep polling ordinary tasks owned outside the router too.
The host may impose a waiting deadline, but a timeout on the same thread cannot safely force-stop a poll that never returns.

If migration fails after admission closes, or the Restart future is dropped, admission stays closed and old checkpoints remain available.
Retry, or choose a recovery policy before calling `resume()?`. `resume` neither recreates old workers nor undoes I/O.
See [Restart and state migration](RESTART_AND_MIGRATION.md).

Production rollout, crash isolation and automatic rollback of external effects are outside the scope.
See [compatibility and scope](COMPATIBILITY.md) for the conditions of use.

[Project README](README.md) · [Documentation index](INDEX.md)
