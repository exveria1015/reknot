// Run from any directory. Stop the pair with Ctrl+C.
import { spawn } from "node:child_process";
import { access, mkdir, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = fileURLToPath(new URL("../../", import.meta.url));
const options = Object.fromEntries(
  process.argv.slice(2).map((arg) => {
    const match = arg.match(/^--(mode|rust-port|js-port)=(.+)$/);
    if (!match) throw new Error(`Unknown argument: ${arg}`);
    return [match[1], match[2]];
  }),
);
const mode = options.mode ?? process.env.QUIZ_RELOAD_MODE ?? "restart";
if (!["drain", "restart"].includes(mode))
  throw new Error("Use --mode=restart or --mode=drain");
const ports = {
  rust: options["rust-port"] ?? "4101",
  js: options["js-port"] ?? "4102",
};
for (const port of Object.values(ports))
  if (!/^\d+$/.test(port) || Number(port) > 65535)
    throw new Error("Invalid port");
try {
  await access(
    path.join(root, "examples/quiz_room/js/node_modules/fastify/package.json"),
  );
} catch {
  console.error("First run: npm ci --prefix examples/quiz_room/js");
  process.exit(1);
}
const children = new Set();
let stopping = false;
function stop() {
  if (stopping) return;
  stopping = true;
  for (const child of children) child.kill();
}
process.on("SIGINT", stop);
process.on("SIGTERM", stop);
process.on("exit", stop);
function child(command, args, options = {}) {
  const process = spawn(command, args, {
    cwd: root,
    windowsHide: true,
    ...options,
  });
  children.add(process);
  process.on("exit", () => children.delete(process));
  return process;
}
console.log("Building the Axum host…");
const build = child("cargo", ["build", "-p", "quiz_host", "--locked"], {
  stdio: "inherit",
});
const code = await new Promise((resolve, reject) => {
  build.on("error", reject);
  build.on("exit", resolve);
});
if (code !== 0 || stopping) process.exit(code ?? 1);
// Cargo metadata also respects a user-provided CARGO_TARGET_DIR.
const metadata = child("cargo", [
  "metadata",
  "--no-deps",
  "--format-version",
  "1",
  "--locked",
]);
let metadataText = "";
metadata.stdout.on("data", (data) => {
  metadataText += data;
});
metadata.stderr.pipe(process.stderr);
await new Promise((resolve, reject) => {
  metadata.on("error", reject);
  metadata.on("exit", (code) =>
    code === 0 ? resolve() : reject(new Error("cargo metadata failed")),
  );
});
const target = JSON.parse(metadataText).target_directory;
const actual = {};
const pids = {};
for (const backend of ["rust", "js"]) {
  const command =
    backend === "rust"
      ? path.join(
          target,
          `debug/quiz_host${process.platform === "win32" ? ".exe" : ""}`,
        )
      : process.execPath;
  const server = child(
    command,
    backend === "rust" ? [] : ["examples/quiz_room/js/server.mjs"],
    {
      env: {
        ...process.env,
        QUIZ_RELOAD_MODE: mode,
        QUIZ_PORT: ports[backend],
      },
    },
  );
  pids[backend] = server.pid;
  let buffered = "";
  server.stderr.on("data", (data) =>
    process.stderr.write(`[${backend}] ${data}`),
  );
  server.stdout.on("data", async (data) => {
    process.stdout.write(`[${backend}] ${data}`);
    buffered += data;
    const match = buffered.match(/QUIZ_READY (\{[^\n]+\})/);
    if (match && !actual[backend]) {
      actual[backend] = JSON.parse(match[1]).port;
      if (actual.rust && actual.js) {
        const url = `http://127.0.0.1:${actual.rust}/?rust=${actual.rust}&js=${actual.js}`;
        console.log(
          `\nQuiz Room (${mode}): ${url}\nOpen another tab for a second player. Stop: Ctrl+C\n`,
        );
        await mkdir(path.join(root, "target/quiz-room"), { recursive: true });
        await writeFile(
          path.join(root, "target/quiz-room/session.json"),
          JSON.stringify(
            { url, ports: actual, pids, launcher: process.pid, mode },
            null,
            2,
          ),
        );
      }
    }
  });
  server.on("error", (error) => {
    console.error(error);
    process.exitCode = 1;
    stop();
  });
  server.on("exit", (code) => {
    if (!stopping) {
      console.error(`${backend} exited (${code}); stopping both servers.`);
      process.exitCode = code || 1;
      stop();
    }
  });
}
