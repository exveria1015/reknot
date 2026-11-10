//! Background Cargo builds exchange paths only; DLL loading stays on the Bevy owner thread.
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn target() -> PathBuf {
    root()
        .join("target/bevy-game-build")
        .join(std::process::id().to_string())
}
fn library() -> PathBuf {
    reknot::development::library_path_in(&target(), "bevy_game_logic")
}

fn spawn(fast: bool) -> std::io::Result<Child> {
    std::fs::create_dir_all(target())?;
    let log = File::create(target().join("build.log"))?;
    let mut command = Command::new(option_env!("CARGO").unwrap_or("cargo"));
    command
        .current_dir(root())
        .args([
            "build",
            "-p",
            "bevy_game_logic",
            "--lib",
            "--no-default-features",
        ])
        .arg("--target-dir")
        .arg(target())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    if fast {
        command.args(["--features", "fast"]);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn()
}

fn failure() -> std::io::Error {
    std::io::Error::other(
        std::fs::read_to_string(target().join("build.log"))
            .unwrap_or_else(|error| error.to_string()),
    )
}

pub fn initial() -> std::io::Result<PathBuf> {
    if !spawn(false)?.wait()?.success() {
        return Err(failure());
    }
    Ok(library())
}

/// Sequential Cargo invocations, using a different target directory from the outer host/test.
pub fn variants() -> std::io::Result<(PathBuf, PathBuf)> {
    let slow_source = initial()?;
    let slow = target().join(reknot::development::library_file_name("game_slow"));
    std::fs::copy(slow_source, &slow)?;
    if !spawn(true)?.wait()?.success() {
        return Err(failure());
    }
    let fast = target().join(reknot::development::library_file_name("game_fast"));
    std::fs::copy(library(), &fast)?;
    Ok((slow, fast))
}

pub struct LiveBuild {
    source: PathBuf,
    seen: String,
    dirty: Option<Instant>,
    child: Option<Child>,
}
impl LiveBuild {
    pub fn new() -> std::io::Result<Self> {
        let source = root().join("examples/bevy_game_logic/src/lib.rs");
        let seen = std::fs::read_to_string(&source)?;
        Ok(Self {
            source,
            seen,
            dirty: None,
            child: None,
        })
    }

    pub fn poll(&mut self) -> std::io::Result<Option<PathBuf>> {
        let source = std::fs::read_to_string(&self.source)?;
        if source != self.seen {
            self.seen = source;
            self.dirty = Some(Instant::now());
        }
        if let Some(child) = &mut self.child {
            if let Some(status) = child.try_wait()? {
                self.child = None;
                if !status.success() {
                    return Err(failure());
                }
                if self.dirty.is_none() {
                    return Ok(Some(library()));
                }
            }
        }
        if self.child.is_none()
            && self
                .dirty
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(250))
        {
            self.child = Some(spawn(false)?);
            self.dirty = None;
            println!("building logic in background (frames continue)");
        }
        Ok(None)
    }
}
impl Drop for LiveBuild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn log_path() -> impl AsRef<Path> {
    target().join("build.log")
}
