# Reproducible comparisons

English | [日本語](../docs_ja/BENCHMARKS.md)

Use the same handler body and measure a successful updated response, not only
object loading. The checked-in [benchmark](../reknot/benches/development.rs) also
compares complete typed calls using JSON and optional MessagePack.

## Measurements on 2026-11-17

These are the initial measurements, before per-attempt metadata/compiler sharing,
Cargo output-directory alignment, cache-scan optimization, superseded-stage checks,
support optimization and native DLL preparation on the build worker. They remain a
historical baseline tied to the source manifest below, not measurements of the current
implementation. The baseline compiled both logic and support at dev `opt-level=0`.

Measured sequentially on the same Ryzen 7 5700X (8 cores / 16 threads): Windows 11 Home
10.0.26300 with Rust 1.99.1, and Ubuntu 24.04 / WSL2 with Rust 1.100.0. Both backends
within each OS use the same toolchain.

| Environment | Native DLL median / p95 (ms) | Layered JIT median / p95 (ms) |
| --- | --- | --- |
| Windows | 655.29 / 705.93 | 801.52 / 861.07 |
| Linux (WSL2) | 236.49 / 249.27 | 352.28 / 379.77 |

**In this initial baseline, JIT did not reduce total edit-to-response latency for this small fixture.** Most JIT
latency is on the worker, including input discovery, Cargo and foundation verification.
Owner-side load/activation alone cannot establish an end-to-end improvement; this comparison
also does not causally separate each operation inside the worker.

Typed-call measurements below are separate from the edit/load timings above.

| Environment | f32 values | JSON median (µs) | MessagePack median (µs) | JSON / MessagePack bytes |
| --- | --- | --- | --- | --- |
| Windows | 1 | 2.54 | 2.31 | 29 / 21 |
| Windows | 64 | 8.06 | 5.20 | 431 / 338 |
| Windows | 4096 | 302.64 | 142.89 | 32447 / 20498 |
| Linux (WSL2) | 1 | 1.84 | 1.65 | 29 / 21 |
| Linux (WSL2) | 64 | 6.93 | 4.11 | 431 / 338 |
| Linux (WSL2) | 4096 | 293.12 | 137.13 | 32447 / 20498 |

[Windows raw data](../benchmarks/windows-x64.json) and [Linux raw data](../benchmarks/linux-x64.json)
retain all samples, cold startup, JIT phase timings and p95. The [source SHA-256 manifest](../benchmarks/source-sha256.json)
identifies the measured implementation.
The manifest records file identities; it does not contain a source snapshot or make
the earlier implementation reproducible from the current checkout alone.

## Rerun

Run alone on an otherwise idle supported Windows x64 MSVC or Linux x86_64 GNU machine:

```sh
cargo bench -p reknot --bench development --features jit-dev,typed-binary --locked -- --output results.json
```

Rustup, Cargo, the platform linker and cached dependencies are required. The Rust
producer is used directly; no PowerShell/Python builder is involved. Cargo runs the benchmark from the `reknot/` crate directory; use an absolute output path to choose another location. The result
path must be new. The command creates isolated temporary fixture workspaces and
retains them for diagnosis; it never overwrites or deletes an existing project.

The current benchmark uses the built-in producer's defaults: JIT logic at
`opt-level=0` and resident support/dependencies at `opt-level=1`. Native DLLs use
`opt-level=0`. These are different development configurations, not equal-optimization
compiler measurements. For a controlled support-profile comparison, set
`LayeredDevOptions::support_optimization` explicitly in the benchmark and record the
source revision and setting with the results. See [build settings](LAYERED_DEVELOPMENT.md#inputs-foundation-and-cache).

## What is measured

| Comparison | Measurement and controls |
| --- | --- |
| Native DLL versus JIT | Source write through watching, 50 ms debounce, build, validation, activation and the first correct new response; owner polls every 1 ms |
| Workload | One typed String handler with the same body. Native links the SDK into a `cdylib`; JIT keeps the SDK in a resident support `dylib` |
| Compilation | Same platform/toolchain, debug=0, codegen-units=1. Native/logic dev opt=0; current JIT support/dependencies opt=1 (baseline: 0). Both producers set `CARGO_INCREMENTAL=0`; the layered object profile also prefers dynamic support |
| Sampling | Native/JIT/JIT/native process order; 2 edited warmups and 9 measured edits per process; 18 observations per backend. Cold startup is recorded separately |
| JSON versus MessagePack | Complete resident typed echo call: input clone, encode, routing, decode, result verification and cleanup; no DLL loading or external I/O |
| Payload | A struct containing a tick and 1, 64 or 4096 `f32` values. Both paths use identical values; named MessagePack maps preserve field names |
| Call sampling | 2 warmup batches, 15 measured batches, alternating order. 1000 calls/batch for small payloads, 100 for 4096 floats; timings are per-call batch averages |

Percentiles use the nearest-rank observation; medians use the upper middle sample
for an even count. Raw samples and phase timings are kept in JSON. No CI test
asserts that one backend must be faster.

## Interpret the result

The edit comparison measures two usable development configurations. It does not
isolate linker time: JIT also checks foundation identity and captures immutable
artifacts, while the normal DLL path uses its own build/watch pipeline. The fixture
is intentionally small and is not evidence for a large game, editor or production service.

Call timings cover owned serialization; they are not zero-copy results. Compare
both encoded size and latency at your payload size. Repeat on your own hardware
and application before selecting a backend or encoding.

## Response latency during reload

The separate `reload_latency` benchmark measures ongoing requests during Drain,
Managed collection, Restart, and three consecutive saves. It includes native
Drain as a control and compares a 64-byte checkpoint with a 256-KiB checkpoint,
and immediate child cleanup with a cooperative 25-ms delay.

```sh
cargo bench -p reknot --bench reload_latency --features jit-dev --locked -- --output /absolute/path/reload-results
python benchmarks/analyze_reload_latency.py /absolute/path/reload-results
```

Use a fresh absolute output directory (on Windows, for example
`C:/benchmarks/reload-results`). The default runs each case in separate processes
in forward and reverse order, with two warmups and nine measured edits per run.
`--case jit-restart-state --samples 1 --warmups 0 --rounds 1` selects a short
functional trial. `--interval-us` and `--cpu-us` adjust the request workload.

Requests arrive at a scheduled rate of 1000/s through the real typed bridge,
with 100 µs of busy work and a cooperative 5-ms wait. Latency starts at the
scheduled arrival, including late submission during an owner-thread stall and
queueing while admission is paused. Each edit has a 500-ms no-edit control.
The executor sleeps 1 ms between iterations; processing time adds to that cadence.

Raw JSON retains every request, owner update, bridge poll, admission transition,
reload event, and JIT metric, including warmups. The owner measurement covers all
of `DevModule::update`, including worker polling and collection; the bridge is
polled separately. Admission changes are observed before/after each update, so
the report includes lower/upper bounds rather than claiming an exact stop time.
State continuity, reply identity, complete arrival accounting, and actual image
reclamation are checked. These are synthetic latency observations, not a speed
gate or production SLA. Each case has 18 measured reloads across two processes;
pooled request percentiles do not turn correlated requests into additional reload
observations.

[Typed JIT](TYPED_JIT.md) · [Binary messages](BINARY_MESSAGES.md)
