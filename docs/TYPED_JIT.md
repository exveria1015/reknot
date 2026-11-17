# Typed development with COFF and ELF

English | [日本語](../docs_ja/TYPED_JIT.md)

Use the same `DevModule`, typed functions, `HandlerHandle` and bridge clients with
native DLLs or the experimental layered object backend. Select the backend when
starting development; a running session keeps that choice and its load budgets.

## Select the backend

| Configuration | Features on the host | Options passed to `DevModule::start` |
| --- | --- | --- |
| Native DLL | Default features, or `typed,dev-tools` | `DevOptions` |
| Windows x64 MSVC COFF / Linux x86_64 GNU ELF | `typed,jit-dev`; `native` is optional | `LayeredDevOptions` |

The layered route requires a logic/support project and compiler-matched resident std/support.
The SDK's built-in Rust producer uses rustup/Cargo and the platform linker, without PowerShell/Python.
Follow [layered development](LAYERED_DEVELOPMENT.md) and the [COFF](JIT_CODE_OWNERSHIP.md)
or [ELF](ELF_JIT.md) layout guide. `reknot-new` generates the native layout;
backend selection does not generate a support crate.

The support crate re-exports the SDK with `typed` enabled. Logic can depend on it
under the name `reknot`, so the handler attributes and exports keep their spelling:

The paths below assume the application `project` and SDK checkout `reknot` are siblings.

```toml
# support/Cargo.toml; use the same SDK revision as the host
[lib]
crate-type = ["dylib"]
[dependencies]
reknot = { path = "../../reknot/reknot", default-features = false, features = ["typed"] }
```

```rust
// support/src/lib.rs
pub use reknot::*;
```

```toml
# logic/Cargo.toml; replace my_support with the support package name
[lib]
crate-type = ["cdylib"]
[dependencies]
reknot = { package = "my_support", path = "../support" }
```

Keep the host linked to the SDK, not the support dylib. Host and logic must use
the same shared handler contracts. Built-in input/output types work unchanged;
custom protocol types retain their schema and explicit version requirements.

## Start and update

Pass layered options in place of native `DevOptions`. The platform chooses COFF
or ELF; the typed host still checks contracts before admitting the candidate.

```rust,no_run
use reknot::jit::development::LayeredDevOptions;
use reknot::typed::{DevModule, Error, Handler, Module};

const ECHO: Handler<String, String> = Handler::new("app.echo", 1);

unsafe fn start() -> Result<DevModule, Error> {
    let options = LayeredDevOptions::builtin(
        "project/Cargo.toml", "my_logic", "my_support",
        "project/target/layered",
    );
    let module = Module::new("app", 1).with(ECHO);
    // Trust the project, producer, dependencies and future edits.
    let mut dev = DevModule::start(module.builder(), options)?;
    let _echo = dev.host().handler(ECHO)?;
    // Existing calls and bridge clients use the same typed API.
    if let Some(event) = dev.update() {
        eprintln!("{event}");
    }
    Ok(dev)
}
```

Both backends wait for the first validated generation before `start` returns.
Later builds run on a worker. Call `update` regularly, including during requests;
it drives attached bridge requests, cleanup and registered workers. An independent
executor can continue using `host().bridge()` and polling its driver directly.

`shutdown()` stops publication and joins build workers, so it can wait for Cargo.
It does not cancel requests or unload code still owned by handles. `Drop` stops
layered publication without waiting for the builder. An interrupted Restart keeps
admission paused; finish cleanup and use the normal host recovery APIs.

## Drain, Restart and Managed

Drain and KeepLoaded remain the defaults. To select cooperative Restart, convert
the options with `DevBackend::from(options).with_reload_mode(ReloadMode::Restart)`
before passing them to `start`. Registered workers keep their typed checkpoints;
ordinary requests are not replayed. Handler contract changes require rebuilding
and restarting the host; automatic Restart does not migrate changed state schemas.

`unsafe DevModule::start_managed(builder, options)` opts into Managed COFF/ELF.
It requires the full [managed lifetime contract](LAYERED_DEVELOPMENT.md), including
future edits and final external callback/Waker return. Native options are rejected.
`update` collects only this session's retired images. Retained calls, handles or
workers can delay collection; dropping the controller never forces an unload.

The typed `ModuleBuilder` keeps its resources, service bindings, request capacity
and Router budget. For KeepLoaded, the tighter Router limit from the builder and
`LayeredDevOptions::max_router_generations` applies; the engine's cumulative load
limit also applies. Managed uses the existing live image/page limits instead.

## Observe failures and recovery

| Event or diagnostic | Behavior |
| --- | --- |
| `Building`, `Reloaded`, `Restarting`, `Superseded` | Same typed events as native development |
| `BuildFailed` | Current handlers continue; fix the source and save |
| `Rejected { error: Error::Contract { .. } }` | Structured contract differences are preserved; the active host is unchanged |
| `ReloadEvent::Layered` | Foundation, capacity, retention and recovery conditions from `LayeredDevState` |
| `layered_snapshot()` | Build metrics, foundation manifest, active state, load counts and image retention; `None` for native |
| `retry_layered()` | Rescan without resetting the engine or budgets; false for native or stopped sessions |

Handle `ReloadEvent` with a fallback arm because it is non-exhaustive.
`LayeredDevState` also adds `Restarting`; update exhaustive matches of that enum.
Foundation changes report `RestartRequired` and preserve the old host; they do not
restart the process automatically. `WaitingForRetirement` and `RecoveryRequired`
keep the existing layered semantics. Build errors and rejected candidates preserve
admission; once cooperative Restart has started, admission is intentionally paused
until it commits or the application recovers.

JSON remains the default; [optional binary messages](BINARY_MESSAGES.md) keep the same ownership and size limits. Selecting
the loader does not establish a speedup for a particular application. Link/load
latency and edit-to-response latency include different work and must be measured
separately.

## Verify the integration

From a repository checkout on either supported platform:

```sh
cargo test -p reknot --no-default-features --features typed,jit-dev --test layered_typed --locked -- --ignored --nocapture
```

The test uses real objects and isolated processes for KeepLoaded and Managed,
retained old calls, stable handles and bridge clients, build failure and recovery,
initial/update contract rejection, foundation changes, retirement backpressure,
worker checkpoints, Restart admission and explicit recovery after an interrupted
Restart, plus producer reuse, non-overwrite and capacity checks. It uses the built-in Rust producer and requires cached Cargo dependencies.

[Typed handlers](TYPED_HANDLERS.md) · [Layered development](LAYERED_DEVELOPMENT.md)
