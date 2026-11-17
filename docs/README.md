# Reknot

English | [日本語](../docs_ja/README.md) · [Documentation index](INDEX.md)

**Edit and try Rust logic while your application keeps running.**

Reknot is a hot-reload SDK for local development.
Keep connections, UI and state in the host, and put the code you want to change in a separate logic crate.
Save an edit to the logic; once the build and contract checks succeed, new work uses the updated code.

For example, you can adjust a bot's replies or input checks without reconnecting.
Write handlers as ordinary Rust functions, then register them with an attribute.

## Start with three ideas

1. **Edit behavior in logic.** Start by changing a reply string.
2. **Keep state in the host.** Connections and input counts stay there across updates.
3. **Save and wait for the reload event.** The next call uses the new behavior.

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

Start by changing `logic-v1` in `logic/src/lib.rs` and saving.
Use `/count` to confirm that the host's input count survives the update.

## After the first successful edit

The [typed-handler guide](TYPED_HANDLERS.md) explains the shared `protocol` crate and adding handlers.
Read [development contracts](DEVELOPMENT_CONTRACTS.md) when changing contracts, running long-lived work,
handling load limits or selecting JIT. The starter uses native DLLs, JSON and Drain;
Restart and JIT setup are not needed for the first edit.

This is an evolving SDK for trusted local code. Restart the host when changing input/output contracts
or when it reports the load limit. See [supported environments and compatibility](COMPATIBILITY.md).

## Compare timings

The [reproducible benchmark](BENCHMARKS.md) reports native DLL versus layered JIT edit-to-response
latency, and full typed round trips with JSON or optional MessagePack. Conditions and raw samples are included.

The checked-in results describe an earlier implementation. The guide separates that
baseline from current build settings and provides commands for measuring your application.
It also covers request latency during Drain, Restart and Managed collection.

## Read next

- [WebSocket quiz room](QUIZ_ROOM.md) — Try real Axum integration alongside a Fastify/JS version in the same browser.
- [Typed handlers](TYPED_HANDLERS.md) — Understand the generated code and add handlers.
- [Troubleshooting](TROUBLESHOOTING.md) — Check why an update, call or shutdown is not completing.
- [Documentation by task](INDEX.md) — Find framework integration, state migration and internal design guides.

## License

Choose either the [MIT License](../LICENSE-MIT) or [Apache License 2.0](../LICENSE-APACHE).
The SPDX expression is `MIT OR Apache-2.0`.
