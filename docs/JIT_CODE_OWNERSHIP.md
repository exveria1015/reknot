# Code ownership and experimental COFF/JITLink

English | [日本語](../docs_ja/JIT_CODE_OWNERSHIP.md)

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

Successfully linked JIT code, data, dependencies and registered unwind tables remain resident
until process exit. Dropping a router, engine controller, rejected candidate or final
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
generation budget separately limits admission attempts.

An external callback can release its last Waker and still execute old instructions.
Counters alone do not establish external quiescence. No safe automatic unload API
is introduced. Existing unsafe DLL reclamation retains its
[external-return and TLS requirements](UNLOAD_SAFETY.md).

## Supported JIT subset

The optional `jit` feature uses a separately built LLVM 23.1.0 bridge on Windows
x86-64. It explicitly uses ORC ObjectLinkingLayer / JITLink and registers COFF
.pdata through RtlAddFunctionTable before publishing the handler entry.
Each image has its own JITDylib; existing images are never patched in place.

Inputs must be trusted, self-contained MSVC x86-64 COFF objects exporting
hot_handler_api with Handler ABI 5. The verification fixture uses no_std,
panic=abort, explicit unwind tables and one simultaneous task per generation.
The host SDK itself retains its normal task model.

The bridge rejects linked PE images, other formats/architectures, TLS and CRT
constructor/destructor sections. It does not search arbitrary process symbols;
unresolved runtime imports fail. The local LLVM patch supports the fixture's
strong local COMDAT leader form and rejects other local forms.

General std, allocators, external crates, runtime startup, thread-local destructors
and panic/foreign-exception propagation are not implemented for this backend.
Accepting current ABI tables and legacy prefixes does not establish support for
ordinary generated typed modules. CodeView sections are accepted; debugger
registration is absent. Stack walking through a JIT frame is checked; panic
unwinding and Rust Drop during exception propagation are separate, unverified properties.

## Build and use

Enabling jit never downloads LLVM or adds a Cargo build script. Build the bridge
from this source checkout with VS 2022 Build Tools, CMake, Windows SDK, the official
[LLVM 23.1.0 SDK](https://github.com/llvm/llvm-project/releases/tag/llvmorg-23.1.0),
[zlib 1.3.1](https://github.com/madler/zlib/releases/tag/v1.3.1) and
[zstd 1.5.7](https://github.com/meta/zstd/releases/tag/v1.5.7) source directories.
The script does not install system software.

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
timings after success and is cleared by the next attempt. The watcher and Cargo
pipeline do not yet build or select COFF objects automatically.

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
cargo test -p reknot --features jit --lib jit::tests::real_coff -- --ignored --nocapture
```

The test compiles fresh COFF fixtures, completes old/new requests after both engine
and router destruction, walks a registered JIT stack, rejects bad ABI/TLS/missing
imports, and checks attempt/byte limits including abandoned and forgotten owners.
It does not prove arbitrary native-code safety, full Rust runtime support or bounded
process memory. Run native reclamation suites when changing shared lease/guard
behavior: DLL and JIT reclamation policies differ.
