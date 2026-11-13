//! Real Send Axum handlers communicate with the LocalSet DLL owner via a bridge.
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use quiz_protocol::*;
use reknot::typed::{BridgeOptions, DevModule, DevOptions, HandlerClient, ReloadEvent, ReloadMode};
use reknot::{
    CancellationToken, HostResource, HostResources, HostServiceFuture, ServiceError, WorkerId,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{watch, Semaphore};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
#[derive(Deserialize)]
struct Question {
    prompt: String,
    choices: Vec<String>,
    answer: u8,
    explanation: String,
}
#[derive(Serialize)]
struct Player {
    id: u64,
    name: String,
    score: u64,
    answered: bool,
    pending: bool,
}
struct Round {
    id: u64,
    question: usize,
    deadline: u64,
    open: bool,
}
struct Room {
    mode: String,
    players: BTreeMap<u64, Player>,
    next_session: u64,
    round: Option<Round>,
    questions: Vec<Question>,
    rules: Option<RuleInfo>,
    clock: ClockPulse,
    reload: Value,
    changed: watch::Sender<u64>,
}
impl Room {
    fn notify(&self) {
        self.changed.send_modify(|n| *n += 1);
    }
    fn expire(&mut self) {
        if let Some(round) = &mut self.round {
            if round.open && now() >= round.deadline {
                round.open = false;
            }
        }
    }
    fn snapshot(&self) -> Value {
        let round = self.round.as_ref().map(|r| {
            let q = &self.questions[r.question];
            json!({"id":r.id, "number":r.question+1, "total":self.questions.len(),
                "prompt":q.prompt, "choices":q.choices, "deadline":r.deadline,
                "open":r.open, "answer":if r.open {None} else {Some(q.answer)},
                "explanation":if r.open {""} else {&q.explanation}})
        });
        json!({"type":"snapshot", "backend":"rust", "mode":self.mode,
            "server_time":now(), "rules":self.rules, "clock":self.clock,
            "reload":self.reload, "players":self.players.values().collect::<Vec<_>>(), "round":round})
    }
}
type SharedRoom = Arc<Mutex<Room>>;
struct Services(SharedRoom);
impl HostResource for Services {
    fn call(&mut self, operation: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        if operation != PULSE {
            return Err(ServiceError::UNSUPPORTED_OPERATION);
        }
        let pulse = serde_json::from_slice(input).map_err(|_| ServiceError::INVALID_ARGUMENT)?;
        let mut room = self.0.lock().unwrap();
        room.clock = pulse;
        room.expire();
        room.notify();
        Ok(0)
    }
    fn start_async(
        &mut self,
        operation: u32,
        input: &[u8],
        cancel: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        if operation != SLEEP {
            return Err(ServiceError::UNSUPPORTED_OPERATION);
        }
        let ms = u64::from_le_bytes(
            input
                .try_into()
                .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
        );
        if ms > 30000 {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        Ok(Box::pin(async move {
            tokio::select! {
                _ = cancel.cancelled() => Err(ServiceError::CANCELLED),
                _ = tokio::time::sleep(Duration::from_millis(ms)) => Ok(Vec::new()),
            }
        }))
    }
}
#[derive(Clone)]
struct App {
    room: SharedRoom,
    info: HandlerClient<(), RuleInfo>,
    answer: HandlerClient<AnswerInput, Result<Verdict, String>>,
    slots: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    id: u64,
    action: String,
    name: Option<String>,
    round: Option<u64>,
    choice: Option<u8>,
}
// Also clears reservations when a socket closes and its future is dropped.
struct Reservation {
    room: SharedRoom,
    session: u64,
    round: u64,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut room = self.room.lock().unwrap();
        if room.round.as_ref().is_some_and(|r| r.id == self.round) {
            if let Some(player) = room.players.get_mut(&self.session) {
                player.pending = false;
            }
            room.notify();
        }
    }
}
async fn command(app: &App, session: u64, cmd: Command) -> Result<Value, String> {
    match cmd.action.as_str() {
        "join" => {
            let name = cmd.name.unwrap_or_default().trim().to_owned();
            if name.is_empty() || name.chars().count() > 20 {
                return Err("名前は1〜20文字で入力してください。".into());
            }
            let mut room = app.room.lock().unwrap();
            if room.players.contains_key(&session) {
                return Err("すでに参加しています。".into());
            }
            room.players.insert(
                session,
                Player {
                    id: session,
                    name,
                    score: 0,
                    answered: false,
                    pending: false,
                },
            );
            room.notify();
            Ok(json!({"session":session}))
        }
        "start" | "next" => {
            let info = app.info.call(()).await.map_err(|e| e.to_string())?;
            let mut room = app.room.lock().unwrap();
            room.expire();
            if !room.players.contains_key(&session) {
                return Err("先に参加してください。".into());
            }
            if room.round.as_ref().is_some_and(|r| r.open) {
                return Err("回答を待っています。".into());
            }
            let id = room.round.as_ref().map_or(1, |r| r.id + 1);
            let question = (id as usize - 1) % room.questions.len();
            room.round = Some(Round {
                id,
                question,
                deadline: now() + info.round_ms,
                open: true,
            });
            room.rules = Some(info);
            for player in room.players.values_mut() {
                player.answered = false;
                player.pending = false;
            }
            room.notify();
            Ok(json!({"round":id}))
        }
        "answer" | "slow_answer" => {
            let round_id = cmd.round.ok_or("問題番号が必要です。")?;
            let choice = cmd.choice.filter(|&c| c < 4).ok_or("選択肢が不正です。")?;
            let correct = {
                let mut room = app.room.lock().unwrap();
                room.expire();
                let round = room.round.as_ref().ok_or("問題が始まっていません。")?;
                if round.id != round_id || !round.open {
                    return Err("この問題の受付は終了しました。".into());
                }
                let correct = room.questions[round.question].answer;
                let player = room
                    .players
                    .get_mut(&session)
                    .ok_or("先に参加してください。")?;
                if player.answered || player.pending {
                    return Err("この問題には回答済みです。".into());
                }
                player.pending = true;
                room.notify();
                correct
            };
            let _reservation = Reservation {
                room: app.room.clone(),
                session,
                round: round_id,
            };
            let verdict = app
                .answer
                .call(AnswerInput {
                    correct,
                    choice,
                    delay_ms: if cmd.action == "slow_answer" {
                        10000
                    } else {
                        0
                    },
                })
                .await
                .map_err(|e| format!("回答を中止しました（再送なし）: {e}"))??;
            let mut room = app.room.lock().unwrap();
            room.expire();
            if !room
                .round
                .as_ref()
                .is_some_and(|r| r.id == round_id && r.open)
            {
                return Err("採点中に受付が終了しました。加点していません。".into());
            }
            let player = room
                .players
                .get_mut(&session)
                .ok_or("接続が閉じられました。")?;
            player.score += u64::from(verdict.points);
            player.answered = true;
            player.pending = false;
            if room.players.values().all(|p| p.answered) {
                room.round.as_mut().unwrap().open = false;
            }
            room.notify();
            Ok(serde_json::to_value(verdict).unwrap())
        }
        _ => Err("不明な操作です。".into()),
    }
}
async fn upgrade(State(app): State<App>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    // Allow the two local comparison pages. Reject cross-site browser sockets.
    if let Some(origin) = headers.get(header::ORIGIN) {
        let valid = origin
            .to_str()
            .ok()
            .and_then(|s| s.strip_prefix("http://"))
            .is_some_and(|s| {
                s.strip_prefix("127.0.0.1:")
                    .or_else(|| s.strip_prefix("localhost:"))
                    .is_some_and(|p| p.parse::<u16>().is_ok())
            });
        if !valid {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let Ok(permit) = app.slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            connection(app, socket).await;
        })
}
async fn send(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    value: Value,
) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            sink.send(Message::Text(value.to_string().into()))
        )
        .await,
        Ok(Ok(()))
    )
}
async fn connection(app: App, socket: WebSocket) {
    let (session, mut changes) = {
        let mut room = app.room.lock().unwrap();
        room.next_session += 1;
        (room.next_session, room.changed.subscribe())
    };
    let (mut sink, mut stream) = socket.split();
    let mut shutdown = app.shutdown.clone();
    let mut jobs = tokio::task::JoinSet::new();
    let initial = app.room.lock().unwrap().snapshot();
    if !send(&mut sink, json!({"type":"hello", "session":session})).await
        || !send(&mut sink, initial).await
    {
        return;
    }
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = changes.changed() => {
                let snapshot = app.room.lock().unwrap().snapshot();
                if !send(&mut sink, snapshot).await { break; }
            }
            Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                if let Ok(result) = result { if !send(&mut sink, result).await { break; } }
            }
            message = stream.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let result = serde_json::from_str::<Command>(&text);
                        match result {
                            Ok(cmd) if jobs.len() < 4 => {
                                let app = app.clone();
                                jobs.spawn(async move {
                                    let id = cmd.id;
                                    match command(&app, session, cmd).await {
                                        Ok(data) => json!({"type":"result", "id":id, "ok":true, "data":data}),
                                        Err(error) => json!({"type":"result", "id":id, "ok":false, "error":error}),
                                    }
                                });
                            }
                            Ok(cmd) => { if !send(&mut sink, json!({"type":"result", "id":cmd.id, "ok":false, "error":"処理中の操作が多すぎます。"})).await { break; } }
                            Err(_) => { if !send(&mut sink, json!({"type":"result", "id":null, "ok":false, "error":"メッセージ形式が不正です。"})).await { break; } }
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
        }
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    let mut room = app.room.lock().unwrap();
    room.players.remove(&session);
    room.notify();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::task::LocalSet::new().run_until(run()).await
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::var("QUIZ_RELOAD_MODE").unwrap_or_else(|_| "restart".into());
    let reload_mode = match mode.as_str() {
        "restart" => ReloadMode::Restart,
        "drain" => ReloadMode::Drain,
        _ => return Err("QUIZ_RELOAD_MODE must be restart or drain".into()),
    };
    let (changed, _) = watch::channel(0);
    let room = Arc::new(Mutex::new(Room {
        mode,
        players: BTreeMap::new(),
        next_session: 0,
        round: None,
        questions: serde_json::from_str(include_str!("../../quiz_room/questions.json"))?,
        rules: None,
        clock: ClockPulse::default(),
        reload: json!({"status":"ready", "message":""}),
        changed,
    }));
    let resources = HostResources::new();
    let service = resources.register(SERVICE, Services(room.clone()));
    let manifest = std::env::var_os("QUIZ_LOGIC_MANIFEST")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../quiz_logic/Cargo.toml")
        });
    eprintln!("Preparing quiz_logic; initial Cargo build may take a while...");
    // SAFETY: this local project, future edits and dependencies are trusted native
    // code. Socket payloads cannot choose code paths or library locations.
    let mut development = unsafe {
        DevModule::start(
            module()
                .builder()
                .resources(resources)
                .bind(1, service.id()),
            DevOptions::new(manifest, "quiz_logic").with_reload_mode(reload_mode),
        )
    }?;
    let host = development.host().clone();
    host.start_worker(WorkerId(1), CLOCK, (), STATE, &ClockState::default())?;
    let initial_info = host.handler(INFO)?.call(()).await?;
    room.lock().unwrap().rules = Some(initial_info);
    let (client, mut driver) = host.bridge(BridgeOptions {
        capacity: 128,
        max_in_flight: 32,
    })?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let app = App {
        room: room.clone(),
        info: client.handler(INFO)?,
        answer: client.handler(ANSWER)?,
        slots: Arc::new(Semaphore::new(32)),
        shutdown: shutdown_rx.clone(),
    };
    let router = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../../quiz_room/web/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../../quiz_room/web/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../../quiz_room/web/style.css"),
                )
            }),
        )
        .route(
            "/api/status",
            get(|State(app): State<App>| async move { Json(app.room.lock().unwrap().snapshot()) }),
        )
        .route("/ws", any(upgrade))
        .with_state(app);
    let port: u16 = std::env::var("QUIZ_PORT")
        .unwrap_or_else(|_| "4101".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    println!(
        "QUIZ_READY {}",
        json!({"backend":"rust", "port":listener.local_addr()?.port()})
    );
    let mut stop = shutdown_rx;
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = stop.changed().await;
            })
            .await
    });
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            _ = &mut signal => break,
            _ = &mut driver => break,
            _ = tick.tick() => {
                if let Some(event) = development.update() {
                    eprintln!("{event}");
                    let status = match &event {
                        ReloadEvent::Building | ReloadEvent::Superseded => "building",
                        ReloadEvent::Restarting => "restarting",
                        ReloadEvent::Reloaded { .. } => "ready",
                        ReloadEvent::BuildFailed { .. } => "build_failed",
                        ReloadEvent::Rejected { .. } => "rejected",
                        ReloadEvent::WatchFailed { .. } => "watch_failed",
                    };
                    let info = if matches!(event, ReloadEvent::Reloaded { .. }) { Some(host.handler(INFO)?.call(()).await?) } else { None };
                    let mut room = room.lock().unwrap();
                    if let Some(info) = info { room.rules = Some(info); }
                    room.reload = json!({"status":status, "message":event.to_string()});
                    room.notify();
                }
            }
        }
    }
    shutdown_tx.send_replace(true);
    let (_, result) = tokio::join!(driver, client.shutdown());
    result?;
    // Stop watching before joining workers so a pending edit cannot recreate one
    // during shutdown. Bridge requests have already joined above.
    drop(development);
    host.stop_worker(WorkerId(1))?;
    std::future::poll_fn(|cx| host.poll_workers(cx)).await;
    std::future::poll_fn(|cx| host.poll_cleanup(cx)).await;
    server.await??;
    Ok(())
}
