# Linux ELF object loading

English | [日本語](../docs_ja/ELF_JIT.md)

`jit::ElfEngine` loads a single trusted ELF relocatable object directly on
`x86_64-unknown-linux-gnu`, without invoking the system linker or an LLVM bridge
at load time. Enable `reknot = { ..., default-features = false, features = ["jit"] }`
in the host. The normal `.so` loader remains available through `native`.

For ordinary development, use the [built-in Rust producer](LAYERED_DEVELOPMENT.md)
without Python. The external Python producer below remains available.

## Checked artifacts and Managed collection

Use [build-layered-elf.py](../scripts/build-layered-elf.py) with Python 3.9+,
rustup, stable Rust and a C linker. Support is a Rust `dylib` dependency of logic
that re-exports the SDK; the builder emits logic as a single library object.
Generate the workspace's `Cargo.lock` before the locked build:

```sh
python3 scripts/build-layered-elf.py \
  -ManifestPath /absolute/project/Cargo.toml -Package my_logic \
  -TargetDirectory /absolute/project/target/layered \
  -SupportLibrary /absolute/project/target/layered/x86_64-unknown-linux-gnu/debug/libmy_support.so \
  -Output /absolute/project/target/elf/logic.o
```

The builder publishes `logic.o.json` (schema 3) and content-addressed copies of
the object, support and std. It captures compiler identity, target, codegen flags,
resolved support features/profile and Cargo.lock hash. The support artifact comes
from the Cargo invocation that compiles logic. Cooperating
builders serialize writes to the Cargo target directory. Cache limits report
`CacheFull`; the builder never deletes another session's files.

```rust,no_run
use reknot::jit::{LayeredArtifact, LayeredEngine, LayeredLimits,
    collect_retired_elf, JitReclaimOptions};
use reknot::{HostResources, RequestRouter};

unsafe fn start() -> Result<RequestRouter, Box<dyn std::error::Error>> {
    let artifact = LayeredArtifact::read("target/elf/logic.o.json")?;
    // Trust native code and the producer's semantic declarations.
    let trusted = artifact.into_trusted();
    let mut engine = LayeredEngine::open(&trusted, LayeredLimits::default())?;
    // Also accept the complete-return/no-unmanaged-escape lifetime contract.
    let loaded = engine.load_managed(trusted.into_managed())?;
    let router = RequestRouter::from_loaded(loaded, HostResources::new())?;
    let _report = collect_retired_elf(JitReclaimOptions::default());
    Ok(router)
}
```

`LayeredArtifact` captures and hashes the object bytes. Before any foundation
initializer, `LayeredEngine::open` verifies std/support hashes, ELF SONAME and
dependency names, and copies the verified bytes into sealed `memfd` files. All
write/resize/seal changes are disabled. The loader verifies the resulting module
paths and keeps both descriptors and libraries resident. Changing the original
path cannot change the resident snapshots. Exactly one verified foundation is
allowed per process; different identities, prior unverified std/support loads and
partial initialization require a fresh process. Standard GNU system libraries
and the process loader/interposition configuration remain trusted; these hashes
do not authenticate a producer or make arbitrary native code safe.

Managed collection shares COFF's ownership and call-return guards. Old requests,
completed handles, locals, external adapters and Wakers must finish/release their
ownership first. Explicit owner-thread maintenance removes libgcc frame
registration, then calls `munmap`. Cleanup, join or OS failures retain the image
with a reason and are not automatically retried. Dropping a controller alone
never frees executable pages. `KeepLoaded` remains the default through `load`.
Use `jit_image_id`, `jit_retention_snapshot`, `JitPolicy` and
`collect_retired_jit` for platform-neutral diagnostics/maintenance, or the ELF
functions above. The `Jit*` names and `LayeredLimits` are aliases of the existing
`Coff*` types.

Managed limits are live images/bytes, independent of cumulative KeepLoaded
attempts and Router generation allowances. Defaults are 256 retained images and
256 MiB on the owner thread, including images from other engines on that thread.
No process-wide cap or automatic reclamation is implied.

## Source builds and watching (`jit-dev`)

Use the same [LayeredDevSession](LAYERED_DEVELOPMENT.md) API on Linux, passing
`scripts/build-layered-elf.py` as the builder and the Cargo support target name
without `lib`/`.so` to `LayeredDevOptions::new`. The worker invokes `python3`.
`start` uses KeepLoaded; `unsafe start_managed` opts all future edits into the
managed contract. Build failures preserve the active router, newer revisions
supersede stale candidates, and foundation changes produce `RestartRequired`.
Managed pressure supports `Wait` and `CancelOldest`; uncertain cleanup produces
`RecoveryRequired`. Cancellation still waits for task destruction and joined
external work. Foundation reuse requires `declared_inputs_complete`; otherwise
every build rechecks support with Cargo.

## Raw loading and switching generations

```rust,no_run
use reknot::jit::{ElfEngine, ElfLimits};
use reknot::{HostResources, RequestRouter};
use std::path::Path;

unsafe fn start(std_so: &Path, support_so: &Path)
    -> Result<RequestRouter, Box<dyn std::error::Error>>
{
    // Trusted objects, dependencies and initializers, built together with the
    // exact same compiler, target, features and panic strategy.
    let mut engine = ElfEngine::open_layered(std_so, support_so, ElfLimits::default())?;
    let first = engine.load("target/elf/logic-v1.o")?;
    let mut router = RequestRouter::from_loaded(first, HostResources::new())?;
    let next = engine.load("target/elf/logic-v2.o")?;
    let candidate = router.prepare_loaded(next)?;
    router.activate(candidate)?;
    Ok(router)
}
```

Register application resources before dispatch. Retain the engine for the whole
reload session to preserve its attempt/memory accounting. Existing requests finish
on their original code; resources stay in the host. Typed hosts can consume the
same `LoadedHandler` through `ModuleBuilder::from_loaded` and `reload_loaded`.

The default policy is process residency: dropping an engine, router, prepared
candidate or final Waker never unmaps the object or removes its unwind table.
Even an entry rejected for an incorrect Handler ABI remains resident because
native code has executed. Failures before unwind registration release the image.
The raw `ElfEngine` uses KeepLoaded; use the checked API above for Managed mode.
Debugger registration remains unsupported.

## Building the resident foundation and object

Use a Rust `dylib` support crate that re-exports the SDK and wraps resident
non-generic dependencies. Its logic consumer uses `cdylib` and depends on that
support crate, as in the repository's
[support](../reknot/tests/fixtures/jit_layered_support.rs) and
[logic](../reknot/tests/fixtures/jit_layered_logic.rs) fixtures. Re-exporting a crate
does not move every generic instantiation out of logic.

From such a workspace (replace `my_logic` with the logic package name):

```sh
mkdir -p target/elf
CARGO_INCREMENTAL=0 \
CARGO_PROFILE_DEV_CODEGEN_UNITS=1 CARGO_PROFILE_DEV_LTO=false \
RUSTFLAGS='-C prefer-dynamic=yes -C relocation-model=pic -C force-unwind-tables=yes -C panic=unwind' \
cargo rustc --lib -p my_logic --target x86_64-unknown-linux-gnu \
  --target-dir target/elf-build -- \
  --emit=obj="$PWD/target/elf/logic-v1.o"

rustc --print target-libdir --target x86_64-unknown-linux-gnu
```

Pass the single `libstd-*.so` from that compiler directory and the generated
`target/elf-build/x86_64-unknown-linux-gnu/debug/lib<support_crate>.so` to
`open_layered`. Use the same flags/toolchain for every generation; change the output
name when rebuilding logic. Keep foundation files immutable while the host runs.
Changing support, std, compiler or dependency configuration requires a fresh process.
The raw API does not verify compiler identity or authenticate library paths.

Both paths are canonicalized before opening either library with `RTLD_NOW |
RTLD_LOCAL`. They and successfully opened dependencies remain resident even if a
later open fails. Imports resolve through the explicit std/support handles, their
ELF dependency scopes, and `libc.so.6`, `libm.so.6`, `libgcc_s.so.1`; no `RTLD_DEFAULT`
search is used. Other JIT images never provide symbols. ELF dependency search,
symbol interposition, preloads and concurrent file replacement remain part of the
trusted process configuration. `ElfEngine::open(limits)` supplies only the fixed
GNU runtime set, for self-contained objects without the Rust support foundation.

## Supported subset and limits

- ELF64 little-endian x86-64 `ET_REL`, one regular symbol table and RELA records.
  Allocated text, read-only data, writable data and BSS are loaded; non-allocated
  debug/metadata sections are discarded. Archives and already linked executables/
  shared libraries are rejected. Duplicate definitions requiring linker selection,
  common symbols, IFUNC definitions and extended numbering are unsupported.
- Relocations: `R_X86_64_NONE`, `64`, `PC64`, `PC32`, `PLT32`, `32`, `32S`,
  `GOTPCREL`, `GOTPCRELX`, `REX_GOTPCRELX`. GOT forms remain indirect; no instruction
  relaxation is necessary. External PLT calls use image-local jump stubs. Direct
  PC-relative data references must fit their displacement; overflow is an error.
- Native/emulated TLS in logic, TLS relocations, initializer/finalizer arrays,
  startup sections, executable stacks and writable executable sections are rejected.
  Use `GenerationLocal` in logic; OS-managed TLS is allowed in the resident support.
- Code pages are RX, read-only sections/GOT are R, and mutable data is RW before
  any entry call. DWARF32 `.eh_frame` with version-1 CIE records is required and
  registered with libgcc. Record bounds and backward CIE references are checked;
  arbitrary CFI programs are trusted native metadata, not independently certified.
  Catch panics inside the logic/support Rust stack before returning over C callbacks.
- Defaults: 32 attempts (including failures), 32 MiB input, 64 MiB per mapped image,
  256 MiB cumulative published mappings per raw engine, plus the owner-thread live
  limits described above. BSS, alignment, page padding, GOT, stubs
  and unwind terminators count toward mapping limits. Dependencies, parser/cache
  allocations and libgcc metadata are outside those limits; opening another engine
  starts a separate cumulative budget but shares live retention accounting on its
  owner thread. There is no process-wide ELF memory cap.
- Parser quotas: 8,192 sections, 1,000,000 symbols and 1,000,000 relocations.
  Other architectures, musl, Mach-O, custom allocators and automatic Cargo archive
  bundling are outside this profile. Loading remains `unsafe`, not a sandbox.

`last_load_metrics` reports read, inspection, materialization and unwind-registration
time; compilation is excluded. `retained_bytes` counts published image pages,
including later entry rejection. `linked_bytes` in metrics excludes page padding.
These metrics do not establish a compilation or end-to-end performance advantage.

The relocation rules follow the [x86-64 psABI](https://gitlab.com/x86-psABIs/x86-64-ABI/-/blob/master/x86-64-ABI/object-files.tex).
Frame registration uses the [GNU unwind runtime](https://github.com/gcc-mirror/gcc/blob/master/libgcc/unwind-dw2-fde.c).

## Verification

On Linux x86_64 GNU with Python 3.9+, stable Rust and a C linker:

```sh
cargo test -p reknot --no-default-features --features jit-dev --lib --locked
cargo test -p reknot --no-default-features --features jit-dev --lib real_elf --locked -- \
  --ignored --nocapture --test-threads=1
```

The first command populates the dependency cache needed by the isolated fixture
workspace. The second compiles real SDK/support/logic objects and exercises cutover,
old requests, host resources, generation locals, resident support TLS, contained
panics, cancellation, controller Drop, rejected imports/TLS/ABI and capacity limits.
Isolated subprocesses also verify sealed identity, preloaded-library rejection,
128 managed updates beyond cumulative allowances, actual FDE/mapping removal,
external-thread joins, failed-release retention and real source-watch workflows
(Wait/CancelOldest, pinned recovery, build errors and foundation restart).
Expected caught panic messages are printed under `--nocapture`. Linux ELF and
Windows COFF checks run in the [object JIT workflow](../.github/workflows/jit.yml).
