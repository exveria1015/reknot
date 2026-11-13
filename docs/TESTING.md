# Testing and Miri

English | [日本語](../docs_ja/TESTING.md)

## Typed handlers

```sh
cargo test -p reknot --no-default-features --features typed --test typed_handlers --test typed_services --test typed_bridge --test typed_contracts --test typed_restart --test typed_cancellation --locked
cargo test -p reknot --test native_typed --locked -- --ignored --test-threads=1
cargo test -p reknot --test scaffold --locked -- --include-ignored
cargo test -p typed_bot_host --no-default-features --features static-logic --locked -- --include-ignored
cargo run -p typed_bot_host --example framework_bridge --no-default-features --features static-logic --locked
```

Static tests cover generation selection before polling, multiple entry points, contract rejection, host state and revocation, size and concurrency limits,
panic reporting, children and cancellation cleanup. Compile-fail doctests cover type mismatches, borrowed inputs and Send constraints.
Real-DLL tests use isolated projects to check edits to local dependencies, coexistence of old requests and new code, recovery by saving after build failure,
contract-change rejection, discarding stale candidates after further edits, adding and watching local dependencies, and avoiding watch loops triggered by build output.
They also cancel a request submitted from another thread to the old generation after cutover, and wait for its registered native children to finish.
Custom-target tests edit sibling/nested modules and local dependencies, reload Cargo configuration from an unrelated host cwd,
and verify that generated Rust output does not retrigger builds.
Bridge tests cover real threads, FIFO admission, capacity including uncollected replies, cancellation while queued or running,
multiple shutdown waiters, owner destruction and continued operation after a result-notification Waker panics.
Generator tests check rejection of existing directories and invalid input. The ignored test builds and runs generated projects
in static and native configurations, then saves edits while running to check cutover and preservation of the host's message count.
Package checks repeat the same generation and execution test from extracted archives.
Generated-console tests hold maintenance for an hour and verify immediate-wake requests still finish.
Both generated and example console hosts test quit and EOF independently during a pending request, including child cleanup;
ordinary input while busy is explicitly rejected. Bridge/worker tests verify all other replies and shutdown notifications survive a Waker panic.
The 1 MiB boundaries are checked in ordinary runs; Miri uses small inputs to check ownership and FFI paths.
The Miri script includes `typed-handlers`, `typed-services` and `typed-bridge` under both borrow models.
Contract tests check both Serde directions, field changes under unchanged type names/versions, source locations and bounded diagnostics.
Typed Restart tests cover migration, final commit, failed/interrupted recovery, child join, queue wakeup and reentrant Drop.
Real DLLs also exercise automatic Restart and explicit state migration. The CLI is installed from an isolated Git snapshot,
then generates a Git-dependent application that exports its schema and runs in native/static modes.
Miri includes `abi-metadata`, `typed-contracts` and `typed-restart`, including reading an actual shorter ABI 5 table.
These tests do not prove semantic compatibility, framework-specific SDK connections, automatic DLL reclamation or guaranteed OS notification delivery.

The `native_watch_` tests create `.cargo` after startup, replace it with invalid configuration and recover by saving.
They also remove, recreate and move a source directory, then verify that later edits still reload.
Generated output and unrelated configuration-directory files must not trigger builds.
Separate intermediate outputs configured through Cargo files or environment variables are also excluded,
while ordinary source edits still reload. Output/source overlap is rejected before compiling.
Windows cases vary the spelling of both existing and not-yet-created output paths and check source-overlap rejection after resolving those paths.
Package checks repeat these tests from extracted archives.

## What each test covers

| Target | Coverage |
| --- | --- |
| `typed_cancellation` | Explicit cancellation of parked calls, Restart progress, latest Waker, repeated cancellation without self-wake, delayed child cleanup |
| `cleanup_notifications` | Requests dropped during cleanup get another bounded pass; idle waits, scheduling panic and reentrant notification |
| `notification_unwind` | Concurrent completion and host notification; final executor Waker Drop panic, alone or with wake panic; subprocess isolation natively, direct execution in both Miri models |
| `source_watcher` | Low-level watch recovery after source/project recreation, initially missing source, manifest/build script replacement, output/log/git filtering |
| `activity_notifications` | Nested child/service registration completes using wakes only, bounded poll passes, no idle self-wake, ownership after a scheduling wake panics |
| `async_service::tests` | Independent cancellation waiters, Waker replacement/release, dropped/reused registrations, cross-thread registration/cancel/drop races, notification panic and reentry |
| `host_resources` | Ownership, revocation, resource IDs/versions, borrow conflicts, output capacity and panic boundaries |
| `cache_lifecycle` | Separate host/CLI processes, image states, leases after Router Drop, normal/forced exit, non-inheritance, namespace contention, trash retry and exclusion of unknown/legacy/link entries |
| `request_routing` | Input copying, Drain, candidate rejection, pause/resume and execution outliving its Router |
| `restart` | Cooperative stop, waiting for old Drain generations, staged migration, failure/interruption/retry and limits |
| `host_commit` | Hook ordering, single invocation, earlier failure, interruption, retry and panic |
| DB host `database` | Real SQLite schema/data rollback, BEGIN/COMMIT contention, cache exclusion, blocked access during recovery and overflow |
| DB host `native_migration` (ignored) | Compatible Drain and incompatible Restart across three real DLLs, rollback/retry, rejection of old resources and reopening the persistent version |
| `managed_activities` | Explicit registration, delayed child completion, unsubscription failure, cleanup after Drop, limits, panic and retry |
| `async_services` | Input/output copying, size and capacity limits, cancellation cleanup, revocation, operation IDs and retaining results when output buffers are too small |
| `threaded_services` | FIFO, capacity, notification, shutdown/join, two logic owner threads, real TCP and cancellation during reads |
| `threaded_io_host` `native_threaded_io` (ignored) | Two real SDK DLL generations, two logic threads, host-owned TCP I/O, Drain and owner-thread destruction |
| `service_contracts` | Multiple requirements, explicit bindings, rejection of initialization schema/format/panics, revocation rechecks and configuration retained across generations |
| `contracts_host` `native_contracts` (ignored) | Missing services, wrong versions and invalid initialization formats across two real SDK DLL generations; multiple synchronous/asynchronous services |
| `native_routing` | Real DLLs on the same TCP connection, checkpoint format changes, external-thread callback return/join/unsubscription failure and old-ABI rejection |
| `native_reclamation` | Retirement, owned references and call-return checks, waiting for external Waker returns, owner thread, rejected candidates, 40 updates, OS residency and Windows copy-removal failure |
| `generation_locals` | Value separation across generations/Routers, borrowing, panics and rejection of old tables or unknown control extensions |
| `native_managed_generation` | Real SDK DLLs, registered-value destruction, cancellation cleanup, reclamation while host notifications remain, and retention after destructor panic |
| Bevy `native_game` ignored test | Real DLLs and Bevy ECS, frame boundaries, delayed stop/restore, result checks for generation/Entity/input revision and owner-thread behavior |
| Tauri `engine` ignored test | Real-DLL builds in an isolated project, Drain/Restart, value continuity, syntax-error recovery and copies independent of the working directory |

`fixtures/handler.rs` is an independent implementation using the bundled, fixed ABI definitions in `abi_v5.rs`, without the SDK or proc macros.
It is kept separate to help detect errors shared by macro-generated handlers and the loader.

## Check packages and documentation

```powershell
python scripts/check-docs.py
./scripts/check-packages.ps1
```

Pass `-Offline` to the package checks if dependencies are already available.
The script packages the three SDK crates and creates a separate workspace using only the extracted archives.
It checks native/static configurations for all targets, an independent ABI DLL, real-DLL reclamation tests and a statically linked consumer.
Unpublished internal dependencies resolve through patches to the extracted crates, without referencing the original source tree.
Each archive must contain its README and license texts. Results and archive hashes remain under
`target/package-checks/`. The script neither publishes to a registry nor reserves crate names.

Documentation checks validate relative file links and heading anchors, and reject public links to local working notes.
Both checks are configured in Windows and Linux CI.

## Routine checks

Run from the repository root. Put Cargo and rustc on PATH, because real-DLL tests invoke `rustc`.

```powershell
cargo fmt --all -- --check
rustfmt --edition 2021 --check reknot/tests/fixtures/handler.rs reknot/tests/fixtures/abi_v5.rs reknot/tests/fixtures/reclamation.rs reknot/tests/fixtures/managed_logic.rs
cargo clippy -p reknot_abi -p reknot_macros -p reknot --all-targets --locked -- -D warnings
cargo clippy -p reknot_abi -p reknot_macros -p reknot --all-targets --all-features --locked -- -D warnings
cargo test -p reknot_abi -p reknot_macros -p reknot --locked
cargo test --workspace --exclude tauri_tokio --all-features --locked
cargo test -p reknot_abi -p reknot_macros -p reknot --release --all-features --locked
```

Check the GUI backend and frontend syntax separately. These tests neither edit ordinary GUI sources nor open a window.

```powershell
cargo clippy -p tauri_tokio -p tauri_tokio_logic --all-targets --locked -- -D warnings
cargo test -p tauri_tokio --bin tauri_tokio --locked -- --ignored --test-threads=1
node --check examples/tauri_tokio/dist/main.js
```

The ignored integration test temporarily changes the test process's working directory, so run it with one test thread.
Follow the [GUI Lab](TAURI_GUI_LAB.md) for visual and interactive checks of the actual window.

The Linux GUI job also launches a real WebView with `tauri-driver 2.1.0` and Xvfb.
A Python test using only the standard library checks example selection and saving, Drain/Restart, request submission,
build errors and recovery, and continuity of the host and counter.
It restores sample sources on exit and saves screenshots and diagnostic logs as artifacts.
The client uses a persistent HTTP/1.1 connection and consumes each response before
the next command. CI runs the full scenario three times with `--strict-transport`,
so even a read disconnect fails without retrying. Each `target/gui-smoke/run-*`
directory contains `requests.jsonl` with request times, paths, attempts, HTTP status
or errors, plus `driver.log`, screenshots and a successful `result.json` with
transport counts. Outside strict mode, only a read may retry once; session changes
and clicks are never replayed. `python scripts/test-check-gui.py` exercises these
rules over real loopback sockets without requiring a WebView.
Visual review and interactive Windows checks remain separate from this automated test.

## Bevy game integration

Check Bevy native integration separately without rendering. See the [minimal game integration](BEVY_GAME.md).

```powershell
cargo clippy -p bevy_game_host -p bevy_game_logic -p bevy_game_protocol --all-targets --all-features --locked -- -D warnings
cargo test -p bevy_game_host --test native_game --locked -- --ignored --test-threads=1
```

## Host resource and SQLite migration

```powershell
cargo clippy -p db_migration_host -p db_migration_logic --all-targets --all-features --locked -- -D warnings
cargo test -p db_migration_host --test database --locked
cargo test -p db_migration_host --test native_migration --locked -- --ignored --test-threads=1
```

Only this example depends on rusqlite and bundled SQLite, which requires a C compiler.
Databases are created under `target/`; existing application databases are not used.
See [migration policy and limits](HOST_RESOURCE_MIGRATION.md).

## Retention and copy cleanup

```powershell
cargo test -p reknot --test cache_lifecycle --locked -- --test-threads=1
```

Host child processes and the real CLI run against isolated roots under `target/tmp/cache-lifecycle-tests/`.
The test does not apply cleanup to the user's default copy directory.
The two ignored functions are child-process entry points invoked only by parent tests.
Alongside common exit, locking and cleanup checks, Windows tests cover deletion denial and junctions; Unix tests cover symlinks and rejection of roots writable by other users.
These OS and file checks are outside Miri's scope. See the [usage contract](RETENTION_AND_CLEANUP.md).

## Miri

`typed_cancellation`, `cleanup_notifications` and `notification_unwind` run under
both borrow models. Package checks run these suites and the native `source_watcher`
tests from extracted SDK archives as well.

`cancellation_notifications` checks wake-driven stop/Restart through
`FuturesUnordered`, multiple/cloned waiters, dropped/replaced registrations,
root completion/Drop and child failure. `native_cancellation` repeats the
cross-DLL cases and checks that both images can be reclaimed afterwards.
`generation_locals` also checks absent and unknown cancellation tails without
reading their function-pointer payloads. Package checks run all three suites;
Miri runs the static suites under both borrow models.

Use a pinned toolchain and Linux interpreter target. This does not change the default toolchain for ordinary builds.

```powershell
rustup toolchain install nightly-2026-11-02 --profile minimal --component miri --component rust-src
cargo +nightly-2026-11-02 miri setup --target x86_64-unknown-linux-gnu
./scripts/check-miri.ps1 -Toolchain nightly-2026-11-02
```

The script runs both Stacked and Tree Borrows with seed 0.
Use `-Seed 1` for another seed, or `-BorrowModel Stacked` / `Tree` to run just one model.
Coverage includes tasks, registered TLS, host-notification races, resources, routing, restart, host commit, managed activities, async services, threaded services and service contracts.
Miri excludes real TCP tests from threaded services and checks only synchronization, notification and ownership.
SQLite and C code are checked natively; Miri covers only management of the core commit hook.

Logs, compiler information, SHA-256 hashes of target sources and Cargo.lock, and exit codes are saved under `target/miri-audit/seed-<seed>/`.
The script continues through the remaining groups after a failure, then exits nonzero if any group failed.
Miri does not replace DLL or network tests.

## Verification scope

CI configures Windows/Linux core and native integration checks, the core minimum Rust version,
both Miri borrow models and Windows/Linux GUI backend tests.
Match results to the source revision being checked. See the [workflow](../.github/workflows/ci.yml).
A successful local Windows run is not evidence of Linux or real-GUI verification.

## Static and native-only configurations

```sh
cargo clippy -p reknot --all-targets --no-default-features --locked -- -D warnings
cargo test -p reknot --no-default-features --locked
cargo check -p reknot --no-default-features --features native --locked
cargo test -p service_host --release --no-default-features --features static-logic --locked
cargo tree -p service_host --no-default-features --features static-logic --edges normal --locked
```

Confirm that the static dependency tree excludes libloading, notify and the old runtime adapters.
The default targets are the three core crates. Select examples explicitly with `-p` or `--workspace`.
