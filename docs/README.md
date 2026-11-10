# Reknot

English | [日本語](../docs_ja/README.md) · [Documentation index](INDEX.md)

**Edit and try Rust logic while your application keeps running.**

Reknot (formerly flow-reloader) is a hot-reload SDK for local development.
Keep connections, UI and state in the host, and put the code you want to change in a separate logic crate.
Save an edit to the logic; once the build and contract checks succeed, new work uses the updated code.

For example, you can adjust a bot's replies or input checks without reconnecting.
Write handlers as ordinary Rust functions, then register them with an attribute.

## Try it

With Rust 1.95 or later and Cargo installed, run this command from the repository root.
Start with the console bot, which needs no external account or token.

```sh
cargo run -p typed_bot_host --locked
```

After the initial build, wait for `Typed bot ready.`, then try the following:

1. Enter `hello`. The reply is `logic-v1 / message 1: HELLO`.
2. In the [logic source](../examples/typed_bot_logic/src/lib.rs), change `logic-v1` to `logic-v2` and save.
3. Wait for `New events use generation ...`, then enter another message. The reply now uses `logic-v2`.

Enter `/summary` to see up to the last 100 input messages. The history stays in the host across logic updates.
Enter `/quit` to exit.

If a build fails, the running logic stays in use. Fix the error and save to retry.

## Create your own application

Generate a starter project from the repository root:

```sh
cargo run -p reknot --bin reknot-new --locked -- ../my-bot --sdk-path ./reknot
cd ../my-bot
cargo run
```

Choose a new directory as the destination. Commit the `Cargo.lock` created on the first run,
then use `cargo run --locked` for subsequent runs.

The project has three crates, each with a separate role:

| Location | What belongs here |
| --- | --- |
| `logic` | Replies, calculations and other behavior you want to edit and try |
| `protocol` | Shared input/output types, handler identities and contract versions |
| `host` | Connections, event registration, persistent state and the update loop |

Start by changing a reply in `logic/src/lib.rs` and saving.
In the generated application, `/count` shows the number of input messages.
The [typed-handler guide](TYPED_HANDLERS.md) explains the SDK source path and how to write handlers.

## What changes during an update

The typed API updates one crate/DLL as a unit.
Its handlers and helper functions run from the same version of the code. A loaded version of the logic is called a **generation**.

After a switch, requests already admitted continue to completion on the old generation.
This is **Drain**. The entry points registered with the host stay in place and route later work to the new generation.
For calls queued from another thread, the owner thread selects the generation when it admits the request for execution.

The typed API also provides **Restart**, which stops registered workers and recreates them from typed checkpoints.
See [typed Restart](TYPED_RESTART.md) for registration, migration and recovery. Drain remains the default.

## Before you use it

This SDK is under development and intended for trusted local Rust code. Its Rust API may change.
Build the host and logic against the same SDK. See [compatibility and scope](COMPATIBILITY.md) for supported environments.

When input/output formats or meanings change, increment the shared contract version and restart the host too.
Derived schemas detect structural changes even under the same Rust type name. Equal schemas do not prove equal meaning.
See [contract diagnostics and schemas](CONTRACTS_AND_SCHEMAS.md).

DLLs stay loaded by default. The default budget is 32 cumulative load attempts, including failed native loads.
Restart the development host when the budget is exhausted. [Explicit DLL reclamation](UNLOAD_SAFETY.md) is available,
but unloading is not automatic, and running work or external effects are not rolled back.

## Read next

- [Typed handlers](TYPED_HANDLERS.md) — Understand the generated code and add handlers.
- [Troubleshooting](TROUBLESHOOTING.md) — Check why an update, call or shutdown is not completing.
- [Documentation by task](INDEX.md) — Find framework integration, state migration and internal design guides.

## License

Choose either the [MIT License](../LICENSE-MIT) or [Apache License 2.0](../LICENSE-APACHE).
The SPDX expression is `MIT OR Apache-2.0`.
