# Optional binary typed messages

English | [日本語](../docs_ja/BINARY_MESSAGES.md)

JSON remains the default. Enable `typed-binary` in both host and logic (in support
for the layered layout), then select `WireFormat::MessagePack` in the shared
handler declaration. Native DLL, static and JIT hosts use the same declaration.

## Select per handler

```rust
use reknot::typed::{Handler, WireFormat};

const SAMPLES: Handler<Vec<f32>, Vec<f32>> = Handler::new("audio.samples", 1)
    .with_format(WireFormat::MessagePack);
```

Ordinary `HandlerHandle::call` and `HandlerClient::call` still return typed values.
JSON and MessagePack handlers can coexist in one module. Registered worker inputs
and results use their handler format; checkpoint serialization remains JSON.
`typed-binary` adds the optional `rmp-serde` dependency and its transitive dependencies.

## Compatibility and bounds

| Property | Behavior |
| --- | --- |
| Format identity | `messagepack-named-v1` is part of `HandlerSpec`; a different format rejects the candidate before changing admission |
| Struct representation | Named maps preserve field names across declaration reordering; positional tuple encoding is not used for structs |
| Serde configuration | Serializer and deserializer use the human-readable representation, matching JSON's selection for custom Serde implementations |
| Contract diagnostics | JSON Schema and explicit versions remain; generated schemas do not prove all custom serialization behavior |
| Encoded limit | 1 MiB independently for input/result, checked during encoding and before decoding |
| Ownership | Owned bytes; no Rust allocation, destructor or borrowed reference crosses the code boundary |
| Errors | Conversion errors and trailing MessagePack data are rejected; malformed input never invokes the handler body |

Changing format is a protocol change: increment the contract version and rebuild/restart
the host. `HandlerSpec` adds a public `format` field; update manual struct literals.
JSON omits the field from serialized metadata, preserving the existing default metadata.
Metadata and diagnostic messages stay readable JSON/text.

The configured [rmp-serde serializer](https://docs.rs/rmp-serde/1.3.1/rmp_serde/encode/struct.Serializer.html)
uses `with_struct_map` and `with_human_readable`; the
[deserializer](https://docs.rs/rmp-serde/1.3.1/rmp_serde/decode/struct.Deserializer.html)
uses the matching representation. Test custom Serde types with the selected format.
The two formats have different value support; choosing MessagePack is not a guarantee
that every JSON-specific custom serializer behaves identically.

## Measure before selecting

Use [the comparison benchmark](BENCHMARKS.md) to measure full typed round trips and
encoded sizes. Small string messages may not benefit; numeric arrays can have different
costs. Smaller encoded bytes alone do not prove lower application latency.

This is a binary serialization option, not zero-copy. A shared toolchain and lockfile
do not remove lifetime, aliasing, alignment or allocator obligations. Bulk resources can
also remain host-owned and be accessed through explicit services.

[Typed handlers](TYPED_HANDLERS.md) · [Development contracts](DEVELOPMENT_CONTRACTS.md)
