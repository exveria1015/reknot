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
fn registry_and_pinned_git_generation_do_not_embed_the_build_machine_sdk_path() {
    let root = directory("sources");
    let registry = root.join("registry");
    assert!(create(&registry, &["--sdk-version", "0.1.0-rc.2"])
        .status
        .success());
    let manifest = std::fs::read_to_string(registry.join("Cargo.toml")).unwrap();
    assert!(manifest.contains("version = \"=0.1.0-rc.2\""));
    assert!(!manifest.contains(env!("CARGO_MANIFEST_DIR")));
    assert!(!manifest.contains("path ="));
    assert!(registry.join("protocol/examples/schema.rs").is_file());
    let git = root.join("git");
    let sha = "0123456789012345678901234567890123456789";
    assert!(create(&git, &["--sdk-rev", sha]).status.success());
    let manifest = std::fs::read_to_string(git.join("Cargo.toml")).unwrap();
    assert!(manifest.contains(env!("CARGO_PKG_REPOSITORY")));
    assert!(manifest.contains(sha));
    assert!(!manifest.contains("path ="));
    for (index, args) in [
        vec!["--sdk-git", "https://example.invalid/sdk"],
        vec!["--sdk-rev", "main"],
        vec!["--sdk-version", "^0.1"],
        vec!["--sdk-version", "1.2.3-"],
        vec!["--sdk-version", "1.2.3-01"],
        vec!["--sdk-version", "1.2.3+meta..build"],
        vec!["--sdk-version", "0.1.0", "--sdk-rev", sha],
    ]
    .iter()
    .enumerate()
    {
        let destination = root.join(format!("bad{index}"));
        assert!(!create(&destination, args).status.success());
        assert!(!destination.exists());
    }
}

#[test]
#[ignore = "installs CLI from an isolated Git SDK and builds the generated Git consumer"]
fn installed_git_cli_generates_a_portable_static_and_native_application() {
    fn copy_tree(source: &Path, destination: &Path) {
        std::fs::create_dir_all(destination).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            assert!(!kind.is_symlink());
            let target = destination.join(entry.file_name());
            if kind.is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let root = directory("installed-git");
    let sdk = root.join("sdk");
    std::fs::create_dir(&sdk).unwrap();
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for name in ["reknot", "reknot_abi", "reknot_macros"] {
        let source = if parent.join(name).exists() {
            parent.join(name)
        } else {
            parent.join(format!("{name}-{}", env!("CARGO_PKG_VERSION")))
        };
        let target = sdk.join(name);
        copy_tree(&source.join("src"), &target.join("src"));
        let original = source.join("Cargo.toml.orig");
        std::fs::copy(
            if original.exists() {
                original
            } else {
                source.join("Cargo.toml")
            },
            target.join("Cargo.toml"),
        )
        .unwrap();
        for file in ["README.md", "LICENSE-MIT", "LICENSE-APACHE"] {
            std::fs::copy(source.join(file), target.join(file)).unwrap();
        }
    }
    std::fs::write(sdk.join("Cargo.toml"), format!("[workspace]\nmembers = [\"reknot\", \"reknot_abi\", \"reknot_macros\"]\nresolver = \"2\"\n[workspace.package]\nedition = \"2021\"\nrust-version = \"{}\"\nlicense = \"MIT OR Apache-2.0\"\nrepository = \"{}\"\n", env!("CARGO_PKG_RUST_VERSION"), env!("CARGO_PKG_REPOSITORY"))).unwrap();
    std::fs::write(sdk.join(".gitignore"), "/target/\n").unwrap();
    checked(
        Command::new("cargo")
            .current_dir(&sdk)
            .args(["generate-lockfile", "--offline"]),
    );
    checked(
        Command::new("git")
            .current_dir(&sdk)
            .args(["init", "--quiet"]),
    );
    checked(Command::new("git").current_dir(&sdk).args(["add", "."]));
    checked(Command::new("git").current_dir(&sdk).args([
        "-c",
        "user.name=Reknot test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "Isolated SDK fixture",
    ]));
    let sha = checked(
        Command::new("git")
            .current_dir(&sdk)
            .args(["rev-parse", "HEAD"]),
    );
    let sha = String::from_utf8(sha.stdout).unwrap().trim().to_owned();
    let normalized = sdk
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let normalized = normalized.strip_prefix("//?/").unwrap_or(&normalized);
    let escaped: String = normalized
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"/-._~:".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    let url = format!(
        "file://{}{}",
        if escaped.starts_with('/') { "" } else { "/" },
        escaped
    );
    let install = root.join("installed");
    checked(
        Command::new("cargo")
            .args([
                "install",
                "--git",
                &url,
                "--rev",
                &sha,
                "--locked",
                "--bin",
                "reknot-new",
                "--root",
            ])
            .arg(&install)
            .arg("reknot"),
    );
    let executable = install
        .join("bin")
        .join(format!("reknot-new{}", std::env::consts::EXE_SUFFIX));
    let project = root.join("portable-app");
    checked(
        Command::new(executable)
            .current_dir(&root)
            .arg(&project)
            .args(["--sdk-git", &url, "--sdk-rev", &sha]),
    );
    let manifest = std::fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(!manifest.contains("path ="));
    checked(
        Command::new("cargo")
            .current_dir(&project)
            .arg("generate-lockfile"),
    );
    // Resolution can select crates newer than the SDK snapshot's lockfile.
    // Fetch this application's complete graph before checking offline runs.
    checked(
        Command::new("cargo")
            .current_dir(&project)
            .args(["fetch", "--locked"]),
    );
    let schema = checked(Command::new("cargo").current_dir(&project).args([
        "run",
        "-p",
        "portable-app-protocol",
        "--example",
        "schema",
        "--locked",
        "--offline",
    ]));
    let schema: serde_json::Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(schema["format"], "reknot-typed-contract-v2");
    for features in [
        vec!["--no-default-features", "--features", "static-logic"],
        vec![],
    ] {
        use std::io::Write;
        let mut child = Command::new("cargo")
            .current_dir(&project)
            .args(["run", "--locked", "--offline"])
            .args(features)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"hello\n/quit\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("logic-v1 / message 1: HELLO"));
    }
}

#[test]
#[ignore = "builds and runs a generated project, including a live native source edit"]
fn generated_workspace_builds_static_and_native_and_keeps_host_state() {
    let project = directory("consumer").join("starter app");
    let output = create(
        &project,
        &["--name", "my-bot", "--sdk-path", env!("CARGO_MANIFEST_DIR")],
    );
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
