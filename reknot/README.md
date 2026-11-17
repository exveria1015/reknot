# reknot

Message-oriented hot reload for trusted Rust logic during local development.
The host owns connections, resources and admission; generation-bound tasks run
the logic. Drain finishes existing work on old code. Restart cooperatively stops
registered workers and recreates them from versioned checkpoints.

Start with `handler`, `export_handlers!` and `typed::ModuleHost` for ordinary
typed functions, stable handles and single-module Drain/Restart updates with structural schema checks. See the
[typed-handler guide](https://github.com/exveria1015/reknot/blob/main/docs/TYPED_HANDLERS.md).
`reknot-new PATH` generates a standalone host/logic/protocol workspace using the SDK
from an exact registry version, a fixed Git commit (`--sdk-rev`) or an explicit local path (`--sdk-path`).
Unpublished source candidates use the Git route; see the guide for installation commands.
The typed host bridge provides Send + Sync clients, bounded admission and explicit
shutdown/join while native tasks remain on their owner thread.

Use `reload_handler`, `HostResources`, `RequestRouter` and `RequestTask` for low-level
native reload and static linking. Default features enable `native` loading and
`dev-tools` build/watch helpers, plus `typed` Serde JSON inputs/results. Logic crates
and static hosts can use `default-features = false`; add `features = ["typed"]`
when using the typed facade.
Optional `typed-binary` adds per-handler named MessagePack with the same owned-byte
boundary and 1 MiB limit. See [binary messages](https://github.com/exveria1015/reknot/blob/main/docs/BINARY_MESSAGES.md).

The optional `jit` feature adds experimental Windows x64 COFF and Linux x86_64 GNU
ELF object loaders with owned `LoadedHandler` routing. The Linux `ElfEngine` needs
no LLVM bridge and supports checked identity and Managed collection; see the
[Linux ELF guide](https://github.com/exveria1015/reknot/blob/main/docs/ELF_JIT.md).
`jit-dev` adds Windows COFF and Linux ELF layered source builds and watching without
requiring `native` or `typed`. The Rust backend needs a compiler-matched resident
std/support foundation; the LLVM backend needs a separately built bridge.
KeepLoaded is the default. Checked Managed loading requires an additional unsafe
lifetime contract and owner-thread collection. See
[code ownership](https://github.com/exveria1015/reknot/blob/main/docs/JIT_CODE_OWNERSHIP.md)
and [layered development](https://github.com/exveria1015/reknot/blob/main/docs/LAYERED_DEVELOPMENT.md).
With `typed,jit-dev`, the same `DevModule` accepts layered options and preserves typed clients and Drain/Restart; see the
[typed JIT guide](https://github.com/exveria1015/reknot/blob/main/docs/TYPED_JIT.md).
`LayeredDevOptions::builtin` and the `reknot-build` CLI include a Rust producer in
this crate archive; ordinary layered builds need no PowerShell/Python. Legacy
external scripts and the LLVM bridge remain separate repository tools.

Native loading is unsafe and assumes trusted SDK-compatible code. Keep host and
logic on the same SDK version. Tasks and resource registries remain on their
owner thread; external work must be explicitly registered and joined. Loaded
native images remain resident until process exit by default. The low-level
`unsafe reclaim_native_image` operation requires caller proof of external execution,
Waker return, TLS and dependency quiescence; it does not refund the load-attempt budget.

The SDK is pre-release. Production rollout, crash isolation, forced termination,
arbitrary Future stack migration and automatic rollback of external effects are
outside its scope.

See the [repository](https://github.com/exveria1015/reknot),
[getting started](https://github.com/exveria1015/reknot/blob/main/docs/GETTING_STARTED.md)
and [compatibility contract](https://github.com/exveria1015/reknot/blob/main/docs/COMPATIBILITY.md).
Native application demos live in the repository's separate example crates;
the package includes static examples and standalone ABI test fixtures.

Licensed under MIT OR Apache-2.0. Both license texts are included in this package.
