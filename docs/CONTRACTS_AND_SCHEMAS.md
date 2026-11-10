# Contract diagnostics and schemas

English | [日本語](../docs_ja/CONTRACTS_AND_SCHEMAS.md)

Typed modules compare generated JSON schemas before activation, including when a
Rust type name and contract version are unchanged. This checks declared structure;
equal schemas do not prove equal behavior or meaning.

## Describe protocol types

Add the schema attribute alongside Serde derives in the shared protocol crate:

```rust
use serde::{Deserialize, Serialize};

#[reknot::schema]
#[derive(Serialize, Deserialize)]
pub struct Message {
    pub author: String,
    #[serde(default)]
    pub attachments: Vec<String>,
}
```

`#[reknot::schema]` derives the SDK's re-exported `JsonSchema`; a direct dependency
on Schemars is unnecessary. Structs and enums are supported. Handler inputs,
outputs and worker state require `Serialize + DeserializeOwned + JsonSchema + 'static`.
Primitives, tuples and common containers already implement the schema trait.

The SDK records JSON Schema 2020-12 in both serialization and deserialization
directions. Supported Serde attributes, including field renaming, defaults and
direction-specific skipping, affect the generated schemas. For custom codecs or
unsupported annotations, supply an accurate `JsonSchema` implementation or an
appropriate `#[schemars(with = "...")]` type. A false schema cannot validate the
custom codec's behavior. Keep the SDK/schema generator version identical in host
and logic; upgrading from rc.1 requires rebuilding both.

## Read a rejection

`Error::Contract { report, source }` retains the original routing failure and an
owned `ContractReport`. Its text identifies the stage, handler, JSON pointer,
expected/actual value and the exported handler's file, line, column and function
when available. For example, a changed field can produce:

```text
host-to-candidate bot.reply/input_schema/deserialize/properties/value/type:
expected "integer", actual "string"
```

| Stage | Compared values | Action |
| --- | --- | --- |
| `host-to-candidate` | Host declaration and candidate declaration | Check the shared protocol, versions and field schemas; rebuild/restart the host for handler contract changes |
| `declaration-to-export` | Candidate declaration and actual exported handlers | Fix missing/extra exports or a mismatched handler declaration |

Source locations are diagnostic data, not compatibility keys: moving a source
file does not reject otherwise equal contracts. They identify the handler, not
the exact field declaration; follow the JSON pointer into the protocol schema.
The candidate's pure metadata callback copies JSON into a bounded host buffer.
Older modules or failed/oversized metadata callbacks may provide no details; the
original rejection and expected contract remain available.

`report.expected` and `report.candidate` preserve full decoded manifests.
`report.differences` contains at most 32 differences, with `truncated` indicating
additional differences. Text values are shortened to 240 characters; the owned
values remain complete. Initialization and candidate metadata each have a 1 MiB
limit. No DLL-owned pointer escapes into the report.

## Export the schema

Use `module().schema_json()?` to obtain a formatted manifest with format marker
`reknot-typed-contract-v2`, module/handler versions, input/output schemas and any
worker-state contract. The inner type schemas are JSON Schema documents; the
outer manifest is Reknot metadata, not itself a JSON Schema validation document.

Generated projects include a runnable example:

```sh
cargo run -p my-bot-protocol --example schema > schema.json
```

Replace the package name with the generated protocol's name. The output can be
stored for review or compared between revisions. Native error reports also
include actual export metadata and source locations.

## Change a contract deliberately

Comparison uses exact generated-schema equality, not permissive compatibility
inference. Even a potentially backward-compatible optional field addition can be
rejected. Struct shape, field names, variants and schema annotations may change
the result; ordering of semantically equivalent schema arrays is not normalized.

Increment handler versions for changes to input/output formats or meaning, and
rebuild/restart the host and its clients. Schema equality cannot detect a changed
unit, interpretation or business rule. Worker state has its own versioned
`WorkerState<S>` and can be converted through [typed Restart](TYPED_RESTART.md).
Schemas do not migrate arbitrary Future stacks or roll back external effects.

[Typed handlers](TYPED_HANDLERS.md) · [Documentation index](INDEX.md)
