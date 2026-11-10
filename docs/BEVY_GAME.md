# A minimal Bevy game integration

English | [日本語](../docs_ja/BEVY_GAME.md)

This example uses Bevy **0.20.1** `App`, `MinimalPlugins` and ECS, with a persistent worker
in a real DLL moving one Actor. It runs without rendering so update rules can be checked first.
The host requires Rust 1.97.1 or later. Only the host depends on Bevy; the logic DLL does not.

## Run the example

Run from the repository root with Cargo and rustc on PATH.

```powershell
cargo run -p bevy_game_host --locked -- --verify
```

This builds separate speed-1 and speed-3 logic variants, verifies a finite number of frames, then exits.
It does not edit source files. To run the same verification as a test, use the command below.
Expected values match the supplied `STEP` constant: normally 1, or 3 with the `fast` feature.
Restore that constant before verification if you have edited it manually.

```powershell
cargo test -p bevy_game_host --test native_game --locked -- --ignored --test-threads=1 --nocapture
```

For continuous development, use the following command. It runs at about 60 frames per second and prints state every 60 frames.
It also submits an ordinary job every 60 frames, with the result scheduled 30 frames later.

```powershell
cargo run -p bevy_game_host --locked
```

Edit `STEP` in `examples/bevy_game_logic/src/lib.rs` and save to start Cargo
in a separate process. Frames keep advancing during the build, followed by Restart on success.
A build failure, such as a syntax error, leaves the current code running; the next edit triggers another build.
Only this one source file is watched. Restart the host after protocol or dependency changes.
Use Ctrl+C to exit. Exiting continuous mode is not a cooperative-shutdown test; `--verify` checks explicit stop and completion.

Build output and logs live under `target/bevy-game-build/<PID>/`. A separate target directory
prevents nested Cargo in tests from waiting indefinitely for the host's build lock.
The initial logic build completes before the game starts. Later builds are checked with `Child::try_wait`.

## Frames and ownership

| Bevy stage | Work |
| --- | --- |
| `PreUpdate` | Updates the frame number, receives candidates, applies Drain and polls Restart once |
| `Update` | Admits new jobs and polls persistent workers and ordinary tasks once each |
| `PostUpdate` | Validates returned messages and lets the host apply them to ECS |

Cutover happens only in `PreUpdate`. Preparing a candidate alone does not change the generation.
`Update` and `PostUpdate` assert that the generation has not changed since the start of the frame.
When adding rendering, pass the committed game state to the renderer afterward.

`GameHost` contains `Rc` and `RequestRouter`, making it `!Send` / `!Sync`.
Bevy's `NonSendMut` keeps DLL loading, polling, service calls and destruction
on the owner thread. The host and services also check the owner-thread ID.
Bevy's `multi_threaded` feature is enabled, but DLL tasks do not move to its worker pool.

Only JSON bytes, frame numbers and versioned checkpoints cross into the DLL.
`World`, component references and Bevy's internal layouts are not shared.
Host services only read the clock synchronously or append to a bounded reply queue.
Limits are eight ordinary jobs, 64 queued replies and 1,024 bytes per message.
Full capacity causes explicit rejection, with no automatic replay.

## Rules for old job results

For ordinary jobs that mutate state, this example checks the following in order:

1. The job's generation matches the currently active generation.
2. The target Entity exists. `Entity::to_bits()` includes its allocation generation, distinguishing a new Entity in the same slot.
3. The `revision` recorded when the job was submitted matches the target's current revision.

Only results passing every check are applied, then `revision` advances. These jobs do not take a position snapshot
as input, so per-frame movement does not increment `revision`. Jobs such as pathfinding that use position as input
also need invalidation rules based on movement or an acceptable tolerance.
These are application rules; the reloader does not automatically determine whether a job result is still valid.

Old jobs can finish after Drain, but their results are discarded as belonging to an old generation.
Drain does not recreate registered workers either, so movement commands from an old worker stop being applied.
The practical default for moving a persistent worker to new logic is **Restart**. Continuous mode uses it too.

## Restart across multiple frames

On receiving stop, the old worker deliberately waits two frames for cleanup, saves its tick count and finishes.
It also has a child registered with `spawn_child`, whose cleanup waits until the next poll after stop.
The real-DLL checks confirm one active registration during execution and zero after shutdown completes.
Ordinary jobs also observe stop and finish without emitting results. They are not replayed.

Instead of storing a `Restart<'_>` that borrows a router in the same struct, the host waits inside
an `async move` block that owns the router. This Future carries over to later frames while the outer code
continues polling ordinary tasks. New requests are rejected during shutdown.
The host clock and ECS schedule keep running; cutover occurs at a frame boundary after shutdown completes.

The checkpoint schema stays at `TICKS` v1, carrying the tick count forward unchanged.
Type and format migration are covered by the [Restart guide](RESTART_AND_MIGRATION.md) and real-DLL tests.
If Restart fails, for example during cleanup, the core contract leaves admission paused.
This minimal host displays the error; it does not implement a general recovery UI.

## Verification scope

The `native_game` integration test checks these behaviors without rendering:

- Loads separately built real DLLs with speeds 1 and 3.
- Preparation leaves the generation unchanged; Drain takes effect at the frame-3 boundary.
- Discards delayed old-generation results, applies new-generation results and rejects mismatched input revisions.
- Reuses an Entity slot after deletion and confirms results for the old Entity cannot modify the new one.
- Cooperatively stops persistent workers from old Drain generations and long-running jobs.
- Advances frames while Restart stays Pending for at least two frames, then applies it at the frame-12 boundary.
- Matches the saved tick count to the new worker's initial value and resumes at speed 3.
- Preserves generation and new-job admission after rejecting a missing DLL.
- Explicitly stops and completes the final worker, then destroys the host and services on their owner thread.
- Also runs ordinary `App::run` for 120 frames, applying persistent-worker and periodic-job results to position.

Rendering, input, GPU resources, physics engines, external callbacks and shutdown of unmanaged children are outside this verification.
It is not a performance measurement or evidence of a complete general-purpose game integration.
Generated DLLs follow the normal KeepLoaded and cumulative load-budget contracts.
