# Multiple service requirements and typed initialization data

English | [日本語](../docs_ja/SERVICE_CONTRACTS_AND_INIT.md)

**Handler ABI 5** handlers can declare required services in addition to their primary target,
and a schema for per-generation initialization data.
They use Services ABI 2, task-control ABI 2 and Activity ABI 1.
Rebuild the host and logic DLL together. Handler ABI 2–4 are rejected.

## Declare requirements in logic

```rust
const REQUIRED: &[ServiceRequirement] = &[
    ServiceRequirement::new(1, COUNTER_V1),
    ServiceRequirement::new(2, FORMAT_V1),
];

#[reknot::reload_handler(PRIMARY, requires = REQUIRED, init = Config)]
async fn handle(ctx: RequestContext) -> Result<(), ServiceError> {
    let config = ctx.initial::<Config>()?;
    let mut count = [0; 4];
    ctx.call_service(1, 0, &config.step.to_le_bytes(), &mut count)?;
    let reply = ctx.call_service_async(2, 0, &count, 64)?.await?;
    ctx.call(0, &reply, &mut [])?;
    Ok(())
}
```

`requires`, `init` and the existing `state` option may appear together in any order. Duplicate options are rejected.
Omitting them keeps the original declaration: one primary target and no initialization data.
`call` / `call_async` address the request's primary target; `call_service` / `call_service_async`
address an additional resource by binding number. Additional asynchronous calls participate in the existing cancellation and join management.

A binding number is a nonzero u64 chosen by the logic for a particular role. It is separate from the service ID and version.
Two resources of the same service type may have different binding numbers.
There may be at most 32 additional requirements. Duplicate binding numbers and invalid service IDs or versions are rejected.
Declarations are copied into a bounded inline array; no foreign array pointer is retained.

## Bind resources in the host

```rust,ignore
let environment = HandlerEnvironment::default()
    .bind(1, counter.id())
    .bind(2, formatter.id())
    .with_initial(InitData::from_value(&Config { step: 2 })?);
let mut router = unsafe {
    RequestRouter::load_with_environment(
        library, resources, environment, RouterOptions::default(),
    )
}?;
```

Assign a **resource ID explicitly** to each binding. The host chooses even when several resources share a service ID and version.
The SDK does not scan the registry for the first match or automatically rebind a revoked resource to a replacement.
Initial loading fails if any required additional service is missing.

Declared service IDs and versions must exactly match the bound resources. Version ranges, priority-based fallback,
optional requirements and per-operation signature checks are not provided.
Define operation numbers, input/output formats and meaning in each service version's contract.
The environment may include unrequested bindings, but only declared ones reach RequestContext.
An environment allows at most 32 bindings. Duplicate numbers are rejected whether or not the handler requires them.

`RequestContext::binding(id)` returns the resolved `ServiceBinding`.
ID/version checks establish contracts between trusted code; they do not isolate permissions against malicious native code.

## Owned, typed initialization data

`Config` implements `InitPayload`:

- `SCHEMA: ServiceKey`: the data's schema ID and version.
- `encode(&self)`: produces owned bytes.
- `decode(&[u8])`: validates format and values, then restores an owned Config.

The complete implementation is in the [shared protocol](../examples/contracts_protocol/src/lib.rs).
It checks a `CFG1` magic value, fixed length, little-endian numbers and a value range of 1–100.
The SDK adds no required serialization dependency for this feature.

The host creates an owned copy with `InitData::from_value(&config)`.
Usually, shared code defines the schema ID and format. Change the version when format or semantic compatibility changes.
`InitData::new(schema, bytes)` can accept externally obtained data, which undergoes the same validation.

The macro generates a pure validation callback. Load and prepare run `decode` in the candidate DLL.
A schema mismatch is rejected before calling the decoder. Matching schemas still reject invalid length, format or values.
Unwinding within the SDK validation callback becomes `PANICKED`.
Initialization data is limited to 1 MiB. A schema ID does not prove Rust layout or type identity.
The SDK also cannot infer the author's intended interpretation of bytes with multiple possible meanings.

Validators, including destructors run during decoding and validation, must be pure and bounded. They must not perform I/O, start child work
or mutate application state. OS initialization during DLL loading remains subject to the existing unsafe contract.
Rejecting invalid data does not isolate or roll back effects from arbitrary DLL initialization code.
The SDK's byte limit does not bound arbitrary additional allocations inside a decoder.

This data is **per-generation configuration**. It is separate from per-request `ctx.input()`, worker Checkpoints
and the old `hot_init` / `with_context` API. This feature does not type all ordinary requests, automatically validate checkpoint formats
or make the old context API safe.

## Prepare, cut over and revoke

`prepare(path)` / `prepare_api(api)` validate a candidate with a copy of the current environment.
To change configuration, bindings or schema, pass a new environment to `prepare_with_environment` /
`prepare_api_with_environment`.
Static APIs also support `from_api_with_environment(api, resources, environment, options)`.

| Check point | Behavior |
| --- | --- |
| Load / prepare | Validates the manifest, all additional services and initialization data. Does not run the handler body or request factory |
| Drain activation | Rechecks the candidate's base generation and other metadata, and confirms resolved additional resources are not revoked |
| Start of Restart | Rechecks additional resources before pausing admission and requesting stop |
| Restart cutover | Checks for revocation during shutdown, stages new workers, then rechecks before cutover |
| New dispatch / worker creation | Rechecks required additional resources; does not call the factory if they are invalid |
| Service calls from existing execution | Checks resource ID and version. Revocation returns REVOKED; no replacement resource is substituted |

Failed preparation and contract rejection before stopping leave existing admission and execution intact.
Revocation after Restart begins stopping causes failure with the old generation retained and admission paused.
Stops and I/O that have already happened are not rolled back.

Candidates and generations own their environment. The request factory copies the resolved service table and initialization bytes during the call.
Old requests use their old DLL, configuration and bindings; new requests use the new combination.
The host decides whether to keep resources valid for the lifetime of existing execution or allow revocation.
Changing initialization configuration does not replace checkpoint migration or host-resource/database migration.

Typical errors are `MissingService`, `ContractMismatch`, `InitSchemaMismatch`,
`InitData(ServiceError)` and `InvalidEnvironment`. Native loading of rejected candidates still
consumes the cumulative generation budget and remains subject to KeepLoaded.

## Run and verify

```powershell
cargo run -p contracts_host --locked
cargo test -p reknot --test service_contracts --locked
cargo test -p contracts_host --test native_contracts --locked -- --ignored --test-threads=1 --nocapture
./scripts/check-miri.ps1 -Toolchain nightly-2026-11-02
```

The example builds separate real v1/v2 DLLs without editing sources, runs a finite set of checks, then exits.
Build output is isolated under `target/contracts-build/<PID>/` so nested Cargo does not wait indefinitely for the host's build lock.
It uses a counter and asynchronous formatting service alongside the primary target.
It checks that candidates with missing services, wrong versions or mismatched initialization data do not change handler state,
and that old/new execution finishes with its own configuration and bindings even after Router Drop.
A separate native fixture checks rejection of old ABI 4 after reading only its short header.

Miri checks the same contract, ownership and cutover paths without DLLs, under both borrow models.
Real-DLL native tests run separately. These checks do not guarantee the correctness of arbitrary decoders or service implementations.
See [testing](TESTING.md) for commands and coverage.
