# WebSocket quiz room: Axum + Reknot and Fastify + JS

English | [日本語](../docs_ja/QUIZ_ROOM.md)

A playable local comparison with shared questions, browser assets and message
formats. Join both rooms, answer once per question, and keep playing while editing
the scoring rules. No accounts, tokens or frontend build are needed.

## Start and play

Use stable Rust/Cargo, a native linker and Node.js 22+ with npm. From the repository root:

```sh
npm ci --prefix examples/quiz_room/js
node examples/quiz_room/start.mjs
```

The launcher builds the Axum host, starts both servers and prints the URL after
both are ready. The first Rust logic build can take a while. Open
`http://127.0.0.1:4101/`, enter a name and join. Open another tab for a second player.
Start a question, answer in each panel and compare the scores. Everyone answering,
or the 30-second deadline, reveals the answer. The next button starts a new round;
the five questions repeat and scores accumulate. Any joined player can advance
a finished round. Stop both servers with Ctrl+C in the launcher terminal.

Alternative ports and Drain mode:

```sh
node examples/quiz_room/start.mjs --mode=drain --rust-port=4201 --js-port=4202
```

Use the printed URL: its `rust` and `js` query parameters select the local ports.
Port `0` lets the OS choose a free port. If npm is not available but pnpm is installed,
`pnpm dlx npm@11 ci --prefix examples/quiz_room/js` installs the same lockfile.

Run servers separately when investigating one backend:

```sh
cargo run -p quiz_host --locked
node examples/quiz_room/js/server.mjs
```

Each server reads `QUIZ_PORT` and `QUIZ_RELOAD_MODE` (`restart` or `drain`). Defaults
are ports 4101/4102 and Restart. Both bind only to `127.0.0.1` and serve identical
assets. There is one independent, in-memory room per server. Scores survive logic
updates, but disconnecting removes that player and restarting the server clears
the room. This is a local development example, not a hosted multiplayer service.

## Edit while connected

| Edit | Rust source | JavaScript source | What to observe |
| --- | --- | --- | --- |
| `POINTS`: 100 → 150 | `examples/quiz_logic/src/lib.rs` | `examples/quiz_room/js/logic.cjs` | New correct answers earn 150; existing scores stay unchanged |
| `LABEL`: rules-v1 → rules-v2 | Same | Same | Rule label and admitted generation in answer results |
| `TICK_MS`: 200 → 100 | Same | Same | Restart recreates the clock; Drain keeps the old clock cadence |
| Invalid syntax | Same | Same | Candidate fails; old generation keeps serving |
| `CONTRACT_VERSION`: 1 → 2 | Same | Same | Candidate rejected before stopping old work; restore 1 to recover |

Select “wait 10 seconds” before answering, then save a logic edit during that
wait. The two modes intentionally produce different results:

| Behavior | Restart (example default) | Drain |
| --- | --- | --- |
| New admission during cutover | Waits until cutover completes | Uses the current generation at admission |
| Admitted delayed answer | Cancelled and joined; no points and no automatic replay | Completes using old rules if the round is still open |
| Registered clock | Stopped, joined and recreated from its tick checkpoint | Original worker keeps running |
| Connections, room, scores, deadline | Host-owned and retained | Host-owned and retained |
| Invalid build or contract | Existing generation continues | Existing generation continues |

After cancellation, answer again explicitly if the round is still open. An answer
finishing after its round ends cannot add points, even if its calculation succeeded.
The UI displays scoring generation, clock generation, checkpoint ticks and an event
log. In Drain, different scoring/clock generations are expected.

## Integration boundaries

The Axum host runs a multithreaded Tokio runtime. A `LocalSet` keeps `DevModule`,
native tasks and cleanup on one owner thread. Axum's `Send` WebSocket tasks use
`HandlerClient` endpoints from `DevModule::bridge`. The bridge driver is polled
from executor notifications, so requests do not wait for the maintenance timer.
The separate 10 ms `update()` timer handles source/build maintenance and reload
progress. No room mutex is held across a logic await. The registered clock awaits
bounded host sleep services, checkpoints its count and publishes copied pulses.
The host owns sockets, players, questions,
deadline validation, answer reservations and score commits.

The Fastify host registers the official WebSocket plugin before its routes and
attaches socket listeners synchronously. Its sample reload adapter loads a fresh
CommonJS module, checks explicit contracts, captures the implementation at
admission, and uses `AbortController` plus join for Restart. Its host also owns
room state and a clock checkpoint. This adapter is application code, not a Fastify
hot-reload feature or a general JavaScript reload library.

| Comparison | Axum + Reknot | Fastify + JavaScript |
| --- | --- | --- |
| Changed unit | Logic crate and watched local dependencies | `logic.cjs` only; helper dependencies are not refreshed |
| Activation | Cargo debug build, native load, typed contract validation | Module reload and explicit runtime contract validation |
| Structural contracts | Generated from Rust/Serde types for both directions | Manually maintained declarations; JS type structure is not inferred |
| Lifecycle implementation | Reknot typed bridge, registered worker and host services | Example adapter and abortable Promise jobs |
| File debounce | 200 ms | 200 ms |
| Unchanged framework | Axum listener and WebSocket sessions | Fastify listener and WebSocket sessions |

Shared schemas are conservative structural checks; they do not prove semantic
compatibility. Changing a protocol may require rebuilding/restarting the host.
The save-time Restart path here requires unchanged worker state contracts; it does
not automatically migrate arbitrary changes. See [typed Restart](TYPED_RESTART.md)
for explicit migration, and [contracts and schemas](CONTRACTS_AND_SCHEMAS.md) for
diagnostics and schema export.

## Verification and measurements

```sh
npm ci --prefix examples/quiz_room/js
cargo clippy -p quiz_host -p quiz_logic -p quiz_protocol --all-targets --locked -- -D warnings
node examples/quiz_room/check.mjs
```

`--js-only` and `--rust-only` select a backend. The runner edits isolated copies
under a short OS temporary directory, starts real servers on free ports, and
connects two real WebSocket clients. It checks correct/wrong answers, duplicate
and stale answers, invalid choices, deadline expiry, preserved scores/connections,
Drain/Restart in-flight behavior, clock continuity/cadence, syntax and contract
rejection, a structural state-schema mismatch, and recovery. Fixture rounds use
a longer deadline to accommodate builds; a separate short round tests expiry.
The CI core jobs run this on Windows and Linux.

`target/quiz-check/results.json` records one warm edit per backend/mode, measured
from file write until a client sees the new generation, along with environment
information. It includes debounce and Rust compilation. Logs and temporary copies
are retained for failure diagnosis. These measurements do not establish throughput,
production latency, memory usage or a universal performance ranking. Compare
repeated edits on the same machine if making a speed claim. Also assess the amount
of integration code, diagnostic usefulness and how easily the lifecycle is understood.

## Limits and references

The demo caps connections at 32, pending commands at four per socket, and incoming
messages at 4 KiB. The Rust bridge has 128 slots and 32 concurrent requests. Slow
outbound clients are disconnected. Room state is transient; authentication,
reconnection identity, persistence and public deployment are outside this example.
All edited native/JS code is trusted, and JS candidate evaluation can execute
top-level code: keep logic module initialization free of external side effects.
The example adapters cannot undo arbitrary user-code side effects or make
non-cooperative/infinite handlers stop.

Rust retains loaded DLLs under the SDK's default cumulative 32-attempt budget.
Restart the host when that budget is exhausted; no automatic unloading is performed.
Host code, questions and UI assets are loaded at startup and need a server restart
after edits. `QUIZ_LOGIC_MANIFEST` (Rust) and `QUIZ_LOGIC_PATH` (JS) select trusted
local logic fixtures for the test runner; these cannot be set by a socket message.

Sources: [Axum WebSocket API](https://docs.rs/axum/0.8.9/axum/extract/ws/index.html),
[Fastify WebSocket plugin](https://github.com/fastify/fastify-websocket),
[framework bridge guide](TYPED_HANDLERS.md#call-from-framework-threads),
[DLL lifetime](UNLOAD_SAFETY.md).
