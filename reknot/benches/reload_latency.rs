//! Open-loop request latency during real saved-source reloads. No speed gate.
//! Run with --output <directory>; raw requests, owner polls and events are retained.
use reknot::jit::development::LayeredDevOptions;
use reknot::typed::{
    BridgeDriver, BridgeOptions, DevBackend, DevModule, DevOptions, Handler, HandlerClient, Module,
    ReloadEvent, ReloadMode, RemoteCall, WorkerState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Command,
    task::{Context, Poll, Waker},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const REQUEST: Handler<(u64, u64, u64), (String, u64)> = Handler::new("latency.request", 1);
const WORK: Handler<u64, Result<(), String>> = Handler::new("latency.worker", 1);
const STATE: WorkerState<Vec<u8>> = WorkerState::new("latency.state", 1);
const SOURCE: &str = include_str!("fixtures/reload_latency.rs");
const WORKER: reknot::WorkerId = reknot::WorkerId(1);

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Case {
    name: &'static str,
    native: bool,
    managed: bool,
    restart: bool,
    cleanup_ms: u64,
    state_bytes: usize,
    burst: bool,
}
const CASES: [Case; 7] = [
    Case {
        name: "native-drain",
        native: true,
        managed: false,
        restart: false,
        cleanup_ms: 0,
        state_bytes: 0,
        burst: false,
    },
    Case {
        name: "jit-drain",
        native: false,
        managed: false,
        restart: false,
        cleanup_ms: 0,
        state_bytes: 0,
        burst: false,
    },
    Case {
        name: "jit-reclaim",
        native: false,
        managed: true,
        restart: false,
        cleanup_ms: 25,
        state_bytes: 64,
        burst: false,
    },
    Case {
        name: "jit-restart",
        native: false,
        managed: true,
        restart: true,
        cleanup_ms: 0,
        state_bytes: 64,
        burst: false,
    },
    Case {
        name: "jit-restart-cleanup",
        native: false,
        managed: true,
        restart: true,
        cleanup_ms: 25,
        state_bytes: 64,
        burst: false,
    },
    Case {
        name: "jit-restart-state",
        native: false,
        managed: true,
        restart: true,
        cleanup_ms: 0,
        state_bytes: 262_144,
        burst: false,
    },
    Case {
        name: "jit-burst",
        native: false,
        managed: false,
        restart: false,
        cleanup_ms: 0,
        state_bytes: 0,
        burst: true,
    },
];

#[derive(Clone, Serialize, Deserialize)]
struct Settings {
    samples: usize,
    warmups: usize,
    rounds: usize,
    interval_us: u64,
    cpu_us: u64,
    response_ms: u64,
    baseline_ms: u64,
    minimum_observation_ms: u64,
    poll_sleep_us: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            samples: 9,
            warmups: 2,
            rounds: 2,
            interval_us: 1_000,
            cpu_us: 100,
            response_ms: 5,
            baseline_ms: 500,
            minimum_observation_ms: 1_000,
            poll_sleep_us: 1_000,
        }
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
fn at(epoch: Instant) -> f64 {
    ms(epoch.elapsed())
}
fn write_json(path: &Path, value: &impl Serialize) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn command_version(name: &str, args: &[&str]) -> String {
    let output = Command::new(name).args(args).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}
fn fixture(dir: &Path, native: bool) {
    fs::create_dir_all(dir.join("logic/src")).unwrap();
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR"))
        .to_string_lossy()
        .replace('\\', "/");
    let dependency = if native {
        format!("{{path=\"{sdk}\",default-features=false,features=[\"typed\"]}}")
    } else {
        fs::create_dir_all(dir.join("support/src")).unwrap();
        fs::write(dir.join("support/src/lib.rs"), "pub use reknot::*;\n").unwrap();
        fs::write(dir.join("support/Cargo.toml"), format!("[package]\nname=\"latency_support\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\ncrate-type=[\"dylib\"]\n[dependencies]\nreknot={{path=\"{sdk}\",default-features=false,features=[\"typed\"]}}\n")).unwrap();
        "{package=\"latency_support\",path=\"../support\"}".into()
    };
    fs::write(dir.join("Cargo.toml"), format!("[workspace]\nmembers=[\"logic\"{}]\nresolver=\"2\"\n[profile.dev]\nopt-level=0\ndebug=0\ncodegen-units=1\nlto=false\npanic=\"unwind\"\n", if native { "" } else { ",\"support\"" })).unwrap();
    fs::write(dir.join("logic/Cargo.toml"), format!("[package]\nname=\"latency_logic\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\ncrate-type=[\"cdylib\"]\n[dependencies]\nreknot={dependency}\n")).unwrap();
    fs::write(dir.join("logic/src/lib.rs"), SOURCE).unwrap();
    let output = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Pending {
    sequence: u64,
    scheduled_ms: f64,
    submitted_ms: f64,
    call: RemoteCall<(String, u64)>,
}
#[derive(Serialize)]
struct Response {
    scheduled_ms: f64,
    submitted_ms: f64,
    completed_ms: f64,
    generation: Option<u64>,
    version: String,
}
#[derive(Serialize)]
struct OwnerPoll {
    start_ms: f64,
    elapsed_ms: f64,
    accepting_before: bool,
    accepting_after: bool,
    released: usize,
    collect_ms: Option<f64>,
}

struct Load<'a> {
    client: &'a HandlerClient<(u64, u64, u64), (String, u64)>,
    driver: &'a mut BridgeDriver,
    settings: &'a Settings,
    epoch: Instant,
    next: Instant,
    sequence: u64,
    pending: Vec<Pending>,
    responses: Vec<Response>,
    driver_polls: Vec<(f64, f64)>,
}
impl Load<'_> {
    fn tick(&mut self, cx: &mut Context<'_>, submit: bool) {
        let now = Instant::now();
        if submit {
            // Catch up every scheduled arrival. Measuring from the scheduled
            // time includes an owner stall even when submission itself is late.
            while self.next <= now {
                self.pending.push(Pending {
                    sequence: self.sequence,
                    scheduled_ms: ms(self.next.duration_since(self.epoch)),
                    submitted_ms: at(self.epoch),
                    call: self.client.call((
                        self.sequence,
                        self.settings.cpu_us,
                        self.settings.response_ms,
                    )),
                });
                self.sequence += 1;
                self.next += Duration::from_micros(self.settings.interval_us);
            }
        }
        let start = Instant::now();
        assert!(Pin::new(&mut *self.driver).poll(cx).is_pending());
        self.driver_polls
            .push((ms(start.duration_since(self.epoch)), ms(start.elapsed())));
        self.pending.retain_mut(|request| {
            let generation = request.call.generation();
            if let Poll::Ready(result) = Pin::new(&mut request.call).poll(cx) {
                let (version, sequence) = result.expect("request failed or queue overflowed");
                assert_eq!(sequence, request.sequence);
                self.responses.push(Response {
                    scheduled_ms: request.scheduled_ms,
                    submitted_ms: request.submitted_ms,
                    completed_ms: at(self.epoch),
                    generation,
                    version,
                });
                false
            } else {
                true
            }
        });
    }
}

fn sample(
    dev: &mut DevModule,
    driver: &mut BridgeDriver,
    client: &HandlerClient<(u64, u64, u64), (String, u64)>,
    dir: &Path,
    case: Case,
    settings: &Settings,
    index: usize,
) -> Value {
    let mut cx = Context::from_waker(Waker::noop());
    if case.managed && !case.restart {
        dev.host()
            .start_worker(
                WORKER,
                WORK,
                case.cleanup_ms,
                STATE,
                &vec![0; case.state_bytes],
            )
            .unwrap();
    }
    let old_image = dev
        .layered_snapshot()
        .and_then(|s| s.live_images.last().copied());
    let epoch = Instant::now();
    let mut load = Load {
        client,
        driver,
        settings,
        epoch,
        next: epoch,
        sequence: 0,
        pending: Vec::new(),
        responses: Vec::new(),
        driver_polls: Vec::new(),
    };
    let save_count = if case.burst { 3 } else { 1 };
    let versions: Vec<_> = (0..save_count)
        .map(|n| format!("sample_{index}_{n}"))
        .collect();
    let sources: Vec<_> = versions
        .iter()
        .map(|v| SOURCE.replace("\"initial\"", &format!("\"{v}\"")))
        .collect();
    let final_version = versions.last().unwrap();
    let mut saves = Vec::new();
    let mut events = Vec::new();
    let mut owner = Vec::new();
    let mut activation = None;
    let mut first_response = None;
    let mut pause_start = None;
    let mut pause_end = None;
    let mut stop_time = None;
    let mut worker_done = None;
    let mut reclaimed = None;
    let mut build_metrics = Value::Null;
    let mut finish_at = None;
    loop {
        let elapsed = at(epoch);
        assert!(
            elapsed < 180_000.0,
            "sample timed out: {:?}",
            dev.layered_snapshot()
        );
        if saves.len() < save_count
            && elapsed >= settings.baseline_ms as f64 + saves.len() as f64 * 100.0
        {
            let start = at(epoch);
            fs::write(dir.join("logic/src/lib.rs"), &sources[saves.len()]).unwrap();
            saves.push(json!({"start_ms": start, "end_ms": at(epoch)}));
        }
        let previous_responses = load.responses.len();
        load.tick(&mut cx, finish_at.is_none());
        let accepting_before = dev.host().is_accepting();
        let start = Instant::now();
        let event = dev.update();
        let end = Instant::now();
        let start_ms = ms(start.duration_since(epoch));
        let end_ms = ms(end.duration_since(epoch));
        let accepting_after = dev.host().is_accepting();
        if accepting_before && !accepting_after {
            assert!(pause_start.is_none(), "unexpected second admission pause");
            pause_start = Some((start_ms, end_ms));
        }
        if !accepting_before && accepting_after {
            pause_end = Some((start_ms, end_ms));
        }
        let snapshot = if case.managed || event.is_some() {
            dev.layered_snapshot()
        } else {
            None
        };
        let released = snapshot
            .as_ref()
            .map_or(0, |s| s.reclamation.released.len());
        if case.managed
            && reclaimed.is_none()
            && snapshot
                .as_ref()
                .is_some_and(|s| s.reclamation.released.contains(&old_image.unwrap()))
        {
            reclaimed = Some(end_ms);
        }
        owner.push(OwnerPoll {
            start_ms,
            elapsed_ms: ms(end.duration_since(start)),
            accepting_before,
            accepting_after,
            released,
            collect_ms: snapshot
                .as_ref()
                .filter(|_| case.managed)
                .map(|s| ms(s.reclamation.elapsed)),
        });
        if let Some(event) = event {
            let name = match event {
                ReloadEvent::Building => "building",
                ReloadEvent::Superseded => "superseded",
                ReloadEvent::Restarting => "restarting",
                ReloadEvent::Reloaded { .. } => {
                    activation = Some(end_ms);
                    if let Some(snapshot) = &snapshot {
                        build_metrics = serde_json::to_value(&snapshot.metrics).unwrap();
                    }
                    "reloaded"
                }
                other => panic!("unexpected reload event: {other}"),
            };
            events.push(json!({"kind":name, "start_ms":start_ms, "end_ms":end_ms}));
        }
        if first_response.is_none() {
            first_response = load.responses[previous_responses..]
                .iter()
                .find(|r| &r.version == final_version)
                .map(|r| r.completed_ms);
        }
        if case.managed && !case.restart {
            if activation.is_some_and(|t| end_ms >= t + 50.0) && stop_time.is_none() {
                assert!(dev.host().stop_worker(WORKER).unwrap());
                stop_time = Some(at(epoch));
            }
            if stop_time.is_some()
                && worker_done.is_none()
                && dev.host().worker_status(WORKER) == Some(reknot::RunStatus::Done)
            {
                assert!(dev.host().remove_worker(WORKER).unwrap());
                worker_done = Some(at(epoch));
            }
        }
        let complete = first_response.is_some_and(|t| end_ms >= t + 200.0)
            && (!case.managed || reclaimed.is_some())
            && saves.last().is_some_and(|s| {
                end_ms >= s["end_ms"].as_f64().unwrap() + settings.minimum_observation_ms as f64
            });
        if complete && finish_at.is_none() {
            finish_at = Some(at(epoch));
        }
        if finish_at.is_some() && load.pending.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_micros(settings.poll_sleep_us));
    }
    assert_eq!(saves.len(), save_count);
    assert!(activation.is_some() && first_response.is_some());
    assert!(case.restart == pause_start.is_some() && case.restart == pause_end.is_some());
    assert_eq!(load.sequence as usize, load.responses.len());
    if case.restart {
        let state = dev
            .host()
            .worker_checkpoint(WORKER, STATE)
            .unwrap()
            .unwrap();
        assert_eq!(state.len(), case.state_bytes);
        assert_eq!(
            state[0],
            (index + 2) as u8,
            "state was not restored exactly once"
        );
    }
    if case.managed {
        assert_eq!(dev.layered_snapshot().unwrap().live_images.len(), 1);
    }
    let pause = pause_start.zip(pause_end).map(|((a, b), (c, d))| {
        json!({
            "close_interval_ms":[a,b], "open_interval_ms":[c,d],
            "observed_ms": d-b, "lower_ms": (c-b).max(0.0), "upper_ms": d-a
        })
    });
    json!({"index":index, "warmup":index < settings.warmups, "saves":saves, "events":events,
        "activation_ms":activation, "first_new_response_ms":first_response, "pause":pause,
        "stop_worker_ms":stop_time, "worker_done_ms":worker_done, "old_image_released_ms":reclaimed,
        "build_metrics":build_metrics, "requests":load.responses, "owner_polls":owner,
        "driver_polls":load.driver_polls, "arrival_end_ms":finish_at, "completed_ms":at(epoch)})
}

fn child(case: Case, dir: &Path, output: &Path, settings: &Settings) {
    fixture(dir, case.native);
    let module = Module::new("latency", 1)
        .with(REQUEST)
        .with(WORK)
        .with_state(STATE);
    let mode = if case.restart {
        ReloadMode::Restart
    } else {
        ReloadMode::Drain
    };
    let options = if case.native {
        let mut options = DevOptions::new(dir.join("Cargo.toml"), "latency_logic");
        options.debounce = Duration::from_millis(50);
        DevBackend::from(options).with_reload_mode(mode)
    } else {
        let mut options = LayeredDevOptions::builtin(
            dir.join("Cargo.toml"),
            "latency_logic",
            "latency_support",
            dir.join("cache"),
        );
        options.debounce = Duration::from_millis(50);
        options.offline = true;
        options.declared_inputs_complete = true;
        DevBackend::from(options).with_reload_mode(mode)
    };
    let start = Instant::now();
    let mut dev = unsafe {
        if case.managed {
            DevModule::start_managed(module.builder(), options)
        } else {
            DevModule::start(module.builder(), options)
        }
    }
    .unwrap();
    let initial_ms = ms(start.elapsed());
    // Poll the actual bridge separately so owner reload measurements do not
    // silently include the synthetic request's CPU work.
    let (client, mut driver) = dev
        .host()
        .bridge(BridgeOptions {
            capacity: 4096,
            max_in_flight: 256,
        })
        .unwrap();
    let requests = client.handler(REQUEST).unwrap();
    if case.restart {
        dev.host()
            .start_worker(
                WORKER,
                WORK,
                case.cleanup_ms,
                STATE,
                &vec![0; case.state_bytes],
            )
            .unwrap();
        dev.update();
        assert_eq!(
            dev.host()
                .worker_checkpoint(WORKER, STATE)
                .unwrap()
                .unwrap()[0],
            1
        );
    }
    let mut samples = Vec::new();
    for index in 0..settings.warmups + settings.samples {
        samples.push(sample(
            &mut dev,
            &mut driver,
            &requests,
            dir,
            case,
            settings,
            index,
        ));
        write_json(
            output,
            &json!({"case":case, "settings":settings, "workspace":dir, "initial_ms":initial_ms, "samples":samples}),
        );
        eprintln!(
            "{} sample {}/{} complete",
            case.name,
            index + 1,
            settings.warmups + settings.samples
        );
    }
    if case.restart {
        dev.host().stop_worker(WORKER).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while dev.host().worker_status(WORKER) != Some(reknot::RunStatus::Done) {
            dev.update();
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        dev.host().remove_worker(WORKER).unwrap();
    }
    dev.shutdown().unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| {
        args.iter()
            .position(|s| s == name)
            .map(|n| args[n + 1].as_str())
    };
    if let Some(name) = arg("--child") {
        let case = CASES.iter().find(|c| c.name == name).copied().unwrap();
        let settings: Settings =
            serde_json::from_slice(&fs::read(arg("--settings").unwrap()).unwrap()).unwrap();
        child(
            case,
            Path::new(arg("--workspace").unwrap()),
            Path::new(arg("--output").unwrap()),
            &settings,
        );
        return;
    }
    let mut settings = Settings::default();
    if let Some(n) = arg("--samples") {
        settings.samples = n.parse().unwrap();
    }
    if let Some(n) = arg("--warmups") {
        settings.warmups = n.parse().unwrap();
    }
    if let Some(n) = arg("--rounds") {
        settings.rounds = n.parse().unwrap();
    }
    if let Some(n) = arg("--interval-us") {
        settings.interval_us = n.parse().unwrap();
    }
    if let Some(n) = arg("--cpu-us") {
        settings.cpu_us = n.parse().unwrap();
    }
    assert!(settings.samples > 0 && settings.rounds > 0 && settings.interval_us > 0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let output = PathBuf::from(arg("--output").unwrap_or("target/reload-latency"));
    fs::create_dir_all(&output).unwrap();
    let output = output.canonicalize().unwrap();
    assert!(
        !output.join("manifest.json").exists(),
        "use a fresh output directory"
    );
    let work = std::env::temp_dir().join(format!("rk-latency-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let settings_path = output.join("settings.json");
    write_json(&settings_path, &settings);
    let mut manifest = json!({"schema":1, "os":std::env::consts::OS, "architecture":std::env::consts::ARCH,
        "rustc":command_version("rustc", &["-Vv"]), "cargo":command_version("cargo", &["-V"]),
        "settings":settings, "workspace":work, "runs":[], "complete":false});
    write_json(&output.join("manifest.json"), &manifest);
    for round in 0..settings.rounds {
        let cases: Vec<_> = if round % 2 == 0 {
            CASES.iter().collect()
        } else {
            CASES.iter().rev().collect()
        };
        for case in cases {
            if arg("--case").is_some_and(|name| name != case.name) {
                continue;
            }
            let name = format!("{round}-{}", case.name);
            let path = output.join(format!("{name}.json"));
            eprintln!("starting {name}");
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--child", case.name, "--workspace"])
                .arg(work.join(&name))
                .arg("--output")
                .arg(&path)
                .arg("--settings")
                .arg(&settings_path)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "{name} failed; partial results are retained"
            );
            manifest["runs"].as_array_mut().unwrap().push(json!({"round":round, "case":case.name, "file":path.file_name().unwrap().to_str().unwrap()}));
            write_json(&output.join("manifest.json"), &manifest);
        }
    }
    assert!(
        !manifest["runs"].as_array().unwrap().is_empty(),
        "no matching case"
    );
    manifest["complete"] = json!(true);
    write_json(&output.join("manifest.json"), &manifest);
    println!("{}", output.join("manifest.json").display());
}
