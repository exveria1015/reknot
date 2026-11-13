import Fastify from "fastify";
import websocket from "@fastify/websocket";
import { readFileSync, watch } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL("../", import.meta.url));
const logicPath = path.resolve(
  process.env.QUIZ_LOGIC_PATH ?? path.join(root, "js/logic.cjs"),
);
const mode = process.env.QUIZ_RELOAD_MODE ?? "restart";
if (!["restart", "drain"].includes(mode))
  throw new Error("QUIZ_RELOAD_MODE must be restart or drain");
const questions = JSON.parse(
  readFileSync(path.join(root, "questions.json"), "utf8"),
);
const expected = {
  version: 1,
  state: { ticks: "nonnegative-integer" },
  answer: { correct: "choice", choice: "choice", delay_ms: "milliseconds" },
};
const sockets = new Set();
const players = new Map();
let nextSession = 0,
  round = null,
  generation = 1;
let reload = { status: "ready", message: "" };
let clock = { ticks: 0, generation: 0, label: "" };
let checkpoint = { ticks: 0 };
let active,
  rules,
  worker,
  paused = null,
  releaseAdmission;
const calls = new Set();
let shuttingDown = false;

function validate(candidate) {
  if (JSON.stringify(candidate.contract) !== JSON.stringify(expected))
    throw new Error(
      `Contract mismatch: expected ${JSON.stringify(expected)}; actual ${JSON.stringify(candidate.contract)}`,
    );
  for (const key of ["info", "answer", "clock"])
    if (typeof candidate[key] !== "function")
      throw new Error(`Missing handler: ${key}`);
  const info = candidate.info(generation);
  if (
    typeof info.label !== "string" ||
    !Number.isSafeInteger(info.correct_points) ||
    info.correct_points < 0 ||
    !Number.isSafeInteger(info.round_ms) ||
    info.round_ms < 1000 ||
    info.round_ms > 300000 ||
    !Number.isSafeInteger(info.tick_ms) ||
    info.tick_ms < 10 ||
    info.tick_ms > 30000
  )
    throw new Error("Invalid rule configuration");
}
active = require(logicPath);
validate(active);
rules = active.info(generation);

function expire() {
  if (round?.open && Date.now() >= round.deadline) round.open = false;
}
function snapshot() {
  const q = round && questions[round.question];
  return {
    type: "snapshot",
    backend: "js",
    mode,
    server_time: Date.now(),
    rules,
    clock,
    reload,
    players: [...players.values()],
    round: round && {
      id: round.id,
      number: round.question + 1,
      total: questions.length,
      prompt: q.prompt,
      choices: q.choices,
      deadline: round.deadline,
      open: round.open,
      answer: round.open ? null : q.answer,
      explanation: round.open ? "" : q.explanation,
    },
  };
}
function send(socket, value) {
  if (socket.readyState !== 1) return;
  if (socket.bufferedAmount > 65536) {
    socket.terminate();
    return;
  }
  socket.send(JSON.stringify(value));
}
function broadcast() {
  const value = snapshot();
  for (const socket of sockets) send(socket, value);
}
function status(value, message = "") {
  reload = { status: value, message };
  broadcast();
}
function context(signal, admittedGeneration) {
  return {
    signal,
    generation: admittedGeneration,
    sleep: (milliseconds) => {
      if (
        !Number.isSafeInteger(milliseconds) ||
        milliseconds < 0 ||
        milliseconds > 30000
      )
        throw new Error("Invalid sleep");
      return sleep(milliseconds, undefined, { signal });
    },
  };
}
function startClock() {
  const controller = new AbortController();
  const implementation = active;
  const ctx = {
    ...context(controller.signal, generation),
    state: checkpoint,
    publish: (pulse) => {
      clock = pulse;
      expire();
      broadcast();
    },
  };
  const done = implementation.clock(ctx).catch((error) => {
    if (!controller.signal.aborted) status("worker_failed", error.message);
  });
  worker = { controller, done };
}
async function admission(signal) {
  if (paused) {
    // Per-socket requests are bounded; also remove cancellation listeners.
    await new Promise((resolve, reject) => {
      const abort = () => reject(new Error("回答を中止しました（再送なし）。"));
      signal.addEventListener("abort", abort, { once: true });
      paused
        .then(resolve)
        .finally(() => signal.removeEventListener("abort", abort));
      if (signal.aborted) abort();
    });
  }
  signal.throwIfAborted();
  if (shuttingDown) throw new Error("Server is closing");
}
async function score(input, socketSignal) {
  await admission(socketSignal);
  const controller = new AbortController();
  const abort = () => controller.abort();
  socketSignal.addEventListener("abort", abort, { once: true });
  const implementation = active,
    admittedGeneration = generation;
  const job = { controller, done: null };
  job.done = (async () =>
    implementation.answer(
      input,
      context(controller.signal, admittedGeneration),
    ))();
  calls.add(job);
  try {
    return await job.done;
  } catch (error) {
    if (controller.signal.aborted)
      throw new Error("更新により回答を中止しました。再送はしていません。");
    throw error;
  } finally {
    calls.delete(job);
    socketSignal.removeEventListener("abort", abort);
  }
}

async function reloadLogic() {
  if (shuttingDown) return;
  status("building");
  const oldCache = require.cache[logicPath];
  let candidate;
  delete require.cache[logicPath];
  try {
    candidate = require(logicPath);
  } catch (error) {
    require.cache[logicPath] = oldCache;
    status("build_failed", error.message);
    return;
  }
  try {
    validate(candidate);
  } catch (error) {
    require.cache[logicPath] = oldCache;
    status("rejected", error.message);
    return;
  }
  if (mode === "restart") {
    paused = new Promise((resolve) => {
      releaseAdmission = resolve;
    });
    status("restarting");
    const pending = [...calls];
    for (const job of pending) job.controller.abort();
    worker.controller.abort();
    await Promise.allSettled([...pending.map((job) => job.done), worker.done]);
    if (shuttingDown) {
      releaseAdmission();
      paused = null;
      return;
    }
    checkpoint = structuredClone(checkpoint);
  }
  active = candidate;
  generation++;
  rules = active.info(generation);
  if (mode === "restart") {
    startClock();
    releaseAdmission();
    paused = null;
  }
  status("ready", `New events use generation ${generation}`);
}
let timer,
  reloading = false,
  dirty = false;
async function flush() {
  if (reloading) {
    dirty = true;
    return;
  }
  reloading = true;
  try {
    do {
      dirty = false;
      await reloadLogic();
    } while (dirty && !shuttingDown);
  } catch (error) {
    status("rejected", error.message);
  } finally {
    reloading = false;
  }
}
const watcher = watch(path.dirname(logicPath), (_, filename) => {
  if (filename && filename.toString() !== path.basename(logicPath)) return;
  clearTimeout(timer);
  timer = setTimeout(flush, 200);
});
watcher.on("error", (error) => status("watch_failed", error.message));

function parse(text) {
  const cmd = JSON.parse(text);
  if (
    !cmd ||
    Array.isArray(cmd) ||
    !Number.isSafeInteger(cmd.id) ||
    cmd.id < 0 ||
    typeof cmd.action !== "string" ||
    Object.keys(cmd).some(
      (k) => !["id", "action", "name", "round", "choice"].includes(k),
    ) ||
    (cmd.name != null && typeof cmd.name !== "string") ||
    (cmd.round != null &&
      (!Number.isSafeInteger(cmd.round) || cmd.round < 0)) ||
    (cmd.choice != null &&
      (!Number.isInteger(cmd.choice) || cmd.choice < 0 || cmd.choice > 255))
  )
    throw new Error("メッセージ形式が不正です。");
  return cmd;
}
async function command(session, cmd, signal) {
  switch (cmd.action) {
    case "join": {
      const name = (cmd.name ?? "").trim();
      if (!name || [...name].length > 20)
        throw new Error("名前は1〜20文字で入力してください。");
      if (players.has(session)) throw new Error("すでに参加しています。");
      players.set(session, {
        id: session,
        name,
        score: 0,
        answered: false,
        pending: false,
      });
      broadcast();
      return { session };
    }
    case "start":
    case "next": {
      await admission(signal);
      expire();
      if (!players.has(session)) throw new Error("先に参加してください。");
      if (round?.open) throw new Error("回答を待っています。");
      const id = (round?.id ?? 0) + 1;
      rules = active.info(generation);
      round = {
        id,
        question: (id - 1) % questions.length,
        deadline: Date.now() + rules.round_ms,
        open: true,
      };
      for (const player of players.values()) {
        player.answered = false;
        player.pending = false;
      }
      broadcast();
      return { round: id };
    }
    case "answer":
    case "slow_answer": {
      expire();
      if (!Number.isInteger(cmd.choice) || cmd.choice < 0 || cmd.choice > 3)
        throw new Error("選択肢が不正です。");
      if (!round?.open || round.id !== cmd.round)
        throw new Error("この問題の受付は終了しました。");
      const player = players.get(session);
      if (!player) throw new Error("先に参加してください。");
      if (player.answered || player.pending)
        throw new Error("この問題には回答済みです。");
      player.pending = true;
      broadcast();
      try {
        const verdict = await score(
          {
            correct: questions[round.question].answer,
            choice: cmd.choice,
            delay_ms: cmd.action === "slow_answer" ? 10000 : 0,
          },
          signal,
        );
        expire();
        if (!round.open || round.id !== cmd.round)
          throw new Error("採点中に受付が終了しました。加点していません。");
        signal.throwIfAborted();
        player.score += verdict.points;
        player.answered = true;
        if ([...players.values()].every((p) => p.answered)) round.open = false;
        return verdict;
      } finally {
        if (round?.id === cmd.round) player.pending = false;
        broadcast();
      }
    }
    default:
      throw new Error("不明な操作です。");
  }
}

const app = Fastify({ logger: false });
await app.register(websocket, { options: { maxPayload: 4096 } });
for (const [route, file, type] of [
  ["/", "index.html", "text/html"],
  ["/app.js", "app.js", "text/javascript"],
  ["/style.css", "style.css", "text/css"],
]) {
  const content = readFileSync(path.join(root, "web", file));
  app.get(route, (_, reply) =>
    reply.type(`${type}; charset=utf-8`).send(content),
  );
}
app.get("/api/status", () => snapshot());
app.get(
  "/ws",
  {
    websocket: true,
    preValidation: (request, reply, done) => {
      const origin = request.headers.origin;
      if (origin && !/^http:\/\/(127\.0\.0\.1|localhost):\d+$/.test(origin)) {
        reply.code(403).send();
        return;
      }
      if (sockets.size >= 32) {
        reply.code(503).send();
        return;
      }
      done();
    },
  },
  (socket) => {
    const session = ++nextSession;
    const controller = new AbortController();
    let outstanding = 0;
    sockets.add(socket);
    // Register listeners synchronously: no message can arrive before admission.
    socket.on("message", async (data, binary) => {
      let cmd;
      try {
        if (binary) throw new Error();
        cmd = parse(data.toString());
      } catch {
        send(socket, {
          type: "result",
          id: null,
          ok: false,
          error: "メッセージ形式が不正です。",
        });
        return;
      }
      if (outstanding >= 4) {
        send(socket, {
          type: "result",
          id: cmd.id,
          ok: false,
          error: "処理中の操作が多すぎます。",
        });
        return;
      }
      outstanding++;
      try {
        send(socket, {
          type: "result",
          id: cmd.id,
          ok: true,
          data: await command(session, cmd, controller.signal),
        });
      } catch (error) {
        send(socket, {
          type: "result",
          id: cmd.id,
          ok: false,
          error: error.message,
        });
      } finally {
        outstanding--;
      }
    });
    socket.on("error", () => socket.terminate());
    socket.on("close", () => {
      controller.abort();
      sockets.delete(socket);
      players.delete(session);
      broadcast();
    });
    send(socket, { type: "hello", session });
    send(socket, snapshot());
  },
);
startClock();
await app.listen({
  host: "127.0.0.1",
  port: Number(process.env.QUIZ_PORT ?? 4102),
});
console.log(
  `QUIZ_READY ${JSON.stringify({ backend: "js", port: app.server.address().port })}`,
);
async function close() {
  if (shuttingDown) return;
  shuttingDown = true;
  watcher.close();
  clearTimeout(timer);
  for (const socket of sockets) socket.terminate();
  for (const job of calls) job.controller.abort();
  worker.controller.abort();
  await Promise.allSettled([worker.done, ...[...calls].map((job) => job.done)]);
  await app.close();
}
process.on("SIGINT", close);
process.on("SIGTERM", close);
