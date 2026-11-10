# Standalone ABI fixtures

`handler.rs` is compiled directly with rustc as a native DLL. It uses the
co-located `abi_v5.rs` snapshot and does not import the SDK or its macros.
Both files are included in the `reknot` package, so native ABI tests can
run from an extracted package without an adjacent workspace checkout.

The snapshot fixes Handler ABI 5, Services ABI 2, task-control ABI 2 and activity
ABI 1. Update it deliberately when adopting an ABI change, alongside rejection
tests for older layouts. It is not regenerated from production sources during
tests. Tests using the real SDK and generated handlers live in the repository's
separate example crates and exercise the other side of this boundary.

`reclamation.rs` uses the same frozen ABI with the actual SDK Waker bridge and
code-lease types included from `src/`. It constrains execution and exposes a return
barrier so the host can verify that the last lease disappearing does not close a DLL.

`managed_logic.rs` is built as a separate cdylib against the actual SDK with native
features disabled. It exercises generated handlers, registered generation locals,
host async notifications and destructor failure across the real allocation/FFI boundary.

`typed_logic.rs` is a real SDK typed module compiled by `native_typed`. The test
creates its local helper dependency in an isolated project and checks source
watching, typed return values, Drain, build recovery and contract rejection.
