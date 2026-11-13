use super::{
    BridgeDriver, BridgeOptions, Error, ModuleBuilder, ModuleClient, ModuleHost, ModuleRestart,
};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::task::{Context, Waker};
use std::time::{Duration, Instant};

/// Source location, Cargo package and save debounce. Cargo's metadata resolves
/// workspace targets and local dependencies; paths need not follow this repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadMode {
    Drain,
    Restart,
}

pub struct DevOptions {
    pub manifest_path: PathBuf,
    pub package: String,
    pub debounce: Duration,
    pub reload_mode: ReloadMode,
}
impl DevOptions {
    pub fn new(manifest_path: impl Into<PathBuf>, package: impl Into<String>) -> Self {
        Self {
            manifest_path: manifest_path.into(),
            package: package.into(),
            debounce: Duration::from_millis(200),
            reload_mode: ReloadMode::Drain,
        }
    }
}
impl DevOptions {
    pub fn with_reload_mode(mut self, mode: ReloadMode) -> Self {
        self.reload_mode = mode;
        self
    }
}
#[derive(Debug)]
pub enum ReloadEvent {
    Building,
    Restarting,
    Reloaded { generation: u64 },
    BuildFailed { diagnostics: String },
    Rejected { error: Error },
    Superseded,
    WatchFailed { message: String },
}
impl std::fmt::Display for ReloadEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Restarting => f.write_str("Restart requested; admission paused while registered roots and children finish"),
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
    watch_refresh: Arc<AtomicBool>,
    watch_errors: mpsc::Receiver<String>,
    seen: u64,
    handled: u64,
    changed: Instant,
    debounce: Duration,
    build: Option<(u64, mpsc::Receiver<Result<BuildOutput, String>>)>,
    bridges: Vec<BridgeDriver>,
    reload_mode: ReloadMode,
    restart: Option<ModuleRestart>,
}
impl DevModule {
    /// # Safety
    /// The project, all its future edits/dependencies/build scripts, and loaded
    /// artifacts must be trusted and obey `ModuleBuilder::load`'s contract.
    /// This grants continued loading of this project's successful builds only.
    pub unsafe fn start(builder: ModuleBuilder, options: DevOptions) -> Result<Self, Error> {
        let project = Project::discover(&options)?;
        let revision = Arc::new(AtomicU64::new(0));
        let watch_refresh = Arc::new(AtomicBool::new(false));
        let (watcher, watch_errors) = project.watch(revision.clone(), watch_refresh.clone())?;
        // Observe edits even during the initial build; update() will rebuild them.
        let library = project.build().map_err(Error::Development)?;
        let host = builder.load(library)?;
        Ok(Self {
            host,
            project,
            _watcher: watcher,
            revision,
            watch_refresh,
            watch_errors,
            seen: 0,
            handled: 0,
            changed: Instant::now(),
            debounce: options.debounce,
            build: None,
            bridges: Vec::new(),
            reload_mode: options.reload_mode,
            restart: None,
        })
    }
    pub fn host(&self) -> &ModuleHost {
        &self.host
    }
    /// Create a Send + Sync endpoint. `update()` drives its owner-side requests
    /// and cancellation cleanup, including when no source files change.
    /// Progress through this endpoint follows the update cadence. For executor-
    /// driven requests, use `host().bridge()` and poll its driver independently.
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
        let _ = self
            .host
            .poll_workers(&mut Context::from_waker(Waker::noop()));
        if let Some(restart) = &mut self.restart {
            return match Pin::new(restart).poll(&mut Context::from_waker(Waker::noop())) {
                std::task::Poll::Pending => None,
                std::task::Poll::Ready(result) => {
                    self.restart = None;
                    Some(match result {
                        Ok(generation) => ReloadEvent::Reloaded { generation },
                        Err(error) => ReloadEvent::Rejected { error },
                    })
                }
            };
        }
        if self.watch_refresh.swap(false, Ordering::AcqRel) {
            // A watch follows a directory instance, not just its path. Re-arm
            // before building, even if missing inputs make Cargo metadata fail.
            match self
                .project
                .watch(self.revision.clone(), self.watch_refresh.clone())
            {
                Ok((watcher, errors)) => {
                    self._watcher = watcher;
                    self.watch_errors = errors;
                    // Cover edits made before the new registrations were ready.
                    self.revision.fetch_add(1, Ordering::Release);
                }
                Err(error) => {
                    self.watch_refresh.store(true, Ordering::Release);
                    return Some(ReloadEvent::WatchFailed {
                        message: error.to_string(),
                    });
                }
            }
        }
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
                        match output
                            .project
                            .watch(self.revision.clone(), self.watch_refresh.clone())
                        {
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
                                match self.reload_mode {
                                    ReloadMode::Drain => match self.host.activate(candidate) {
                                        Ok(generation) => ReloadEvent::Reloaded { generation },
                                        Err(error) => ReloadEvent::Rejected { error },
                                    },
                                    ReloadMode::Restart => {
                                        match self.host.restart_prepared(candidate) {
                                            Ok(restart) => {
                                                self.restart = Some(restart);
                                                ReloadEvent::Restarting
                                            }
                                            Err(error) => ReloadEvent::Rejected { error },
                                        }
                                    }
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
    rust_directories: HashSet<PathBuf>,
    excluded: HashSet<PathBuf>,
}
impl Inputs {
    fn exclude_outputs(&mut self, outputs: &[PathBuf]) -> Result<(), Error> {
        self.excluded.extend(outputs.iter().cloned());
        // Resolve inputs and outputs together: either side can use a different
        // spelling from the filesystem, including a previously absent root.
        for paths in [
            &mut self.files,
            &mut self.directories,
            &mut self.rust_directories,
            &mut self.excluded,
        ] {
            *paths = std::mem::take(paths)
                .into_iter()
                .map(|path| normalize(&path))
                .collect();
        }
        if outputs.iter().map(|path| normalize(path)).any(|output| {
            self.directories
                .iter()
                .any(|dir| output.starts_with(dir) || dir.starts_with(&output))
                || self
                    .rust_directories
                    .iter()
                    .any(|dir| dir.starts_with(&output))
        }) || self
            .files
            .iter()
            .any(|file| self.excluded.iter().any(|dir| file.starts_with(dir)))
        {
            return Err(Error::Configuration("Cargo output directory overlaps source inputs; move build output outside source directories".into()));
        }
        Ok(())
    }

    fn excludes(&self, path: &Path) -> bool {
        self.excluded.iter().any(|dir| path.starts_with(dir))
            || path.components().any(|part| part.as_os_str() == ".git")
    }

    fn watch_roots(&self) -> impl Iterator<Item = &Path> {
        self.files
            .iter()
            .filter_map(|path| path.parent())
            .chain(self.directories.iter().map(PathBuf::as_path))
            .chain(self.rust_directories.iter().map(PathBuf::as_path))
    }

    fn needs_watch_refresh(&self, path: &Path) -> bool {
        !self.excludes(path) && self.watch_roots().any(|root| root.starts_with(path))
    }

    fn contains(&self, path: &Path) -> bool {
        // Explicit source inputs, never target/, cache copies, logs or metadata.
        if self.excludes(path) {
            return false;
        }
        self.files.contains(path)
            || self.directories.iter().any(|dir| path.starts_with(dir))
            || ((path.extension().is_none() || path.extension().is_some_and(|ext| ext == "rs"))
                && self
                    .rust_directories
                    .iter()
                    .any(|dir| path.starts_with(dir)))
    }
}
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_root: PathBuf,
    target_directory: PathBuf,
    // Older Cargo metadata may omit the separate intermediate output path.
    build_directory: Option<PathBuf>,
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
            .current_dir(manifest.parent().unwrap())
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
        // Match Cargo's configuration search from the manifest's directory.
        // This also watches member-local and ancestor configuration files.
        for directory in manifest.parent().unwrap().ancestors() {
            inputs.files.insert(directory.join(".cargo/config.toml"));
            inputs.files.insert(directory.join(".cargo/config"));
        }
        for dependency in &metadata.packages {
            if dependency.source.is_some() || !needed.contains(&dependency.id) {
                continue;
            }
            let manifest = normalize(&dependency.manifest_path);
            let root = manifest.parent().unwrap();
            inputs.excluded.insert(root.join("target"));
            inputs.files.insert(manifest.clone());
            inputs.files.insert(root.join("build.rs"));
            inputs.directories.insert(root.join("src"));
            for target in &dependency.targets {
                let source = normalize(&target.src_path);
                // Rust modules beside custom lib/bin/build-script entry points
                // need recursive watching too, including a crate-root lib.rs.
                inputs
                    .rust_directories
                    .insert(source.parent().unwrap().to_path_buf());
                inputs.files.insert(source);
            }
        }
        let output_roots = [
            normalize(&metadata.target_directory),
            normalize(
                metadata
                    .build_directory
                    .as_deref()
                    .unwrap_or(&metadata.target_directory),
            ),
        ];
        // Cargo resolves config, environment overrides and path templates.
        // Intermediate artifacts need the same exclusion as final artifacts.
        inputs.exclude_outputs(&output_roots)?;
        let target = output_roots[0].join("reknot-typed").join(&options.package);
        // Cargo first creates a missing build directory as a randomly named
        // sibling and renames it. Prepare both metadata roots and the actual
        // --target-dir before watching so setup neither looks like a source
        // edit nor races Windows directory observers during that rename.
        for output in output_roots.iter().chain(std::iter::once(&target)) {
            std::fs::create_dir_all(output).map_err(|e| {
                Error::Development(format!(
                    "cannot create Cargo output directory {}: {e}",
                    output.display()
                ))
            })?;
        }
        // Creation can reveal the spelling of a previously missing source root
        // (for example src versus SRC on Windows). Recheck before compiling.
        inputs.exclude_outputs(&output_roots)?;
        let target = normalize(&target);
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
        watch_refresh: Arc<AtomicBool>,
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
                        ) =>
                    {
                        // Ignore known output paths before canonicalize opens
                        // them on Windows. Cargo can be renaming a temporary
                        // output directory at the same time. Unmatched spelling
                        // still goes through normalization and the filters below.
                        let paths: Vec<_> = event
                            .paths
                            .iter()
                            .filter(|path| !inputs.excludes(path))
                            .map(|path| normalize(path))
                            .collect();
                        let topology_changed =
                            matches!(
                                event.kind,
                                EventKind::Create(_)
                                    | EventKind::Remove(_)
                                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
                            ) && paths.iter().any(|path| inputs.needs_watch_refresh(path));
                        if topology_changed {
                            watch_refresh.store(true, Ordering::Release);
                        }
                        if topology_changed
                            || paths.iter().any(|path| {
                                // Windows also reports a parent's last-write
                                // change for edits under an excluded directory.
                                // Directory metadata alone is not a source edit.
                                inputs.contains(path)
                                    && !(matches!(event.kind, EventKind::Modify(kind)
                                        if !matches!(kind, notify::event::ModifyKind::Name(_)))
                                        && path.is_dir())
                            })
                        {
                            revision.fetch_add(1, Ordering::Release);
                        }
                    }
                    Err(error) => {
                        let _ = tx.try_send(error.to_string());
                    }
                    _ => {}
                }
            })
            .map_err(|e| Error::Development(format!("cannot start source watcher: {e}")))?;
        // Keep nonrecursive ancestor watches so absent roots can be created and
        // removed/replaced roots can be registered again. Register recursive
        // roots last so overlapping watches cannot downgrade their coverage.
        let parents: HashSet<_> = self
            .inputs
            .watch_roots()
            .flat_map(Path::ancestors)
            .collect();
        for parent in parents {
            if parent.is_dir() {
                watcher
                    .watch(parent, RecursiveMode::NonRecursive)
                    .map_err(|e| {
                        Error::Development(format!("cannot watch {}: {e}", parent.display()))
                    })?;
            }
        }
        for directory in self.inputs.directories.union(&self.inputs.rust_directories) {
            if directory.is_dir() {
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
            .current_dir(self.manifest.parent().unwrap())
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
#[cfg(not(windows))]
fn normalize(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(windows)]
fn normalize(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    // Match Cargo and notify using filesystem spelling, without assuming every
    // Windows directory is case-insensitive. Missing/removed paths still need
    // filtering, so resolve the longest existing ancestor and keep the suffix.
    let path = path
        .ancestors()
        .find_map(|ancestor| {
            ancestor.canonicalize().ok().map(|mut resolved| {
                resolved.extend(path.strip_prefix(ancestor).unwrap().components());
                resolved
            })
        })
        .unwrap_or_else(|| path.to_path_buf());
    // Cargo needs ordinary drive/UNC paths. Keep OsStr components intact rather
    // than round-tripping filenames through a lossy Unicode conversion.
    let mut components = path.components();
    if let Some(Component::Prefix(prefix)) = components.next() {
        let root = match prefix.kind() {
            Prefix::VerbatimDisk(drive) => Some(PathBuf::from(format!("{}:", char::from(drive)))),
            Prefix::VerbatimUNC(server, share) => {
                let mut root = std::ffi::OsString::from(r"\\");
                root.push(server);
                root.push(r"\");
                root.push(share);
                Some(PathBuf::from(root))
            }
            _ => None,
        };
        if let Some(mut root) = root {
            root.extend(components);
            return root;
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(not(miri))]
    fn discovery_prepares_actual_cargo_target_before_watch_registration() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("reknot-prewatch-{}-{nonce:x}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(directory.join(".cargo")).unwrap();
        std::fs::write(
            directory.join("Cargo.toml"),
            "[package]\nname = \"prewatch_outputs\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"lib.rs\"\ncrate-type = [\"cdylib\"]\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("Cargo.lock"),
            "version = 4\n[[package]]\nname = \"prewatch_outputs\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        std::fs::write(directory.join("lib.rs"), "").unwrap();
        std::fs::write(
            directory.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"Final/Artifacts\"\nbuild-dir = \"Intermediate\"\n",
        )
        .unwrap();
        // No watcher or compiler has run yet. Check the actual --target-dir,
        // not just Cargo metadata's roots: missing this leaf caused CI failures.
        let project = Project::discover(&DevOptions::new(
            directory.join("Cargo.toml"),
            "prewatch_outputs",
        ))
        .unwrap();
        assert!(project.target.ends_with("reknot-typed/prewatch_outputs"));
        assert!(project.target.is_dir());
        assert!(!project.target.join("debug").exists());
        assert!(project.inputs.excludes(&project.target));
    }

    #[test]
    fn legacy_metadata_without_separate_build_directory_is_accepted() {
        let metadata: Metadata = serde_json::from_str(
            r#"{"packages":[],"workspace_root":"project","target_directory":"project/target","resolve":null}"#,
        )
        .unwrap();
        assert!(metadata.build_directory.is_none());
    }

    #[test]
    fn watch_refresh_tracks_roots_and_ancestors_without_including_output() {
        let root = PathBuf::from("project");
        let inputs = Inputs {
            files: HashSet::from([root.join("Cargo.toml"), root.join(".cargo/config.toml")]),
            directories: HashSet::from([root.join("src")]),
            rust_directories: HashSet::from([root.join("implementation")]),
            excluded: HashSet::from([root.join("target"), root.join("build-output")]),
        };
        assert!(inputs.needs_watch_refresh(&root));
        for path in [".cargo", "src", "implementation"] {
            assert!(inputs.needs_watch_refresh(&root.join(path)), "{path}");
        }
        for path in [
            "Cargo.toml",
            ".cargo/config.toml",
            "src/handler.rs",
            "src/nested",
            "implementation/value.rs",
            "target",
            "build-output",
            ".git",
            "unrelated",
        ] {
            assert!(!inputs.needs_watch_refresh(&root.join(path)), "{path}");
        }
    }

    #[test]
    fn source_filter_excludes_build_outputs_and_accepts_atomic_manifest_saves() {
        let root = PathBuf::from("project");
        let inputs = Inputs {
            files: HashSet::from([root.join("Cargo.toml"), root.join("Cargo.lock")]),
            directories: HashSet::from([root.join("src")]),
            ..Default::default()
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

    #[test]
    fn custom_target_modules_include_nested_edits_but_exclude_output_and_git() {
        let root = PathBuf::from("project");
        let inputs = Inputs {
            rust_directories: HashSet::from([root.clone()]),
            excluded: HashSet::from([root.join("target"), root.join("build-output")]),
            ..Default::default()
        };
        for path in [
            "lib.rs",
            "implementation/value.rs",
            "implementation/nested/value.rs",
            "implementation/nested",
        ] {
            assert!(inputs.contains(&root.join(path)), "{path}");
        }
        for path in [
            "target/debug/build/generated.rs",
            "build-output/generated.rs",
            ".git/backup.rs",
            "notes.log",
            "value.rs.tmp",
        ] {
            assert!(!inputs.contains(&root.join(path)), "{path}");
        }
    }
}
