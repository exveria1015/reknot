use super::{BridgeDriver, BridgeOptions, Error, ModuleBuilder, ModuleClient, ModuleHost};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::task::{Context, Waker};
use std::time::{Duration, Instant};

/// Source location, Cargo package and save debounce. Cargo's metadata resolves
/// workspace targets and local dependencies; paths need not follow this repository.
pub struct DevOptions {
    pub manifest_path: PathBuf,
    pub package: String,
    pub debounce: Duration,
}
impl DevOptions {
    pub fn new(manifest_path: impl Into<PathBuf>, package: impl Into<String>) -> Self {
        Self {
            manifest_path: manifest_path.into(),
            package: package.into(),
            debounce: Duration::from_millis(200),
        }
    }
}
#[derive(Debug)]
pub enum ReloadEvent {
    Building,
    Reloaded { generation: u64 },
    BuildFailed { diagnostics: String },
    Rejected { error: Error },
    Superseded,
    WatchFailed { message: String },
}
impl std::fmt::Display for ReloadEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Building => f.write_str("Building edited logic; current handlers remain available"),
            Self::Reloaded { generation } => write!(f, "New events use generation {generation}"),
            Self::BuildFailed { diagnostics } => write!(f, "Build failed; keeping the current generation. Fix and save to retry.\n{diagnostics}"),
            Self::Rejected { error } => write!(f, "Candidate rejected; keeping the current generation: {error}"),
            Self::Superseded => f.write_str("Source changed during the build; discarding this candidate and rebuilding"),
            Self::WatchFailed { message } => write!(f, "Source watch failed: {message}; restart the development host if saving no longer triggers builds"),
        }
    }
}

/// Poll `update()` regularly on the owner thread (for example every 25 ms).
/// Initial compilation is synchronous; subsequent builds run on a resident host
/// thread. Only one build runs at a time. Saving does not replay events.
pub struct DevModule {
    host: ModuleHost,
    project: Project,
    _watcher: RecommendedWatcher,
    revision: Arc<AtomicU64>,
    watch_errors: mpsc::Receiver<String>,
    seen: u64,
    handled: u64,
    changed: Instant,
    debounce: Duration,
    build: Option<(u64, mpsc::Receiver<Result<BuildOutput, String>>)>,
    bridges: Vec<BridgeDriver>,
}
impl DevModule {
    /// # Safety
    /// The project, all its future edits/dependencies/build scripts, and loaded
    /// artifacts must be trusted and obey `ModuleBuilder::load`'s contract.
    /// This grants continued loading of this project's successful builds only.
    pub unsafe fn start(builder: ModuleBuilder, options: DevOptions) -> Result<Self, Error> {
        let project = Project::discover(&options)?;
        let revision = Arc::new(AtomicU64::new(0));
        let (watcher, watch_errors) = project.watch(revision.clone())?;
        // Observe edits even during the initial build; update() will rebuild them.
        let library = project.build().map_err(Error::Development)?;
        let host = builder.load(library)?;
        Ok(Self {
            host,
            project,
            _watcher: watcher,
            revision,
            watch_errors,
            seen: 0,
            handled: 0,
            changed: Instant::now(),
            debounce: options.debounce,
            build: None,
            bridges: Vec::new(),
        })
    }
    pub fn host(&self) -> &ModuleHost {
        &self.host
    }
    /// Create a Send + Sync endpoint. `update()` drives its owner-side requests
    /// and cancellation cleanup, including when no source files change.
    pub fn bridge(&mut self, options: BridgeOptions) -> Result<ModuleClient, Error> {
        let (client, driver) = self.host.bridge(options)?;
        self.bridges.push(driver);
        Ok(client)
    }
    /// Nonblocking maintenance. Call while idle and while requests are running.
    /// Handles build completion, source edits, and dropped-request cleanup.
    pub fn update(&mut self) -> Option<ReloadEvent> {
        let _ = self
            .host
            .poll_cleanup(&mut Context::from_waker(Waker::noop()));
        self.bridges.retain_mut(|driver| {
            Pin::new(driver)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        });
        if let Ok(message) = self.watch_errors.try_recv() {
            return Some(ReloadEvent::WatchFailed { message });
        }
        let current = self.revision.load(Ordering::Acquire);
        if current != self.seen {
            self.seen = current;
            self.changed = Instant::now();
        }
        if let Some((revision, receiver)) = &self.build {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err("Cargo build worker stopped unexpectedly".into())
                }
            };
            let revision = *revision;
            self.build = None;
            self.handled = revision;
            let result = match result {
                Ok(output) => {
                    if output.project.inputs != self.project.inputs
                        || output.project.target != self.project.target
                    {
                        // Register new dependency inputs before trusting a build.
                        // Rebuild once under that watch set to cover earlier edits.
                        match output.project.watch(self.revision.clone()) {
                            Ok((watcher, errors)) => {
                                self._watcher = watcher;
                                self.watch_errors = errors;
                                self.project = output.project;
                                self.revision.fetch_add(1, Ordering::Release);
                                return Some(ReloadEvent::Superseded);
                            }
                            Err(error) => {
                                return Some(ReloadEvent::WatchFailed {
                                    message: error.to_string(),
                                })
                            }
                        }
                    }
                    self.project = output.project;
                    output.library
                }
                Err(error) => Err(error),
            };
            if self.revision.load(Ordering::Acquire) != revision {
                return Some(ReloadEvent::Superseded);
            }
            return Some(match result {
                Err(diagnostics) => ReloadEvent::BuildFailed { diagnostics },
                Ok(path) => {
                    // SAFETY: start's caller authorized every replacement produced
                    // by this trusted project. Preparation still checks contracts.
                    match unsafe { self.host.prepare(path) } {
                        Err(error) => ReloadEvent::Rejected { error },
                        Ok(candidate) => {
                            if self.revision.load(Ordering::Acquire) != revision {
                                ReloadEvent::Superseded
                            } else {
                                match self.host.activate(candidate) {
                                    Ok(generation) => ReloadEvent::Reloaded { generation },
                                    Err(error) => ReloadEvent::Rejected { error },
                                }
                            }
                        }
                    }
                }
            });
        }
        if self.seen == self.handled || self.changed.elapsed() < self.debounce {
            return None;
        }
        let project = self.project.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("reknot-typed-build".into())
            .spawn(move || {
                let result =
                    Project::discover(&DevOptions::new(&project.manifest, &project.package))
                        .map_err(|e| e.to_string())
                        .map(|project| {
                            let library = project.build();
                            BuildOutput { project, library }
                        });
                let _ = tx.send(result);
            });
        match worker {
            Ok(_) => {
                self.build = Some((self.seen, rx));
                Some(ReloadEvent::Building)
            }
            Err(error) => {
                self.handled = self.seen;
                Some(ReloadEvent::BuildFailed {
                    diagnostics: error.to_string(),
                })
            }
        }
    }
}

#[derive(Clone)]
struct Project {
    manifest: PathBuf,
    package: String,
    package_id: String,
    target: PathBuf,
    inputs: Inputs,
}
struct BuildOutput {
    project: Project,
    library: Result<PathBuf, String>,
}
#[derive(Clone, Default, PartialEq, Eq)]
struct Inputs {
    files: HashSet<PathBuf>,
    directories: HashSet<PathBuf>,
}
impl Inputs {
    fn contains(&self, path: &Path) -> bool {
        // Explicit source inputs, never target/, cache copies, logs or metadata.
        self.files.contains(path) || self.directories.iter().any(|dir| path.starts_with(dir))
    }
}
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_root: PathBuf,
    target_directory: PathBuf,
    resolve: Option<Resolve>,
}
#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: PathBuf,
    source: Option<String>,
    targets: Vec<Target>,
}
#[derive(Deserialize)]
struct Target {
    kind: Vec<String>,
    src_path: PathBuf,
}
#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}
#[derive(Deserialize)]
struct Node {
    id: String,
    dependencies: Vec<String>,
}
impl Project {
    fn discover(options: &DevOptions) -> Result<Self, Error> {
        let manifest = options.manifest_path.canonicalize().map_err(|e| {
            Error::Development(format!(
                "cannot open {}: {e}",
                options.manifest_path.display()
            ))
        })?;
        // Cargo must see the same spelling for workspace roots and path
        // dependencies. Windows canonicalize() adds a verbatim-path prefix;
        // mixing that with Cargo's ordinary paths breaks workspace inheritance.
        let manifest = normalize(&manifest);
        let output = Command::new("cargo")
            .args([
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--manifest-path",
            ])
            .arg(&manifest)
            .output()
            .map_err(|e| Error::Development(format!("cannot start cargo metadata: {e}")))?;
        if !output.status.success() {
            return Err(Error::Development(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        let metadata: Metadata = serde_json::from_slice(&output.stdout)
            .map_err(|e| Error::Development(format!("invalid Cargo metadata: {e}")))?;
        let package = metadata
            .packages
            .iter()
            .find(|p| p.name == options.package && p.source.is_none())
            .ok_or_else(|| {
                Error::Configuration(format!(
                    "local Cargo package {} was not found",
                    options.package
                ))
            })?;
        if !package
            .targets
            .iter()
            .any(|target| target.kind.iter().any(|kind| kind == "cdylib"))
        {
            return Err(Error::Configuration(format!(
                "{} needs [lib] crate-type = [\"cdylib\"]",
                options.package
            )));
        }
        let mut needed = HashSet::from([package.id.clone()]);
        if let Some(resolve) = &metadata.resolve {
            loop {
                let previous = needed.len();
                for node in &resolve.nodes {
                    if needed.contains(&node.id) {
                        needed.extend(node.dependencies.iter().cloned());
                    }
                }
                if previous == needed.len() {
                    break;
                }
            }
        }
        let mut inputs = Inputs::default();
        let workspace = normalize(&metadata.workspace_root);
        inputs.files.insert(workspace.join("Cargo.toml"));
        inputs.files.insert(workspace.join("Cargo.lock"));
        inputs.files.insert(workspace.join(".cargo/config.toml"));
        inputs.files.insert(workspace.join(".cargo/config"));
        for dependency in &metadata.packages {
            if dependency.source.is_some() || !needed.contains(&dependency.id) {
                continue;
            }
            let manifest = normalize(&dependency.manifest_path);
            let root = manifest.parent().unwrap();
            inputs.files.insert(manifest.clone());
            inputs.files.insert(root.join("build.rs"));
            inputs.directories.insert(root.join("src"));
            for target in &dependency.targets {
                // Custom lib paths and build scripts are also inputs.
                inputs.files.insert(normalize(&target.src_path));
            }
        }
        let target = normalize(&metadata.target_directory)
            .join("reknot-typed")
            .join(&options.package);
        if inputs
            .directories
            .iter()
            .any(|dir| target.starts_with(dir) || dir.starts_with(&target))
            || inputs.files.iter().any(|file| file.starts_with(&target))
        {
            return Err(Error::Configuration("Cargo target directory overlaps source inputs; move build output outside source directories".into()));
        }
        Ok(Self {
            manifest,
            package: options.package.clone(),
            package_id: package.id.clone(),
            target,
            inputs,
        })
    }
    fn watch(
        &self,
        revision: Arc<AtomicU64>,
    ) -> Result<(RecommendedWatcher, mpsc::Receiver<String>), Error> {
        let inputs = self.inputs.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let mut watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                match event {
                    Ok(event)
                        if matches!(
                            event.kind,
                            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                        ) && event
                            .paths
                            .iter()
                            .any(|path| inputs.contains(&normalize(path))) =>
                    {
                        revision.fetch_add(1, Ordering::Release);
                    }
                    Err(error) => {
                        let _ = tx.try_send(error.to_string());
                    }
                    _ => {}
                }
            })
            .map_err(|e| Error::Development(format!("cannot start source watcher: {e}")))?;
        let parents: HashSet<_> = self
            .inputs
            .files
            .iter()
            .filter_map(|p| p.parent())
            .collect();
        for parent in parents {
            if parent.exists() {
                watcher
                    .watch(parent, RecursiveMode::NonRecursive)
                    .map_err(|e| {
                        Error::Development(format!("cannot watch {}: {e}", parent.display()))
                    })?;
            }
        }
        for directory in &self.inputs.directories {
            if directory.exists() {
                watcher
                    .watch(directory, RecursiveMode::Recursive)
                    .map_err(|e| {
                        Error::Development(format!("cannot watch {}: {e}", directory.display()))
                    })?;
            }
        }
        Ok((watcher, rx))
    }
    fn build(&self) -> Result<PathBuf, String> {
        let output = Command::new("cargo")
            .args(["build", "--locked", "--lib", "--package"])
            .arg(&self.package)
            .arg("--manifest-path")
            .arg(&self.manifest)
            .arg("--target-dir")
            .arg(&self.target)
            .arg("--message-format=json-render-diagnostics")
            .env("CARGO_INCREMENTAL", "0")
            .output()
            .map_err(|e| format!("cannot run cargo build: {e}"))?;
        let mut diagnostics = String::new();
        let mut library = None;
        for line in output.stdout.split(|b| *b == b'\n') {
            let Ok(event) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            if event["reason"] == "compiler-message" {
                if let Some(rendered) = event["message"]["rendered"].as_str() {
                    append_diagnostic(&mut diagnostics, rendered);
                }
            }
            if event["reason"] == "compiler-artifact" && event["package_id"] == self.package_id {
                if let Some(files) = event["filenames"].as_array() {
                    for filename in files.iter().filter_map(|v| v.as_str()) {
                        if filename.ends_with(std::env::consts::DLL_SUFFIX) {
                            library = Some(PathBuf::from(filename));
                        }
                    }
                }
            }
        }
        append_diagnostic(&mut diagnostics, &String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            return Err(diagnostics);
        }
        library.ok_or_else(|| "Cargo succeeded without a native cdylib artifact; check crate-type and target configuration".into())
    }
}
fn append_diagnostic(output: &mut String, input: &str) {
    let remaining = (64 * 1024usize).saturating_sub(output.len());
    let mut end = input.len().min(remaining);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&input[..end]);
}
fn normalize(path: &Path) -> PathBuf {
    // Windows Cargo metadata and notify may disagree on the extended-path prefix.
    #[cfg(windows)]
    {
        let path = path.to_string_lossy();
        if let Some(rest) = path.strip_prefix("\\\\?\\UNC\\") {
            return PathBuf::from(format!("\\\\{rest}"));
        }
        if let Some(rest) = path.strip_prefix("\\\\?\\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_filter_excludes_build_outputs_and_accepts_atomic_manifest_saves() {
        let root = PathBuf::from("project");
        let inputs = Inputs {
            files: HashSet::from([root.join("Cargo.toml"), root.join("Cargo.lock")]),
            directories: HashSet::from([root.join("src")]),
        };
        assert!(inputs.contains(&root.join("Cargo.toml")));
        assert!(inputs.contains(&root.join("src/nested/handler.rs")));
        for path in [
            "target/debug/logic.dll",
            "target/reknot-typed/out.json",
            ".git/index",
            "notes.log",
            "Cargo.toml.tmp",
        ] {
            assert!(!inputs.contains(&root.join(path)));
        }
    }
}
