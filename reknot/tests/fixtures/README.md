# Standalone ABI fixtures

Linux `real_elf` tests compile the layered support/logic sources as a resident
Rust `.so` and ELF64 PIC objects, and compile the standalone ABI fixture for
entry/import rejection. They exercise cutover, generation locals, support TLS,
contained panic, cancellation, RX code permissions, residency after Drop and
attempt/input/image/session budgets. Run with `--features jit-dev --lib real_elf --
--ignored --nocapture --test-threads=1` on Linux x86_64 GNU after caching the SDK
dependencies. Python 3.9+ and rustup are needed by the checked-artifact tests,
which also cover foundation identity, Managed collection and source watching
as described below. These tests require no LLVM bridge.

`jit_layered_support.rs` and `jit_layered_logic.rs` form an isolated Cargo project
in the layered JIT test. Support is a compiler-matched Rust dylib containing the
real SDK and non-generic serde_json services; logic is emitted as a single COFF
with CGU=1, LTO disabled, dynamic std and panic=unwind. Support uses native TLS;
logic uses GenerationLocal. Rejection variants include native TLS and an external
TLS index without a TLS section. The test also rejects the separate emulated-TLS
fixture, missing support imports and malformed inputs. These fixtures are included
in the package and only reference its own SDK, not sibling example sources.
The LLVM and Rust loaders use the same generated objects and DLLs. The Rust
integration runs without a bridge; its fast-build variant uses a Cargo library
object with opt-level=0/debuginfo=0 and repeats the same panic/cancellation checks.
`REKNOT_JIT_LAYERED_CRATE_TYPE`, `REKNOT_JIT_LAYERED_OPT` and
`REKNOT_JIT_LAYERED_DEBUG` select those test build settings.
The verified-identity integration also builds these sources through the repository's
PowerShell 7 `build-layered-jit.ps1`, including foundation reuse and dependency-change
rejection. Fresh child processes check safe open/load, immutable object snapshots,
DLL tampering, file/directory pinning, preloaded DLL rejection and old/new requests
after a mismatched foundation is refused. It requires the repository checkout and
cached Cargo dependencies; outputs stay under `target/layered-identity-fixtures`.

`jit_std_handler.rs` uses the same frozen ABI with a compiler-matched dynamic std
DLL, emulated TLS and panic=unwind. The optional Windows JIT integration compiles
it with nightly Rust and checks allocation/synchronization, old/new TLS isolation,
thread-exit destructors, concurrent TLS collection and nested panic cleanup.

`jit_handler.rs` and `jit_compute.rs` use the frozen ABI without std, allocation,
CRT or TLS. They compile as COFF objects for the optional real JIT test, with one
task slot per image. Variants deliberately request bad ABI, TLS and unresolved
imports to verify rejection. This constrained fixture is not a general runtime.

`handler.rs` is compiled directly with rustc as a native DLL. It uses the
co-located `abi_v5.rs` snapshot and does not import the SDK or its macros.
Both files are included in the `reknot` package, so native ABI tests can
run from an extracted package without an adjacent workspace checkout.

The snapshot fixes Handler ABI 5, Services ABI 2, task-control ABI 2 and activity
ABI 1. Update it deliberately when adopting an ABI change, alongside rejection
tests for older layouts. It is not regenerated from production sources during
tests. Tests using the real SDK and generated handlers live in the repository's
separate example crates and exercise the other side of this boundary.

`reclamation.rs` uses the same frozen ABI with the actual SDK Waker bridge and
code-lease types included from `src/`. It constrains execution and exposes a return
barrier so the host can verify that the last lease disappearing does not close a DLL.

`managed_logic.rs` is built as a separate cdylib against the actual SDK with native
features disabled. It exercises generated handlers, registered generation locals,
host async notifications and destructor failure across the real allocation/FFI boundary.

`typed_logic.rs` is a real SDK typed module compiled by `native_typed`. The test
creates its local helper dependency in an isolated project and checks source
watching, typed return values, Drain, build recovery and contract rejection.

`cancellation_logic.rs` is shared by static regression tests and a real SDK DLL.
It uses `FuturesUnordered` to check nested/multiple cancellation waits, dropped
registrations, root/child cleanup and Restart. Native tests also reclaim both
generations after every cancellation registration has been released.

# Layered development and managed reclamation

Linux x86_64 GNU runs `cargo test -p reknot --no-default-features --features jit-dev --lib real_elf --locked -- --ignored --nocapture --test-threads=1`
with Python 3.9+, stable Rust and a C linker. The ELF checked suite uses the same
SDK fixtures and isolated subprocesses for sealed foundation identity, preloaded
library rejection, 128 managed updates, physical `_Unwind_Find_FDE` and
`/proc/self/maps` release checks, complete external-thread return and injected
unwind/munmap failures. Source watching covers Wait/CancelOldest, pinned cleanup,
failed builds and foundation restart. See [the ELF guide](../../../docs/ELF_JIT.md).

The `jit_stage_de_tests` acceptance tests use the layered SDK fixtures with the
PowerShell producer. Run on Windows x64 MSVC without LLVM:

```powershell
cargo test -p reknot --release --no-default-features --features jit-dev --lib stage_de --locked -- --ignored --nocapture --test-threads=1
```

Tests cover real saves, superseded builds, syntax failure, foundation restart,
source-directory replacement, newly discovered local dependencies, attempt limits,
writer serialization, interrupted publication, cache corruption and capacity.
The failed-dependency regression adds a local dependency that cannot compile, then
fixes only that dependency. Both KeepLoaded and Managed must rebuild without retry,
preserve the active generation, and still report changed foundations as restart-required.
Managed fixtures cover 128 physical releases with default cumulative engine/Router
limits (neither allowance is consumed), completed-but-held requests, locals,
panic payloads, cancellation, reentrant host collection and late resident notifications.
The joined-thread adapter holds a synchronization barrier after releasing its Waker,
then proves final return through join. Adapter panic, forgotten ownership and failed
local destruction remain retained. This fixture does not certify external runtimes.
Development pressure cases cover Wait, CancelOldest, joined external callbacks,
exact page capacity, superseded pending candidates and pinned cleanup failures.
They verify that current admission survives, completed handles still prevent release,
and automatic collection leaves other controllers' images alone. KeepLoaded exhaustion
stops new builds as well as loads, including after retry.
Windows unit tests also inject unwind-removal/VirtualFree failures and inspect actual
`VirtualQuery` / `RtlLookupFunctionEntry` state. Injected failures are not naturally
occurring OS errors. Serialize performance tests to avoid measuring concurrent Cargo.
The edit-latency test compares the same producer/input-discovery pipeline on a
worker and synchronously, with a warm Cargo target, 50 ms debounce and 5 ms owner
polling. It records save-to-first-new-response and the longest old-response gap;
three samples per mode are descriptive measurements, not a latency guarantee.

`jit_typed_logic.rs` is compiled through the real COFF/ELF producer by
`layered_typed`. A typed support dylib re-exports the SDK; isolated processes verify
KeepLoaded/Managed, typed contract diagnostics, stable calls/clients, failed-build
recovery, foundation changes and cooperative worker Restart with checkpoints.
See [typed JIT development](../../../docs/TYPED_JIT.md#verify-the-integration).

The layered typed fixture also runs with `typed-binary` on host/support. The test
selects MessagePack in the same declarations and checks both worker and ordinary
call results. `layered_typed` uses the built-in Rust producer by default; set
`REKNOT_TYPED_JIT_PRODUCER=script` to exercise the legacy producer path.
