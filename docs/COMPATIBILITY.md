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
| GUI backend | Windows and Linux CI integration without opening a window |
| GUI interaction | Linux WebView under Xvfb; editor, Drain/Restart and build-error recovery |
| macOS / other targets | No CI coverage in this repository |

CI configuration describes what is checked, not a guarantee that a particular
revision has passed. See [testing](TESTING.md) and the
[workflow](../.github/workflows/ci.yml). Interactive window behavior is checked
separately using the [GUI guide](TAURI_GUI_LAB.md).

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
| Host and logic | Build together against the same SDK and compatible target/toolchain |
| Service, input, init and checkpoint formats | Application-defined, explicitly versioned where applicable |

ABI metadata checks reject incompatible tables; they do not prove native code
safety or payload meaning. Native-loading APIs are `unsafe`. Initialization,
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
- Bound development sessions. Loaded images use KeepLoaded; failed load attempts
  also consume the shared cumulative budget. Restart the process when exhausted,
  and explicitly clean eligible exited-session files as needed.
- Explicit `unsafe reclaim_native_image` requires host proof of external return,
  Waker return, TLS and dependency quiescence. See [DLL lifetime](UNLOAD_SAFETY.md).

Production rollout, crash isolation, arbitrary Future stack migration,
automatic rollback of external effects, forced termination and automatic DLL
unloading are outside the current scope. See [development operation](DEVELOPMENT.md)
for recovery and [retention and cleanup](RETENTION_AND_CLEANUP.md)
for cache operations.
