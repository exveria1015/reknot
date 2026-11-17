//! Reproducible edit-to-response and typed-boundary comparisons; no speed gate.
use reknot::jit::development::LayeredDevOptions;
use reknot::typed::{DevModule, DevOptions, Handler, Module, ReloadEvent, WireFormat};
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

const EDIT: Handler<String, String> = Handler::new("bench.edit", 1);
const SOURCE: &str = r#"use reknot::typed::{Handler,Module};
const ECHO:Handler<String,String>=Handler::new("bench.edit",1);
const VERSION:&str="version";
#[reknot::handler(ECHO)] fn echo(input:String)->String { format!("{VERSION}:{input}") }
fn module()->Module { Module::new("bench.edit",1).with(ECHO) }
reknot::export_handlers!(module(),[echo]);
"#;
fn ready<F: Future + Unpin>(mut future: F) -> F::Output {
    for _ in 0..100 {
        if let Poll::Ready(value) =
            Pin::new(&mut future).poll(&mut Context::from_waker(Waker::noop()))
        {
            return value;
        }
    }
    panic!("benchmark handler did not finish")
}
fn fixture(dir: &Path, jit: bool) {
    fs::create_dir_all(dir.join("logic/src")).unwrap();
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR"))
        .to_string_lossy()
        .replace('\\', "/");
    let dependency = if jit {
        fs::create_dir_all(dir.join("support/src")).unwrap();
        fs::write(dir.join("support/src/lib.rs"), "pub use reknot::*;\n").unwrap();
        fs::write(dir.join("support/Cargo.toml"),format!("[package]\nname=\"bench_support\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\ncrate-type=[\"dylib\"]\n[dependencies]\nreknot={{path=\"{sdk}\",default-features=false,features=[\"typed\"]}}\n")).unwrap();
        "{package=\"bench_support\",path=\"../support\"}".to_owned()
    } else {
        format!("{{path=\"{sdk}\",default-features=false,features=[\"typed\"]}}")
    };
    fs::write(dir.join("Cargo.toml"),format!("[workspace]\nmembers=[\"logic\"{}]\nresolver=\"2\"\n[profile.dev]\nopt-level=0\ndebug=0\ncodegen-units=1\nlto=false\npanic=\"unwind\"\n",if jit {",\"support\""} else {""})).unwrap();
    fs::write(dir.join("logic/Cargo.toml"),format!("[package]\nname=\"bench_logic\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\ncrate-type=[\"cdylib\"]\n[dependencies]\nreknot={dependency}\n")).unwrap();
    fs::write(dir.join("logic/src/lib.rs"), SOURCE).unwrap();
    let lock = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        lock.status.success(),
        "{}",
        String::from_utf8_lossy(&lock.stderr)
    );
}
fn edit_child(backend: &str, dir: &Path, output: &Path, samples: usize) {
    fixture(dir, backend == "jit");
    let start = Instant::now();
    let module = Module::new("bench.edit", 1).with(EDIT);
    let mut dev = unsafe {
        if backend == "jit" {
            let mut options = LayeredDevOptions::builtin(
                dir.join("Cargo.toml"),
                "bench_logic",
                "bench_support",
                dir.join("cache"),
            );
            options.debounce = Duration::from_millis(50);
            options.offline = true;
            options.declared_inputs_complete = true;
            DevModule::start(module.builder(), options)
        } else {
            let mut options = DevOptions::new(dir.join("Cargo.toml"), "bench_logic");
            options.debounce = Duration::from_millis(50);
            DevModule::start(module.builder(), options)
        }
    }
    .unwrap();
    let initial_ms = start.elapsed().as_secs_f64() * 1000.0;
    let echo = dev.host().handler(EDIT).unwrap();
    assert_eq!(ready(echo.call("probe".into())).unwrap(), "version:probe");
    let mut times = Vec::new();
    let mut phases = Vec::new();
    // Two edited builds warm each producer's incremental/object path.
    for n in 0..samples + 2 {
        let version = format!("v{n}");
        let source = SOURCE.replace("\"version\"", &format!("\"{version}\""));
        let start = Instant::now();
        fs::write(dir.join("logic/src/lib.rs"), source).unwrap();
        loop {
            if let Some(event) = dev.update() {
                match event {
                    ReloadEvent::Reloaded { .. } => break,
                    ReloadEvent::Building | ReloadEvent::Superseded => {}
                    _ => panic!("{backend}: {event}"),
                }
            }
            assert!(
                start.elapsed() < Duration::from_secs(180),
                "reload timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            ready(echo.call("probe".into())).unwrap(),
            format!("{version}:probe")
        );
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        if n >= 2 {
            times.push(ms);
            if let Some(snapshot) = dev.layered_snapshot() {
                phases.push(serde_json::to_value(snapshot.metrics).unwrap());
            }
        }
    }
    dev.shutdown().unwrap();
    fs::write(output,serde_json::to_vec_pretty(&json!({"backend":backend,"initial_ms":initial_ms,"edit_to_response_ms":times,"jit_phases_ms":phases})).unwrap()).unwrap();
}
fn summary(values: &[f64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    json!({"n":values.len(),"median":sorted[sorted.len()/2],"p95":sorted[(sorted.len()*95).div_ceil(100)-1],"min":sorted[0],"max":sorted[sorted.len()-1]})
}

#[reknot::schema]
#[derive(Clone, Serialize, Deserialize)]
struct Frame {
    tick: u64,
    samples: Vec<f32>,
}
const JSON_FRAME: Handler<Frame, Frame> = Handler::new("bench.json", 1);
const PACK_FRAME: Handler<Frame, Frame> =
    Handler::new("bench.pack", 1).with_format(WireFormat::MessagePack);
#[reknot::handler(JSON_FRAME)]
fn json_frame(frame: Frame) -> Frame {
    frame
}
#[reknot::handler(PACK_FRAME)]
fn pack_frame(frame: Frame) -> Frame {
    frame
}
fn boundary_module() -> Module {
    Module::new("bench.boundary", 1)
        .with(JSON_FRAME)
        .with(PACK_FRAME)
}
reknot::export_handlers!(boundary_module(), [json_frame, pack_frame]);
fn boundary() -> Vec<Value> {
    let host = unsafe { boundary_module().builder().from_api(handler_api()) }.unwrap();
    let handles = [
        host.handler(JSON_FRAME).unwrap(),
        host.handler(PACK_FRAME).unwrap(),
    ];
    let mut rows = Vec::new();
    for count in [1, 64, 4096] {
        let input = Frame {
            tick: 17,
            samples: (0..count).map(|i| i as f32 * 0.25 + 0.125).collect(),
        };
        let json_bytes = serde_json::to_vec(&input).unwrap().len();
        let mut packed = Vec::new();
        input
            .serialize(
                &mut rmp_serde::Serializer::new(&mut packed)
                    .with_struct_map()
                    .with_human_readable(),
            )
            .unwrap();
        let iterations = if count >= 4096 { 100 } else { 1000 };
        let mut batches = [Vec::new(), Vec::new()];
        for batch in 0..17 {
            for mode in if batch % 2 == 0 { [0, 1] } else { [1, 0] } {
                let start = Instant::now();
                for _ in 0..iterations {
                    let result =
                        ready(handles[mode].call(std::hint::black_box(input.clone()))).unwrap();
                    assert_eq!(result.tick, input.tick);
                    assert_eq!(result.samples, input.samples);
                    std::hint::black_box(result);
                }
                if batch >= 2 {
                    batches[mode]
                        .push(start.elapsed().as_secs_f64() * 1_000_000.0 / iterations as f64);
                }
            }
        }
        rows.push(json!({"float_count":count,"iterations_per_batch":iterations,"json_bytes":json_bytes,"messagepack_bytes":packed.len(),"json_us_per_call":summary(&batches[0]),"messagepack_us_per_call":summary(&batches[1]),"json_batches_us":batches[0],"messagepack_batches_us":batches[1]}));
    }
    rows
}
fn main() {
    let args = std::env::args()
        .skip(1)
        .filter(|arg| arg != "--bench")
        .collect::<Vec<_>>();
    if args.first().is_some_and(|s| s == "--edit-child") {
        edit_child(
            &args[1],
            Path::new(&args[2]),
            Path::new(&args[3]),
            args[4].parse().unwrap(),
        );
        return;
    }
    if args.first().is_some_and(|s| s == "--help") {
        println!("cargo bench -p reknot --bench development --features jit-dev,typed-binary --locked -- --output RESULTS.json\nRun alone, on an idle machine. Uses cached Cargo dependencies; no performance pass/fail threshold.");
        return;
    }
    assert!(
        args.len() == 2 && args[0] == "--output",
        "pass --output RESULTS.json (or --help)"
    );
    let output = PathBuf::from(&args[1]);
    assert!(!output.exists(), "output already exists");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let dir = std::env::temp_dir().join(format!("rk-bench-{}-{stamp}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let mut runs = Vec::new();
    for (index, backend) in ["native", "jit", "jit", "native"].iter().enumerate() {
        let result_path = dir.join(format!("{index}.json"));
        let workspace = dir.join(index.to_string());
        let result = Command::new(std::env::current_exe().unwrap())
            .arg("--edit-child")
            .arg(backend)
            .arg(&workspace)
            .arg(&result_path)
            .arg("9")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let value: Value = serde_json::from_slice(&fs::read(result_path).unwrap()).unwrap();
        eprintln!("completed {backend} run {}", index + 1);
        runs.push(value);
    }
    let values = |name: &str| {
        runs.iter()
            .filter(|r| r["backend"] == name)
            .flat_map(|r| {
                r["edit_to_response_ms"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| n.as_f64().unwrap())
            })
            .collect::<Vec<_>>()
    };
    let rustc = Command::new("rustc").arg("-Vv").output().unwrap();
    let record = json!({"benchmark_version":1,"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"logical_cpus":std::thread::available_parallelism().unwrap().get(),"rustc":String::from_utf8_lossy(&rustc.stdout).trim(),
        "edit":{"unit":"ms","fixture":"single typed String handler; same body; native links SDK into cdylib; JIT uses resident SDK support dylib","profile":"native dev opt=0; JIT logic opt=0, support/dependencies opt=1; debug=0 codegen-units=1","debounce_ms":50,"poll_ms":1,"warmup_edits_per_run":2,"order":["native","jit","jit","native"],"native":summary(&values("native")),"jit":summary(&values("jit")),"runs":runs},
        "boundary":{"unit":"us per complete typed echo call (batch average)","host":"release; resident callbacks; no DLL load or I/O; includes clone, encoding, routing, decoding, verification and cleanup","warmup_batches":2,"rows":boundary()}});
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&output, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    println!("{}",serde_json::to_string_pretty(&json!({"native_ms":record["edit"]["native"],"jit_ms":record["edit"]["jit"],"output":output})).unwrap());
}
