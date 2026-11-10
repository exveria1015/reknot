# reknot_abi

Versioned C ABI tables for Reknot message handlers, host services, owned
tasks and wake notifications. This is a support crate for `reknot` and
`reknot_macros`; applications normally use `reknot` re-exports.

Allocations are destroyed by the side that created them. Native callers must
preserve table, data, callback and code lifetimes and obey owner-thread rules.
Build host and logic against the same SDK version. ABI metadata validation does
not isolate untrusted native code.

See the [repository](https://github.com/exveria1015/reknot) for the current
API and ownership contracts.

Licensed under MIT OR Apache-2.0. Both license texts are included in this package.
