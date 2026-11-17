# Development contracts, when you need them

English | [日本語](../docs_ja/DEVELOPMENT_CONTRACTS.md)

For a first run, follow the [README](README.md). The generated host already drives
updates and keeps state outside the edited logic. Consult this page when changing
the structure or lifetime of an application.

## Choose the relevant contract

| What you are doing | What to keep in mind | Details |
| --- | --- | --- |
| Edit a reply or calculation | Change `logic`, save, wait for the reload event; admitted work finishes on its original code (Drain) | [Typed handlers](TYPED_HANDLERS.md) |
| Change inputs, outputs, encoding or meaning | Update the shared protocol and its explicit version; rebuild/restart the host | [Schemas](CONTRACTS_AND_SCHEMAS.md), [binary messages](BINARY_MESSAGES.md) |
| Keep connections or application state | Own them in the host and expose services to logic | [Host resources](HOST_RESOURCES_AND_ROUTING.md) |
| Run a long-lived worker | Register it and its checkpoint; select cooperative Restart when it must be recreated | [Typed Restart](TYPED_RESTART.md) |
| Spawn children or callbacks | Register their lifetime and join cleanup; cancellation does not undo external effects | [Managed activities](MANAGED_ACTIVITIES.md) |
| Reach the default 32 cumulative KeepLoaded attempts | Restart the development process; failed native loads can consume attempts too | [Retention](RETENTION_AND_CLEANUP.md) |
| Use JIT instead of DLL linking | Keep std/support resident; put stable non-generic work in support; rebuild/restart after foundation changes | [Typed JIT](TYPED_JIT.md) |
| Keep generation-local data in JIT logic | Use `GenerationLocal`; compiler `thread_local!` is unsupported in the reloadable object | [Generation locals](MANAGED_LOCALS_AND_NOTIFICATIONS.md) |
| Reclaim JIT code with Managed | Explicitly accept the additional lifetime contract, including the final return of external callbacks and Wakers | [Layered development](LAYERED_DEVELOPMENT.md) |

## Keep defaults until a requirement changes

The starter uses native DLLs, JSON, Drain and KeepLoaded. It needs Rust/Cargo and
the platform toolchain. Restart, binary messages and JIT are separate choices;
none is required to edit the first handler. Both encodings use owned messages,
bounded to 1 MiB per input/result; metadata and checkpoints remain JSON.

Use only trusted local code, dependencies and build scripts. Host and logic must
use the same SDK. A compiler/lockfile match is an identity check, not proof that
borrowed Rust objects can safely outlive an unloaded generation.

[Compatibility](COMPATIBILITY.md) · [Troubleshooting](TROUBLESHOOTING.md)
