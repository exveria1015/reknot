#![cfg(all(not(miri), feature = "typed"))]
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

fn directory(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("reknot-new-{}-{nonce}-{label}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root
}
fn create(destination: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_reknot-new"))
        .arg(destination)
        .args(extra)
        .output()
        .unwrap()
}
fn checked(command: &mut Command) -> std::process::Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
#[test]
fn creates_standalone_workspace_and_refuses_to_replace_existing_content() {
    let root = directory("no-clobber");
    let project = root.join("日本語 project");
    let output = create(&project, &["--name", "my-bot"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest = std::fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(manifest.contains("default-members = [\"host\"]"));
    assert!(manifest.contains("default-features = false"));
    assert!(std::fs::read_to_string(project.join("logic/src/lib.rs"))
        .unwrap()
        .contains("use my_bot_protocol"));
    assert!(!project.join(".git").exists());
    assert!(
        !project.join("Cargo.lock").exists(),
        "generation must not silently run Cargo"
    );
    std::fs::write(project.join("logic/src/lib.rs"), "user edited content").unwrap();
    assert!(!create(&project, &["--name", "my-bot"]).status.success());
    assert_eq!(
        std::fs::read_to_string(project.join("logic/src/lib.rs")).unwrap(),
        "user edited content"
    );
    let empty = root.join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert!(!create(&empty, &[]).status.success());
    assert_eq!(std::fs::read_dir(empty).unwrap().count(), 0);
}
#[test]
fn bad_names_or_sdk_paths_fail_before_creating_any_project() {
    let root = directory("invalid");
    for (index, name) in [
        "Bad Name",
        "../escape",
        "-flag",
        "9starts-with-digit",
        "a\n[dependencies]",
    ]
    .iter()
    .enumerate()
    {
        let project = root.join(format!("case{index}"));
        assert!(!create(&project, &["--name", name]).status.success());
        assert!(!project.exists());
    }
    let project = root.join("bad-sdk");
    assert!(!create(&project, &["--sdk-path", root.to_str().unwrap()])
        .status
        .success());
    assert!(!project.exists());
}

#[test]
#[ignore = "builds and runs a generated project, including a live native source edit"]
fn generated_workspace_builds_static_and_native_and_keeps_host_state() {
    let project = directory("consumer").join("starter app");
    let output = create(&project, &["--name", "my-bot"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    checked(
        Command::new("cargo")
            .current_dir(&project)
            .args(["generate-lockfile", "--offline"]),
    );
    checked(Command::new("cargo").current_dir(&project).args([
        "check",
        "--workspace",
        "--all-targets",
        "--all-features",
        "--locked",
        "--offline",
    ]));
    let metadata = checked(Command::new("cargo").current_dir(&project).args([
        "metadata",
        "--no-deps",
        "--format-version",
        "1",
        "--locked",
        "--offline",
    ]));
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout).unwrap();
    let executable = PathBuf::from(metadata["target_directory"].as_str().unwrap())
        .join("debug")
        .join(format!("my-bot-host{}", std::env::consts::EXE_SUFFIX));
    for features in [
        &["--no-default-features", "--features", "static-logic"][..],
        &[][..],
    ] {
        checked(
            Command::new("cargo")
                .current_dir(&project)
                .args(["build", "--locked", "--offline"])
                .args(features),
        );
        // Launch the built host directly, so timeout cleanup owns the exact
        // process rather than leaving a cargo-run child alive after a failure.
        let mut command = Command::new(&executable);
        command
            .current_dir(&project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut app = Application::start(&mut command);
        app.wait_for("Ready.");
        app.send("hello");
        app.wait_for("logic-v1 / message 1: HELLO");
        if features.is_empty() {
            let path = project.join("logic/src/lib.rs");
            let source = std::fs::read_to_string(&path).unwrap();
            std::fs::write(path, source.replace("logic-v1", "logic-v2")).unwrap();
            app.wait_for("New events use generation 2");
        }
        app.send("world");
        let label = if features.is_empty() {
            "logic-v2"
        } else {
            "logic-v1"
        };
        app.wait_for(&format!("{label} / message 2: WORLD"));
        app.send("/count");
        app.wait_for("Host received 2 messages");
        app.send("/quit");
        app.finish();
    }
}

struct Application {
    child: std::process::Child,
    lines: Receiver<String>,
    transcript: String,
}
impl Application {
    fn start(command: &mut Command) -> Self {
        use std::io::{BufRead, BufReader};
        let mut child = command.spawn().unwrap();
        let (send, lines) = mpsc::channel();
        let outputs: [Box<dyn std::io::Read + Send>; 2] = [
            Box::new(child.stdout.take().unwrap()),
            Box::new(child.stderr.take().unwrap()),
        ];
        for output in outputs {
            let send = send.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(output).lines() {
                    let Ok(line) = line else { break };
                    if send.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        Self {
            child,
            lines,
            transcript: String::new(),
        }
    }
    fn send(&mut self, text: &str) {
        use std::io::Write;
        writeln!(self.child.stdin.as_mut().unwrap(), "{text}").unwrap();
    }
    fn wait_for(&mut self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!("waiting for {expected:?}: {error}\n{}", self.transcript)
                });
            self.transcript.push_str(&line);
            self.transcript.push('\n');
            if line.contains(expected) {
                return;
            }
        }
    }
    fn finish(&mut self) {
        self.child.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{}", self.transcript);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "host shutdown stalled\n{}",
                self.transcript
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Application {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
