# Code ownership and experimental COFF/JITLink

English | [日本語](../docs_ja/JIT_CODE_OWNERSHIP.md)

Linux x86_64 GNU also has a Rust [ELF object backend](ELF_JIT.md), using the
same `LoadedHandler` ownership, checked `LayeredEngine`, Managed lifetime contract
and `LayeredDevSession`. Linux uses sealed memfd foundation snapshots and libgcc
deregistration/munmap; the COFF build commands and Windows OS calls below are
platform-specific. See the ELF guide for the Python builder and Linux details.

## Ownership boundary

`LoadedHandler` carries a validated handler table together with its code owner.
It is a move-only, owner-thread value: no Clone, Send, Sync or raw table getter.
`RequestRouter::from_loaded` and `prepare_loaded` consume it. Typed equivalents
are `ModuleBuilder::from_loaded`, `ModuleHost::reload_loaded`, `restart_loaded`
and `restart_loaded_with`. Existing native and static entry points remain available.

Trusting/loading native code still requires unsafe. After a trusted loader produces
LoadedHandler, preparation, contract checks, activation and requests use safe APIs.
ABI validation cannot establish the correctness of arbitrary machine code.
`LoadedHandler::from_static_api` is also unsafe: the caller promises process-resident
callbacks and the complete handler contract. It does not pin a dynamic library.

Each request retains its original table, Future, destructor and code generation.
Existing tasks are never rebound to new poll functions or layouts. Futures, managed
activities, generation locals and escaped host Wakers retain code leases. A host
call-return guard owns a lease as well as counting the call: releasing the last other
handle inside a callback cannot destroy its owner before returning to host code.
The ABI remains byte messages, resource IDs and explicit state migration.

## Retention and limits

By default, successfully linked JIT code, data, dependencies and registered unwind tables remain resident
until process exit. Explicitly managed Rust COFF/ELF images can instead use the
[managed reclamation contract](LAYERED_DEVELOPMENT.md). Dropping a router, engine controller, rejected candidate or final
Waker never automatically reclaims JIT code. `mem::forget` can retain resources;
safety does not require every destructor to run. This retention policy does not prove
that arbitrary Rust code can be safely unloaded.

If materialization fails before the entry is published, LLVM may release that
unpublished allocation. Any already registered unwind table is removed first.
Once the entry is returned, even ABI rejection retains the linked image.

`JitLimits` bounds attempts and input bytes before code publication: defaults are
32 attempts per engine and 32 MiB per object. Failed, rejected and abandoned loads
consume a slot without refund. Reuse the same engine for a reload session. A new
engine has a separate budget; this is not a process-wide memory cap. Input size
does not bound BSS, allocation padding or LLVM overhead. The router's existing
generation budget separately limits admission attempts. These cumulative limits
remain in force for LLVM and KeepLoaded COFF/ELF; the checked Managed path
uses live image/page capacity instead and does not consume either attempt allowance.

An external callback can release its last Waker and still execute old instructions.
Counters alone do not establish external quiescence. The safe, explicit managed object collector
requires an additional lifetime contract accepted before execution; ordinary loads stay resident.
Existing unsafe DLL reclamation retains its
[external-return and TLS requirements](UNLOAD_SAFETY.md).

## Supported JIT subset

The optional `jit` feature offers `CoffEngine` (Rust; described below),
`ElfEngine` (Rust; see the [ELF guide](ELF_JIT.md)) and `JitEngine` (LLVM). The latter uses a separately built LLVM 23.1.0 bridge on Windows
x86-64. It explicitly uses ORC ObjectLinkingLayer / JITLink and registers COFF
.pdata through RtlAddFunctionTable before publishing the handler entry.
Each image has its own JITDylib; existing images are never patched in place.

Inputs must be trusted MSVC x86-64 COFF objects exporting hot_handler_api with
Handler ABI 5. `JitEngine::open` accepts self-contained objects. The optional
`open_with_runtime` supplies a compiler-matched, resident Rust std DLL and Windows
runtime symbols. `open_layered` additionally supplies an explicit support DLL and
rejects native/emulated TLS in logic. The bridge ABI is 3; rebuild an older bridge
before using this SDK. Handler ABI 5 and generation services are unchanged.

The bridge rejects linked PE images, other formats/architectures, native TLS and CRT
constructor/destructor sections. It does not search arbitrary process symbols;
unresolved runtime imports fail. The local LLVM patch supports the fixture's
strong local COMDAT leader form and rejects other local forms.

The loader accepts one object, not an archive of arbitrary dependent crates.
Custom allocators, arbitrary native libraries, CRT startup sections, foreign
exceptions and automatic Cargo/typed-module bundling remain unsupported.
CodeView sections are accepted; debugger registration is absent.

## Two-layer logic and resident support

`JitEngine::open_layered(bridge, std_dll, support_dll, limits)` loads std first,
then the specified support DLL through the normal Windows loader. Both paths
are canonicalized before either DLL is loaded. Support dependencies are searched
in its own directory and System32; the loader does not add the working directory
or PATH. Only std, that support DLL and the fixed Windows runtime set below supply
symbols to JIT generations. Merely loading another support DLL into the process
does not make its exports available to a different engine.

The initial executable fixture uses a **compiler-matched Rust dylib**, re-exporting
the SDK and exposing non-generic `parse_total(&[u8]) -> Result<u64, ServiceError>`
and `parse_count() -> u64` services. JSON parsing and SDK support live in the PE
DLL; the handler, local types and generic instances can still live in the COFF.
Re-export alone does not relocate every generic instance into support. This is
an experimental Rust ABI boundary, not a versioned public support C ABI. The host
continues to use existing Handler ABI and generation-service tables.

Build support and logic with exactly the same rustc, target, dependencies and
features. Use prefer-dynamic=yes, panic=unwind, codegen-units=1 and lto=false;
emit one logic object with Cargo/rustc. The fixture uses stable Rust without
`-Z tls-model=emulated`. Do not statically bundle host/router dependencies into
logic or pass every dependency rlib to the JIT. Missing exports fail before
publication. Support and std stay resident even if initialization later fails;
changing either requires a new process. These raw APIs leave compiler/DLL identity
to the caller. The checked Rust API below enforces it from a build manifest.

Logic must use `GenerationLocal`, not `thread_local!`. Before linking, the bridge
checks every section and symbol, including unused ones: `.tls*`, CRT startup and
destructor sections, native TLS symbols such as `_tls_index`, and `__emutls_*`
are rejected. std/support may use OS-managed TLS. These checks do not rule out
dynamic `TlsAlloc`/`FlsAlloc`, process hooks or unmanaged callbacks; the trusted
source and dependency lifetime contracts still apply.

The same LoadedHandler, router preparation/activation, generation leases and
KeepLoaded policy apply. Host resources survive cutover; a new generation gets
new locals. Old futures and local destructors finish against their original
code, including after controller Drop. A failed local destructor is not retried.
Panic must be caught before a C callback returns; the resident SDK may catch a
logic panic within its Rust call stack. Payload destruction must finish while
the originating code is retained. This profile does not unload code. The Rust
backend below uses the same ownership and layered runtime contracts.

```rust,no_run
use reknot::jit::{JitEngine, JitLimits};
use reknot::{HostResources, RequestRouter};
use std::path::Path;

unsafe fn start_layered(std_dll: &Path, support_dll: &Path)
    -> Result<RequestRouter, Box<dyn std::error::Error>>
{
    // All inputs are trusted and built against this exact resident foundation.
    let mut engine = JitEngine::open_layered(
        "reknot_jit_bridge.dll", std_dll, support_dll, JitLimits::default(),
    )?;
    let first = engine.load("logic-v1.obj")?;
    let mut router = RequestRouter::from_loaded(first, HostResources::new())?;
    let next = engine.load("logic-v2.obj")?;
    let prepared = router.prepare_loaded(next)?;
    router.activate(prepared)?;
    Ok(router)
}
```

Register application resources before dispatch and retain the same engine for
the reload session. The fixture/test below provides a reproducible Cargo build;
the checked Rust backend can use the [jit-dev session](LAYERED_DEVELOPMENT.md).

## Rust COFF backend

`CoffEngine::open_layered(std_dll, support_dll, CoffLimits::default())` provides
the layered profile without an LLVM bridge or SDK. It is available under `jit`
on Windows x86_64 MSVC and returns the same `LoadedHandler`. Opening and loading
remain unsafe boundaries for trusted native code and exact foundation matching.

```rust,no_run
use reknot::jit::{CoffEngine, CoffLimits};
use reknot::{HostResources, RequestRouter};

unsafe fn start_rust_coff() -> Result<RequestRouter, Box<dyn std::error::Error>> {
    let mut engine = CoffEngine::open_layered(
        "compiler/std-matching.dll", "support.dll", CoffLimits::default(),
    )?;
    let first = engine.load("logic-v1.obj")?;
    let mut router = RequestRouter::from_loaded(first, HostResources::new())?;
    let next = engine.load("logic-v2.obj")?;
    let candidate = router.prepare_loaded(next)?;
    router.activate(candidate)?;
    Ok(router)
}
```

The supported subset is one regular COFF/bigobj, with local section symbols,
NoDuplicates/Any/Associative COMDATs and bounded weak-alias chains. Multiple
external definitions requiring COMDAT selection, common symbols, weak library
search, archives, PE input and startup/TLS are rejected. The AMD64 relocations
are ABSOLUTE, ADDR64, ADDR32, ADDR32NB, REL32 through REL32_5, SECTION and SECREL;
other kinds and out-of-range results fail. Debug sections are inspected and
discarded; PDB generation and debugger registration are not provided.

Only the specified DLLs and fixed runtime set resolve imports. `__imp_` slots
and executable-target jump stubs reside in each image. The actual host CRT's
`type_info` vtable supplies its special symbol; there is no process-wide symbol
search or fabricated exception runtime. Code becomes RX, constants R and mutable
data RW before instruction-cache flush and unwind registration. Version-1 x64
UNWIND_INFO is supported; chained records are rejected. No object callback runs
before registration succeeds.

`CoffLimits` combines `load: JitLimits` (32 KeepLoaded attempts and 32 MiB input by default)
with `max_image_bytes` (64 MiB, including BSS, import slots, stubs and page
padding). `max_retained_images` / `max_retained_bytes` additionally bound owner-thread
retention across controllers (defaults: 256 images / 256 MiB); explicit managed
collection refunds only successfully released image pages. Parser quotas are 8,192 sections, 1,000,000 symbol records and 1,000,000
relocations. Failed KeepLoaded attempts consume the cumulative budget; Managed
attempts are diagnostic only. Pre-execution failures remove
unwind registration before freeing the image; if removal fails, both are kept.
After the first possible callback, raw `CoffEngine` loads retain the image and its
unwind table until process exit. The checked `LayeredEngine::load_managed` path
permits explicit collection after successful loading and retirement; an entry/ABI
rejection pins even a managed image. Controller Drop never performs collection.

`last_load_metrics` uses `copy_ms` for inspection, `add_ms = 0` and
`materialize_ms` for allocation, resolution, relocation, protection and unwind
registration. It does not measure compilation. `linked_bytes` excludes page
padding; LLVM and Rust may retain different debug content. Compare whole-load
and first-response intervals on identical input, and measure edit-to-response
separately. A faster loader does not establish a faster compiler.

## Checked foundation and artifact identity

For ordinary layered reloads, use `LayeredArtifact` and `LayeredEngine` instead of
the raw path-based entry points. `LayeredArtifact::read` safely reads a bounded
schema-2 COFF or schema-3 ELF manifest, validates the supported profile and
captures the exact object bytes after checking size and SHA-256. Subsequent path replacement cannot change
that captured object. The declared object must be a regular file inside the manifest's directory
tree. When the producer's cache is elsewhere, keep the companion object beside the `.json` file
or pass the authorized cache directory to `LayeredArtifact::read_in`.

The following commands and file-pinning details describe Windows COFF. For
Linux sealed snapshots and the Python builder, see the [ELF guide](ELF_JIT.md).

One explicit `unsafe into_trusted()` accepts native-code and producer trust:
the manifest must truthfully describe the compilation foundation, and code,
DLL initializers and dependencies must satisfy the callback/lifetime contracts.
A matching hash does not prove authenticity, memory safety or ABI compatibility.
Other native/unsafe loaders must also cooperate with process DLL initialization.
Trust the process loading configuration and dependency resolution, with no
redirection of these DLLs. Windows can redirect even a specified path; the loaded
path check runs after initialization and cannot undo an initializer. See
[LoadLibraryExW](https://learn.microsoft.com/en-us/windows/win32/api/libloaderapi/nf-libloaderapi-loadlibraryexw).
After this boundary, `LayeredEngine::open` and `load` are safe:

```rust,no_run
use reknot::jit::{CoffLimits, LayeredArtifact, LayeredEngine};
use reknot::{HostResources, RequestRouter};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let first = LayeredArtifact::read("target/layered/logic-v1.obj.json")?;
// SAFETY: these are known local builds from our trusted producer; their code
// and dependencies satisfy the layered ABI and lifetime contract.
let first = unsafe { first.into_trusted() };
let mut engine = LayeredEngine::open(&first, CoffLimits::default())?;
let mut router = RequestRouter::from_loaded(engine.load(first)?, HostResources::new())?;

let next = LayeredArtifact::read("target/layered/logic-v2.obj.json")?;
// SAFETY: the same native-code/producer trust applies to this local build.
let next = unsafe { next.into_trusted() };
let candidate = router.prepare_loaded(engine.load(next)?)?;
router.activate(candidate)?;
# Ok(())
# }
```

The foundation identity includes rustc's version/commit, target, Cargo.lock hash,
support package/features/profile, codegen contract and both DLL hashes/sizes and
basenames. Physical directories and logic optimization/debug levels are excluded.
`open` validates both DLLs before initializing either, then retains read handles
that deny writes/deletion on the files and their canonical parent directories
until process exit. Sibling files may still be created or edited. Use a statically
linked host: std/support already loaded outside the checked session are rejected.
There is one verified foundation per process; identical opens reuse it, while a
different foundation or partially failed initialization requires a fresh process.

`load` consumes the trusted artifact and compares it with the resident foundation
before linking the captured bytes. A mismatch cannot execute that object or
activate it in a router; it still consumes an attempt. A Cargo.lock change is
conservatively a foundation change even if its effect is only in logic.
Manifest size is limited to 256 KiB, each DLL to 256 MiB, and object size defaults
to 32 MiB (`read_with_limit` can change it, subject to the engine's own limit).
Image limits apply to both lifetime policies; attempt limits apply to KeepLoaded.
`last_load_metrics` excludes
artifact reading/hashing; include `LayeredArtifact::read` in reload latency.

## Building layered logic for iteration

After preparing a Cargo workspace with a resident support dylib and a logic crate
that depends on it, use [build-layered-jit.ps1](../scripts/build-layered-jit.ps1)
with PowerShell 7:

```powershell
./scripts/build-layered-jit.ps1 -ManifestPath path/to/workspace/Cargo.toml `
    -Package logic -TargetDirectory target/layered `
    -SupportDll target/layered/x86_64-pc-windows-msvc/debug/support.dll `
    -Output target/layered/logic.obj -Offline
```

Prepare Cargo.lock and cached dependencies first when using `-Offline`. The
workspace's dev profile must use panic=unwind and lto=false. The builder fixes
dynamic std, unwind tables and one logic codegen unit, and restores the caller's
environment on exit. It finds support through the logic dependency graph and
captures the support artifact reported by the invocation that compiles logic.
The compiler's std DLL and Cargo.lock are held while compiling. The support DLL
is opened for read sharing after Cargo finishes. Environments with `RUSTC_WRAPPER` or
`RUSTC_WORKSPACE_WRAPPER` set are rejected; trust local Cargo configuration too.
It does not turn an ordinary SDK-linked crate into a layered one.

Cargo normally emits a linked artifact even with an additional `--emit=obj`.
This helper requests `--crate-type lib`, producing a real Cargo rlib plus the COFF
and avoiding a logic PE DLL/PDB link. Support is still built as its declared dylib.
The default logic settings are opt-level=0 and debuginfo=0 for iteration; use
`-Optimization 1 -DebugInfo 2` when optimization/debug information matters. Lower
optimization can increase retained code and execution cost. Measure your workload;
the fixture is not evidence of unchanged application throughput.

The schema-2 sidecar points at immutable, hash-addressed copies under `foundation/`
and `objects/` in `-CacheDirectory`, or beside the output when it is omitted.
The `.obj` beside the manifest can supply the same hash-checked bytes when the
cache is outside the manifest directory tree. A unique staging object prevents Cargo from certifying a stale output;
the manifest is published last. Existing copies remain available for old hosts.

For subsequent logic edits, add `-FoundationManifest target/layered/logic.obj.json`
to reuse the recorded foundation and skip metadata discovery/support building.
The compiler, lockfile and DLL bytes must still match; Cargo must report the same
support artifact/profile/features. If a dependency requires rebuilding the pinned
support, the build fails without publishing a new manifest. Rerun without
`-FoundationManifest` to prepare a new foundation and start a fresh host for it.
The safe loader refuses to mix it with the old foundation. The optional
[jit-dev session](LAYERED_DEVELOPMENT.md) connects this builder to watching and explicit
managed maintenance. Script startup, hashing and immutable
copies cost time beyond the Cargo compilation alone.

## Rust runtime, TLS and panic

Use `open_with_runtime(bridge, rust_std_dll, limits)` with the exact compiler's
std DLL. Compile with prefer-dynamic, panic=unwind and emulated TLS. Rust's internal
ABI is not stable between compilers; the unsafe loader contract requires a match.
The object builder records compiler information, flags, paths and hashes in a
sidecar JSON file; the loader does not automatically validate that manifest.
The default allocator is supplied by the resident std runtime. Rust allocations,
trait objects and panic payloads remain inside their originating runtime; the
Reknot boundary continues to use the versioned C ABI.

The explicit import set is std plus System32's vcruntime140, ucrtbase, kernel32,
ntdll, userenv, ws2_32, bcryptprimitives and api-ms-win-core-synch-l1-2-0.
Executable exports receive image-local jump stubs for COFF relative calls and
SEH personality RVAs; LLVM synthesizes import slots for data. Missing imports
fail before publication. No lookup falls back to another generation or the host.
Runtime DLLs stay loaded, including on later errors. Generations using the same
std DLL share its global state, including panic hooks; this is not runtime isolation.

In the separate `open_with_runtime` profile, `thread_local!` uses LLVM emulated-TLS
descriptors in each JIT image. The bridge
keys storage by descriptor identity and actual OS thread, preserving distinct
values for old/new generations and supporting threads that predate the load.
Compiler-inserted TLS access preserves the Windows last-error value.
Rust registers and runs value destructors through its resident std runtime.
The bridge frees the backing storage only after a duplicated thread HANDLE is
signaled, after all thread-exit callbacks. It never relies on FLS/DLL destructor
ordering or reusable thread IDs. Collection runs on load, on a thread's first JIT
TLS access, or via `collect_finished_threads()`, which covers every engine using
that bridge. Live-thread storage stays allocated; main-thread storage may remain
until process exit. These allocations are outside the object-byte budget.

Panic is supported when caught inside the generation with `catch_unwind`.
Registered unwind tables and local personality stubs allow Rust cleanup frames
to run; nested catch/resume and panic-payload Drop are covered by the fixture.
Every Reknot C callback must still contain its panic. Uncaught panic, abort mode,
foreign exceptions and unwinding across the host C ABI are not a recovery path.
See [Rust's FFI unwind contract](https://doc.rust-lang.org/nomicon/ffi.html#ffi-and-unwinding).
Keeping code resident supports late TLS destructors and std callbacks; it does
not make unrelated unsafe Rust or arbitrary native code memory-safe.

## Build and use

Enabling jit never downloads LLVM or adds a Cargo build script. Build the bridge
from this source checkout with Visual Studio C++ Build Tools, CMake, Windows SDK, the official
[LLVM 23.1.0 SDK](https://github.com/llvm/llvm-project/releases/tag/llvmorg-23.1.0),
[zlib 1.3.1](https://github.com/madler/zlib/releases/tag/v1.3.1) and
[zstd 1.5.7](https://github.com/meta/zstd/releases/tag/v1.5.7) source directories.
The script does not install system software.
Use a CMake version that supports your installed Visual Studio version. The script
uses CMake's default Visual Studio generator (or the existing build directory's
cached generator), rather than requiring a specific Visual Studio release. It
finds CMake on PATH or through Visual Studio Installer's `vswhere` tool.

Optionally run `./scripts/setup-jit.ps1` to fetch the pinned archives, check their
SHA-256 hashes and extract them under target/jit-deps; then pass those directories
to the build script. The dedicated Windows JIT workflow uses this path.

```powershell
./scripts/build-jit.ps1 -LlvmRoot C:/deps/llvm-23.1.0 `
  -ZlibSource C:/deps/zlib-1.3.1 -ZstdSource C:/deps/zstd-1.5.7
```

The output is target/jit-bridge/Release/reknot_jit_bridge.dll. Vendored LLVM files
retain their [original license](../native/jit/vendor/llvm/LICENSE.TXT); see
[the local patch notes](../native/jit/vendor/llvm/README.md).
The crates.io SDK archive does not contain this separately built bridge.

```rust,no_run
use reknot::{HostResources, RequestRouter};
use reknot::jit::{JitEngine, JitLimits};

fn start() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: matching trusted bridge and self-contained objects that obey
    // Reknot's complete handler ABI and owner-thread contracts.
    let mut engine = unsafe {
        JitEngine::open("reknot_jit_bridge.dll", JitLimits::default())?
    };
    let first = unsafe { engine.load("logic-v1.obj")? };
    let mut router = RequestRouter::from_loaded(first, HostResources::new())?;
    let next = unsafe { engine.load("logic-v2.obj")? };
    let candidate = router.prepare_loaded(next)?;
    router.activate(candidate)?;
    Ok(())
}
```

Enable reknot's jit feature for this example and register host resources before
dispatch. `last_load_metrics()` reports read/copy/add/materialize/registration
timings after success and is cleared by the next attempt. For the checked Rust
backend, the [jit-dev session](LAYERED_DEVELOPMENT.md) builds and selects COFF objects.

For the emulated-TLS profile, install a nightly toolchain explicitly. The dedicated workflow pins
nightly-2026-11-02. Emulated TLS uses the unstable
[`-Z tls-model=emulated`](https://doc.rust-lang.org/unstable-book/compiler-flags/tls-model.html)
compiler option; a build that emits native TLS in logic is rejected. The two-layer
profile above does not require this option and rejects emulated TLS as well.

```powershell
./scripts/build-jit-object.ps1 -Source logic.rs -Output target/logic-v1.obj `
  -Toolchain nightly-2026-11-02
$runtime = (Get-Content target/logic-v1.obj.json -Raw | ConvertFrom-Json).rust_std_dll
```

The builder handles a standalone source crate and std, without resolving Cargo
dependencies. Pass the recorded runtime path to the host:

```rust,no_run
use reknot::jit::{JitEngine, JitLimits};
use reknot::{HostResources, RequestRouter};
use std::path::Path;

unsafe fn start_std(std_dll: &Path) -> Result<RequestRouter, Box<dyn std::error::Error>> {
    let mut engine = JitEngine::open_with_runtime(
        "reknot_jit_bridge.dll", std_dll, JitLimits::default(),
    )?;
    let loaded = engine.load("target/logic-v1.obj")?;
    Ok(RequestRouter::from_loaded(loaded, HostResources::new())?)
}
```

## Verification

Core tests cover task/destructor/late-Waker ownership, abandoned and rejected
candidates, host return guards, typed Drain and Restart/migration. Compile-fail
examples check move/thread restrictions. The Miri script includes loaded-handler;
its intentional mem::forget leak check runs only outside Miri.

The actual JIT test is explicitly ignored unless selected. Build private test
helpers and run:

```powershell
./scripts/build-jit.ps1 -LlvmRoot C:/deps/llvm-23.1.0 `
  -ZlibSource C:/deps/zlib-1.3.1 -ZstdSource C:/deps/zstd-1.5.7 -Testing
$env:REKNOT_JIT_BRIDGE = (Resolve-Path target/jit-bridge/Release/reknot_jit_bridge.dll).Path
$env:REKNOT_JIT_TOOLCHAIN = 'nightly-2026-11-02'
$env:REKNOT_JIT_LAYERED_TOOLCHAIN = 'stable'
cargo test -p reknot --features jit --lib real_coff --locked -- --ignored --nocapture
```

The test compiles fresh COFF fixtures, completes old/new requests after both engine
and router destruction, walks a registered JIT stack, rejects bad ABI/TLS/missing
imports, and checks attempt/byte limits including abandoned and forgotten owners.
The std fixture exercises Vec/String/HashMap, Arc/Mutex/channels, time/environment,
std threads, TLS alignment/initialization and last-error preservation, destructor-created TLS, old/new TLS
isolation, concurrent collection, nested panic/resume and cleanup/payload Drop.
Old/new requests and late TLS destructors also run after engine/router destruction.
It does not prove arbitrary native-code safety, every std operation or bounded
process memory. Run native reclamation suites when changing shared lease/guard
behavior: DLL and JIT reclamation policies differ.

The layered SDK fixture builds one support DLL and old/new logic COFFs in an
isolated Cargo workspace under `target/jit-layered-fixtures/<pid>`. Build logs and
objects remain there for inspection. It checks resident JSON/TLS services,
generation-local separation and delayed destruction, old/new responses, initializer
and access panics, payload Drop, managed-child cancellation, destructor failure
without retry, and rejection of native/emulated TLS, missing support/imports,
malformed COFF and bad Handler ABI. Rejected loads leave the old router active.
The COFF test requires cached Cargo dependencies and a nightly toolchain for its
deliberately rejected emulated-TLS fixture; the accepted two-layer input uses stable.

To check the Rust backend alone, skip all LLVM setup and bridge environment variables:

```powershell
$env:REKNOT_JIT_TOOLCHAIN = 'nightly-2026-11-02'
cargo test -p reknot --no-default-features --features jit --lib jit::coff --locked
cargo test -p reknot --no-default-features --features jit --lib real_coff_rust_layered --locked -- --ignored --nocapture
cargo test -p reknot --no-default-features --features jit --lib real_coff_verified_layered_identity --locked -- --ignored --nocapture
```
