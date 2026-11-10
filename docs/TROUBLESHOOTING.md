# Troubleshooting

English | [日本語](../docs_ja/TROUBLESHOOTING.md)

For the typed API, start with the symptoms below.
Lower-level errors are listed in the [message API section](#message-api).

## Saving does not trigger an update

In a generated project, check that you are editing `logic/src/lib.rs`.
Changes to `host` need a host restart. Changes to `protocol` also require more than a logic-only update.

Then check the host log:

| Message or symptom | Action |
| --- | --- |
| No `Building edited logic` message | Check that `DevModule::update()` runs while waiting for input and that the edited file is watched |
| `Build failed` | Fix the compiler error and save. For a `--locked` error, update `Cargo.lock` with ordinary Cargo commands first |
| `Candidate rejected` | Compare the reported contract with the shared protocol. For an intended format or meaning change, increment the contract version and rebuild/restart the host too |
| `Source changed during the build` | A newer edit superseded the candidate. Wait for the next build result |
| `load-attempt budget reached` | Restart the development host. Reclaiming DLLs does not refund consumed attempts |

`New events use generation ...` confirms activation.
See [watched inputs](DEVELOPMENT.md#watched-source-inputs) for files outside the watch set and newly created `.cargo` directories.

## A call does not return

`HandlerClient` sends a request to a queue. The driver on the owner thread executes it.
Keep calling `DevModule::update()`, or polling `BridgeDriver` in a custom host, while awaiting the result.
If the owner thread only waits for the result, it cannot process the queue. The generated example uses `select!` to do both.

| Error | What to check |
| --- | --- |
| `Error::Busy` | Concurrent request and queue limits. Completed but unconsumed replies still occupy slots; consume results or drop futures you no longer need |
| `Error::BridgeClosed` | Whether the client was closed or the owner driver was dropped |
| Typed message conversion error | Whether the types are JSON-compatible, each input/output fits within 1 MiB, and host/logic contracts agree |

## Cancellation does not finish

Cancellation includes a stop request to registered children. Request completion waits until those children finish.
Check that they respond to cancellation and that external callbacks are being unregistered and joined.

Keep driving the owner while awaiting `close()` or `shutdown()`. Dropping the driver alone is not confirmation of a successful join.
For `poll_cleanup` after dropping local calls and similar cases, see the [API shutdown contract](API_REFERENCE.md#cancellation-and-shutdown).
The SDK does not automatically stop unregistered spawn tasks or external work that never finishes.

## Message API

| Error or symptom | Check and recovery |
| --- | --- |
| `Abi` / handler ABI mismatch | Rebuild both sides against the same SDK. Handler ABI is 5; Services and task-control ABI are 2 |
| `ContractMismatch` | Match resource and handler service IDs/versions. Migrating the service implementation is the application's responsibility |
| `StateSchemaMismatch` | Drain requires the same schema. Use Restart with checkpoint conversion for format changes |
| `WrongRouter` / `StaleCandidate` | Prepare again from the target router's current generation |
| `GenerationLimit` | The cumulative load budget is exhausted. Restart the development process; failed native loads consume attempts too |
| `Paused` after failed or interrupted Restart | Choose a recovery policy, then retry or call `resume()?`. Resuming does not recreate old workers |
| Restart stays Pending | Poll externally owned `RequestTask`s too. Make logic respond to stop requests and finish child work |
| `WorkerFailed` | Inspect state; unregister or fix the worker before retrying if needed. Do not blindly replay ordinary requests |
| `CheckpointTooLarge` / migration failure | Check format, schema and size limits. Old checkpoints remain available |
| Service `BUSY` / `BUFFER_TOO_SMALL` | Release conflicting borrows or provide a large enough output buffer. There is no automatic retry |
| `REVOKED` / missing resource | Check revocation, the registry and the `ResourceId` generation. Use a valid ID from the correct registry |

A persistent worker staying on old code during Drain is expected.
If the GUI's new request text changes but the counter increment does not, select Restart and rebuild.

## Builds and reloads

Check the paths watched by the host and the build log. The router itself does not watch files.
`service_host` handles changes at message boundaries or while polling requests; the GUI builds in the background.
The host explicitly starts `dev-tools` watching. Static mode does not watch or rebuild logic.

For access-denied errors on Windows, inspect the **path** in the error.
Cargo's output directory and the user temporary directory must be writable. DLL copies go under
`reknot/managed-v1/sessions/<SessionId>/copies/`, not the working directory.
Check permissions, processes and file locks; do not delete copies of DLLs still in use.
Rebuild incomplete artifacts fully before preparing a candidate.

The GUI's HTML, CSS and JavaScript are embedded host assets. Rebuild and restart the host after changing them.
The Rust logic DLL is the hot-reload target.

[Project README](README.md) · [Documentation index](INDEX.md)
