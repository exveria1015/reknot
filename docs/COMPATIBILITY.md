# Compatibility and scope

English | [日本語](../docs_ja/COMPATIBILITY.md)

Reknot is a pre-release SDK for updating trusted Rust logic during local
development. The Rust API can change before release. Native and static logic
use the same message API and ownership contracts.

## Toolchains and platforms

| Component | Requirement / coverage |
| --- | --- |
| Core crates and non-Bevy examples | Declared minimum Rust 1.95; core MSRV checks are configured in CI |
| Bevy example | Rust 1.97.1+; headless integration, without a window or GPU |
| SQLite migration example | A C compiler for bundled SQLite |
| Tauri GUI | Platform Tauri prerequisites; WebView2 on Windows; no npm or Tauri CLI |
| Native core and examples | Windows and Linux CI jobs are configured |
| Experimental Rust COFF (`jit`) | Windows x64 MSVC; compiler-matched resident std/support DLLs; no LLVM required |
| Experimental Rust ELF (`jit`) | Linux x86_64 GNU/glibc; ELF64 ET_REL/PIC objects, resident compiler-matched std/support; no LLVM required; checked identity, opt-in Managed collection and Python 3.9+ `jit-dev` |
| Layered development (`jit-dev`) | COFF + PowerShell 7 on Windows, or ELF + Python 3.9+ on Linux; repository layered builder |
| Experimental LLVM COFF (`jit`) | Windows x64, MSVC COFF and a separately built LLVM 23.1.0 bridge; emulated TLS additionally requires nightly Rust |
| GUI backend | Windows and Linux CI integration without opening a window |
| GUI interaction | Linux WebView under Xvfb; editor, Drain/Restart and build-error recovery |
| macOS / other targets | No CI coverage in this repository |

CI configuration describes what is checked, not a guarantee that a particular
revision has passed. See [testing](TESTING.md) and the
[workflow](../.github/workflows/ci.yml). Interactive window behavior is checked
separately using the [GUI guide](TAURI_GUI_LAB.md).
COFF and ELF checks have a separate [Windows/Linux workflow](../.github/workflows/jit.yml).
See the [Linux ELF guide](ELF_JIT.md) for its build contract and limitations.
See [code ownership](JIT_CODE_OWNERSHIP.md) for the accepted object/runtime profiles.

## API and ABI

For ordinary functions, use `handler`, `export_handlers!` and `typed::ModuleHost`.
The lower-level entry points for controlling host resources and admission are `reload_handler`, `HostResources`, `RequestRouter` and `RequestTask`.
The typed API follows the same ABI and ownership contracts.
Features select the loading mode independently of Debug/Release.
See [build configurations](API_REFERENCE.md#build-configurations) for the settings.

| Boundary | Version / contract |
| --- | --- |
| Handler ABI | 5; original prefix retained, optional metadata tail v1 |
| Typed contract | v2; schemas in both Serde directions, `JsonSchema` required |
| Host services ABI | 2 |
| Task-control ABI | 2 |
| Cancellation notifications | Optional tail ABI 1; required by pending `RequestContext::cancelled()` waits |
| Host and logic | Build together against the same SDK and compatible target/toolchain |
| Service, input, init and checkpoint formats | Application-defined, explicitly versioned where applicable |

ABI metadata checks reject incompatible tables; they do not prove native code
safety or payload meaning. Native execution requires an `unsafe` trust contract;
the checked COFF/ELF API accepts it through `into_trusted`, then checks identity through
safe `LayeredEngine` methods. Raw native-loading APIs remain `unsafe`. Initialization,
finalization, callbacks, owned data and code lifetimes must all meet SDK contracts.
See the [API reference](API_REFERENCE.md) and [breaking changes](../CHANGELOG.md).

## Application responsibilities

- Keep DLL tasks and host resources on their owner thread. Other threads may submit requests through a typed `HandlerClient`,
  but cannot move the DLL tasks or resource handles themselves.
- Drive polling, cancellation cleanup and registered child/callback joins.
  Register external work explicitly; unmanaged spawn tasks are not discovered.
- Define service and state schemas, bounded queues and shutdown deadlines.
  Cooperatively stop workers before Restart; only registered workers are recreated.
- Implement resource exclusion, transaction rollback and durable recovery when
  migrating host resources. Router installation and database commit are not
  one crash-atomic operation.
- Bound development sessions. Images default to KeepLoaded; failed load attempts
  also consume the shared cumulative budget. Restart the process when exhausted,
  and explicitly clean eligible exited-session files as needed.
- Managed COFF/ELF is an additional opt-in lifetime contract. It uses live image/page
  capacity instead of cumulative attempts, and requires owner-thread maintenance,
  completed-handle disposal and audited external joins. std/support stay resident;
  changing their identity requires restarting the process. See [layered development](LAYERED_DEVELOPMENT.md).
- Explicit `unsafe reclaim_native_image` requires host proof of external return,
  Waker return, TLS and dependency quiescence. See [DLL lifetime](UNLOAD_SAFETY.md).

Production rollout, crash isolation, arbitrary Future stack migration,
automatic rollback of external effects, forced termination and automatic DLL
unloading are outside the current scope. See [development operation](DEVELOPMENT.md)
for recovery and [retention and cleanup](RETENTION_AND_CLEANUP.md)
for cache operations.
