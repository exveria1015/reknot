# Preparing a release

English | [日本語](../docs_ja/RELEASING.md)

The first Reknot candidate is a **source RC**, `v0.1.0-rc.1`. The `reknot`,
`reknot_abi` and `reknot_macros` crate versions are `0.1.0-rc.1`; internal registry
requirements use that exact version. Examples and generated application versions
are independent of the SDK version. A source RC does not publish crates to crates.io.

## Select the source revision

Use `main` as the integration and default branch. Include the rename, typed API,
framework bridge, project generator, documentation and verification scripts in the
candidate. Keep the working tree clean and record `git rev-parse HEAD` in the
release notes. Do not move a published tag to a different commit.

For a later candidate, update all three SDK versions, the exact internal dependency
requirements, `Cargo.lock`, the changelog and the intended tag together. Run
`cargo metadata --format-version 1 --offline` after the version change and inspect
the lockfile diff; unrelated dependency upgrades are not part of release preparation.

## Verify the candidate

The final source revision must pass all nine jobs in
[CI](../.github/workflows/ci.yml): Windows/Linux core and package checks,
Windows/Linux core MSRV, Bevy MSRV, both Miri borrow models and Windows/Linux GUI.
The core jobs also run typed native reload, the generated project, the framework
bridge and the extracted-package consumer. Linux GUI includes real WebView
interaction; Windows GUI covers the backend without opening a window.

Run the package publish rehearsal from a clean checkout:

```sh
cargo publish --dry-run --locked -p reknot_abi -p reknot_macros -p reknot
```

Use a Cargo version supporting multi-package publication (the verified development
toolchain is Rust/Cargo 1.99.1). The build MSRV remains Rust 1.95. A successful
dry-run or patched extracted-package consumer does not reserve names, prove
registry ownership or verify an actual download from crates.io.

Confirm the candidate can be fetched into a fresh checkout and the documented
commands resolve there. Verify repository links after changing the default branch
or repository name. Keep local logs outside the source tree committed to Git.

## Prepare and publish the source RC

Prepare a GitHub **draft prerelease** with title `Reknot v0.1.0-rc.1`, tag
`v0.1.0-rc.1` and the verified full commit SHA as its target. Include:

- The source SHA and the exact successful CI run URL.
- The crate and command rename, typed API, bridge and generator highlights.
- Windows/Linux coverage; Rust 1.95 core and Rust 1.97.1 Bevy requirements.
- The limitations below and links to the changelog and compatibility contract.
- Source-checkout instructions and the issue reporting destination.

Publish the prerelease when ready to make the candidate available. Creating a draft
alone does not publish its tag. After publication, fetch the tag and confirm that
it resolves to the verified commit. Use a new version for subsequent fixes.
The following commands are for an already published candidate:

```sh
git clone --branch v0.1.0-rc.1 --depth 1 https://github.com/exveria1015/reknot.git
cd reknot
cargo run -p typed_bot_host --locked
```

Keep this checkout when using `reknot-new`: generated projects refer to its SDK
sources by path. `cargo install` or a source-free generator is not a supported
installation route for this RC. See [typed handlers](TYPED_HANDLERS.md).

## State the limits

Reknot targets trusted Rust logic during local development. Build host and logic
together against the same SDK and compatible target/toolchain. The typed API
supports one DLL and Drain; low-level Restart is separate. Contract-version changes
require rebuilding/restarting the host as documented.

Images stay loaded by default, and the default budget is 32 cumulative load
attempts, including failed native loads. Explicit unsafe reclamation needs the
application's quiescence proof and does not refund the budget. Automatic unloading,
external-effect rollback, production rollout and macOS validation are outside the
current scope. Windows interactive GUI behavior is not covered by the automated
backend tests. See [compatibility](COMPATIBILITY.md) and [testing](TESTING.md).

## Publish crates separately

A later crates.io release is a separate distribution step. Recheck name availability
and account ownership immediately before publication. Use one agreed version across
the SDK packages and exact internal requirements, rerun the final CI and dry-run,
then publish `reknot_abi` and `reknot_macros` before `reknot`.

After publication, build a new consumer outside this workspace with registry
dependencies only: no `path`, Git dependency, `[patch]` or inherited local Cargo
configuration. Check both default typed/native and `default-features = false`
configurations. Only this verifies the actual registry installation route.

## Report problems

Report reproducible issues at
[Reknot issues](https://github.com/exveria1015/reknot/issues). Include the version or
commit, OS/target, `rustc --version`, feature flags, reproduction steps and relevant
error output. Remove credentials and private application data before posting.
