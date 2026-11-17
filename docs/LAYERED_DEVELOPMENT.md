# Layered development and managed object reclamation

English | [日本語](../docs_ja/LAYERED_DEVELOPMENT.md)

The experimental layered route supports Windows x64 MSVC (COFF) and Linux x86_64 GNU (ELF).
It requires compiler-matched resident std/support, rustup/Cargo and the platform linker.
`LayeredDevOptions::builtin` uses the SDK's Rust producer without launching PowerShell/Python.
`jit-dev` includes `jit` and `notify`; `native` and `typed` remain optional.

Follow the [COFF](JIT_CODE_OWNERSHIP.md) or [ELF](ELF_JIT.md) logic/support layout.
Pass the support Cargo target name without a platform prefix/extension. Handler ABI 5,
identity checks, Managed collection and the admission states below are shared.
The existing `LayeredDevOptions::new` script path remains available; only that route requires
PowerShell 7 on Windows or Python 3.9+ on Linux.

## Starting and driving development

For typed functions and existing `ModuleHost` clients, [use the layered backend of
DevModule](TYPED_JIT.md). It shares this builder, admission and collection pipeline.

```rust,no_run
use reknot::HostResources;
use reknot::jit::development::{LayeredDevOptions, LayeredDevSession};

# fn example() -> Result<(), String> {
let mut options = LayeredDevOptions::builtin(
    "project/Cargo.toml", "my_logic", "my_support",
    "project/target/layered-cache",
);
options.toolchain = "stable".into();
let resources = HostResources::new(); // Register the application's services here.
// Trust this project's producer, dependencies and future edits for native execution.
let mut session = unsafe { LayeredDevSession::start(options, resources)? };
// Call regularly from the existing owner-thread event loop:
if let Some(event) = session.poll_update() {
    eprintln!("{:?}: {:?}", event.state, event.diagnostic);
}
// Before the first Activated event, router() is None. Dispatch and poll requests
// through the host executor; continue driving poll_cancelled_requests as usual.
// At application shutdown, join the build worker explicitly:
session.shutdown()?;
# Ok(()) }
```

Initial discovery/setup can block in `start`. Watches are installed before the first
build. Subsequent discovery, hashing, Cargo and artifact capture run on one resident
host worker; `poll_update` never waits for that worker. Linking, entry, validation and
activation still execute on the owner thread. There is one running job, one latest
observed revision, and a result channel of capacity one. Captured process output and
diagnostics are bounded; the session stores the latest event rather than an event log.

The built-in producer checks whether a job is still current between preparation,
foundation work, Cargo generation and publication. An obsolete job skips the
remaining stages and reports `Superseded`; the snapshot diagnostic identifies its
stopping stage. Already running Cargo/build scripts finish normally, and publication
is not interrupted once started. Final revision checks still prevent stale activation.
External producers are checked around their invocation, not interrupted inside it.
The standalone builder API/CLI has no source watcher and completes its requested build.

The built-in producer shares fresh Cargo metadata and pre-build compiler information
with watch planning within each build attempt, under the Cargo-output writer lock.
Metadata uses the same compiler settings and output directory as generation. Each edit
and retry prepares again; no metadata cache is retained across edits. Both cache-capacity
scans and post-build compiler/std/support/lockfile checks remain in place.

The default debounce is 50 ms. New root requests enter the new generation; admitted
requests and their cleanup keep their original code and host resources. `snapshot`
distinguishes initial failure (`has_active_generation == false`) from continued service.
`BuildFailed`, `Rejected`, `Superseded`, `WatchUnavailable`, `CacheFull`,
`BudgetExhausted`, `WaitingForRetirement`, `RetentionBlocked`, `RecoveryRequired`
and `RestartRequired` do not change the active Router. `CancelOldest` may cancel
requests in a retired generation while preserving current admission.

Revision checks run before loading and immediately before activation. A result from
an older observed revision is discarded. Input-graph changes install fresh watches
and require another build under that registration, even if artifact generation failed
after input discovery succeeded. Fixing a newly added dependency can then trigger a
build by saving that dependency alone. Directory replacement, watch errors
and rescan notifications trigger re-registration and a full check. This is an observed
event order, not an atomic filesystem snapshot; a delayed save notification belongs to
the next update. A candidate already executed before becoming stale remains accounted
for under its lifetime policy.

## Build without a development loop

The CLI runs the same Rust producer. Select an unused `--output` path. Success creates
an object and an identity-checked manifest with `.json` appended to that path.
Cache/output filesystems must support hard links, such as NTFS or ext4.

```sh
cargo run -p reknot --no-default-features --features jit-dev --bin reknot-build --locked -- --manifest-path project/Cargo.toml --package my_logic --support-target my_support --cache-dir project/target/layered --output project/target/first.object
```

Use `--foundation-manifest` for reuse, `--features`/`--no-default-features` for dependency
selection, and `--help` for other settings. `jit::builder::build(&BuildOptions)` runs the same
library implementation. It locks the Cargo output directory and checks the actual support
artifact, compiler and lockfile. Foundation/object files are content-addressed; existing outputs
are not overwritten. Windows freezes foundation files with read sharing; Linux compares bytes
before and after the build. Artifacts establish identity, not code trust.

## Inputs, foundation and cache

The built-in producer defaults to Cargo dev `opt-level=1` for resident support and
dependencies, while keeping the reloadable logic at `opt-level=0`. This can reduce
checkpoint JSON processing time, but adds initial compilation work and can increase
edit-to-response time. The effect depends on the state type and where generic code
is instantiated; measure your workload before changing the setting.
`LayeredDevOptions::support_optimization` and `BuildOptions::support_optimization`
accept `Some(0..=3)` or `None` to inherit Cargo configuration. The CLI equivalent
is `--support-optimization 0..3|inherit`. Cargo package overrides still apply.
The setting is applied consistently to support and logic Cargo invocations and
participates in the foundation build key through the captured child environment.
Actual support profile/artifact identity is still checked before reuse. A changed
resident foundation requires a host restart. External producers retain their own
profile selection; this option does not change their scripts.

The shared watcher follows local transitive dependencies, manifests, lockfiles,
toolchain files, Cargo configuration and explicit `additional_inputs`. Each
`LayeredInput` declares `Logic` or `Foundation`. Build scripts and external include
inputs are not inferred completely. `declared_inputs_complete` defaults to false,
which rechecks the foundation through Cargo on every job. Set it to true only for
known, fully declared inputs to enable logic-only foundation reuse. Unknown inputs
must not be silently treated as logic edits.

The foundation build key hashes the resolver, compiler, builder, fixed child environment,
configuration and declared foundation contents. It selects a reuse attempt; actual
`foundation_id` matching remains the execution gate. A failed reuse attempt gets one
full recheck of the same revision, using structured results rather than stderr matching.
Changed foundation identity reports `RestartRequired` before logic entry. The old
Router continues; this does not restart the process or transfer its connections.
Restoring source text does not necessarily reproduce identical library bytes. Only an
actual matching identity returns to ordinary reload.

Cargo runs from the manifest directory with a captured child environment. Use
`reconfigure(toolchain, features, no_default_features)` to capture environment/settings
changes explicitly. `retry()` requests another full check without recreating the engine.

The cache contains a reused `cargo/` target, immutable `foundation/` and `objects/`
copies, and unique `sessions/<id>/jobs/<id>/` outputs. A target writer lock serializes
cooperating builders; each worker holds its session lease through completion.
Windows uses schema 2; Linux uses schema 3. Version-one `build-result.json` binds the producer result to the job;
`build-receipt.json` also records the captured manifest hash, foundation/object identity
and timings. Receipts grant neither native-code trust nor managed lifetime permission.

`max_cache_bytes` defaults to 4 GiB. The producer checks existing usage before generation
and reserves headroom conservatively before immutable publication. It refuses publication
when capacity is exhausted; it never deletes cache files. Cargo's mutable outputs and
error receipts can cross that threshold during a build: this is a generation/publication
guard, not an OS disk quota. Disk cleanup is independent of code reclamation.

`shutdown` stops result publication and waits for a running build to finish. It may block.
Drop stops publication and detaches the worker; it does not claim that Cargo has joined.
The worker retains the lease until it actually exits.

## KeepLoaded and Managed

`JitPolicy` distinguishes two lifetimes. The shared `Jit*` names and
`LayeredLimits` are compatible aliases of the existing `Coff*` types:

| Policy | Retired logic | Capacity |
| --- | --- | --- |
| `KeepLoaded` | Retained until process exit, even after requests finish | Cumulative load/admission limits; restart to renew capacity |
| `Managed` | Reclaimable after every tracked owner and executing call has ended | Live image/page limits; successful collection makes capacity reusable |

Managed loads do not consume either the engine's or Router's cumulative 32-attempt
allowance. `load_attempts()` counts all attempts for diagnostics;
`keep_loaded_attempts()` counts only the attempts charged against
`LayeredLimits::load.max_generations`. Native libraries, raw COFF/ELF, LLVM and static/custom
Router paths retain their existing accounting. A Managed policy does not preserve
application state across process exit, and does not imply that every image can be freed.

## Managed lifetime contract

Normal `LayeredEngine::load` and `LayeredDevSession::start` retain KeepLoaded behavior.
To permit reclamation, explicitly accept an additional unsafe contract **before entry**:

```rust,no_run
use reknot::jit::{LayeredLimits, LayeredArtifact, LayeredEngine};
# fn example() -> Result<(), Box<dyn std::error::Error>> {
let manifest = if cfg!(target_os = "linux") { "logic.o.json" } else { "logic.obj.json" };
let trusted = unsafe { LayeredArtifact::read(manifest)?.into_trusted() };
let mut engine = LayeredEngine::open(&trusted, LayeredLimits::default())?;
let managed = unsafe { trusted.into_managed() }; // Additional managed-v1 contract.
let handler = engine.load_managed(managed)?;
let image_id = handler.jit_image_id(); // Diagnostic only; cannot reacquire code.
# let _ = image_id; Ok(()) }
```

All logic code, static data, vtables, destructors and panic payloads must remain inside
tracked ownership. Owner-thread Router calls, GenerationLocal, local children and
resident host notifications are the base profile. No unmanaged thread, callback,
logic Waker, hook, atexit handler, dynamic TLS value or stored support-side reference may
outlive that ownership. Dependencies, entry and validation obey the same contract.

External `ManagedActivity` adapters need individual audits. `Ready`, including
`Ready(Err)`, must certify final callback/Waker return, joined threads, closed registration
and destruction of their owned logic values. An unregister request, reference count,
timeout or activity-failure acknowledgement does not prove this. Unknown termination
stays Pending; a poisoned adapter is retained. Tests cover a lazy, joined-thread fixture
with a barrier after dropping its Waker. That is not certification of a third-party runtime.

`unsafe start_managed` accepts this extra obligation for every future edit in a development
session. The public policy is `Managed`; the artifact's lifetime-contract profile is still
version one. Manifest fields cannot promote a trusted artifact to managed status.

## Managed capacity and cancellation

`LayeredDevOptions::managed_reload` applies to `start_managed`. Its default is
`ManagedReloadOptions { max_live_generations: 2, pressure: ManagedPressure::Wait }`.
The live count includes the active image, loaded candidates and retained images published
by this session. At least two slots are required so the current handler can serve while
a replacement is validated. Other controllers still contribute to the owner-thread
image/byte limits in `LayeredLimits`.

```rust,no_run
use reknot::jit::development::{LayeredDevOptions, LayeredDevSession, ManagedPressure};
use reknot::HostResources;

# fn example(mut options: LayeredDevOptions) -> Result<(), String> {
options.managed_reload.max_live_generations = 2;
// Opt in only if terminating unfinished old requests is acceptable.
options.managed_reload.pressure = ManagedPressure::CancelOldest;
let mut session = unsafe { LayeredDevSession::start_managed(options, HostResources::new())? };
// The owner loop calls poll_update regularly, and polls/drops its RequestTasks.
# session.shutdown()?; Ok(()) }
```

- `Wait` preserves old requests and reports `WaitingForRetirement` until their code
  ownership ends. It does not time out or cancel them.
- `CancelOldest` requests cancellation of roots in this session's oldest retired image
  when capacity blocks an update. The current image and other controllers are excluded.
  Each root is destroyed on its executor's next poll; activities must still stop/join,
  executing callbacks return, locals be destroyed, and completed handles be dropped.
  Side effects are not rolled back. A running native call cannot be preempted.

For direct Router use, `cancel_retired_generation(id)` requests the same cancellation
and returns the number of newly requested roots. The cancelled root completes with
`RunStatus::Error` after cleanup. Cancellation cannot release externally held handles
or untracked ownership.

Managed `poll_update` drives detached cleanup, registered Router workers, retired
bookkeeping and collection for **this session's image IDs only**. Call it regularly;
there is no background collector. `poll_update_with_context(cx)` also lets cleanup and
workers wake the host executor. Source/build completion still needs periodic polling.
The host must separately poll and drop externally owned `RequestTask`s.
Registered workers also retain their completed task for status/checkpoint access;
save any needed checkpoint and call `remove_worker(id)` to end that ownership.
`collect_retired(options)` remains available for explicit maintenance; KeepLoaded
sessions return None. `shutdown` joins the build worker but does not cancel requests.

Capacity is checked before starting a build and again before allocation/entry, using
the exact planned page size once available. If a completed build must wait, the session
holds one captured candidate and retries admission without rebuilding or executing it.
A newer observed revision supersedes that candidate. KeepLoaded budget exhaustion
also stops further builds; `retry` does not reset the allowance.

`RecoveryRequired` identifies an owned image that was pinned by failed cleanup or
release; updates stop instead of accumulating more failed generations. `RetentionBlocked`
reports insufficient owner-thread image/page capacity when no owned retired image can
be waited on. The active Router remains available. A retained request, failed destructor,
unknown external termination or another controller can therefore still block progress;
Managed removes the cumulative update ceiling, not these lifetime obligations.

Session snapshots include `policy`, both attempt counters, `live_images` and the most
recent `reclamation` report. IDs and counts are diagnostic and grant no code ownership.

## Explicit collection and diagnostics

Progress requests and cancellation/activities, drop completed handles, then call
`router.release_retired_generations()`. Call `jit::collect_retired_jit(options)` on the
loading thread. It can also run after the controller is dropped. It never forces task
termination and never executes arbitrary logic while holding the image registry.

Each physical load gets a unique `JitImageId`. `jit_retention_snapshot()` reports policy,
state, allocated page bytes, references, executing calls, locals, activities and failure
reasons. These values confer no ownership. `collect_retired_jit` selects the native Rust
backend; `collect_retired_coff` and `collect_retired_elf` remain available for
explicit platform selection. The native-library retention API is unchanged.
LoadedHandler, PreparedHandler and RequestRouter expose the same diagnostic
`jit_image_id()`. Failed pre-execution rollback also stays charged; its record has
no foundation identity and an empty object hash because publication never completed.

Collection rechecks retirement, activity/local completion, failures and returned calls,
then uses exclusive Arc ownership to seal the image against new leases. Only owner-thread
maintenance releases the image: Windows removes its unwind table and calls
`VirtualFree(base, 0, MEM_RELEASE)`; Linux removes its libgcc frame registration
and calls `munmap`. Failure pins the allocation without automatic retry; successful release refunds the live
image budget. Last-lease Drop and owner-thread teardown never release machine code.
Thread teardown conservatively leaks any unreclaimed image/table. std/support and their
Windows file pins or Linux sealed file descriptors remain resident even when
every logic image is released.

`LayeredLimits` defaults are 32 cumulative **KeepLoaded** attempts, 32 MiB per object, 64 MiB per allocation,
and owner-thread totals of 256 retained images / 256 MiB of allocated image pages across
controllers. KeepLoaded, failed releases and pinned images count toward those totals;
they are not a process-wide memory bound across owner threads. Router attempts have a
separate default of 32 (`max_router_generations` in development options); Managed COFF/ELF
handlers bypass this allowance. Neither KeepLoaded attempt budget is refunded.
The live image/page limits apply to both policies. No released-generation detail history is accumulated: snapshots keep
live/retained records and cumulative successful release totals.

The collector evaluates at most `max_candidates` records per call and rotates them for
fairness. It reports release IDs, waiting/retained IDs, OS errors and elapsed time; it
does not promise an interruptible OS call or a hard real-time deadline.
