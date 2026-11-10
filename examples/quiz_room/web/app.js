"use strict";
const query = new URLSearchParams(location.search);
const statuses = {
  ready: "更新を待っています",
  building: "変更を読み込み中…",
  restarting: "処理を終了して切り替え中…",
  build_failed: "ビルド／構文エラー：直して保存してください",
  rejected: "契約不一致：現在のルールで継続中",
  watch_failed: "ファイル監視エラー",
  worker_failed: "時計ワーカーエラー",
};
const rooms = ["rust", "js"].map((backend, index) => {
  const card = document
    .querySelector("#room-template")
    .content.firstElementChild.cloneNode(true);
  card.classList.add(backend);
  document.querySelector("#rooms").append(card);
  const find = (selector) => card.querySelector(selector);
  find(".stack-number").textContent =
    `0${index + 1} / ${backend === "rust" ? "NATIVE" : "JAVASCRIPT"}`;
  find(".stack-title").textContent =
    backend === "rust" ? "Axum + Reknot" : "Fastify + JS";
  find(".stack-subtitle").textContent =
    backend === "rust"
      ? "型付きAPIで、実行中のRustを更新"
      : "モジュールを更新し、同じ部屋で続行";
  const room = {
    backend,
    card,
    find,
    session: null,
    snapshot: null,
    socket: null,
    nextId: 0,
    pending: new Map(),
    logs: [],
    previousReload: "",
    previousRound: null,
    previousPlayers: "",
  };
  for (let choice = 0; choice < 4; choice++) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "choice";
    button.hidden = true;
    const letter = document.createElement("span");
    letter.textContent = "ABCD"[choice];
    const label = document.createElement("strong");
    label.style.fontWeight = "400";
    button.append(letter, label);
    button.addEventListener("click", () => {
      const action = document.querySelector("#slow").checked
        ? "slow_answer"
        : "answer";
      request(room, action, { round: room.snapshot.round.id, choice });
    });
    find(".choices").append(button);
  }
  find(".reconnect").addEventListener("click", () => connect(room));
  return room;
});
function log(room, text) {
  room.logs.unshift(
    `${new Date().toLocaleTimeString("ja-JP", { hour12: false })}  ${text}`,
  );
  room.logs.length = Math.min(room.logs.length, 12);
  room.find(".events").textContent = room.logs.join("\n");
}
function connected(room) {
  return room.socket?.readyState === WebSocket.OPEN;
}
function mine(room) {
  return room.snapshot?.players.find((p) => p.id === room.session);
}
function result(room, message, ok) {
  room.find(".result").textContent = message;
  room.find(".result").className = `result ${ok ? "success" : "error"}`;
}
function request(room, action, fields = {}) {
  if (!connected(room)) return;
  const id = ++room.nextId;
  room.pending.set(id, action);
  room.socket.send(JSON.stringify({ id, action, ...fields }));
  if (action.includes("answer"))
    result(
      room,
      action === "slow_answer"
        ? "10秒間、回答を処理中… 今の間にルールを保存できます。"
        : "回答を送信中…",
      true,
    );
}
function connect(room) {
  if (room.socket && room.socket.readyState < WebSocket.CLOSING) return;
  // Custom ports are useful for isolated test servers. Hostnames stay local.
  const port =
    query.get(room.backend) ?? (room.backend === "rust" ? "4101" : "4102");
  if (!/^\d+$/.test(port) || Number(port) > 65535 || Number(port) < 1) return;
  room.socket = new WebSocket(`ws://127.0.0.1:${port}/ws`);
  room.session = null;
  room.snapshot = null;
  room.previousRound = null;
  room.previousPlayers = "";
  room.find(".connection").textContent = "接続中…";
  room.find(".reconnect").hidden = true;
  room.socket.addEventListener("open", () => {
    room.find(".connection").textContent = "接続中";
    room.find(".connection").className = "connection online";
    log(room, `接続 :${port}`);
    renderControls();
  });
  room.socket.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data);
    if (message.type === "hello") {
      room.session = message.session;
      room.find(".session").textContent = `#${message.session}`;
    }
    if (message.type === "snapshot") {
      room.snapshot = message;
      render(room);
    }
    if (message.type === "result") {
      const action = room.pending.get(message.id);
      room.pending.delete(message.id);
      if (!message.ok) {
        result(room, message.error, false);
        log(room, message.error);
      } else if (action?.includes("answer")) {
        const v = message.data;
        const text = `${v.correct ? "正解！" : "不正解"} +${v.points} pt · ${v.label} / g${v.generation}`;
        result(room, text, v.correct);
        log(room, text);
      } else {
        log(
          room,
          action === "join" ? "ルームに参加しました" : "次の問題を開始しました",
        );
      }
    }
  });
  room.socket.addEventListener("close", () => {
    room.find(".connection").textContent = "未接続";
    room.find(".connection").className = "connection offline";
    room.find(".reconnect").hidden = false;
    room.find(".reload-status").textContent =
      "サーバーを起動して再接続してください";
    for (const button of room.card.querySelectorAll(".choice"))
      button.disabled = true;
    room.pending.clear();
    log(room, "切断しました。起動後に「再接続」を押してください。");
    renderControls();
  });
  room.socket.addEventListener("error", () => {});
}
function render(room) {
  const s = room.snapshot,
    r = s.round,
    player = mine(room);
  room.find(".mode").textContent = s.mode.toUpperCase();
  room.find(".rule-label").textContent = s.rules?.label ?? "—";
  room.find(".points").textContent = `${s.rules?.correct_points ?? "—"} pt`;
  room.find(".generation").textContent = `g${s.rules?.generation ?? 0}`;
  room.find(".clock-gen").textContent = `g${s.clock.generation}`;
  room.find(".ticks").textContent = s.clock.ticks;
  room.find(".question-number").textContent = r
    ? `QUESTION ${String(r.number).padStart(2, "0")} / ${String(r.total).padStart(2, "0")}`
    : "WAITING ROOM";
  room.find(".question").textContent =
    r?.prompt ??
    (player
      ? "準備OK。問題をはじめましょう。"
      : "参加するとクイズを開始できます。");
  room.find(".explanation").textContent = r?.explanation ?? "";
  if (room.previousRound !== r?.id) {
    room.previousRound = r?.id;
    room.find(".result").textContent = "選択肢を選んで回答してください。";
    room.find(".result").className = "result";
  }
  room.card.querySelectorAll(".choice").forEach((button, i) => {
    button.hidden = !r;
    button.querySelector("strong").textContent = r?.choices[i] ?? "";
    button.disabled =
      !connected(room) ||
      !player ||
      !r?.open ||
      player.answered ||
      player.pending;
    button.classList.toggle("correct", Boolean(r && !r.open && r.answer === i));
  });
  const players = [...s.players].sort(
    (a, b) => b.score - a.score || a.id - b.id,
  );
  const signature = JSON.stringify(players);
  if (room.previousPlayers !== signature) {
    room.previousPlayers = signature;
    const list = room.find(".players");
    list.replaceChildren();
    for (const [i, p] of players.entries()) {
      const row = document.createElement("li");
      for (const [className, text] of [
        ["rank", String(i + 1).padStart(2, "0")],
        [
          "player-name",
          `${p.name}${p.id === room.session ? "（あなた）" : ""}`,
        ],
        ["state", p.pending ? "採点中" : p.answered ? "回答済み" : ""],
        ["", `${p.score} pt`],
      ]) {
        const item = document.createElement(className ? "span" : "b");
        item.className = className;
        item.textContent = text;
        row.append(item);
      }
      list.append(row);
    }
    if (!players.length) {
      const li = document.createElement("li");
      li.className = "empty";
      li.textContent = "まだ参加者はいません。最初のプレイヤーになろう。";
      list.append(li);
    }
  }
  room.find(".player-count").textContent = `${players.length} PLAYERS`;
  room.find(".reload-status").textContent =
    statuses[s.reload.status] ?? s.reload.status;
  const reloadKey = JSON.stringify(s.reload);
  if (room.previousReload !== reloadKey) {
    room.previousReload = reloadKey;
    log(room, s.reload.message || statuses[s.reload.status]);
  }
  renderControls();
}
function renderControls() {
  const joinable = rooms.some((r) => connected(r) && !mine(r));
  document.querySelector("#join").disabled = !joinable;
  const playable = rooms.some(
    (r) => connected(r) && mine(r) && !r.snapshot?.round?.open,
  );
  document.querySelector("#next").disabled = !playable;
  document.querySelector("#next").firstChild.textContent = rooms.some(
    (r) => r.snapshot?.round,
  )
    ? "次の問題へ "
    : "問題をはじめる ";
  const count = rooms.filter((r) => connected(r) && mine(r)).length;
  document.querySelector("#notice").textContent = count
    ? `${count}つの部屋に参加中。同じ操作でも、得点と進行はそれぞれの部屋に保存されます。`
    : "まずは名前を入力して参加してください。";
}
document.querySelector("#join-form").addEventListener("submit", (event) => {
  event.preventDefault();
  for (const room of rooms)
    if (!mine(room))
      request(room, "join", { name: document.querySelector("#name").value });
});
document.querySelector("#next").addEventListener("click", () => {
  for (const room of rooms)
    if (mine(room) && !room.snapshot?.round?.open) request(room, "next");
});
setInterval(() => {
  for (const room of rooms) {
    const r = room.snapshot?.round;
    room.find(".timer").textContent = r
      ? r.open
        ? `${Math.max(0, Math.ceil((r.deadline - Date.now()) / 1000))} s`
        : "ANSWER"
      : "—";
  }
}, 100);
rooms.forEach(connect);
renderControls();
