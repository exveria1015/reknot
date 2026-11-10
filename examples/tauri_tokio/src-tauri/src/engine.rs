use reknot::{
    Checkpoint, HostResource, HostResources, RequestRouter, RequestTask, RouterOptions,
    ServiceError, ServiceKey, WorkerId,
};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Child, Command as Process, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

const VIEW: ServiceKey = ServiceKey::new(0x4755_4901, 1);
const COUNTER: ServiceKey = ServiceKey::new(0x434f_554e, 1);
type Shared = Arc<Mutex<Dashboard>>;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Frame {
    pub kind: String,
    pub generation: u64,
    pub label: String,
    pub accent: String,
    pub counter: u64,
    pub step: u64,
    pub text: String,
    pub stopped: bool,
}
#[derive(Clone, Serialize)]
pub struct Dashboard {
    pub status: String,
    pub building: bool,
    pub accepting: bool,
    pub generation: u64,
    pub worker: Frame,
    pub checkpoint: u64,
    pub budget_used: usize,
    pub budget_limit: usize,
    pub in_flight: usize,
    pub uptime_ms: u64,
    pub host_id: u32,
    pub mode: String,
    pub replies: Vec<Frame>,
    pub history: Vec<u64>,
    pub logs: Vec<String>,
    pub build_error: String,
    pub source_path: String,
}
pub enum Command {
    Build,
    Mode(String),
    Request { text: String, slow: bool },
    Resume,
}
pub struct Controller {
    tx: mpsc::SyncSender<Command>,
    receiver: Mutex<Option<mpsc::Receiver<Command>>>,
    shared: Shared,
    path: PathBuf,
    pub initial_source: String,
    pub stop: Arc<AtomicBool>,
}
impl Controller {
    pub fn new() -> Result<Self, String> {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tauri_tokio_logic/src/lib.rs");
        Self::from_path(path)
    }
    fn from_path(path: PathBuf) -> Result<Self, String> {
        let path = path.canonicalize().map_err(|e| e.to_string())?;
        let initial_source = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let (tx, receiver) = mpsc::sync_channel(32);
        Ok(Self {
            tx,
            receiver: Mutex::new(Some(receiver)),
            path: path.clone(),
            initial_source,
            stop: Arc::new(AtomicBool::new(false)),
            shared: Arc::new(Mutex::new(Dashboard {
                status: "Starting up".into(),
                building: true,
                accepting: false,
                generation: 0,
                worker: Frame::default(),
                checkpoint: 0,
                budget_used: 0,
                budget_limit: 32,
                in_flight: 0,
                uptime_ms: 0,
                host_id: std::process::id(),
                mode: "restart".into(),
                replies: Vec::new(),
                history: Vec::new(),
                logs: Vec::new(),
                build_error: String::new(),
                source_path: path.display().to_string(),
            })),
        })
    }
    pub fn snapshot(&self) -> Dashboard {
        self.shared.lock().unwrap().clone()
    }
    pub fn read_source(&self) -> Result<String, String> {
        std::fs::read_to_string(&self.path).map_err(|e| e.to_string())
    }
    pub fn save_source(&self, source: String) -> Result<(), String> {
        if source.len() > 65_536 {
            return Err("Source must be no larger than 64 KiB".into());
        }
        std::fs::write(&self.path, source).map_err(|e| e.to_string())?;
        self.send(Command::Build)
    }
    pub fn send(&self, command: Command) -> Result<(), String> {
        self.tx
            .try_send(command)
            .map_err(|_| "The command queue is busy. Please try again shortly.".into())
    }
    pub fn spawn(&self) -> std::thread::JoinHandle<()> {
        let receiver = self.receiver.lock().unwrap().take().unwrap();
        let path = self.path.clone();
        let shared = self.shared.clone();
        let stop = self.stop.clone();
        std::thread::spawn(move || {
            if let Err(error) = run(path, &shared, &stop, receiver) {
                let mut state = shared.lock().unwrap();
                state.status = "Could not start the sample".into();
                state.building = false;
                state.build_error = error.to_string();
            }
        })
    }
}

struct View {
    shared: Shared,
    start: Instant,
}
impl HostResource for View {
    fn call(
        &mut self,
        operation: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, ServiceError> {
        match operation {
            0 => {
                if output.len() < 8 {
                    return Err(ServiceError::BUFFER_TOO_SMALL);
                }
                output[..8]
                    .copy_from_slice(&(self.start.elapsed().as_millis() as u64).to_le_bytes());
                Ok(8)
            }
            1 => {
                if input.len() > 8192 {
                    return Err(ServiceError::INVALID_ARGUMENT);
                }
                let frame: Frame =
                    serde_json::from_slice(input).map_err(|_| ServiceError::INVALID_ARGUMENT)?;
                let mut state = self.shared.lock().unwrap();
                if frame.kind == "worker" {
                    state.history.push(frame.counter);
                    if state.history.len() > 28 {
                        state.history.remove(0);
                    }
                    state.worker = frame;
                } else {
                    state.replies.insert(0, frame);
                    state.replies.truncate(6);
                }
                Ok(0)
            }
            _ => Err(ServiceError::UNSUPPORTED_OPERATION),
        }
    }
}
fn log(shared: &Shared, text: impl Into<String>) {
    let mut state = shared.lock().unwrap();
    let time = state.uptime_ms as f64 / 1000.;
    state
        .logs
        .insert(0, format!("{time:06.1}s  {}", text.into()));
    state.logs.truncate(60);
}
struct Build {
    child: Child,
    log: PathBuf,
}
fn build(logic: &std::path::Path, target: &std::path::Path) -> std::io::Result<Build> {
    std::fs::create_dir_all(target)?;
    let log = target.join("gui-build.log");
    let output = File::create(&log)?;
    let mut command = Process::new(option_env!("CARGO").unwrap_or("cargo"));
    command
        .args(["build", "--lib", "-p", "tauri_tokio_logic", "--jobs", "1"])
        .arg("--target-dir")
        .arg(target)
        .current_dir(logic)
        .env("CARGO_INCREMENTAL", "0")
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // Background compiler, no console window.
    }
    Ok(Build {
        child: command.spawn()?,
        log,
    })
}
fn poll_requests(requests: &mut Vec<RequestTask>, shared: &Shared, cx: &mut Context<'_>) {
    requests.retain_mut(|task| match Pin::new(&mut *task).poll(cx) {
        Poll::Pending => true,
        Poll::Ready(status) => {
            log(
                shared,
                format!("request / gen {} / {status:?}", task.generation()),
            );
            false
        }
    });
}

fn run(
    path: PathBuf,
    shared: &Shared,
    stop: &AtomicBool,
    receiver: mpsc::Receiver<Command>,
) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let logic = path.parent().unwrap().parent().unwrap();
    let target = reknot::development::default_target_dir(logic);
    let library = reknot::development::library_path_in(&target, "tauri_tokio_logic");
    let options = RouterOptions::default();
    let host = HostResources::new();
    let view = host.register(
        VIEW,
        View {
            shared: shared.clone(),
            start,
        },
    );
    let mut watcher = reknot::development::start_source_watcher(logic);
    let mut router: Option<RequestRouter> = None;
    let mut requests = Vec::new();
    let mut job: Option<Build> = None;
    let mut dirty = Some(Instant::now() - Duration::from_secs(1));
    let mut last_source = String::new();
    let mut mode = "restart".to_string();
    let mut cx = Context::from_waker(Waker::noop());
    while !stop.load(Ordering::Acquire) {
        shared.lock().unwrap().uptime_ms = start.elapsed().as_millis() as u64;
        for command in receiver.try_iter() {
            match command {
                Command::Build => dirty = Some(Instant::now()),
                Command::Mode(value) => {
                    mode = value;
                    shared.lock().unwrap().mode = mode.clone();
                }
                Command::Resume => {
                    if let Some(router) = router.as_mut() {
                        router.resume()?;
                        log(shared, "Request admission resumed");
                        shared.lock().unwrap().status = "Running".into();
                    }
                }
                Command::Request { text, slow } => {
                    if let Some(router) = router.as_ref() {
                        if requests.len() >= 8 {
                            log(
                                shared,
                                "Up to 8 requests can run at once. Wait for one to finish.",
                            );
                            continue;
                        }
                        let bytes =
                            serde_json::to_vec(&serde_json::json!({"text":text, "slow":slow}))?;
                        match router.dispatch(view.id(), &bytes) {
                            Ok(task) => {
                                log(
                                    shared,
                                    format!(
                                        "admitted / gen {}{}",
                                        task.generation(),
                                        if slow { " / 5 seconds" } else { "" }
                                    ),
                                );
                                requests.push(task);
                            }
                            Err(error) => log(shared, error.to_string()),
                        }
                    }
                }
            }
        }
        if reknot::development::source_changed(&mut watcher)
            && std::fs::read_to_string(&path).unwrap_or_default() != last_source
        {
            dirty = Some(Instant::now());
        }
        if job.is_none() && dirty.is_some_and(|time| time.elapsed() >= Duration::from_millis(350)) {
            dirty = None;
            last_source = std::fs::read_to_string(&path)?;
            job = Some(build(logic, &target)?);
            let mut state = shared.lock().unwrap();
            state.status = if router.is_some() {
                "Building — old logic is running"
            } else {
                "Building initial logic"
            }
            .into();
            state.building = true;
            state.build_error.clear();
            drop(state);
            log(shared, "Started cargo build in the background");
        }
        let finished = if let Some(job) = job.as_mut() {
            job.child.try_wait()?
        } else {
            None
        };
        if let Some(result) = finished {
            let finished = job.take().unwrap();
            if !result.success() {
                let error = std::fs::read_to_string(finished.log).unwrap_or_default();
                let mut state = shared.lock().unwrap();
                state.status = "Build failed — keeping old logic".into();
                state.building = false;
                state.build_error = error
                    .chars()
                    .rev()
                    .take(12_000)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                drop(state);
                log(
                    shared,
                    "Build failed. Fix the source and save to try again.",
                );
            } else {
                let result: Result<u64, Box<dyn std::error::Error>> = (|| {
                    // The editor compiles this fixed local SDK module. Native
                    // ownership and initializer contracts remain the author's responsibility.
                    if let Some(router) = router.as_mut() {
                        let candidate = unsafe { router.prepare(&library) }?;
                        if mode == "drain" {
                            return Ok(router.activate(candidate)?);
                        }
                        shared.lock().unwrap().accepting = false;
                        shared.lock().unwrap().status = "Restarting — migrating state".into();
                        log(shared, "Restart: stop → migrate → recreate");
                        let mut restart = router.restart(candidate, |_, saved, schema| {
                            if saved.schema != schema || saved.bytes.len() != 8 {
                                return Err(ServiceError::CONTRACT_MISMATCH);
                            }
                            Ok(saved.clone())
                        })?;
                        let deadline = Instant::now() + Duration::from_secs(8);
                        loop {
                            poll_requests(&mut requests, shared, &mut cx);
                            match Pin::new(&mut restart).poll(&mut cx) {
                                Poll::Ready(result) => return Ok(result?),
                                Poll::Pending
                                    if stop.load(Ordering::Acquire)
                                        || Instant::now() >= deadline =>
                                {
                                    return Err("Shutdown wait interrupted. Old state is retained; request admission is paused.".into());
                                }
                                _ => std::thread::sleep(Duration::from_millis(16)),
                            }
                        }
                    } else {
                        let mut first = unsafe {
                            RequestRouter::load_with_options(
                                &library,
                                host.clone(),
                                options.clone(),
                            )
                        }?;
                        first.start_worker(
                            WorkerId(1),
                            view.id(),
                            b"worker",
                            Checkpoint::new(COUNTER, 0u64.to_le_bytes().to_vec()),
                        )?;
                        let id = first.generation();
                        router = Some(first);
                        Ok(id)
                    }
                })();
                let mut state = shared.lock().unwrap();
                state.building = false;
                match result {
                    Ok(id) => {
                        state.status = "Running".into();
                        drop(state);
                        log(shared, format!("{mode}: new requests now use gen {id}"));
                    }
                    Err(error) => {
                        state.status = "Cutover failed — state retained".into();
                        state.build_error = error.to_string();
                    }
                }
            }
        }
        if let Some(router) = router.as_mut() {
            let _ = router.poll_workers(&mut cx);
            poll_requests(&mut requests, shared, &mut cx);
            let mut state = shared.lock().unwrap();
            state.generation = router.generation();
            state.accepting = router.is_accepting();
            state.in_flight = router.in_flight();
            if let Some(saved) = router.worker_checkpoint(WorkerId(1)) {
                if let Ok(bytes) = saved.bytes.as_slice().try_into() {
                    state.checkpoint = u64::from_le_bytes(bytes);
                }
            }
        }
        {
            let mut state = shared.lock().unwrap();
            state.budget_used = options.generations.used();
            state.budget_limit = options.generations.limit();
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    if let Some(mut build) = job {
        let _ = build.child.kill();
        let _ = build.child.wait();
    }
    // Every foreign future drops here, on its owner thread, before host resources.
    drop(requests);
    drop(router);
    drop(view);
    drop(host);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        cwd: PathBuf,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
            std::env::set_current_dir(&self.cwd).unwrap();
            let temp = std::env::temp_dir().canonicalize().unwrap();
            let root = self.root.canonicalize().unwrap();
            assert_eq!(root.parent(), Some(temp.as_path()));
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    fn source(label: &str, step: u64) -> String {
        include_str!("../../../tauri_tokio_logic/src/lib.rs")
            .lines()
            .filter_map(|line| {
                if line.contains("compile_error!(\"GUI sample: intentional build error\")") {
                    return None;
                }
                Some(if line.starts_with("const LABEL:") {
                    format!("const LABEL: &str = \"{label}\";")
                } else if line.starts_with("const STEP:") {
                    format!("const STEP: u64 = {step};")
                } else {
                    line.to_string()
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn wait(controller: &Controller, predicate: impl Fn(&Dashboard) -> bool) -> Dashboard {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let state = controller.snapshot();
            if predicate(&state) {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "timed out: {}\n{}",
                state.status,
                state.build_error
            );
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    /// Builds isolated native DLLs; leaves the user's live editor/source alone.
    #[test]
    #[ignore = "native build integration; run explicitly with --ignored --test-threads=1"]
    fn native_gui_engine_reload_and_recovery() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("reknot-gui-test-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        let flow = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../reknot")
            .canonicalize()
            .unwrap();
        let manifest = format!("[package]\nname = \"tauri_tokio_logic\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n[lib]\ncrate-type = [\"cdylib\"]\n[dependencies]\nreknot = {{ path = {}, default-features = false }}\nserde = {{version=\"1\",features=[\"derive\"]}}\nserde_json=\"1\"\n", serde_json::to_string(&flow.to_string_lossy()).unwrap());
        std::fs::write(root.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(root.join("src/lib.rs"), source("Test / v1", 1)).unwrap();
        let controller = Controller::from_path(root.join("src/lib.rs")).unwrap();
        let mut fixture = Fixture {
            root: root.clone(),
            cwd: std::env::current_dir().unwrap(),
            stop: controller.stop.clone(),
            thread: None,
        };
        // A desktop launch CWD in which ./target/hot cannot be created.
        let launch = root.join("launch");
        std::fs::create_dir(&launch).unwrap();
        std::fs::write(launch.join("target"), "not a directory").unwrap();
        std::env::set_current_dir(&launch).unwrap();
        fixture.thread = Some(controller.spawn());
        let initial = wait(&controller, |s| s.worker.counter > 0);
        assert_eq!(initial.worker.label, "Test / v1");
        controller.send(Command::Mode("drain".into())).unwrap();
        controller.save_source(source("Test / v2", 5)).unwrap();
        let drain = wait(&controller, |s| s.generation == 2 && !s.building);
        assert_eq!(drain.worker.generation, 1);
        assert!(drain.worker.counter >= initial.worker.counter);
        controller
            .send(Command::Request {
                text: "same host".into(),
                slow: false,
            })
            .unwrap();
        let reply = wait(&controller, |s| !s.replies.is_empty());
        assert_eq!(reply.replies[0].generation, 2);
        assert!(reply.replies[0].text.contains("Test / v2"));
        controller.send(Command::Mode("restart".into())).unwrap();
        controller.send(Command::Build).unwrap();
        let restarted = wait(&controller, |s| s.worker.generation == 3);
        assert_eq!(restarted.worker.step, 5);
        assert!(restarted.worker.counter >= drain.worker.counter);
        controller
            .save_source(source("Broken", 5) + "\ncompile_error!(\"intentional test error\");")
            .unwrap();
        let failed = wait(&controller, |s| !s.build_error.is_empty() && !s.building);
        assert_eq!(failed.generation, 3);
        assert!(failed.accepting);
        let still_live = wait(&controller, |s| s.worker.counter > failed.worker.counter);
        assert_eq!(still_live.worker.generation, 3);
        controller.save_source(source("Recovered / v3", 2)).unwrap();
        let recovered = wait(&controller, |s| s.worker.generation == 4 && !s.building);
        assert_eq!(recovered.worker.label, "Recovered / v3");
        assert!(recovered.worker.counter >= failed.worker.counter);
        assert_eq!(recovered.budget_used, 4); // Compile errors never load a DLL.
    }
}
