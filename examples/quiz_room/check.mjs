// Real-process, real-WebSocket checks. Only isolated source copies are edited.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import {
  mkdir,
  mkdtemp,
  readFile,
  writeFile,
  copyFile,
} from "node:fs/promises";
import { tmpdir, platform, cpus } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const root = fileURLToPath(new URL("../../", import.meta.url));
const resultsDir = path.join(root, "target/quiz-check");
await mkdir(resultsDir, { recursive: true });
const fixture = await mkdtemp(path.join(tmpdir(), "rq-"));
const jsOnly = process.argv.includes("--js-only");
const rustOnly = process.argv.includes("--rust-only");
const children = new Set(),
  connections = new Set();
const observations = [];
const backends = jsOnly ? ["js"] : rustOnly ? ["rust"] : ["rust", "js"];
async function run(command, args, options = {}) {
  const child = spawn(command, args, {
    cwd: root,
    windowsHide: true,
    ...options,
  });
  children.add(child);
  let output = "";
  child.stdout.on("data", (d) => {
    output += d;
  });
  child.stderr.on("data", (d) => {
    output += d;
  });
  await new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("exit", (code) => {
      children.delete(child);
      code === 0
        ? resolve()
        : reject(new Error(`${command} failed (${code})\n${output}`));
    });
  });
}
async function until(predicate, label, timeout = 120000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const result = await predicate();
    if (result) return result;
    await sleep(25);
  }
  throw new Error(`Timed out: ${label}`);
}
class Peer {
  constructor(port) {
    this.id = 0;
    this.pending = new Map();
    this.snap = null;
    this.session = null;
    this.clockHistory = [];
    this.ws = new WebSocket(`ws://127.0.0.1:${port}/ws`);
    connections.add(this);
    this.ws.addEventListener("message", ({ data }) => {
      const value = JSON.parse(data);
      if (value.type === "snapshot") {
        this.snap = value;
        this.clockHistory.push({ ...value.clock });
      }
      if (value.type === "hello") this.session = value.session;
      if (value.type === "result" && this.pending.has(value.id)) {
        this.pending.get(value.id)(value);
        this.pending.delete(value.id);
      }
    });
    this.ws.addEventListener("error", () => {});
  }
  async ready() {
    await until(() => this.session && this.snap, "WebSocket handshake", 10000);
    return this;
  }
  send(action, fields = {}) {
    const id = ++this.id;
    const result = new Promise((resolve) => this.pending.set(id, resolve));
    this.ws.send(JSON.stringify({ id, action, ...fields }));
    return Promise.race([
      result,
      sleep(35000, null, { ref: false }).then(() => {
        throw new Error(`No reply to ${action}`);
      }),
    ]);
  }
  close() {
    this.ws.close();
    connections.delete(this);
  }
  player() {
    return this.snap.players.find((p) => p.id === this.session);
  }
}
async function launch(backend, mode, logicPath, manifest) {
  const env = {
    ...process.env,
    QUIZ_PORT: "0",
    QUIZ_RELOAD_MODE: mode,
    QUIZ_LOGIC_PATH: logicPath,
    QUIZ_LOGIC_MANIFEST: manifest,
    CARGO_TARGET_DIR: path.join(root, "target/quiz-build"),
  };
  const executable =
    backend === "rust"
      ? path.join(
          root,
          `target/debug/quiz_host${platform() === "win32" ? ".exe" : ""}`,
        )
      : process.execPath;
  const args = backend === "rust" ? [] : ["examples/quiz_room/js/server.mjs"];
  const child = spawn(executable, args, { cwd: root, env, windowsHide: true });
  children.add(child);
  let output = "",
    port,
    exited = false;
  child.stdout.on("data", (data) => {
    output += data;
    const match = output.match(/QUIZ_READY (\{[^\n]+\})/);
    if (match) port = JSON.parse(match[1]).port;
  });
  child.stderr.on("data", (data) => {
    output += data;
  });
  child.on("exit", () => {
    exited = true;
    children.delete(child);
  });
  child.on("error", (error) => {
    output += error.message;
    exited = true;
  });
  try {
    await until(
      () => {
        if (exited) throw new Error(output);
        return port;
      },
      `${backend} startup`,
      240000,
    );
  } catch (error) {
    await writeFile(path.join(resultsDir, `${backend}-${mode}.log`), output);
    throw error;
  }
  return {
    child,
    port,
    save: () =>
      writeFile(path.join(resultsDir, `${backend}-${mode}.log`), output),
  };
}
async function prepareRust() {
  const sdk = path.join(root, "reknot").replaceAll("\\", "/");
  for (const name of ["quiz_protocol", "quiz_logic"]) {
    const dir = path.join(fixture, name);
    await mkdir(path.join(dir, "src"), { recursive: true });
    await copyFile(
      path.join(root, "examples", name, "src/lib.rs"),
      path.join(dir, "src/lib.rs"),
    );
    const manifest = (
      await readFile(path.join(root, "examples", name, "Cargo.toml"), "utf8")
    ).replace("../../reknot", sdk);
    await writeFile(path.join(dir, "Cargo.toml"), manifest);
  }
  await writeFile(
    path.join(fixture, "Cargo.toml"),
    '[workspace]\nmembers = ["quiz_logic", "quiz_protocol"]\nresolver = "2"\n[workspace.package]\nedition = "2021"\nrust-version = "1.95"\nlicense = "MIT OR Apache-2.0"\n',
  );
  await copyFile(
    path.join(root, "Cargo.lock"),
    path.join(fixture, "Cargo.lock"),
  );
  // The fixture is a smaller workspace; resolve it before DevModule's --locked build.
  await run("cargo", [
    "metadata",
    "--offline",
    "--format-version",
    "1",
    "--manifest-path",
    path.join(fixture, "Cargo.toml"),
  ]);
}
async function scenario(backend, mode) {
  const logicPath =
    backend === "rust"
      ? path.join(fixture, "quiz_logic/src/lib.rs")
      : path.join(fixture, "logic.cjs");
  const original = await readFile(
    path.join(
      root,
      backend === "rust"
        ? "examples/quiz_logic/src/lib.rs"
        : "examples/quiz_room/js/logic.cjs",
    ),
    "utf8",
  );
  // Longer fixture deadline leaves room for debug DLL builds on busy CI machines.
  const source = original.replace("30000;", "120000;");
  await writeFile(logicPath, source);
  const server = await launch(
    backend,
    mode,
    logicPath,
    path.join(fixture, "quiz_logic/Cargo.toml"),
  );
  const a = await new Peer(server.port).ready(),
    b = await new Peer(server.port).ready();
  const started = Date.now();
  try {
    const html = await fetch(`http://127.0.0.1:${server.port}/`).then((r) =>
      r.text(),
    );
    assert.match(html, /TWO STACKS/);
    assert.equal((await a.send("join", { name: "Ada" })).ok, true);
    assert.equal((await b.send("join", { name: "Lin" })).ok, true);
    assert.equal((await a.send("join", { name: "Again" })).ok, false);
    assert.equal((await a.send("start")).ok, true);
    await until(() => a.snap.round?.id === 1, "round one");
    assert.equal(
      a.snap.round.answer,
      null,
      "do not reveal the answer while open",
    );
    assert.equal(
      (await a.send("answer", { round: 1, choice: 0 })).data.points,
      100,
    );
    assert.equal((await a.send("answer", { round: 1, choice: 0 })).ok, false);
    assert.equal(
      (await b.send("answer", { round: 1, choice: 0 })).data.points,
      100,
    );
    await until(() => !a.snap.round.open, "round one reveal");
    assert.equal(a.snap.round.answer, 0);
    await a.send("next");
    await until(() => a.snap.round.id === 2, "round two");
    const delayed = a.send("slow_answer", { round: 2, choice: 1 });
    await until(() => a.player()?.pending, "answer reservation");
    // Wait for owner admission before modifying source, including on fast JS.
    await sleep(100);
    const tickBefore = a.snap.clock.ticks;
    const changed = source
      .replaceAll("rules-v1", "rules-v2")
      .replace("= 100;", "= 150;")
      .replace("= 200;", "= 100;");
    const editTime = performance.now();
    await writeFile(logicPath, changed);
    await until(
      () => a.snap.rules?.generation === 2 && a.snap.rules.label === "rules-v2",
      "v2 activation",
    );
    const editToVisibleMs = Math.round(performance.now() - editTime);
    const answer = await delayed;
    if (mode === "restart") {
      assert.equal(
        answer.ok,
        false,
        "Restart cancels the in-flight ordinary request",
      );
      await until(() => !a.player().pending, "cancel reservation release");
      assert.equal(
        a.player().score,
        100,
        "cancelled answer has no score or replay",
      );
      const retry = await a.send("answer", { round: 2, choice: 1 });
      assert.equal(retry.data.points, 150);
      await until(() => a.snap.clock.generation === 2, "restarted clock");
    } else {
      assert.equal(answer.ok, true);
      assert.equal(answer.data.generation, 1);
      assert.equal(answer.data.points, 100, "Drain captures old scoring logic");
      assert.equal(
        a.snap.clock.generation,
        1,
        "Drain retains the original clock",
      );
    }
    assert.ok(a.snap.clock.ticks >= tickBefore, "clock checkpoint survives");
    assert.equal(
      (await b.send("answer", { round: 2, choice: 0 })).data.points,
      0,
      "wrong answers score zero",
    );
    await until(() => !a.snap.round.open, "round two reveal");
    await a.send("next");
    await until(() => a.snap.round.id === 3, "round three");
    assert.equal(
      (await a.send("answer", { round: 2, choice: 1 })).ok,
      false,
      "reject stale round",
    );
    assert.equal(
      (await a.send("answer", { round: 3, choice: 2 })).data.points,
      150,
    );
    await b.send("answer", { round: 3, choice: 0 });
    await until(() => !a.snap.round.open, "round three reveal");
    const scoreBefore = a.player().score;
    const sessionBefore = a.session;

    await writeFile(
      logicPath,
      changed + "\nthis is deliberately invalid syntax;\n",
    );
    await until(
      () => a.snap.reload.status === "build_failed",
      "syntax failure",
    );
    assert.equal(a.snap.rules.generation, 2);
    await writeFile(
      logicPath,
      changed.replace(
        "CONTRACT_VERSION" + (backend === "rust" ? ": u32" : "") + " = 1;",
        "CONTRACT_VERSION" + (backend === "rust" ? ": u32" : "") + " = 2;",
      ),
    );
    await until(
      () => a.snap.reload.status === "rejected",
      "contract rejection",
    );
    assert.equal(a.snap.rules.generation, 2);
    assert.equal(a.player().score, scoreBefore);
    assert.equal(a.session, sessionBefore);
    // Check structural mismatch independently of the explicit contract version.
    let protocolSource;
    const protocolPath = path.join(fixture, "quiz_protocol/src/lib.rs");
    if (backend === "rust") {
      protocolSource = await readFile(protocolPath, "utf8");
      await writeFile(
        protocolPath,
        protocolSource.replace(
          "pub struct ClockState {",
          'pub struct ClockState {\n    #[serde(rename = "count")]',
        ),
      );
      await writeFile(logicPath, changed);
    } else {
      await writeFile(
        logicPath,
        changed.replace(
          "nonnegative-integer",
          "different-state-shape",
        ),
      );
    }
    await until(
      () =>
        a.snap.reload.status === "building" ||
        a.snap.reload.message.includes("count") ||
        a.snap.reload.message.includes("different-state-shape"),
      "state check started",
    );
    await until(
      () =>
        a.snap.reload.status === "rejected" &&
        (a.snap.reload.message.includes("count") ||
          a.snap.reload.message.includes("different-state-shape")),
      "state schema rejection",
    );
    assert.equal(a.snap.rules.generation, 2);
    if (protocolSource) await writeFile(protocolPath, protocolSource);
    await writeFile(
      logicPath,
      changed.replaceAll("rules-v2", "rules-v3").replace("= 150;", "= 175;"),
    );
    await until(() => a.snap.rules.generation === 3, "recovery to v3");
    assert.equal(a.player().score, scoreBefore);
    assert.equal(a.session, sessionBefore);
    assert.equal(
      a.ws.readyState,
      WebSocket.OPEN,
      "the original WebSocket remains open",
    );
    await a.send("next");
    await until(() => a.snap.round.id === 4, "round four");
    assert.equal(
      (await a.send("answer", { round: 4, choice: 3 })).data.points,
      175,
    );
    await b.send("answer", { round: 4, choice: 0 });
    await until(() => !a.snap.round.open, "round four reveal");
    await writeFile(
      logicPath,
      changed
        .replaceAll("rules-v2", "rules-v4")
        .replace("= 150;", "= 175;")
        .replace("= 120000;", "= 1500;"),
    );
    await until(() => a.snap.rules.generation === 4, "short-deadline rules");
    await a.send("next");
    await until(() => a.snap.round.id === 5, "round five");
    assert.equal(
      (await a.send("answer", { round: 5, choice: 4 })).ok,
      false,
      "invalid choice",
    );
    await until(() => !a.snap.round.open, "clock-driven deadline", 5000);
    assert.equal(a.snap.round.answer, 0);
    assert.equal(
      (await a.send("answer", { round: 5, choice: 0 })).ok,
      false,
      "deadline rejects late answer",
    );
    // Observe the clock after cutover: no rewind or old-generation late publish.
    await sleep(600);
    for (let i = 1; i < a.clockHistory.length; i++) {
      assert.ok(
        a.clockHistory[i].ticks >= a.clockHistory[i - 1].ticks,
        "clock never rewinds",
      );
      assert.ok(
        a.clockHistory[i].generation >= a.clockHistory[i - 1].generation,
        "old worker cannot publish after new worker",
      );
    }
    const t0 = a.snap.clock.ticks;
    await sleep(1100);
    const ticksPerWindow = a.snap.clock.ticks - t0;
    assert.ok(
      ticksPerWindow >= 2 && ticksPerWindow <= (mode === "restart" ? 15 : 8),
      "bounded single clock cadence",
    );
    const record = {
      backend,
      mode,
      editToVisibleMs,
      ticksPer1100ms: ticksPerWindow,
      score: a.player().score,
      durationMs: Date.now() - started,
      checks:
        "gameplay, duplicate, stale, invalid-choice, deadline, state, connection, in-flight, clock, syntax, contract, structural-schema, recovery",
    };
    observations.push(record);
    console.log(JSON.stringify(record));
  } finally {
    a.close();
    b.close();
    await sleep(100);
    server.child.kill(platform() === "win32" ? "SIGTERM" : "SIGINT");
    await server.save();
    await until(
      () => server.child.exitCode !== null || server.child.signalCode !== null,
      "server exit",
      10000,
    );
    if (platform() !== "win32")
      assert.equal(server.child.exitCode, 0, "graceful process shutdown");
  }
}
try {
  if (!jsOnly) {
    await run("cargo", ["build", "-p", "quiz_host", "--locked"]);
    await prepareRust();
  }
  for (const backend of backends)
    for (const mode of ["restart", "drain"]) await scenario(backend, mode);
  const report = {
    date: new Date().toISOString(),
    platform: platform(),
    node: process.version,
    cpu: cpus()[0]?.model,
    fixture,
    observations,
    note: "One warm edit per backend/mode on this machine; wall time from file write to visible generation. Includes debounce and Rust compilation. Not a throughput benchmark.",
  };
  await writeFile(
    path.join(resultsDir, "results.json"),
    JSON.stringify(report, null, 2) + "\n",
  );
  console.log(
    `PASS: ${observations.length} scenarios. Report: ${path.join(resultsDir, "results.json")}`,
  );
} finally {
  for (const peer of connections) peer.close();
  for (const child of children) child.kill();
  // Keep the short-path fixture and logs for diagnosing any native build failure.
}
