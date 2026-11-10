# Contributing

Start with the [typed-handler guide](docs/TYPED_HANDLERS.md) and
[compatibility contract](docs/COMPATIBILITY.md). The supported use case is trusted
Rust logic reloaded during local development.

## Source layout

| Path | Responsibility |
| --- | --- |
| `reknot/` | Host resources, owned tasks, routing and optional native tooling |
| `reknot_macros/` | Handler generation for native and static logic |
| `reknot_abi/` | Versioned C ABI tables and ownership contracts |
| `examples/` | Runnable hosts, logic and integration samples |
| `docs/`, `docs_ja/` | Public guides in English and Japanese |
| `scripts/` | Verification helpers |

## Verify a change

Use the commands in [testing](docs/TESTING.md) / [テスト](docs_ja/TESTING.md).
Run formatting and the tests covering the affected behavior. Changes to ABI,
ownership, cancellation or cutover also need the relevant native integrations
and Miri groups. Native tests, Miri and interactive GUI checks cover different
behavior; record which checks actually ran and any environment restrictions.

For a release, follow the [release procedure](docs/RELEASING.md) /
[リリースの準備](docs_ja/RELEASING.md). Report reproducible problems through
[GitHub issues](https://github.com/exveria1015/reknot/issues).

CI checks Windows/Linux core and native behavior, the declared core MSRV, both
Miri borrow models and the Windows/Linux GUI backend. A successful older run does not
verify a new change. Match the result to the proposed source revision.

Use `scripts/check-docs.py` for public Markdown links and
`scripts/check-packages.ps1` for extracted SDK archives and a consumer. Repository
native demos belong in separate, unpublished example crates. Packaged SDK targets
must not include source files from sibling crates. Keep the standalone ABI fixture
snapshot independent from generated SDK handlers.

## Documentation

Keep README focused on starting the project. Put current behavior, prerequisites
and limitations in the public guides, and user-visible API changes in
[CHANGELOG.md](CHANGELOG.md). When a shared contract changes, update the English
and Japanese guides together. Pair files by name in `docs/` and `docs_ja/`, keeping
the same heading hierarchy, section order, tables and executable examples. Translate
prose naturally while preserving conditions, limits and failure behavior.
Keep language-switch links at the top of each page. The root README mirrors
`docs/README.md`, with links relative to the repository root.

Keep local plans, investigation notes and dated verification logs in the ignored
`contexts/` directory. Public documents must remain usable without those files
and must not link to them. Describe repeatable checks and their scope in public
guides rather than embedding a local session's success log.
