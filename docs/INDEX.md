# Documentation by task

English | [日本語](../docs_ja/INDEX.md)

If you are new to the SDK, start with the [README example](README.md#try-it).
Use this index to find a topic when you need it during implementation or development.

## Build an application

- [WebSocket quiz room](QUIZ_ROOM.md) — Play and compare Axum + Reknot with Fastify + JS while editing logic.

- [Typed handlers](TYPED_HANDLERS.md) — Generate a project, write ordinary functions and connect them to the host.
- [Examples](../examples/README.md) — Find commands and the behavior each sample demonstrates.
- [Tauri GUI lab](TAURI_GUI_LAB.md) — Edit Rust in a window, reload it and recover from build failures.
- [Minimal Bevy integration](BEVY_GAME.md) — Update at frame boundaries, handle old-generation results and recreate workers.

## Connect the host and logic

- [Call from framework threads](TYPED_HANDLERS.md#call-from-framework-threads) — Pass events through a typed client.
- [Integrate through the lower-level API](GETTING_STARTED.md) — Start with host resources and byte messages.
- [Host resources and Drain](HOST_RESOURCES_AND_ROUTING.md) — Register and revoke resources, call services and activate candidates.
- [Async services and threads](ASYNC_SERVICES_AND_THREADS.md) — Await host I/O, cancel operations and notify across threads.
- [Service contracts and typed initialization](SERVICE_CONTRACTS_AND_INIT.md) — Declare required services and pass per-generation configuration.

- [Contract diagnostics and schemas](CONTRACTS_AND_SCHEMAS.md) — Derive schemas, inspect structural differences and export contracts.

## Preserve state and finish work

- [Typed Restart](TYPED_RESTART.md) — Register typed workers, migrate state and recover after interruption.
- [Restart and state migration](RESTART_AND_MIGRATION.md) — Cooperatively stop registered workers, migrate checkpoints and recreate them.
- [Host resource and database migration](HOST_RESOURCE_MIGRATION.md) — Coordinate SQLite and cache access, commit hooks and recovery.
- [Managed children and external callbacks](MANAGED_ACTIVITIES.md) — Register child work, request cancellation and wait for it to finish.
- [Managed TLS and host notifications](MANAGED_LOCALS_AND_NOTIFICATIONS.md) — Destroy generation-local values, handle delayed notifications and define dependency responsibilities.

## Continue development

- [Development operation](DEVELOPMENT.md) — Watch sources, build and switch, recover from failure and observe limits.
- [Troubleshooting](TROUBLESHOOTING.md) — Find checks by symptom or error.
- [Retention and copy cleanup](RETENTION_AND_CLEANUP.md) — Inspect loaded images, cache sessions and files left after exit.
- [Compatibility and scope](COMPATIBILITY.md) — Check prerequisites, SDK/ABI requirements and application responsibilities.

## Look up APIs and internals

- [API reference](API_REFERENCE.md) — API roles, typed-call contracts, build configurations and ABI details.
- [Architecture and requirements](ARCHITECTURE.md) — Understand ownership, update boundaries, state and failure handling.
- [Ownership, borrowing and destruction](LIFETIME_REVIEW.md) — Follow asynchronous work and code-generation lifetimes.
- [DLL lifetime and unmanaged work](UNLOAD_SAFETY.md) — Understand retention, explicit reclamation and external/TLS cleanup conditions.

## Verify changes

- [Testing and Miri](TESTING.md) — Rerun checks and understand what each covers.
- [Preparing a release](RELEASING.md) — Confirm the source, CI gates, distribution route and release limits.
- [Changelog](../CHANGELOG.md) — Review user-visible changes and breaking APIs.
- [Contributing](../CONTRIBUTING.md) — Find source layout, verification commands and documentation conventions.

[Project README](README.md)
