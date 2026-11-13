# Typed handlers

English | [日本語](../docs_ja/TYPED_HANDLERS.md)

This guide takes you from generating a project to editing a reply and calling it from the host.
Handlers can be synchronous or asynchronous functions. The SDK converts their inputs and results.

To try a running example first, start with the [console bot in the README](README.md#try-it).

## Create your own project

With Rust 1.95 or later and Cargo installed, run these commands from the SDK repository root.
This example creates the project in `../my-bot`, which must not exist yet.

```sh
cargo run -p reknot --bin reknot-new --locked -- ../my-bot --sdk-path ./reknot
cd ../my-bot
cargo run
```

After the initial build, wait for `Ready.`, then enter some text.
Change `logic-v1` in `logic/src/lib.rs`, save, and wait for `New events use generation ...`.
Your next input gets the updated reply. `/count` shows the host's input count; `/quit` exits.

Commit the `Cargo.lock` created on the first run. Use `cargo run --locked` afterward.
The generated README also covers how to run the project, including static linking.
To change the name or SDK location, see [generation options](#destination-and-sdk-location) below.

## Know where to edit

The generated project has three crates.
Start in `logic` when changing replies or calculations.

| File | Role |
| --- | --- |
| `logic/src/lib.rs` | Handler functions with attributes, plus their helpers |
| `protocol/src/lib.rs` | Input/output types, handler identities and contract versions, and the module definition |
| `host/src/main.rs` | Input handling, persistent state, calls into logic and update maintenance |

The host owns connections, databases, history and anything else that must survive an update.
Pass the required data into the logic, then use its result in the host.

## Write a handler

A project generated as `my-bot` has a reply handler like this.
`REPLY` is the handler contract declared in the shared protocol.

```rust
use my_bot_protocol::{module, Message, REPLY};

#[reknot::handler(REPLY)]
async fn reply(message: Message) -> Result<String, String> {
    let text = message.text.trim();
    if text.is_empty() {
        return Err("Please enter a message.".into());
    }
    Ok(format!(
        "logic-v1 / message {}: {}",
        message.received,
        decorate(text)
    ))
}

fn decorate(text: &str) -> String {
    text.to_uppercase()
}

reknot::export_handlers!(module(), [reply]);
```

The `handler` attribute generates an entry point. `export_handlers!` exports the crate's handlers together.
An update replaces this handler and helpers such as `decorate` as one DLL.

A handler takes one owned input and returns one owned output. Use `()` for no input, or a struct or tuple for several values.
Implement Serde's `Serialize` and `Deserialize` and add `#[reknot::schema]` to your own types.
Borrowed SDK objects, methods taking `&self` and generic functions are not supported.

Here, `Result<String, String>` represents a reply or an input error.
A call from the host wraps that value in another `Result` from the SDK.
The outer result reports conversion or execution failure; the inner result is the application's outcome.

## Define the input/output contract

`protocol/src/lib.rs` holds the contract shared by the host and logic.
For the example above, it defines:

```rust
use reknot::typed::{Handler, Module};
use serde::{Deserialize, Serialize};

pub const REPLY: Handler<Message, Result<String, String>> =
    Handler::new("my-bot.reply", 1);

#[reknot::schema]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub text: String,
    pub received: u64,
}

pub fn module() -> Module {
    Module::new("my-bot", 1).with(REPLY)
}
```

`"my-bot.reply"` is a stable identity; `1` is the input/output contract version.
You can keep that identity when renaming the Rust function.
A loaded code generation and an input/output contract version are separate values.

**Keeping the same type name does not establish input/output compatibility.**
When fields, Serde attributes, data formats or meanings change, increment the corresponding contract version and rebuild/restart the host too.
The SDK compares generated structural schemas in both Serde directions, including fields and supported Serde attributes.
It rejects structural differences before activation; semantic changes still need explicit versioning. See [diagnostics and schemas](CONTRACTS_AND_SCHEMAS.md).

## Apply each kind of change

| What changes | How to apply it |
| --- | --- |
| Existing handler behavior, reply text or helper functions | Save and wait for the build and checks to succeed |
| Input/output format or meaning | Increment the corresponding contract version; rebuild and restart the host too |
| Added or removed handlers | Update the protocol definitions, `export_handlers!` and host registrations; rebuild and restart the host |
| Host code, SDK or ABI | Rebuild host and logic, then restart the host |

The generated host keeps its current generation when a build fails or a contract does not match.
It does not automatically rerun work from a failed candidate or replay completed events.

That covers routine handler edits.
Read on when integrating a custom host or an existing framework.

## Call from the host

The generated host provides a `client` for sending requests and a loop that drives updates.
Obtain the entry point once with `client.handler(REPLY)`; you do not need to register it again after an update.

This excerpt shows the caller's part. **Keep the update loop running on the owner thread while awaiting the result.**

```rust,ignore
let reply = client.handler(REPLY)?;
let result = reply.call(Message {
    text: "hello".to_owned(),
    received: 1,
}).await?;
```

The generated host polls `BridgeDriver` directly with the executor's Waker, so
request admission, resumption and cancellation do not wait for a maintenance tick.
In reload mode it also calls `DevModule::update()` about every 25 ms for source
changes, build results, registered workers and other maintenance.
The static generated host needs no maintenance timer.

`/count`, `/quit` and EOF remain available during a request. `/quit` or EOF cancels
the current request and waits for its registered children. Additional ordinary
input while busy is rejected with a message; send the next request after its reply.

### Integrate a custom host

Start development updates with `DevModule::start(module().builder(), DevOptions::new(manifest, package))`,
then obtain `(client, driver)` with `development.host().bridge(BridgeOptions::default())`.
Poll `driver` on the owner thread with the executor, including while awaiting
`client.shutdown()`. Continue calling `development.update()` for maintenance.
`development.bridge(...)` remains available for manual polling, but its requests
advance only when `update()` is called.
The initial build is synchronous. Later builds run in the background.

`start` is `unsafe`. The caller must ensure that the project, its dependencies and future edits obey the SDK's native execution contracts.
See [compatibility and scope](COMPATIBILITY.md).

For direct calls on the same thread, you can also use `development.host().handler(REPLY)`.
The [console bot host](../examples/typed_bot_host/src/main.rs) demonstrates these local calls.

| Entry point | When it selects a generation |
| --- | --- |
| Local `HandlerHandle::call` | At `call`, before the first poll |
| Queued `HandlerClient::call` | When the owner thread dequeues and admits the request for execution |

Both execute the request on its selected generation through completion. A request waiting in the queue has not selected a generation yet.

## Call from framework threads

`ModuleClient` and `HandlerClient` are `Send + Sync`; the returned `RemoteCall` is `Send`.
Clone `reply` into the framework's event handlers.

Keep `ModuleHost` and `BridgeDriver`, which run the DLL, on the thread that owns them.
With Tokio, use `current_thread` or `LocalSet` and continue driving updates there.

A [runnable example](../examples/typed_bot_host/examples/framework_bridge.rs) calls from a separate Tokio runtime thread.
Run it from the SDK repository root:

```sh
cargo run -p typed_bot_host --example framework_bridge --no-default-features --features static-logic --locked
```

Convert framework events into owned input data in the host.
Send the returned reply through the host's SDK too. No Discord, Serenity or Twilight adapter is included.

## Cancel and wait for shutdown

`call.cancel()` requests cancellation. Keep awaiting the request to observe completion of registered child work.
For a local `Call`, its first cancellation wakes the last registered Waker so a
parked call can finish through selective combinators such as `FuturesUnordered`.
Repeated cancellation does not keep waking a call waiting for child cleanup.
A result that finished first may still be returned. Dropping the request future also requests cancellation.

To close the whole bridge, choose one of these operations:

- `client.close().await` — Close admission and wait for accepted work to finish.
- `client.shutdown().await` — Close admission, cancel accepted work and wait for registered child cleanup.

Keep driving the owner while waiting.
On the owner thread, use `select!` to drive maintenance alongside shutdown, as the generated host does.
This wait covers only that bridge's requests.

Dropping the driver does not replace a successful shutdown wait.
For cleanup after dropping a local call or its driver, see the [API shutdown contract](API_REFERENCE.md#cancellation-and-shutdown).
The SDK does not force uncooperative work to stop or undo replies already sent.

## Use host resources during a call

Simple handlers can take input and return a result.
When a handler needs database access or I/O along the way, add `RequestContext` as its first argument and call a host service.

See [host services in typed handlers](API_REFERENCE.md#host-services-from-typed-handlers) for bindings, available APIs and child registration.
Keep persistent state in the host. Do not assume that the SDK manages independent, unregistered spawn tasks.

## Run without watching sources

Select static linking from the generated project's root.
Handler code and calls stay the same. Rebuild and restart the application to apply edits.

```sh
cargo run --no-default-features --features static-logic --locked
```

## Destination and SDK location

`reknot-new` writes into a new directory. It refuses an existing directory, even an empty one.
Generation only writes files; it does not build the project or initialize Git.

| Option | When to use it |
| --- | --- |
| `--name my-bot` | Choose a Cargo package name independently of the directory, for example when the directory contains spaces or Japanese characters |
| `--sdk-version VERSION` | Pin an exact crates.io SDK version; the default is the CLI version |
| `--sdk-rev COMMIT` | Pin a full Git commit SHA for a source candidate |
| `--sdk-git URL` | Override the Git repository; requires `--sdk-rev` |
| `--sdk-path PATH` | Develop against a local `reknot` crate directory |

The default dependency is an exact crates.io version. This source RC is not published there yet;
use its verified full commit SHA from the release notes (replace `VERIFIED_COMMIT` below):

```sh
cargo install --git https://github.com/exveria1015/reknot.git --rev VERIFIED_COMMIT --locked --bin reknot-new reknot
reknot-new ../my-bot --sdk-rev VERIFIED_COMMIT
```

This route does not need a retained SDK checkout. After registry publication, use
`cargo install reknot --version 0.1.0-rc.2 --locked --bin reknot-new` and `reknot-new ../my-bot`.
Choose one source per invocation. Only `--sdk-path` creates a local-path dependency.
The generated protocol also includes `cargo run -p my-bot-protocol --example schema` for JSON schema export;
use the actual protocol package name from its `Cargo.toml`.

## Look up the detailed contracts

- [Typed API contracts and limits](API_REFERENCE.md#typed-application-api) — Contract checks, JSON conversion, capacity, threads and shutdown.
- [Development operation](DEVELOPMENT.md) — Watched files, dependency changes and recovery from build failures.
- [DLL lifetime and reclamation](UNLOAD_SAFETY.md) — Default retention and explicit reclamation conditions. The default load-attempt budget is 32; reclamation does not refund it.

The typed API supports single-module Drain and [typed Restart](TYPED_RESTART.md), plus [structural schema checks](CONTRACTS_AND_SCHEMAS.md).
Atomic updates across DLLs and cross-module generation sets remain outside this API.

[Project README](README.md) · [Documentation index](INDEX.md)
