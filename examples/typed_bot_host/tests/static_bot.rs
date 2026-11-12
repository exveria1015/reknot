#![cfg(all(feature = "static-logic", not(feature = "reload")))]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn console_bot_keeps_host_history_across_typed_calls() {
    let mut app = Application::start(Path::new(env!("CARGO_BIN_EXE_typed_bot_host")));
    app.wait_for("Typed bot ready.");
    app.send("hello");
    app.wait_for("logic-v1 / message 1: HELLO");
    app.send("world");
    app.wait_for("logic-v1 / message 2: WORLD");
    app.send("/summary");
    app.wait_for("2 messages kept by the host: hello, world");
    app.send("/quit");
    app.wait_exit();
}

#[test]
#[ignore = "builds isolated native/static bots to test cancellation while a child is pending"]
fn quit_and_eof_cancel_the_pending_call_and_join_its_children() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sdk = source
        .join("../../reknot")
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .to_owned();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("bot-control-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("Cargo.toml"), "[workspace]\nmembers = [\"typed_bot_host\", \"typed_bot_logic\", \"typed_bot_protocol\"]\nresolver = \"2\"\n[workspace.package]\nedition = \"2021\"\nrust-version = \"1.95\"\nlicense = \"MIT OR Apache-2.0\"\n").unwrap();
    for (name, entry) in [
        ("typed_bot_host", "main.rs"),
        ("typed_bot_logic", "lib.rs"),
        ("typed_bot_protocol", "lib.rs"),
    ] {
        let original = source.parent().unwrap().join(name);
        let destination = project.join(name);
        std::fs::create_dir_all(destination.join("src")).unwrap();
        let manifest = std::fs::read_to_string(original.join("Cargo.toml")).unwrap();
        std::fs::write(
            destination.join("Cargo.toml"),
            manifest.replace("../../reknot", &sdk),
        )
        .unwrap();
        std::fs::copy(
            original.join("src").join(entry),
            destination.join("src").join(entry),
        )
        .unwrap();
    }
    std::fs::create_dir_all(project.join("typed_bot_host/examples")).unwrap();
    std::fs::copy(
        source.join("examples/framework_bridge.rs"),
        project.join("typed_bot_host/examples/framework_bridge.rs"),
    )
    .unwrap();
    let logic = project.join("typed_bot_logic/src/lib.rs");
    let original = std::fs::read_to_string(&logic).unwrap();
    let modified = original
        .replace(
            "async fn reply(message:",
            "async fn reply(context: reknot::RequestContext, message:",
        )
        .replace(
            "    let text = message.text.trim();",
            r#"
    if message.text == "wait" {
        context.spawn_child(|stop| async move {
            eprintln!("CHILD_STARTED");
            stop.cancelled().await;
            let mut remaining = 3;
            std::future::poll_fn(|cx| {
                if remaining == 0 { std::task::Poll::Ready(()) }
                else { remaining -= 1; cx.waker().wake_by_ref(); std::task::Poll::Pending }
            }).await;
            eprintln!("CHILD_JOINED");
            Ok(())
        }).unwrap();
        std::future::pending::<()>().await;
    }
    let text = message.text.trim();
"#,
        );
    assert_ne!(modified, original);
    std::fs::write(logic, modified).unwrap();
    checked(
        Command::new("cargo")
            .current_dir(&project)
            .args(["generate-lockfile", "--offline"]),
    );
    // Use a fixture-only directory rather than ambient Cargo target settings.
    let output = project.join("host-output");
    let executable = output
        .join("debug")
        .join(format!("typed_bot_host{}", std::env::consts::EXE_SUFFIX));
    for features in [
        vec!["--no-default-features", "--features", "static-logic"],
        vec![],
    ] {
        checked(
            Command::new("cargo")
                .current_dir(&project)
                .args([
                    "build",
                    "-p",
                    "typed_bot_host",
                    "--offline",
                    "--locked",
                    "--target-dir",
                ])
                .arg(&output)
                .args(features),
        );
        for quit in [true, false] {
            let mut app = Application::start(&executable);
            app.wait_for("Typed bot ready.");
            app.send("wait");
            app.wait_for("CHILD_STARTED");
            app.send("busy");
            app.wait_for("Request in progress");
            if quit {
                app.send("/quit");
            } else {
                app.child.stdin.take();
            }
            app.wait_for("CHILD_JOINED");
            app.wait_exit();
        }
    }
}

fn checked(command: &mut Command) -> std::process::Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

struct Application {
    child: std::process::Child,
    lines: mpsc::Receiver<String>,
    transcript: String,
}
impl Application {
    fn start(executable: &Path) -> Self {
        use std::io::{BufRead, BufReader};
        let mut child = Command::new(executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
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
        writeln!(self.child.stdin.as_mut().unwrap(), "{text}").unwrap();
    }
    fn wait_for(&mut self, expected: &str) {
        let timeout = if expected == "Typed bot ready." {
            120
        } else {
            5
        };
        let deadline = Instant::now() + Duration::from_secs(timeout);
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!("waiting for {expected}: {error}\n{}", self.transcript)
                });
            self.transcript.push_str(&line);
            self.transcript.push('\n');
            if line.contains(expected) {
                return;
            }
        }
    }
    fn wait_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{}", self.transcript);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "shutdown stalled\n{}",
                self.transcript
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Application {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
