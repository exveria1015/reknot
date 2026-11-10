# Integrate through the lower-level API

English | [日本語](../docs_ja/GETTING_STARTED.md)

Use this guide when you want to control host resource registration and the timing of loads and cutovers.
To write ordinary functions and let the SDK handle input/output conversion and updates on save, use the [typed-handler guide](TYPED_HANDLERS.md).

## Keep a connection alive across updates

With Rust and Cargo installed, run this command from the repository root.
See [compatibility and scope](COMPATIBILITY.md) for required versions and environments.

```sh
cargo run -p service_host --locked
```

Connect a TCP client to `127.0.0.1:4000` and send newline-terminated text.
Change the reply label in the [logic source](../examples/service_logic/src/lib.rs), save, then send another message on the same connection.
The response comes from the new code.

Sending `slow` followed by a newline starts a request that takes about five seconds. Update during that request to see old code continue running.

This sample handles one connection and builds synchronously.
It processes changes at message boundaries and while polling a request, so it does not switch while idle waiting for input.
For editing and reloading in a window, see the [Tauri GUI lab](TAURI_GUI_LAB.md).
To try stopping and recreating workers, see the [Restart guide](RESTART_AND_MIGRATION.md).

## Separate the host and logic

Make the host a long-lived executable and the logic a replaceable library.
Both depend on the same `reknot` SDK.

| Component | What it owns |
| --- | --- |
| Host | Sockets, databases, persistent state, request admission and task polling |
| Logic | Input-dependent behavior and calls to operations exposed by the host |

Do not pass the host's Rust objects themselves into the DLL.
Share service IDs, versions, operation numbers and input/output byte formats between the two sides.
Matching IDs do not automatically validate the meaning of the data.

### Create the logic crate

For example, a crate at `examples/my_logic` can use this Cargo.toml.
Add the new crate to the workspace's `members`.

```toml
[package]
name = "my_logic"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
reknot = { path = "../../reknot", default-features = false }
```

In native mode, the host does not need a static dependency on the logic crate.
It loads the built library at runtime.

## Call a host operation

This handler passes its input to host operation 0:

```rust
use reknot::{RequestContext, ServiceError, ServiceKey};

const SESSION: ServiceKey = ServiceKey::new(0x4c49_4e45, 1);

#[reknot::reload_handler(SESSION)]
async fn handle(ctx: RequestContext) -> Result<(), ServiceError> {
    ctx.call(0, ctx.input(), &mut [])?;
    Ok(())
}
```

Define operation 0 in the host. It might copy the input into a bounded send queue.
Keep `HostResource::call` short and nonblocking.
Socket reads, writes and message framing belong in the host.

See [service_host](../examples/service_host/src/main.rs) for a complete host,
and [host resources and admission](HOST_RESOURCES_AND_ROUTING.md) for service registration.

## Load and call the logic

Set up the host in this order:

1. Register resources in `HostResources`.
2. Load the logic with `unsafe RequestRouter::load(path, resources)`.
3. Submit work with `dispatch(resource.id(), input)` and poll the returned future.

`dispatch` copies the input and retains access to the selected code generation and resource.
The host is responsible for driving the future.

The `unsafe` loading contract requires the native code to obey SDK rules, including initialization, finalization and callbacks.
Apply the same conditions to future replacements. See [compatibility and scope](COMPATIBILITY.md).

## Choose a cutover mode

### Switch between requests: Drain

Prepare a candidate with `unsafe prepare(path)`, then make it the new admission target with `activate(candidate)`.
Existing requests continue on the old generation; newly admitted requests use the new one.
Service contracts and state schemas must match.

See [host resources and Drain](HOST_RESOURCES_AND_ROUTING.md) for the detailed sequence.

### Recreate persistent workers: Restart

Register workers with `start_worker` and explicitly save the required state in checkpoints.
`restart(candidate, migrate)` waits for cooperative shutdown, transforms the state and recreates the workers.

Keep polling ordinary host-owned tasks while shutdown is pending.
Only registered workers are recreated. Ordinary requests are not automatically replayed.
[Restart and state migration](RESTART_AND_MIGRATION.md) also explains recovery after failure.

See [build configurations](API_REFERENCE.md#build-configurations) for static linking and feature selection.

[Project README](README.md) · [Documentation index](INDEX.md)
