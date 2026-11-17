//! Owner-thread layered development; Cargo and artifact reads run on a host worker.
use super::coff::retention::collect_selected_jit;
use super::{
    jit_retention_snapshot, CoffImageId, CoffImageState, CoffLimits, CoffPolicy,
    CoffReclaimOptions, CoffReclaimReport, CoffRetentionSnapshot, JitError, LayeredArtifact,
    LayeredEngine, TrustedLayeredArtifact,
};
use crate::watch_inputs::{normalize, Inputs};
use crate::{HostResources, RequestRouter};
use notify::RecommendedWatcher;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc,
};
use std::task::{Context, Poll, Waker};

#[path = "jit_development_target.rs"]
mod target;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use target::RouterTarget;
pub(crate) use target::{Activation, DevelopmentTarget, TargetFailure};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRevision(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayeredInputKind {
    Logic,
    Foundation,
}
#[derive(Clone, Debug)]
pub struct LayeredInput {
    pub path: PathBuf,
    pub kind: LayeredInputKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ManagedPressure {
    /// Preserve old requests; wait until all their ownership has ended.
    #[default]
    Wait,
    /// Cancel roots of the oldest retired generation on their next executor poll.
    /// Cleanup/join and completed-handle Drop still precede physical collection.
    CancelOldest,
}
#[derive(Clone, Copy, Debug)]
pub struct ManagedReloadOptions {
    /// Includes active, candidate and retired images published by this session.
    /// At least two, allowing a candidate without discarding the working handler.
    pub max_live_generations: usize,
    pub pressure: ManagedPressure,
}
impl Default for ManagedReloadOptions {
    fn default() -> Self {
        Self {
            max_live_generations: 2,
            pressure: ManagedPressure::Wait,
        }
    }
}
#[derive(Clone, Debug)]
pub struct LayeredDevOptions {
    pub manifest_path: PathBuf,
    pub package: String,
    /// Cargo's dylib target name (without platform prefix/suffix).
    pub support_target: String,
    /// External script path. Empty selects the built-in Rust producer; prefer
    /// `builtin` when no custom producer is needed.
    pub builder: PathBuf,
    pub cache_directory: PathBuf,
    pub toolchain: String,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub offline: bool,
    /// Built-in producer only: default Cargo dev optimization for support and
    /// dependencies (Some(1)). None inherits Cargo configuration; logic stays 0.
    pub support_optimization: Option<u32>,
    pub additional_inputs: Vec<LayeredInput>,
    /// Enables reuse only when all build-script/include/environment inputs are
    /// known and declared. False always rechecks the foundation with Cargo.
    pub declared_inputs_complete: bool,
    pub debounce: Duration,
    pub limits: CoffLimits,
    /// Cumulative Router allowance for KeepLoaded; ignored by managed admission.
    pub max_router_generations: usize,
    pub max_cache_bytes: u64,
    /// Applies only to the additional unsafe start_managed entry point.
    pub managed_reload: ManagedReloadOptions,
}
impl LayeredDevOptions {
    /// Use the SDK's Rust producer with Cargo/rustup and the platform linker.
    /// No external script, PowerShell or Python installation is required.
    pub fn builtin(
        manifest: impl Into<PathBuf>,
        package: impl Into<String>,
        support_target: impl Into<String>,
        cache: impl Into<PathBuf>,
    ) -> Self {
        Self::new(manifest, package, support_target, PathBuf::new(), cache)
    }
    pub fn new(
        manifest: impl Into<PathBuf>,
        package: impl Into<String>,
        support_target: impl Into<String>,
        builder: impl Into<PathBuf>,
        cache: impl Into<PathBuf>,
    ) -> Self {
        Self {
            manifest_path: manifest.into(),
            package: package.into(),
            support_target: support_target.into(),
            builder: builder.into(),
            cache_directory: cache.into(),
            toolchain: "stable".into(),
            features: Vec::new(),
            no_default_features: false,
            offline: false,
            support_optimization: Some(1),
            additional_inputs: Vec::new(),
            declared_inputs_complete: false,
            debounce: Duration::from_millis(50),
            limits: CoffLimits::default(),
            max_router_generations: 32,
            max_cache_bytes: 4 * 1024 * 1024 * 1024,
            managed_reload: ManagedReloadOptions::default(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayeredDevState {
    Idle,
    Debouncing,
    Building {
        revision: SourceRevision,
    },
    Superseded,
    /// A typed Restart has paused admission while registered work finishes.
    Restarting {
        revision: SourceRevision,
    },
    Activated {
        generation: u64,
        revision: SourceRevision,
    },
    BuildFailed,
    Rejected,
    RestartRequired {
        current: String,
        next: String,
    },
    WatchUnavailable,
    WaitingForRetirement {
        image: CoffImageId,
    },
    RecoveryRequired {
        image: CoffImageId,
    },
    RetentionBlocked {
        images: usize,
        bytes: usize,
        required_bytes: Option<usize>,
    },
    BudgetExhausted,
    CacheFull,
    Stopped,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LayeredBuildMetrics {
    pub worker_ms: f64,
    pub build_ms: f64,
    pub lock_wait_ms: f64,
    pub artifact_read_ms: f64,
    pub owner_ms: f64,
}
#[derive(Clone, Debug)]
pub struct LayeredDevSnapshot {
    pub state: LayeredDevState,
    pub revision: SourceRevision,
    pub has_active_generation: bool,
    pub load_attempts: usize,
    pub keep_loaded_attempts: usize,
    pub policy: CoffPolicy,
    pub live_images: Vec<CoffImageId>,
    pub reclamation: CoffReclaimReport,
    pub diagnostic: Option<String>,
    pub manifest: Option<PathBuf>,
    pub metrics: LayeredBuildMetrics,
}
#[derive(Clone, Debug)]
pub struct LayeredDevEvent {
    pub state: LayeredDevState,
    pub diagnostic: Option<String>,
}

#[derive(Clone)]
struct Plan {
    inputs: Inputs,
    logic: Inputs,
    foundation_key: String,
}
struct Job {
    revision: u64,
    id: u64,
    options: LayeredDevOptions,
    environment: BTreeMap<OsString, OsString>,
    session: String,
    directory: PathBuf,
    prior: Option<(String, PathBuf)>,
    recheck: bool,
    cancellation: Option<BuildCancellation>,
}
struct BuildCancellation {
    revision: Arc<AtomicU64>,
    refresh: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}
impl Job {
    fn is_current(&self) -> bool {
        self.cancellation.as_ref().is_none_or(|cancel| {
            cancel.revision.load(Ordering::Acquire) == self.revision
                && !cancel.refresh.load(Ordering::Acquire)
                && !cancel.stop.load(Ordering::Acquire)
        })
    }
    fn check_current(&self, stage: &'static str) -> Result<(), BuildFailure> {
        if self.is_current() {
            Ok(())
        } else {
            Err(superseded(stage))
        }
    }
}
struct Built {
    artifact: LayeredArtifact,
    manifest: PathBuf,
    metrics: LayeredBuildMetrics,
}
struct BuildOutput {
    // Successful discovery remains useful even when compilation/capture fails:
    // newly discovered inputs still need watches so edits can recover the build.
    plan: Plan,
    artifact: Result<Built, BuildFailure>,
}
struct Produced {
    manifest: PathBuf,
    captured: Vec<u8>,
    metrics: LayeredBuildMetrics,
}
struct BuildAttempt {
    plan: Plan,
    produced: Result<Produced, BuildFailure>,
    reused: bool,
}
struct BuildFailure {
    state: LayeredDevState,
    message: String,
}
impl From<super::builder::BuildError> for BuildFailure {
    fn from(error: super::builder::BuildError) -> Self {
        Self {
            state: if error.cache_full {
                LayeredDevState::CacheFull
            } else {
                LayeredDevState::BuildFailed
            },
            message: error.message,
        }
    }
}
fn build_failed(message: String) -> BuildFailure {
    BuildFailure {
        state: LayeredDevState::BuildFailed,
        message,
    }
}
fn superseded(stage: &'static str) -> BuildFailure {
    BuildFailure {
        state: LayeredDevState::Superseded,
        message: format!("obsolete build stopped at {stage}"),
    }
}
struct Completed {
    revision: u64,
    id: u64,
    result: Result<BuildOutput, BuildFailure>,
}
struct PendingArtifact {
    revision: u64,
    artifact: TrustedLayeredArtifact,
    required_bytes: Option<usize>,
}

/// Owner-thread source builds, checked COFF/ELF loading and raw Router admission.
/// Drop stops publication without waiting for Cargo; `shutdown` joins the worker.
pub struct LayeredDevSession {
    core: LayeredDevCore,
    target: RouterTarget,
}
impl LayeredDevSession {
    /// # Safety
    /// Trust this project, future edits, dependencies, build scripts and producer
    /// to satisfy `LayeredArtifact::into_trusted` for every build.
    pub unsafe fn start(
        options: LayeredDevOptions,
        resources: HostResources,
    ) -> Result<Self, String> {
        Self::start_inner(options, resources, false)
    }
    /// # Safety
    /// In addition to `start`, every build must satisfy
    /// `TrustedLayeredArtifact::into_managed`, including external callback return.
    pub unsafe fn start_managed(
        options: LayeredDevOptions,
        resources: HostResources,
    ) -> Result<Self, String> {
        Self::start_inner(options, resources, true)
    }
    fn start_inner(
        options: LayeredDevOptions,
        resources: HostResources,
        managed: bool,
    ) -> Result<Self, String> {
        let router_options = crate::RouterOptions {
            generations: crate::GenerationBudget::new(options.max_router_generations)
                .map_err(|e| e.to_string())?,
            ..Default::default()
        };
        let core = LayeredDevCore::start(options, managed, router_options.generations.clone())?;
        Ok(Self {
            core,
            target: RouterTarget {
                router: None,
                resources,
                options: router_options,
            },
        })
    }
    pub fn router(&self) -> Option<&RequestRouter> {
        self.target.router.as_ref()
    }
    pub fn router_mut(&mut self) -> Option<&mut RequestRouter> {
        self.target.router.as_mut()
    }
    pub fn snapshot(&self) -> LayeredDevSnapshot {
        self.core.snapshot(self.target.has_active())
    }
    /// Explicit retry/rescan; never resets the engine or attempt allowance.
    pub fn retry(&mut self) {
        self.core.retry();
    }
    pub fn reconfigure(
        &mut self,
        toolchain: String,
        features: Vec<String>,
        no_default_features: bool,
    ) {
        self.core
            .reconfigure(toolchain, features, no_default_features);
    }
    /// Nonblocking progress. Call regularly, including while requests run.
    pub fn poll_update(&mut self) -> Option<LayeredDevEvent> {
        self.poll_update_with_context(&mut Context::from_waker(Waker::noop()))
    }
    /// Cleanup and worker notifications can wake this executor. Source/build
    /// completion still needs periodic polling; poll owned RequestTasks separately.
    pub fn poll_update_with_context(&mut self, cx: &mut Context<'_>) -> Option<LayeredDevEvent> {
        self.core.poll_update(&mut self.target, cx)
    }
    /// Collect this session's managed images only, on the owner thread.
    pub fn collect_retired(&mut self, options: CoffReclaimOptions) -> Option<CoffReclaimReport> {
        self.core.collect_retired(&mut self.target, options)
    }
    pub fn shutdown(&mut self) -> Result<(), String> {
        self.core.shutdown()
    }
    #[cfg(all(test, windows, target_arch = "x86_64", target_env = "msvc"))]
    pub(crate) fn test_build_blocking(
        &mut self,
    ) -> Result<(LayeredArtifact, LayeredBuildMetrics), String> {
        self.core.test_build_blocking()
    }
}

/// Not Send: linking, entry, Router and collection stay on the creating thread.
/// Drop stops publication but does not wait for Cargo. Use shutdown for a joined stop.
pub(crate) struct LayeredDevCore {
    options: LayeredDevOptions,
    environment: BTreeMap<OsString, OsString>,
    session: String,
    directory: PathBuf,
    plan: Plan,
    watcher: RecommendedWatcher,
    watch_errors: mpsc::Receiver<String>,
    revision: Arc<AtomicU64>,
    refresh: Arc<AtomicBool>,
    foundation_dirty: Arc<AtomicBool>,
    observed: u64,
    submitted: u64,
    changed_at: Instant,
    next_job: u64,
    running: Option<(u64, u64)>,
    pending: Option<PendingArtifact>,
    live_images: Vec<CoffImageId>,
    reclamation: CoffReclaimReport,
    tx: Option<mpsc::SyncSender<Job>>,
    rx: mpsc::Receiver<Completed>,
    worker: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    prior: Option<(String, PathBuf)>,
    engine: Option<LayeredEngine>,
    restart_revision: Option<SourceRevision>,
    managed: bool,
    generations: crate::GenerationBudget,
    state: LayeredDevState,
    diagnostic: Option<String>,
    manifest: Option<PathBuf>,
    metrics: LayeredBuildMetrics,
}
impl LayeredDevCore {
    pub(crate) fn start(
        mut options: LayeredDevOptions,
        managed: bool,
        generations: crate::GenerationBudget,
    ) -> Result<Self, String> {
        if !cfg!(any(
            all(windows, target_arch = "x86_64", target_env = "msvc"),
            all(
                target_os = "linux",
                target_arch = "x86_64",
                target_env = "gnu"
            )
        )) {
            return Err(JitError::UnsupportedTarget.to_string());
        }
        options.manifest_path = normalize(
            &options
                .manifest_path
                .canonicalize()
                .map_err(|e| e.to_string())?,
        );
        if !options.builder.as_os_str().is_empty() {
            options.builder =
                normalize(&options.builder.canonicalize().map_err(|e| e.to_string())?);
        }
        options.cache_directory =
            normalize(&std::path::absolute(&options.cache_directory).map_err(|e| e.to_string())?);
        for input in &mut options.additional_inputs {
            input.path = normalize(&std::path::absolute(&input.path).map_err(|e| e.to_string())?);
        }
        if options.max_cache_bytes == 0
            || options.max_router_generations == 0
            || options.package.is_empty()
            || options.support_target.is_empty()
            || (managed
                && (options.managed_reload.max_live_generations < 2
                    || options.limits.max_retained_images < 2))
        {
            return Err("invalid development configuration".into());
        }
        let environment = std::env::vars_os().collect();
        let plan = discover(&options, &environment)?;
        let session = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos()
        );
        let directory = options.cache_directory.join("sessions").join(&session);
        fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let lease = File::create_new(directory.join("lease")).map_err(|e| e.to_string())?;
        lease.lock().map_err(|e| e.to_string())?;
        let revision = Arc::new(AtomicU64::new(1));
        let refresh = Arc::new(AtomicBool::new(false));
        let foundation_dirty = Arc::new(AtomicBool::new(true));
        let (watcher, watch_errors) = plan.inputs.watch_classified(
            revision.clone(),
            refresh.clone(),
            foundation_dirty.clone(),
            plan.logic.clone(),
        )?;
        let (tx, jobs) = mpsc::sync_channel::<Job>(1);
        let (results, rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::Builder::new()
            .name("reknot-layered-build".into())
            .spawn(move || {
                let _lease = lease;
                while let Ok(job) = jobs.recv() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let revision = job.revision;
                    let id = job.id;
                    let result = build(job);
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if results
                        .send(Completed {
                            revision,
                            id,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            options,
            environment,
            session,
            directory,
            plan,
            watcher,
            watch_errors,
            revision,
            refresh,
            foundation_dirty,
            observed: 1,
            submitted: 0,
            changed_at: Instant::now(),
            next_job: 1,
            running: None,
            pending: None,
            live_images: Vec::new(),
            reclamation: CoffReclaimReport::default(),
            tx: Some(tx),
            rx,
            worker: Some(worker),
            stop,
            prior: None,
            engine: None,
            restart_revision: None,
            managed,
            generations,
            state: LayeredDevState::Debouncing,
            diagnostic: None,
            manifest: None,
            metrics: Default::default(),
        })
    }
    pub(crate) fn snapshot(&self, has_active_generation: bool) -> LayeredDevSnapshot {
        LayeredDevSnapshot {
            state: self.state.clone(),
            revision: SourceRevision(self.revision.load(Ordering::Acquire)),
            has_active_generation,
            load_attempts: self.engine.as_ref().map_or(0, LayeredEngine::load_attempts),
            keep_loaded_attempts: self
                .engine
                .as_ref()
                .map_or(0, LayeredEngine::keep_loaded_attempts),
            policy: if self.managed {
                CoffPolicy::Managed
            } else {
                CoffPolicy::KeepLoaded
            },
            live_images: self.live_images.clone(),
            reclamation: self.reclamation.clone(),
            diagnostic: self.diagnostic.clone(),
            manifest: self.manifest.clone(),
            metrics: self.metrics.clone(),
        }
    }
    fn event(
        &mut self,
        state: LayeredDevState,
        diagnostic: Option<String>,
    ) -> Option<LayeredDevEvent> {
        self.state = state.clone();
        self.diagnostic = diagnostic.clone();
        Some(LayeredDevEvent { state, diagnostic })
    }
    /// Explicit retry/rescan; never resets the existing engine or its attempt budget.
    pub fn retry(&mut self) {
        self.foundation_dirty.store(true, Ordering::Release);
        self.revision.fetch_add(1, Ordering::AcqRel);
    }
    /// File watching cannot detect environment/toolchain selection changes. Capture
    /// the current host environment explicitly and recheck with these build settings.
    pub fn reconfigure(
        &mut self,
        toolchain: String,
        features: Vec<String>,
        no_default_features: bool,
    ) {
        self.options.toolchain = toolchain;
        self.options.features = features;
        self.options.no_default_features = no_default_features;
        self.environment = std::env::vars_os().collect();
        self.retry();
    }
    fn rewatch(&mut self) -> Result<(), String> {
        let (watcher, errors) = self.plan.inputs.watch_classified(
            self.revision.clone(),
            self.refresh.clone(),
            self.foundation_dirty.clone(),
            self.plan.logic.clone(),
        )?;
        self.watcher = watcher;
        self.watch_errors = errors;
        self.retry();
        Ok(())
    }
    pub(crate) fn poll_update<T: DevelopmentTarget>(
        &mut self,
        target: &mut T,
        cx: &mut Context<'_>,
    ) -> Option<LayeredDevEvent> {
        target.maintain(cx, self.managed);
        if self.stop.load(Ordering::Acquire) {
            return None;
        }
        if self.managed {
            self.collect_retired(target, CoffReclaimOptions::default());
        }
        if let Some(revision) = self.restart_revision {
            return match target.poll_restart(cx) {
                Poll::Pending => None,
                Poll::Ready(result) => {
                    self.restart_revision = None;
                    match result {
                        Ok(generation) => self.event(
                            LayeredDevState::Activated {
                                generation,
                                revision,
                            },
                            None,
                        ),
                        Err(error) => self.reject_target(error),
                    }
                }
            };
        }
        if let Ok(message) = self.watch_errors.try_recv() {
            self.refresh.store(true, Ordering::Release);
            return self.event(LayeredDevState::WatchUnavailable, Some(message));
        }
        if self.refresh.swap(false, Ordering::AcqRel) {
            if let Err(e) = self.rewatch() {
                self.refresh.store(true, Ordering::Release);
                return self.event(LayeredDevState::WatchUnavailable, Some(e));
            }
        }
        let revision = self.revision.load(Ordering::Acquire);
        if revision != self.observed {
            self.observed = revision;
            self.changed_at = Instant::now();
        }
        match self.rx.try_recv() {
            Ok(done) => {
                if self.running != Some((done.revision, done.id)) {
                    return self.event(
                        LayeredDevState::Rejected,
                        Some("unexpected worker job".into()),
                    );
                }
                self.running = None;
                if self.stale(done.revision) {
                    let error = match &done.result {
                        Err(error) => Some(error),
                        Ok(output) => output.artifact.as_ref().err(),
                    };
                    let diagnostic = error
                        .filter(|error| error.state == LayeredDevState::Superseded)
                        .map(|error| error.message.clone());
                    return self.event(LayeredDevState::Superseded, diagnostic);
                }
                let output = match done.result {
                    Ok(output) => output,
                    Err(e) => return self.event(e.state, Some(e.message)),
                };
                if output.plan.inputs != self.plan.inputs || output.plan.logic != self.plan.logic {
                    self.plan = output.plan;
                    if let Err(e) = self.rewatch() {
                        self.refresh.store(true, Ordering::Release);
                        return self.event(LayeredDevState::WatchUnavailable, Some(e));
                    }
                    return self.event(
                        LayeredDevState::Superseded,
                        Some("input graph changed; rebuilding under refreshed watches".into()),
                    );
                }
                self.plan = output.plan;
                let built = match output.artifact {
                    Ok(built) => built,
                    Err(e) => return self.event(e.state, Some(e.message)),
                };
                self.prior = Some((self.plan.foundation_key.clone(), built.manifest.clone()));
                self.manifest = Some(built.manifest);
                self.metrics = built.metrics;
                if let Some(engine) = &self.engine {
                    if engine.foundation_id() != built.artifact.foundation_id() {
                        return self.event(
                            LayeredDevState::RestartRequired {
                                current: engine.foundation_id().into(),
                                next: built.artifact.foundation_id().into(),
                            },
                            None,
                        );
                    }
                }
                // The continuing unsafe start contract applies only to captured
                // bytes returned by this session's own producer.
                self.pending = Some(PendingArtifact {
                    revision: done.revision,
                    artifact: unsafe { built.artifact.into_trusted() },
                    required_bytes: None,
                });
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return self
                    .changed_event(LayeredDevState::BuildFailed, "build worker exited".into());
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if let Some(mut pending) = self.pending.take() {
            if self.stale(pending.revision) {
                return self.event(LayeredDevState::Superseded, None);
            }
            if let Some((state, message)) = self.admission_block(target, pending.required_bytes) {
                self.pending = Some(pending);
                return self.changed_event(state, message);
            }
            let start = Instant::now();
            if self.engine.is_none() {
                match LayeredEngine::open(&pending.artifact, self.options.limits) {
                    Ok(engine) => self.engine = Some(engine),
                    Err(e) => return self.reject(e),
                }
            }
            if pending.required_bytes.is_none() {
                match self
                    .engine
                    .as_mut()
                    .unwrap()
                    .required_image_bytes(&pending.artifact)
                {
                    Ok(bytes) => pending.required_bytes = Some(bytes),
                    Err(e) => return self.reject(e),
                }
            }
            // Exact page requirement is known before allocation/entry. Keep one
            // captured candidate while waiting; never rebuild it on every poll.
            if let Some((state, message)) = self.admission_block(target, pending.required_bytes) {
                self.pending = Some(pending);
                return self.changed_event(state, message);
            }
            if self.stale(pending.revision) {
                return self.event(LayeredDevState::Superseded, None);
            }
            let engine = self.engine.as_mut().unwrap();
            let loaded = if self.managed {
                engine.load_managed(unsafe { pending.artifact.into_managed() })
            } else {
                engine.load(pending.artifact)
            };
            // Entry rejection is a published, pinned image too. Keep its identity
            // so repeated failed edits cannot accumulate unobserved generations.
            if let Some(id) = engine.last_image_id() {
                self.live_images.push(id);
            }
            let loaded = match loaded {
                Ok(h) => h,
                Err(e) => return self.reject(e),
            };
            let candidate = match target.prepare(loaded) {
                Ok(candidate) => candidate,
                Err(error) => return self.reject_target(error),
            };
            if self.stale(pending.revision) {
                return self.event(LayeredDevState::Superseded, None);
            }
            let generation = match target.activate(candidate) {
                Ok(Activation::Ready(generation)) => generation,
                #[cfg(feature = "typed")]
                Ok(Activation::Restarting) => {
                    let revision = SourceRevision(pending.revision);
                    self.restart_revision = Some(revision);
                    self.metrics.owner_ms = start.elapsed().as_secs_f64() * 1000.0;
                    return self.event(LayeredDevState::Restarting { revision }, None);
                }
                Err(error) => return self.reject_target(error),
            };
            self.metrics.owner_ms = start.elapsed().as_secs_f64() * 1000.0;
            return self.event(
                LayeredDevState::Activated {
                    generation,
                    revision: SourceRevision(pending.revision),
                },
                None,
            );
        }
        if self.running.is_none()
            && self.submitted != revision
            && self.changed_at.elapsed() >= self.options.debounce
        {
            if let Some((state, message)) = self.admission_block(target, None) {
                return self.changed_event(state, message);
            }
            let id = self.next_job;
            self.next_job = match id.checked_add(1) {
                Some(next) => next,
                None => {
                    return self.changed_event(
                        LayeredDevState::BudgetExhausted,
                        "development job identity exhausted; restart required".into(),
                    )
                }
            };
            let job = Job {
                revision,
                id,
                options: self.options.clone(),
                environment: self.environment.clone(),
                session: self.session.clone(),
                directory: self.directory.join("jobs").join(id.to_string()),
                prior: self.prior.clone(),
                recheck: self.foundation_dirty.swap(false, Ordering::AcqRel),
                cancellation: Some(BuildCancellation {
                    revision: self.revision.clone(),
                    refresh: self.refresh.clone(),
                    stop: self.stop.clone(),
                }),
            };
            if self.tx.as_ref().unwrap().try_send(job).is_err() {
                return self.event(
                    LayeredDevState::BuildFailed,
                    Some("build worker unavailable".into()),
                );
            }
            self.running = Some((revision, id));
            self.submitted = revision;
            return self.event(
                LayeredDevState::Building {
                    revision: SourceRevision(revision),
                },
                None,
            );
        }
        None
    }

    fn changed_event(
        &mut self,
        state: LayeredDevState,
        message: String,
    ) -> Option<LayeredDevEvent> {
        if self.state == state && self.diagnostic.as_ref() == Some(&message) {
            None
        } else {
            self.event(state, Some(message))
        }
    }

    fn wait_for_retirement(
        &self,
        target: &impl DevelopmentTarget,
        image: CoffImageId,
    ) -> (LayeredDevState, String) {
        if self.options.managed_reload.pressure == ManagedPressure::CancelOldest {
            target.cancel_retired(image);
        }
        (LayeredDevState::WaitingForRetirement { image },
         format!("waiting for image {} ({:?}); requests must be polled/dropped and cleanup joined before collection",
             image.get(), self.options.managed_reload.pressure))
    }

    fn admission_block(
        &mut self,
        target: &impl DevelopmentTarget,
        required_bytes: Option<usize>,
    ) -> Option<(LayeredDevState, String)> {
        if !self.managed {
            let used = self
                .engine
                .as_ref()
                .map_or(0, LayeredEngine::keep_loaded_attempts);
            if used >= self.options.limits.load.max_generations {
                return Some((LayeredDevState::BudgetExhausted,
                    format!("KeepLoaded load limit reached ({used}); restart required to continue updating")));
            }
            let budget = &self.generations;
            let limit = budget.limit().min(self.options.max_router_generations);
            if budget.used() >= limit {
                return Some((LayeredDevState::BudgetExhausted,
                    format!("Router KeepLoaded limit reached ({}); restart required to continue updating", budget.used())));
            }
        }
        let snapshot = jit_retention_snapshot();
        self.prune_images(&snapshot);
        let active = target.active_image();
        let oldest = self
            .live_images
            .iter()
            .copied()
            .find(|id| Some(*id) != active);
        if self.managed {
            if let Some(image) = snapshot.images.iter().find(|i| {
                self.live_images.contains(&i.id)
                    && matches!(
                        i.state,
                        CoffImageState::Pinned | CoffImageState::MemoryRetained
                    )
            }) {
                return Some((
                    LayeredDevState::RecoveryRequired { image: image.id },
                    format!(
                        "image {} cannot be reclaimed: {:?}; restart required",
                        image.id.get(),
                        image.reasons
                    ),
                ));
            }
            if self.live_images.len() >= self.options.managed_reload.max_live_generations {
                if let Some(id) = oldest {
                    return Some(self.wait_for_retirement(target, id));
                }
            }
        }
        if snapshot.images.len() >= self.options.limits.max_retained_images
            || required_bytes.unwrap_or(1)
                > self
                    .options
                    .limits
                    .max_retained_bytes
                    .saturating_sub(snapshot.retained_bytes)
        {
            if self.managed {
                if let Some(id) = oldest {
                    return Some(self.wait_for_retirement(target, id));
                }
            }
            return Some((LayeredDevState::RetentionBlocked {
                images: snapshot.images.len(), bytes: snapshot.retained_bytes, required_bytes,
            }, "JIT retention capacity unavailable; current requests continue, KeepLoaded/Pinned images require process restart".into()));
        }
        None
    }

    fn prune_images(&mut self, snapshot: &CoffRetentionSnapshot) {
        self.live_images
            .retain(|id| snapshot.images.iter().any(|image| image.id == *id));
    }
    fn stale(&self, revision: u64) -> bool {
        self.revision.load(Ordering::Acquire) != revision || self.refresh.load(Ordering::Acquire)
    }
    fn reject(&mut self, error: JitError) -> Option<LayeredDevEvent> {
        let state = if matches!(
            error,
            JitError::GenerationLimit { .. } | JitError::RetentionLimit
        ) {
            LayeredDevState::BudgetExhausted
        } else {
            LayeredDevState::Rejected
        };
        self.event(state, Some(error.to_string()))
    }
    fn reject_target(&mut self, error: TargetFailure) -> Option<LayeredDevEvent> {
        let state = if error.budget {
            LayeredDevState::BudgetExhausted
        } else {
            LayeredDevState::Rejected
        };
        self.event(state, Some(error.message))
    }
    /// Release completed Router bookkeeping and collect this session's managed
    /// images only. Also called by poll_update; other controllers are not collected.
    pub(crate) fn collect_retired(
        &mut self,
        target: &mut impl DevelopmentTarget,
        options: CoffReclaimOptions,
    ) -> Option<CoffReclaimReport> {
        if !self.managed {
            return None;
        }
        target.release_retired();
        let report = collect_selected_jit(options, &self.live_images);
        self.reclamation = report.clone();
        self.prune_images(&jit_retention_snapshot());
        Some(report)
    }
    pub fn shutdown(&mut self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| "build worker panicked".to_owned())?;
        }
        self.running = None;
        self.pending = None;
        self.state = LayeredDevState::Stopped;
        Ok(())
    }

    // A synchronous comparator for the real edit-to-response test. Reuse the
    // exact worker pipeline, frozen environment, input plan and producer flags.
    // This is deliberately absent from the public/release API.
    #[cfg(all(test, windows, target_arch = "x86_64", target_env = "msvc"))]
    pub(crate) fn test_build_blocking(
        &mut self,
    ) -> Result<(LayeredArtifact, LayeredBuildMetrics), String> {
        assert!(self.worker.is_none() && self.stop.load(Ordering::Acquire));
        let id = self.next_job;
        self.next_job += 1;
        let output = build(Job {
            revision: id,
            id,
            options: self.options.clone(),
            environment: self.environment.clone(),
            session: self.session.clone(),
            directory: self.directory.join("jobs").join(id.to_string()),
            prior: self.prior.clone(),
            recheck: false,
            cancellation: None,
        })
        .map_err(|e| e.message)?;
        let built = output.artifact.map_err(|e| e.message)?;
        self.prior = Some((output.plan.foundation_key, built.manifest));
        Ok((built.artifact, built.metrics))
    }
}
impl Drop for LayeredDevCore {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.tx.take();
    }
}

pub(super) fn command(
    program: impl AsRef<std::ffi::OsStr>,
    env: &BTreeMap<OsString, OsString>,
    manifest: &Path,
) -> Command {
    let mut c = Command::new(program);
    c.env_clear()
        .envs(env)
        .current_dir(manifest.parent().unwrap());
    c
}
pub(super) fn capture(
    mut command: Command,
    limit: usize,
) -> Result<(bool, Vec<u8>, String), String> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    fn drain(mut pipe: impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
        let mut saved = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = pipe.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let keep = n.min(limit.saturating_sub(saved.len()));
            saved.extend_from_slice(&buf[..keep]);
        }
        Ok(saved)
    }
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let stdout = std::thread::spawn(move || drain(out, limit));
    let stderr = std::thread::spawn(move || drain(err, 64 * 1024));
    let status = child.wait().map_err(|e| e.to_string())?;
    let stdout = stdout
        .join()
        .map_err(|_| "stdout reader panicked")?
        .map_err(|e| e.to_string())?;
    let stderr = stderr
        .join()
        .map_err(|_| "stderr reader panicked")?
        .map_err(|e| e.to_string())?;
    Ok((
        status.success(),
        stdout,
        String::from_utf8_lossy(&stderr).into_owned(),
    ))
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(super) fn read_limit(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err(format!("input exceeds {limit} bytes: {}", path.display()));
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_root: PathBuf,
    target_directory: PathBuf,
    build_directory: Option<PathBuf>,
    resolve: Resolve,
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
    name: String,
    src_path: PathBuf,
    crate_types: Vec<String>,
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

fn discover(
    options: &LayeredDevOptions,
    environment: &BTreeMap<OsString, OsString>,
) -> Result<Plan, String> {
    let mut c = command("cargo", environment, &options.manifest_path);
    // Resolve build-directory templates under the producer's target override.
    c.env("CARGO_TARGET_DIR", options.cache_directory.join("cargo"));
    c.arg(format!("+{}", options.toolchain))
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            super::artifact::native_target(),
            "--manifest-path",
        ])
        .arg(&options.manifest_path);
    if options.offline {
        c.arg("--offline");
    }
    if options.no_default_features {
        c.arg("--no-default-features");
    }
    if !options.features.is_empty() {
        c.arg("--features").arg(
            options
                .features
                .iter()
                .map(|f| {
                    if f.contains('/') {
                        f.clone()
                    } else {
                        format!("{}/{f}", options.package)
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    let (ok, metadata, error) = capture(c, 16 * 1024 * 1024)?;
    if !ok {
        return Err(error);
    }
    let mut rustc = command("rustup", environment, &options.manifest_path);
    rustc.args(["run", &options.toolchain, "rustc", "-vV"]);
    let (ok, compiler, error) = capture(rustc, 64 * 1024)?;
    if !ok {
        return Err(error);
    }
    plan_from_metadata(options, environment, &metadata, &compiler)
}

fn plan_from_metadata(
    options: &LayeredDevOptions,
    environment: &BTreeMap<OsString, OsString>,
    metadata: &[u8],
    compiler: &[u8],
) -> Result<Plan, String> {
    let m: Metadata = serde_json::from_slice(metadata).map_err(|e| e.to_string())?;
    let logic = m
        .packages
        .iter()
        .find(|p| p.name == options.package && p.source.is_none())
        .ok_or("logic package not found")?;
    let mut needed = HashSet::from([logic.id.clone()]);
    loop {
        let size = needed.len();
        for n in &m.resolve.nodes {
            if needed.contains(&n.id) {
                needed.extend(n.dependencies.iter().cloned());
            }
        }
        if needed.len() == size {
            break;
        }
    }
    if !m.packages.iter().any(|p| {
        needed.contains(&p.id)
            && p.id != logic.id
            && p.targets.iter().any(|t| {
                t.name == options.support_target && t.crate_types.iter().any(|k| k == "dylib")
            })
    }) {
        return Err("support dylib must be a dependency of logic".into());
    }
    let mut inputs = Inputs::default();
    let mut logic_inputs = Inputs::default();
    for p in &m.packages {
        if p.source.is_some() || !needed.contains(&p.id) {
            continue;
        }
        let root = p.manifest_path.parent().unwrap();
        if p.id != logic.id {
            logic_inputs.excluded.insert(normalize(root));
        }
        inputs
            .files
            .extend([p.manifest_path.clone(), root.join("build.rs")]);
        inputs.directories.insert(root.join("src"));
        inputs.excluded.insert(root.join("target"));
        for t in &p.targets {
            inputs.files.insert(t.src_path.clone());
            inputs
                .rust_directories
                .insert(t.src_path.parent().unwrap().into());
            if p.id == logic.id
                && t.crate_types
                    .iter()
                    .any(|k| k == "lib" || k == "cdylib" || k == "rlib")
            {
                logic_inputs.files.insert(t.src_path.clone());
                logic_inputs.directories.insert(root.join("src"));
                logic_inputs
                    .rust_directories
                    .insert(t.src_path.parent().unwrap().into());
            }
        }
    }
    inputs.files.extend([
        m.workspace_root.join("Cargo.toml"),
        m.workspace_root.join("Cargo.lock"),
    ]);
    if !options.builder.as_os_str().is_empty() {
        inputs.files.insert(options.builder.clone());
    }
    for dir in options.manifest_path.parent().unwrap().ancestors() {
        inputs.files.extend([
            dir.join(".cargo/config"),
            dir.join(".cargo/config.toml"),
            dir.join("rust-toolchain"),
            dir.join("rust-toolchain.toml"),
        ]);
    }
    if let Some(home) = environment
        .get(&OsString::from("CARGO_HOME"))
        .map(PathBuf::from)
        .or_else(|| {
            environment
                .get(&OsString::from(if cfg!(windows) {
                    "USERPROFILE"
                } else {
                    "HOME"
                }))
                .map(|p| PathBuf::from(p).join(".cargo"))
        })
    {
        inputs
            .files
            .extend([home.join("config"), home.join("config.toml")]);
    }
    for input in &options.additional_inputs {
        let path = normalize(&std::path::absolute(&input.path).map_err(|e| e.to_string())?);
        if path.is_dir() {
            inputs.directories.insert(path.clone());
        } else {
            inputs.files.insert(path.clone());
        }
        if input.kind == LayeredInputKind::Logic {
            if path.is_dir() {
                logic_inputs.directories.insert(path);
            } else {
                logic_inputs.files.insert(path);
            }
        } else {
            logic_inputs.excluded.insert(path);
        }
    }
    let outputs = vec![
        m.target_directory.clone(),
        m.build_directory.unwrap_or(m.target_directory),
        options.cache_directory.clone(),
    ];
    inputs.exclude_outputs(&outputs)?;
    for out in outputs
        .iter()
        .chain(std::iter::once(&options.cache_directory.join("cargo")))
    {
        fs::create_dir_all(out).map_err(|e| e.to_string())?;
    }
    inputs.exclude_outputs(&outputs)?;
    logic_inputs.exclude_outputs(&outputs)?;
    // Hash declared foundation contents, resolver, compiler, builder and the exact
    // child environment. No Cargo-private .fingerprint format is part of this key.
    let mut hasher = Sha256::new();
    if options.builder.as_os_str().is_empty() {
        hasher.update(b"reknot-rust-builder-v1");
    }
    hasher.update(metadata);
    hasher.update(options.toolchain.as_bytes());
    for (k, v) in environment {
        hasher.update((k.len() as u64).to_le_bytes());
        hasher.update(k.as_encoded_bytes());
        hasher.update((v.len() as u64).to_le_bytes());
        hasher.update(v.as_encoded_bytes());
    }
    hasher.update(compiler);
    let mut files = inputs.files.clone();
    let mut seen = HashSet::new();
    fn walk(
        dir: &Path,
        inputs: &Inputs,
        files: &mut HashSet<PathBuf>,
        seen: &mut HashSet<PathBuf>,
    ) -> Result<(), String> {
        if inputs.excludes(dir) || !dir.is_dir() {
            return Ok(());
        }
        let real = dir.canonicalize().map_err(|e| e.to_string())?;
        if !seen.insert(real) {
            return Ok(());
        }
        for e in fs::read_dir(dir).map_err(|e| e.to_string())? {
            let p = e.map_err(|e| e.to_string())?.path();
            if inputs.excludes(&p) {
                continue;
            }
            if p.is_dir() {
                walk(&p, inputs, files, seen)?;
            } else if inputs.contains(&p) {
                files.insert(p);
            }
        }
        Ok(())
    }
    for dir in inputs.directories.union(&inputs.rust_directories) {
        walk(dir, &inputs, &mut files, &mut seen)?;
    }
    let mut files: Vec<_> = files
        .into_iter()
        .filter(|p| !inputs.is_logic_input(p, &logic_inputs))
        .collect();
    files.sort();
    for path in files {
        hasher.update(path.as_os_str().as_encoded_bytes());
        if path.is_file() {
            hasher.update(hash(&read_limit(&path, 64 * 1024 * 1024)?));
        } else {
            hasher.update(b"<absent>");
        }
    }
    Ok(Plan {
        inputs,
        logic: logic_inputs,
        foundation_key: format!("{:x}", hasher.finalize()),
    })
}

fn build(job: Job) -> Result<BuildOutput, BuildFailure> {
    let start = Instant::now();
    job.check_current("before_preparation")?;
    let mut attempt = build_attempt(&job, "first", job.recheck)?;
    // A failed reuse gets one fresh preparation, including metadata and Plan.
    // Keep the last successful discovery even if preparing the retry fails.
    if attempt.reused
        && attempt.produced.as_ref().is_err_and(|e| {
            !matches!(
                e.state,
                LayeredDevState::CacheFull | LayeredDevState::Superseded
            )
        })
        && job.is_current()
    {
        match build_attempt(&job, "recheck", true) {
            Ok(rechecked) => attempt = rechecked,
            Err(error) => attempt.produced = Err(error),
        }
    }
    let artifact = attempt.produced.and_then(|produced| {
        job.check_current("before_artifact_read")?;
        finish_build(&job, &attempt.plan, produced, start)
    });
    Ok(BuildOutput {
        plan: attempt.plan,
        artifact,
    })
}

fn build_attempt(job: &Job, suffix: &str, recheck: bool) -> Result<BuildAttempt, BuildFailure> {
    job.check_current("before_preparation")?;
    let output = job
        .directory
        .join(suffix)
        .join(if cfg!(target_os = "linux") {
            "logic.o"
        } else {
            "logic.obj"
        });
    let mut options = super::builder::BuildOptions::new(
        &job.options.manifest_path,
        &job.options.package,
        &job.options.support_target,
        &job.options.cache_directory,
        &output,
    );
    options.toolchain = job.options.toolchain.clone();
    options.features = job.options.features.clone();
    options.no_default_features = job.options.no_default_features;
    options.offline = job.options.offline;
    options.support_optimization = job.options.support_optimization;
    options.max_cache_bytes = job.options.max_cache_bytes;
    let prepared = if job.options.builder.as_os_str().is_empty() {
        Some(
            super::builder::prepare_with_environment(&options, &job.environment)
                .map_err(BuildFailure::from)?,
        )
    } else {
        None
    };
    job.check_current("after_preparation")?;
    let plan = match &prepared {
        Some(prepared) => plan_from_metadata(
            &job.options,
            prepared.environment(),
            prepared.metadata(),
            prepared.compiler_info(),
        ),
        None => discover(&job.options, &job.environment),
    }
    .map_err(build_failed)?;
    let reuse = if !recheck && job.options.declared_inputs_complete {
        job.prior
            .as_ref()
            .filter(|(key, _)| key == &plan.foundation_key)
            .map(|(_, path)| path.as_path())
    } else {
        None
    };
    let reused = reuse.is_some();
    let produced = produce(job, &plan, &output, prepared, reuse);
    Ok(BuildAttempt {
        plan,
        produced,
        reused,
    })
}

fn produce(
    job: &Job,
    plan: &Plan,
    output: &Path,
    prepared: Option<super::builder::PreparedBuild<'_>>,
    reuse: Option<&Path>,
) -> Result<Produced, BuildFailure> {
    job.check_current("after_plan")?;
    let dir = output.parent().expect("job output has a directory");
    fs::create_dir_all(dir).map_err(|e| build_failed(e.to_string()))?;
    let receipt = dir.join("build-result.json");
    let (ok, diagnostic, value) = if let Some(prepared) = prepared {
        let result = match prepared
            .build_while_current(reuse, &|_| job.is_current())
            .map_err(BuildFailure::from)?
        {
            super::builder::BuildOutcome::Built(result) => result,
            super::builder::BuildOutcome::Superseded(stage) => return Err(superseded(stage)),
        };
        let mut value = serde_json::to_value(result).map_err(|e| build_failed(e.to_string()))?;
        value["version"] = 1.into();
        value["status"] = "Built".into();
        value["session"] = job.session.clone().into();
        value["revision"] = job.revision.into();
        value["job"] = job.id.into();
        value["foundation_build_key"] = plan.foundation_key.clone().into();
        File::create_new(&receipt)
            .and_then(|mut f| f.write_all(serde_json::to_vec_pretty(&value).unwrap().as_slice()))
            .map_err(|e| build_failed(e.to_string()))?;
        (true, String::new(), value)
    } else {
        let mut c = command(
            if cfg!(target_os = "linux") {
                "python3"
            } else {
                "pwsh"
            },
            &job.environment,
            &job.options.manifest_path,
        );
        if !cfg!(target_os = "linux") {
            c.args(["-NoProfile", "-NonInteractive", "-File"]);
        }
        c.arg(&job.options.builder)
            .arg("-ManifestPath")
            .arg(&job.options.manifest_path)
            .arg("-Package")
            .arg(&job.options.package)
            .arg("-TargetDirectory")
            .arg(job.options.cache_directory.join("cargo"))
            .arg("-SupportDll")
            .arg(
                job.options
                    .cache_directory
                    .join("cargo")
                    .join(super::artifact::native_target())
                    .join("debug")
                    .join(if cfg!(target_os = "linux") {
                        format!("lib{}.so", job.options.support_target)
                    } else {
                        format!("{}.dll", job.options.support_target)
                    }),
            )
            .arg("-Output")
            .arg(output)
            .arg("-Toolchain")
            .arg(&job.options.toolchain)
            .arg("-ReceiptPath")
            .arg(&receipt)
            .arg("-CacheDirectory")
            .arg(&job.options.cache_directory)
            .arg("-SessionId")
            .arg(&job.session)
            .arg("-Revision")
            .arg(job.revision.to_string())
            .arg("-JobId")
            .arg(job.id.to_string())
            .arg("-FoundationBuildKey")
            .arg(&plan.foundation_key)
            .arg("-MaxCacheBytes")
            .arg(job.options.max_cache_bytes.to_string());
        if job.options.offline {
            c.arg("-Offline");
        }
        if job.options.no_default_features {
            c.arg("-NoDefaultFeatures");
        }
        if !job.options.features.is_empty() {
            c.arg("-Features").arg(job.options.features.join(","));
        }
        if let Some(prior) = reuse {
            c.arg("-FoundationManifest").arg(prior);
        }
        let (ok, _, diagnostic) = capture(c, 256 * 1024).map_err(build_failed)?;
        let value: serde_json::Value = serde_json::from_slice(
            &read_limit(&receipt, 256 * 1024)
                .map_err(|e| build_failed(format!("{e}\n{diagnostic}")))?,
        )
        .map_err(|e| build_failed(e.to_string()))?;
        (ok, diagnostic, value)
    };
    if value["version"] != 1
        || value["session"] != job.session
        || value["revision"] != job.revision
        || value["job"] != job.id
    {
        return Err(build_failed(
            "producer receipt does not match submitted job".into(),
        ));
    }
    if !ok || value["status"] != "Built" {
        return Err(BuildFailure {
            state: if value["status"] == "CacheFull" {
                LayeredDevState::CacheFull
            } else {
                LayeredDevState::BuildFailed
            },
            message: format!("{}\n{diagnostic}", value["message"]),
        });
    }
    let manifest = output.with_extension(if cfg!(target_os = "linux") {
        "o.json"
    } else {
        "obj.json"
    });
    let captured = read_limit(&manifest, 256 * 1024).map_err(build_failed)?;
    if value["manifest_sha256"] != hash(&captured) {
        return Err(BuildFailure {
            state: LayeredDevState::Rejected,
            message: "manifest differs from build receipt".into(),
        });
    }
    Ok(Produced {
        manifest,
        captured,
        metrics: LayeredBuildMetrics {
            build_ms: value["build_ms"].as_f64().unwrap_or(0.0),
            lock_wait_ms: value["lock_wait_ms"].as_f64().unwrap_or(0.0),
            ..Default::default()
        },
    })
}

fn finish_build(
    job: &Job,
    plan: &Plan,
    produced: Produced,
    start: Instant,
) -> Result<Built, BuildFailure> {
    let Produced {
        manifest,
        captured,
        mut metrics,
    } = produced;
    let read = Instant::now();
    let artifact =
        LayeredArtifact::from_manifest_bytes(&captured, job.options.limits.load.max_object_bytes)
            .map_err(|e| BuildFailure {
            state: LayeredDevState::Rejected,
            message: e.to_string(),
        })?;
    metrics.artifact_read_ms = read.elapsed().as_secs_f64() * 1000.0;
    metrics.worker_ms = start.elapsed().as_secs_f64() * 1000.0;
    let receipt = serde_json::json!({"version":1,"session":job.session,"job":job.id,"revision":job.revision,
        "foundation_build_key":plan.foundation_key,"foundation_id":artifact.foundation_id(),"object_sha256":artifact.object_sha256(),
        "manifest_sha256":hash(&captured),"metrics":metrics});
    File::create_new(job.directory.join("build-receipt.json"))
        .and_then(|mut f| f.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()))
        .map_err(|e| build_failed(e.to_string()))?;
    Ok(Built {
        artifact,
        manifest,
        metrics,
    })
}
