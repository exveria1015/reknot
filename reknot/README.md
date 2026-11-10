# reknot

Message-oriented hot reload for trusted Rust logic during local development.
The host owns connections, resources and admission; generation-bound tasks run
the logic. Drain finishes existing work on old code. Restart cooperatively stops
registered workers and recreates them from versioned checkpoints.

Start with `handler`, `export_handlers!` and `typed::ModuleHost` for ordinary
typed functions, stable handles and single-module Drain updates. See the
[typed-handler guide](https://github.com/exveria1015/reknot/blob/main/docs/TYPED_HANDLERS.md).
`reknot-new PATH` generates a standalone host/logic/protocol workspace using the SDK
source that built the command (`--sdk-path` overrides the reknot crate path).
The typed host bridge provides Send + Sync clients, bounded admission and explicit
shutdown/join while native tasks remain on their owner thread.

Use `reload_handler`, `HostResources`, `RequestRouter` and `RequestTask` for low-level
native reload and static linking. Default features enable `native` loading and
`dev-tools` build/watch helpers, plus `typed` Serde JSON inputs/results. Logic crates
and static hosts can use `default-features = false`; add `features = ["typed"]`
when using the typed facade.

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
