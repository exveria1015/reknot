# Changelog

User-visible changes are recorded here. See the
[GitHub releases](https://github.com/exveria1015/reknot/releases) for published
tags and their verification records. A version entry alone does not announce publication.

## Unreleased

### Typed development and message formats

- Select native DLL or checked layered COFF/ELF builds through the same typed
  `DevModule`. Existing `DevOptions` keep native behavior; `LayeredDevOptions`
  select the object backend. Preserve typed handles, bridge admission, structured
  contract rejection, Drain and cooperative Restart with worker checkpoints.
  `start_managed` explicitly accepts the additional lifetime contract. Share
  source/build admission and per-session collection with `LayeredDevSession`;
  report foundation restart and retention conditions through `layered_snapshot`
  and `ReloadEvent::Layered`. `shutdown` joins build workers without cancelling
  requests. The layered logic/support layout remains required; the generator's
  native default is unchanged. See [typed JIT development](docs/TYPED_JIT.md).
- Add optional `typed-binary` MessagePack handlers via `Handler::with_format`.
  Named fields and explicit format identity preserve contract diagnostics; JSON
  remains the default and metadata/checkpoints stay JSON. Both formats keep the
  1 MiB encoded limit and owned-byte boundary. This is serialization, not zero-copy.
  See [binary messages](docs/BINARY_MESSAGES.md).

### Build pipeline and artifact preparation

- Add the built-in Rust COFF/ELF producer, `LayeredDevOptions::builtin`,
  `jit::builder::build` and `reknot-build`. Ordinary layered development no longer
  needs PowerShell/Python. Preserve checked foundation capture, immutable output,
  writer locking and capacity limits; retain the existing external script path.
- Default the built-in layered producer's resident support/dependencies to dev
  opt-level 1 while leaving logic at 0. Add `support_optimization` and the CLI's
  `--support-optimization` override, including Cargo profile inheritance. This
  changes compilation costs and can increase edit-to-response time; see
  [build settings](docs/LAYERED_DEVELOPMENT.md#inputs-foundation-and-cache).
- Prepare native development DLL copies and verify their bytes on the build
  worker. Keep DLL initialization, contract validation, and activation on the
  owner thread; discard unused copies without loading them.
- Stop superseded layered development jobs between preparation, generation and
  publication stages. Let running Cargo processes finish; skip obsolete reuse
  retries and report the stopping stage through the layered diagnostic. Preserve
  all checks for adopted artifacts and the final owner-thread revision checks.
- Share Cargo metadata and pre-build compiler information within each built-in
  JIT build attempt, using the actual Cargo output directory. Refresh preparation
  on retries and retain dependency watches after generation failures. Reduce
  duplicate file queries in both cache-capacity scans; keep post-build identity checks.

### Documentation and benchmarks

- Add a three-concept entry path in the README and generated starter, with
  separate development contracts for advanced lifetime, Restart and JIT work.
- Add reproducible native DLL/JIT edit-to-response and JSON/MessagePack typed
  round-trip benchmarks with warmups, raw samples and environment details.
  Add ongoing-request measurements for Drain, Managed collection, Restart and
  consecutive saves, plus a raw-data analyzer. Keep historical measurements
  separate from current producer settings in the [benchmark guide](docs/BENCHMARKS.md).
- Align the English/Japanese task indexes, API guides and test instructions.

### Rust API migration

- Manual `HandlerSpec` literals need the new `format` field; use `WireFormat::Json`
  to retain the default encoding. Changing encoding requires a contract-version
  update and rebuilding/restarting the host.
- `ReloadEvent` is now non-exhaustive: callers matching every variant must include
  a fallback. `LayeredDevState` adds `Restarting`, requiring exhaustive matches to
  be updated.
- `LayeredDevOptions` adds producer and support-profile settings. Prefer its
  `new`/`builtin` constructors to listing every field in a struct literal.

### Experimental object loaders and lifetime management

- Add experimental `jit::ElfEngine` / `ElfLimits` for Linux x86_64 GNU ELF64
  relocatable objects, without an LLVM bridge. Resolve PIC/GOT/PLT relocations
  against explicit resident std/support shared libraries and GNU runtimes,
  protect code RX/data RW, register `.eh_frame` before entry and preserve handler
  ownership through cutover, panic and controller Drop. Bound input, image and
  cumulative session memory; reject TLS/startup hooks and unsupported relocations.
  KeepLoaded remains the default. Add schema-3 checked artifacts, sealed memfd
  std/support snapshots, automatic foundation matching, and opt-in Managed
  libgcc deregistration/munmap through the shared ownership registry. Add the
  Python ELF producer and Linux `jit-dev` source watching with Wait/CancelOldest
  pressure, failed-build recovery and foundation restart reporting. Preserve
  existing Windows APIs and add common `Jit*` aliases. Test real Linux objects,
  128 managed updates, OS release, failed cleanup and source-watch flows in CI.
- Add optional `jit-dev` layered development sessions: one background producer,
  shared source watching, revision/graph checks, structured job receipts, immutable
  cache publication and foundation restart reporting. Preserve active requests on
  failed/stale builds; retain the engine and its KeepLoaded allowance through retries.
- Refresh layered dependency watches even when artifact generation fails after input
  discovery, so fixing a newly added local dependency triggers another build without
  an explicit retry. Preserve the active generation and foundation restart checks.
- Add explicitly trusted managed COFF artifacts and owner-thread collection, with
  unique physical image IDs, live page budgets, local/activity diagnostics, exclusive
  ownership sealing, unwind removal and checked VirtualFree. KeepLoaded remains the
  default; DLL foundations, raw/LLVM loads and uncertain releases remain resident.
  Audit external activity adapters individually; no general runtime unload guarantee.
- Name the reclaimable policy `Managed` and replace its cumulative engine/Router
  attempt limits with reusable live image/page capacity. KeepLoaded keeps its existing
  limits. Managed development defaults to two live generations and `Wait`; optional
  `CancelOldest` requests cancellation of the oldest retired roots and still waits for
  cleanup, join and ownership release. Maintain only the session's images, preserve a
  built candidate during capacity waits and report pinned failures without accumulating
  more generations. Add selective `RequestRouter::cancel_retired_generation`.
- Add move-only, owner-thread `LoadedHandler` values that carry handler tables
  and code owners together, with safe router and typed Drain/Restart entry points.
  Host call-return guards now retain code as well as counting active calls.
- Add an opt-in `jit` feature and separately built LLVM 23.1.0 COFF/JITLink bridge
  for trusted, self-contained Windows x64 objects. Bound load attempts and input
  bytes, register unwind tables before publication, and keep linked images resident
  on ABI rejection, abandonment and Drop. The LLVM backend retains this policy;
  the checked Rust backend has the opt-in development/managed path above.
- Extend the experimental JIT bridge with a compiler-matched resident Rust
  std DLL, emulated TLS and in-generation panic unwinding. Preserve TLS per thread
  and generation; reclaim backing storage only after OS-confirmed thread exit.
  Add `open_with_runtime`, `collect_finished_threads` and a nightly object builder.
  Native TLS, arbitrary dependent archives and cross-host unwinding remain unsupported.
- Add `JitEngine::open_layered` and bridge ABI 3 for a resident, explicitly named
  support DLL alongside std. Reject native and emulated TLS in logic while allowing
  OS-managed TLS in support. Exercise actual SDK-generated COFF generations with
  resident JSON services, GenerationLocal cleanup, managed cancellation and panic.
  Rebuild the bridge; support/compiler identity remains an unsafe caller contract.
  The Rust dylib fixture is compiler-specific; a stable support ABI
  and LLVM code reclamation remain outside that backend.
- Add `jit::CoffEngine` and `CoffLimits`, a Rust COFF/bigobj backend for the
  Windows x64 MSVC layered profile without LLVM. Bound image allocation including
  BSS, validate symbols/relocations, apply W^X protection and register native unwind
  tables before executing code. Preserve LoadedHandler ownership and KeepLoaded;
  roll back pre-execution failures. Reject unsupported forms rather than emulate
  CRT, TLS or startup. Run both backends against the same layered SDK fixtures.
- Add `build-layered-jit.ps1` for Cargo-managed layered COFF builds. Use a logic
  library artifact to avoid the redundant PE DLL/PDB link, with configurable
  optimization/debug information. Schema-2 manifests identify immutable DLL/object
  copies; freeze the actual Cargo support dependency while building logic. Reuse a
  verified foundation with `-FoundationManifest`, failing before publication when
  dependencies require rebuilding support. Changed foundations require host restart.
- Add `LayeredArtifact`, `TrustedLayeredArtifact` and `LayeredEngine` to move
  foundation matching into safe Rust APIs. Capture immutable object bytes, check
  compiler/build/DLL identities, and pin both DLL files and their canonical parent
  directories until process exit. Reject mismatches and externally preloaded DLLs;
  preserve the old generation after rejection. Native-code/producer trust remains
  explicit through `unsafe into_trusted()`; hashes do not establish code safety.

### Development and CI fixes

- Include benchmark sources in the isolated Git SDK used by CLI installation
  tests, so Cargo can resolve the manifest's explicit benchmark targets.
- Keep `dev-tools` usable without `typed` by gating the shared typed/JIT watcher
  to its consumers; check that feature combination and JIT fixture formatting in CI.
- Keep the GUI smoke client's HTTP/1.1 connection open instead of forwarding
  per-request `Connection: close` through tauri-driver's native connection pool.
  CI repeats the real WebView test three times with read retries disabled and
  retains per-request transport diagnostics.
- Wait for native-driver and proxy HTTP readiness before creating a GUI test
  session, including when immediately starting another complete test run.
- Skip known excluded build-output notifications before Windows path
  canonicalization, avoiding unnecessary opens while Cargo changes its output.
  Add a regression check that the dedicated Cargo target exists before watching.

## 0.1.0-rc.2

Source release candidate for typed contracts and Restart, the WebSocket quiz,
notification-driven hosts and reload lifecycle hardening. Publication is separate.

### Examples and host driving

- Playable WebSocket quiz comparison: Axum + Reknot and Fastify + JavaScript share
  questions/UI, with host-owned connections and scores, editable scoring rules,
  registered clock checkpoints, Drain/Restart and failure recovery. Includes a
  two-server launcher, Japanese/English guides and Windows/Linux process tests.
- Drive generated-host and quiz bridge requests directly from executor wakes;
  source/build maintenance keeps its separate cadence. Console hosts accept quit
  and EOF during a request and wait for registered cancellation cleanup.

### Cancellation and notification safety

- Wake parked typed calls on explicit cancellation, so selective Future
  combinators finish cleanup and unblock Restart. Repeated cancellation is idle.
- Schedule another bounded cleanup pass for requests dropped during cleanup,
  preserving queued work if the scheduling Waker panics or reenters cleanup.
- Contain the final executor Waker destructor panic when a host notification
  races with request completion, including a simultaneous notification panic.
- Notify each `RequestContext::cancelled()` and `ChildStop::cancelled()` wait
  independently, including inside selective Future combinators. Stop and Restart
  no longer stall on these composed waits. Dropped waits release their Wakers.
- Add an optional cancellation ABI 1 tail to task-control ABI 2, preserving the
  base and generation-services prefixes. Rebuild host and logic together: new
  request cancellation waits fail explicitly on a host without this tail.
- Contain executor Waker notification and destructor panics on the originating
  side of the C ABI, including nested cancellation notifications during Restart.
- Schedule a new activity poll when a child registers another child or starts an
  asynchronous service, so notification-driven requests do not stall.
- Notify every `CancellationToken::cancelled()` waiter independently, release
  dropped registrations, and synchronize registration with concurrent cancellation.
- Preserve other bridge result/shutdown notifications when one Waker panics.
  A service worker unwinding from a result notification closes admission, fails
  queued work and notifies join instead of leaving clients waiting indefinitely.

### Source watching and package verification

- Exclude Cargo's separate intermediate build directory from source watches,
  including configuration and environment overrides, and reject output/source
  overlap. Create validated output roots before watching so Cargo's initial
  directory setup does not trigger an extra reload.
- Prepare the dedicated `reknot-typed/<package>` target before registering source
  watches too, avoiding Cargo's initial temporary-directory rename while Windows
  directory observers are active.
- Resolve Windows input and output path spelling through the filesystem so
  differently cased Cargo output paths stay excluded, including new directories.
  Recheck source/output overlap after creating the output roots.
- Shorten package-verification and watch-fixture paths so nested Cargo builds
  fit Windows linker path limits with newer Cargo artifact layouts.
- Rearm the low-level `SourceWatcher` after source/project directory recreation;
  also observe replacement of manifest/build scripts and newly created inputs.
- Watch Rust modules beside custom Cargo targets, excluding build output, and
  run metadata/build commands from the selected manifest's directory so local
  Cargo configuration applies consistently.
- Refresh source watches when input directories are created, removed or replaced.
  New `.cargo` directories and recreated source directories keep triggering
  reloads, including recovery after metadata or build failures.

### Cache configuration and format (breaking)

- Use Reknot names throughout the SDK and documentation. `REKNOT_CACHE_ROOT` is
  the only cache-root environment variable; earlier aliases are no longer read.
  Its unset default remains `<temp>/reknot/`, and CLI `--root` takes precedence.
- Write and recognize only `reknot-cache-v1` session metadata under `managed-v1/`.
  Earlier source-candidate caches are unsupported and are not migrated or cleaned.
  Unknown session/trash formats and PID-only entries are preserved. Configure the
  current cache root explicitly if an earlier alias selected a custom location.

### Breaking typed API changes

- Inputs, outputs and worker state now require `JsonSchema`. Add `#[reknot::schema]`
  to Serde protocol types, then rebuild host and logic together. Typed initialization
  uses contract v2; an rc.1 typed module is rejected. `Error::Contract` now contains
  a structured report.
- Both serialization and deserialization schemas are compared before activation,
  so field/Serde differences under the same Rust type name and version are detected.
  Schema equality is conservative and does not establish semantic compatibility.

### Capabilities

- Contract errors identify expected/actual values, JSON pointer, handler and source
  location, with bounded differences and full copied manifests for inspection.
- `#[reknot::schema]`, `TypeSchema` and `Module::schema_json()` generate/export JSON
  Schema 2020-12 contracts. Generated projects include a schema export example.
- `WorkerState`, `StateMigration` and `ModuleRestart` expose cooperative Restart,
  typed checkpoint migration and a final host commit through `ModuleHost`.
  Ordinary requests are not replayed; failure/interruption keeps admission paused.
  Queued bridge calls wait for admission and wake on recovery or successful cutover.
- `DevOptions::with_reload_mode(ReloadMode::Restart)` opts into save-time Restart
  for unchanged state contracts. Drain remains the default.
- `reknot-new` generates exact registry dependencies by default and supports a fixed
  Git revision or explicit local path. Source candidates can install the CLI and
  generate native/static applications from Git without keeping an SDK checkout.
- Handler ABI 5 retains its original prefix and gains an optional metadata tail.
  Legacy tables are read within their declared size. Miri, real-DLL evolution,
  installed-CLI and extracted-package tests cover the new paths.


## 0.1.0-rc.1

First Reknot source release candidate. The three SDK crates use `0.1.0-rc.1`;
the intended tag is `v0.1.0-rc.1`. This candidate uses a source checkout and does
not claim crates.io availability or a standalone installed project generator.
See the [release procedure](docs/RELEASING.md) for the final verification gates.

### Breaking API changes

- Adopt the Reknot core crate and source directory names: `reknot`, `reknot_abi`
  and `reknot_macros`. Rebuild host and logic together against the renamed SDK.
  The CLI commands are `reknot-new` and `reknot-cache`. The service example uses
  `REKNOT_ADDR`. Default cache copies now live under `<temp>/reknot/` and use
  `REKNOT_CACHE_ROOT`. The cache compatibility initially retained in this source
  candidate is superseded by the breaking cache changes in `0.1.0-rc.2`.
  The native ABI tables and `hot_handler_api` entry point are unchanged.
- Use `reload_handler`, `HostResources`, `RequestRouter` and `RequestTask` for
  both native reload and static linking. The earlier runner, direct/snapshot
  macros, attached runtimes and compatibility aliases have been removed.
- Use versioned `Checkpoint` / `InitPayload` values and application codecs for
  saved state and initial configuration.
- Low-level owned tasks use `TaskHandle`, `TaskVTable` and `FfiFuture`.
  Native loading errors use `LoadError`; routing errors use `RouteError`.
- Native images default to KeepLoaded. The old unload-policy API has been removed;
  explicit `unsafe reclaim_native_image` requires host proof of external/TLS/dependency
  quiescence. Retained images and exited-session files have separate cleanup APIs.
- Cargo/build/watch helpers are in the feature-gated `development` module.

### Capabilities

- `reknot-new` generates a no-clobber host/logic/protocol project with native/static
  modes and a bilingual guide. Typed Send + Sync clients connect framework threads
  to an owner-thread bridge with bounded queues, cancellation and shutdown/join.
  Windows Cargo paths are normalized before metadata/build to preserve workspace inheritance.
- Typed `handler` functions and `export_handlers!` module exports, Serde JSON
  input/results, stable `HandlerHandle`s and explicit contract versions. Single-DLL
  Drain preserves admitted generations and host state. `DevModule` provides
  background builds, local dependency watching, stale-result rejection and recovery
  after failed builds. See `docs/TYPED_HANDLERS.md` for limits and the console bot.
- Message-boundary Drain and cooperative worker Restart with versioned
  checkpoints, staged factories and a final synchronous host commit hook.
- Explicit child-task and callback registration, cancellation cleanup and
  join tracking before request completion and Restart.
- Async host services, bounded threaded I/O, required service bindings and
  typed per-generation initialization data.
- Native retention diagnostics, managed cache sessions and the `reknot-cache`
  inspection / exited-session cleanup CLI.
- Explicit owner-thread DLL reclamation, host call-return guards, retention reasons,
  retired-generation release and separate close/copy-removal outcomes. Handler ABI 5
  and cumulative attempt-budget semantics are unchanged.
- Examples for TCP, static logic, threaded I/O, service contracts, SQLite
  resource migration, a headless Bevy game and a Tauri GUI.
- Registered `GenerationLocal<T>` values with owner-thread destruction and
  cleanup-failure retention, plus resident `HostNotification` handles for host I/O.
  Delayed host notifications detach after registered request cleanup and can
  outlive a reclaimed DLL. Individual dependency shutdown stays application-owned.
  An optional task-control tail adds generation-services ABI 1 without changing
  Handler ABI 5 or the task-control ABI 2 base layout.

### Build and compatibility

- Default features are `native`, `dev-tools` and `typed`. Typed logic/static hosts
  use `default-features = false, features = ["typed"]` (Serde/JSON, no native tooling).
  Low-level logic crates and static hosts
  can use `default-features = false`; `native` alone omits watch/build helpers.
- Plain Cargo build/test selects the three core crates. Examples remain
  explicit workspace targets. The workspace lockfile is committed.
- Native contracts and threaded I/O demos are separate `contracts_host` and
  `threaded_io_host` workspace packages. Shared contract types live in
  `contracts_protocol`; packaged core examples and ABI fixtures do not read
  sibling crate sources.
- SDK packages include README and license texts. Package verification extracts
  all three archives, checks every target with native/static features and runs
  an independent ABI DLL check plus a statically linked consumer.
- Declared minimum Rust versions are 1.95 for the core and 1.97.1 for the Bevy
  example. Handler ABI 5, Services ABI 2 and task-control ABI 2 are retained.
  Rebuild host and logic together against the same SDK.

See [compatibility and scope](docs/COMPATIBILITY.md) / [互換性と対象範囲](docs_ja/COMPATIBILITY.md)
and [testing](docs/TESTING.md) for contracts, checks and verification limits.
