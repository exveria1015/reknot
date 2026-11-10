# Edit and reload with the Tauri GUI

English | [日本語](../docs_ja/TAURI_GUI_LAB.md)

An English-language development UI for trusted local Rust code.

## Start the app

Run from the repository root:

```powershell
cargo run -p tauri_tokio --locked
```

No npm or Tauri CLI is required. On Windows, you need the Tauri build prerequisites and WebView2.
Startup builds the logic DLL in a separate process. The GUI remains responsive while dependencies compile on the first run.
Closing the window stops the engine thread and cleans up its Futures and host resources on that thread.

## Edit in the window

1. Change `LABEL`, `ACCENT` and `STEP` in the editor on the right. The **v1 · +1** and **v2 · +5** buttons also load example edits.
2. Use **Save & reload** or Ctrl+S to save to `examples/tauri_tokio_logic/src/lib.rs`.
3. A background build runs. Only a successful build proceeds to candidate validation and cutover. The host PID and uptime stay unchanged.
4. Compare the counter, saved value, admission generation, worker generation, LOADS and history.

The editing buffer is separate from the saved file. Example, syntax-error and startup-code buttons affect the file only when you save.
Saving from an external editor also triggers a build. It does not overwrite the in-window buffer; use **Reload file** to read the file again.
For repeatable steps, load and save **v1 · +1** before changing to v2. Saving modifies the actual logic source file.

The window's HTML, CSS and JavaScript are host assets. Rebuild and restart the host after changing the UI structure.
This example hot-reloads Rust logic, which updates colors, text and values through the host resource API.

## Drain and Restart

| Mode | Persistent counter | New requests |
| --- | --- | --- |
| Drain | Continues on the old generation with the old increment | Replies with the new generation's text |
| Restart | Restores the checkpoint and resumes with the new generation's increment | Uses the new generation after cutover |

If the v1 worker is running and you save v2 in Drain mode, a Mint / v1 preview is expected.
Use **Send now** to see a response from the new generation. To update the counter too, select Restart and save or rebuild.
Selecting a mode alone does not trigger cutover; it applies after the next successful build.

Restart state is an eight-byte u64 counter, saved on stop and restored with the same schema.
This example does not convert schemas. See the [Restart example](RESTART_AND_MIGRATION.md) for migration between formats.
Values survive reloads within the same host, not application exit.

## Requests already running

Start a **5-second request**, then edit the logic while it is pending.
With Drain, it finishes on the code that admitted it; the next request uses new code.
With Restart, it responds to stop by shortening its wait, replies using old code, then allows cutover. Its response includes **completed after a stop request**.
Ordinary requests are never replayed automatically.

This GUI demonstrates a host retaining its WebView and UI state. TCP connection preservation is checked separately in `service_host` and real-DLL tests.

## Recover from a build error

1. Select **Build error** and save.
2. **Build details** shows the Rust error. The old counter and request processing continue.
3. Select a v1/v2 example to remove the syntax error added by the sample, then save to retry.

Build failure happens before DLL loading, so it consumes no generation slot.
ABI/schema rejection and shutdown-wait failure are separate cases. An interrupted Restart leaves admission paused; use **Resume requests** explicitly when appropriate.
The example provides neither forced stop for logic that never returns from poll nor automatic recovery from arbitrary runtime panics.

The default retention budget is 32 slots. Once exhausted, close the window and restart the development process.
DLL copies go under `reknot/managed-v1/sessions/<SessionId>/copies/` in the user's temporary directory.
A read-only startup working directory is supported.
Inspect copies with `reknot-cache inspect`; after exit, clean only eligible sessions with `reknot-cache clean --apply`.
See [retention and copy cleanup](RETENTION_AND_CLEANUP.md) for details.

## Implementation

- `examples/tauri_tokio/dist/`: handwritten UI sources and editor. Tauri embeds these files; they are not generated build output and must not be deleted.
- `examples/tauri_tokio/src-tauri/gen/schemas/`: tauri-build output, excluded from Git and regenerated during builds.
- `examples/tauri_tokio/src-tauri/src/main.rs`: Tauri commands and thread join on application exit.
- `examples/tauri_tokio/src-tauri/src/engine.rs`: background builds, source watching, admission cutover and host UI resources.
- `examples/tauri_tokio_logic/src/lib.rs`: the Handler ABI 5 async handler.

SDK service operations are short and synchronous. They query the host for time and display data; a local execution loop polls Futures every 16 ms.
Saving is restricted to this logic file. Use the sample for development with trusted local Rust code.
Build logs are saved as `gui-build.log` in the logic target directory.
Tauri commands and global invoke follow the [official Tauri documentation](https://v2.tauri.app/develop/calling-rust/).

## Verification

In a real window, check startup, counter updates, example edits, source saving and generation changes.
With the same host PID, Drain should leave worker / gen 1 running; Restart should switch to the Coral / v2 worker with a +5 increment.

Automated checks use a native integration test with an isolated temporary project.
Without editing ordinary GUI sources, it checks operation when the working directory cannot contain a target directory, Drain, Restart, counter preservation,
continued old-worker execution during a syntax error, recovery after fixing it and the generation budget. The test stops the engine and cleans up its temporary project afterward.

```powershell
cargo test -p tauri_tokio --bin tauri_tokio --locked -- --ignored --test-threads=1
cargo clippy -p tauri_tokio -p tauri_tokio_logic --all-targets --locked -- -D warnings
node --check examples/tauri_tokio/dist/main.js
```

Headless integration checks native DLLs and the GUI backend. It is separate from interacting with a real window.
See [testing](TESTING.md) for CI configuration and commands.
