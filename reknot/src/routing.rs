use crate::generation::GenerationOwner;
#[cfg(feature = "native")]
use crate::native::{load, LoadError};
use crate::{
    FfiFuture, HandlerApi, HandlerRequest, HostResources, ResourceId, RunStatus, ServiceError,
    ServiceKey, HANDLER_ABI_VERSION,
};
use crate::{HandlerEnvironment, ServiceBinding};
use std::cell::{Cell, RefCell};
use std::fmt;
use std::future::Future;
#[cfg(feature = "native")]
use std::path::Path;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

mod control;
mod limits;
pub(crate) mod restart;
pub use control::Checkpoint;
use control::Control;
pub use limits::{GenerationBudget, RouterOptions};
use restart::Worker;
pub use restart::{Restart, WorkerId};

static NEXT_ROUTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub enum RouteError {
    InvalidEnvironment,
    MissingService {
        binding: u64,
        expected: ServiceKey,
    },
    InitSchemaMismatch {
        expected: ServiceKey,
        found: ServiceKey,
    },
    InitData(ServiceError),
    InitDiagnostic {
        error: ServiceError,
        metadata: Vec<u8>,
    },
    HostCommit(ServiceError),
    #[cfg(feature = "native")]
    Load(LoadError),
    Abi {
        version: u32,
        size: u32,
    },
    InvalidApi,
    ContractMismatch {
        expected: ServiceKey,
        found: ServiceKey,
    },
    Resource(ServiceError),
    WrongRouter,
    StaleCandidate,
    Paused,
    InvalidTask,
    GenerationExhausted,
    GenerationLimit {
        limit: usize,
    },
    InvalidLimits,
    StateSchemaMismatch,
    CheckpointTooLarge,
    WorkerExists(WorkerId),
    WorkerFailed(WorkerId),
    Migration {
        worker: WorkerId,
        error: ServiceError,
    },
    RestartInProgress,
    ActivityFailed {
        generation: u64,
    },
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEnvironment => {
                write!(f, "invalid service bindings in handler environment")
            }
            Self::MissingService { binding, expected } => {
                write!(f, "missing service binding {binding}: {expected:?}")
            }
            Self::InitSchemaMismatch { expected, found } => write!(
                f,
                "initialization schema mismatch: expected {expected:?}, found {found:?}"
            ),
            Self::InitData(error) => write!(f, "initialization data rejected: {error}"),
            Self::InitDiagnostic { error, .. } => write!(
                f,
                "initialization data rejected: {error} (candidate metadata available)"
            ),
            Self::HostCommit(error) => write!(f, "host resource commit failed: {error}"),
            #[cfg(feature = "native")]
            Self::Load(error) => write!(f, "handler load failed: {error}"),
            Self::Abi { version, size } => {
                write!(f, "handler ABI mismatch: version {version}, size {size}")
            }
            Self::InvalidApi => write!(f, "invalid handler API or service contract"),
            Self::ContractMismatch { expected, found } => write!(
                f,
                "service contract mismatch: expected {expected:?}, found {found:?}"
            ),
            Self::Resource(error) => write!(f, "request resource unavailable: {error}"),
            Self::WrongRouter => write!(f, "candidate belongs to another router"),
            Self::StaleCandidate => {
                write!(f, "candidate was prepared for an older active generation")
            }
            Self::Paused => write!(f, "request admission is paused"),
            Self::InvalidTask => write!(f, "handler returned an invalid task"),
            Self::GenerationExhausted => write!(f, "generation or request count exhausted"),
            Self::GenerationLimit { limit } => {
                write!(f, "retained generation budget reached ({limit})")
            }
            Self::InvalidLimits => write!(f, "generation and checkpoint limits must be positive"),
            Self::StateSchemaMismatch => {
                write!(f, "checkpoint schema mismatch; state migration required")
            }
            Self::CheckpointTooLarge => write!(f, "checkpoint exceeds configured byte limit"),
            Self::WorkerExists(id) => write!(f, "worker {} already registered", id.0),
            Self::WorkerFailed(id) => write!(f, "worker {} did not stop successfully", id.0),
            Self::Migration { worker, error } => {
                write!(f, "worker {} migration failed: {error}", worker.0)
            }
            Self::RestartInProgress => write!(f, "restart is already in progress"),
            Self::ActivityFailed { generation } => {
                write!(f, "registered activity failed in generation {generation}")
            }
        }
    }
}
impl std::error::Error for RouteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(feature = "native")]
            Self::Load(error) => Some(error),
            Self::Resource(error) => Some(error),
            Self::HostCommit(error) => Some(error),
            _ => None,
        }
    }
}

struct Generation {
    id: u64,
    api: HandlerApi,
    in_flight: Cell<usize>,
    scope: Rc<crate::generation_scope::GenerationScope>,
    image: Option<GenerationOwner>,
    controls: RefCell<Vec<Weak<Control>>>,
    completion: Rc<RefCell<Option<std::task::Waker>>>,
    activity_failed: Cell<bool>,
    environment: HandlerEnvironment,
    bindings: Vec<ServiceBinding>,
}

impl Drop for RequestRouter {
    fn drop(&mut self) {
        if let Some(image) = &self.active.image {
            image.retire();
        }
    }
}

struct AdmissionCount {
    generation: Rc<Generation>,
    armed: bool,
}
impl Drop for AdmissionCount {
    fn drop(&mut self) {
        if self.armed {
            self.generation
                .in_flight
                .set(self.generation.in_flight.get() - 1);
        }
    }
}

/// An inert candidate. Preparation does not start request handlers or alter
/// routing. Native module initialization must itself obey the unsafe load contract.
pub struct PreparedHandler {
    router: u64,
    base: u64,
    api: HandlerApi,
    image: Option<GenerationOwner>,
    environment: HandlerEnvironment,
    bindings: Vec<ServiceBinding>,
}

impl PreparedHandler {
    /// Native artifact identity without extending its lifetime. Static APIs have none.
    #[cfg(feature = "native")]
    pub fn artifact_id(&self) -> Option<crate::ArtifactId> {
        self.image.as_ref().map(|image| image.id)
    }

    pub fn based_on(&self) -> u64 {
        self.base
    }
    pub fn service(&self) -> ServiceKey {
        self.api.service
    }
}

/// Thread-bound message admission. Activation and dispatch are serialized by
/// Rust borrows; already admitted tasks stay bound to their original generation.
pub struct RequestRouter {
    id: u64,
    resources: HostResources,
    active: Rc<Generation>,
    retired: Vec<Rc<Generation>>,
    accepting: bool,
    options: RouterOptions,
    workers: Vec<Worker>,
    restarting: bool,
}

impl RequestRouter {
    /// # Safety
    /// The module must be trusted, including initializers/finalizers. Its API
    /// must obey HandlerRequest's copy/retain contract and the owned FFI future
    /// protocol. All calls, polling and destruction occur on this thread.
    /// Executor Wakers used with managed notifications must belong to resident
    /// host code, independently of reloadable generations.
    #[cfg(feature = "native")]
    pub unsafe fn load(
        path: impl AsRef<Path>,
        resources: HostResources,
    ) -> Result<Self, RouteError> {
        Self::load_with_options(path, resources, RouterOptions::default())
    }

    /// # Safety
    /// Every callback and any escaped Waker must remain valid independently of
    /// the router (normally statically linked code). The same task and thread
    /// contracts as load apply; no DLL lease is added for this extension point.
    pub unsafe fn from_api(api: HandlerApi, resources: HostResources) -> Result<Self, RouteError> {
        Self::from_api_with_options(api, resources, RouterOptions::default())
    }

    /// # Safety
    /// Same native code and lifetime contract as load. The shared budget also
    /// counts rejected loads and can be reused across router reconstruction.
    #[cfg(feature = "native")]
    pub unsafe fn load_with_options(
        path: impl AsRef<Path>,
        resources: HostResources,
        options: RouterOptions,
    ) -> Result<Self, RouteError> {
        Self::load_with_environment(path, resources, HandlerEnvironment::default(), options)
    }

    /// # Safety
    /// Same trusted native code contract as load. Init validators must be pure;
    /// factories must copy bindings and initial data before returning.
    #[cfg(feature = "native")]
    pub unsafe fn load_with_environment(
        path: impl AsRef<Path>,
        resources: HostResources,
        environment: HandlerEnvironment,
        options: RouterOptions,
    ) -> Result<Self, RouteError> {
        if options.max_checkpoint_bytes == 0 {
            return Err(RouteError::InvalidLimits);
        }
        options.generations.reserve()?;
        let (api, image) = load(path.as_ref())?;
        Self::create(api, Some(image), resources, options, environment)
    }

    /// # Safety
    /// Same static callback lifetime contract as from_api.
    pub unsafe fn from_api_with_options(
        api: HandlerApi,
        resources: HostResources,
        options: RouterOptions,
    ) -> Result<Self, RouteError> {
        Self::from_api_with_environment(api, resources, HandlerEnvironment::default(), options)
    }

    /// # Safety
    /// Same static callback lifetime contract as from_api, also covering the
    /// pure initialization validator and registered activity callbacks.
    pub unsafe fn from_api_with_environment(
        api: HandlerApi,
        resources: HostResources,
        environment: HandlerEnvironment,
        options: RouterOptions,
    ) -> Result<Self, RouteError> {
        validate(api)?;
        if options.max_checkpoint_bytes == 0 {
            return Err(RouteError::InvalidLimits);
        }
        options.generations.reserve()?;
        Self::create(api, None, resources, options, environment)
    }

    fn create(
        api: HandlerApi,
        image: Option<GenerationOwner>,
        resources: HostResources,
        options: RouterOptions,
        environment: HandlerEnvironment,
    ) -> Result<Self, RouteError> {
        validate(api)?;
        let bindings = {
            let _call = image.as_ref().map(GenerationOwner::enter);
            crate::environment::negotiate(&api, &environment, &resources)?
        };
        let id = NEXT_ROUTER
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| RouteError::GenerationExhausted)?;
        if let Some(image) = &image {
            image.activate();
        }
        Ok(Self {
            id,
            resources,
            active: Rc::new(Generation {
                id: 1,
                api,
                scope: crate::generation_scope::GenerationScope::new(
                    image.as_ref().map(GenerationOwner::lease),
                ),
                image,
                in_flight: Cell::new(0),
                controls: RefCell::new(Vec::new()),
                completion: Rc::new(RefCell::new(None)),
                activity_failed: Cell::new(false),
                environment,
                bindings,
            }),
            retired: Vec::new(),
            accepting: true,
            options,
            workers: Vec::new(),
            restarting: false,
        })
    }

    /// Loads and validates an inert candidate; old requests and admission continue.
    ///
    /// # Safety
    /// The candidate must obey load's native code, initialization and ABI contracts.
    #[cfg(feature = "native")]
    pub unsafe fn prepare(&self, path: impl AsRef<Path>) -> Result<PreparedHandler, RouteError> {
        self.prepare_with_environment(path, self.active.environment.clone())
    }
    /// # Safety
    /// Same native code and pure validation contract as load_with_environment.
    #[cfg(feature = "native")]
    pub unsafe fn prepare_with_environment(
        &self,
        path: impl AsRef<Path>,
        environment: HandlerEnvironment,
    ) -> Result<PreparedHandler, RouteError> {
        self.active
            .id
            .checked_add(1)
            .ok_or(RouteError::GenerationExhausted)?;
        self.options.generations.reserve()?;
        let (api, image) = load(path.as_ref())?;
        self.candidate(api, Some(image), environment)
    }

    /// # Safety
    /// The statically supplied callbacks must obey from_api's lifetime contract.
    pub unsafe fn prepare_api(&self, api: HandlerApi) -> Result<PreparedHandler, RouteError> {
        self.prepare_api_with_environment(api, self.active.environment.clone())
    }
    /// # Safety
    /// Same static callback and validator contract as from_api_with_environment.
    pub unsafe fn prepare_api_with_environment(
        &self,
        api: HandlerApi,
        environment: HandlerEnvironment,
    ) -> Result<PreparedHandler, RouteError> {
        validate(api)?;
        self.check_service(api.service)?;
        self.options.generations.reserve()?;
        self.candidate(api, None, environment)
    }

    fn candidate(
        &self,
        api: HandlerApi,
        image: Option<GenerationOwner>,
        environment: HandlerEnvironment,
    ) -> Result<PreparedHandler, RouteError> {
        validate(api)?;
        self.check_service(api.service)?;
        let bindings = {
            let _call = image.as_ref().map(GenerationOwner::enter);
            crate::environment::negotiate(&api, &environment, &self.resources)?
        };
        if let Some(image) = &image {
            image.prepared();
        }
        Ok(PreparedHandler {
            router: self.id,
            base: self.active.id,
            api,
            image,
            environment,
            bindings,
        })
    }

    /// Drain cutover: only requests admitted after this call use the new API.
    /// Paused admission stays paused. A stale/wrong-router candidate is rejected.
    pub fn activate(&mut self, candidate: PreparedHandler) -> Result<u64, RouteError> {
        if self.restarting {
            return Err(RouteError::RestartInProgress);
        }
        if candidate.api.state_schema != self.active.api.state_schema {
            return Err(RouteError::StateSchemaMismatch);
        }
        self.commit(candidate)
    }

    fn commit(&mut self, candidate: PreparedHandler) -> Result<u64, RouteError> {
        if candidate.router != self.id {
            return Err(RouteError::WrongRouter);
        }
        if candidate.base != self.active.id {
            return Err(RouteError::StaleCandidate);
        }
        self.check_service(candidate.api.service)?;
        crate::environment::check_bindings(&candidate.bindings, &self.resources)?;
        let id = self
            .active
            .id
            .checked_add(1)
            .ok_or(RouteError::GenerationExhausted)?;
        if let Some(image) = &candidate.image {
            image.activate();
        }
        let next = Rc::new(Generation {
            id,
            api: candidate.api,
            scope: crate::generation_scope::GenerationScope::new(
                candidate.image.as_ref().map(GenerationOwner::lease),
            ),
            image: candidate.image,
            in_flight: Cell::new(0),
            controls: RefCell::new(Vec::new()),
            completion: self.active.completion.clone(),
            activity_failed: Cell::new(false),
            environment: candidate.environment,
            bindings: candidate.bindings,
        });
        self.retired.retain(|generation| {
            generation.in_flight.get() != 0 || generation.activity_failed.get()
        });
        self.retired.push(self.active.clone());
        if let Some(image) = &self.active.image {
            image.retire();
        }
        self.active = next;
        Ok(id)
    }

    pub fn dispatch(&self, resource: ResourceId, input: &[u8]) -> Result<RequestTask, RouteError> {
        if !self.accepting {
            return Err(RouteError::Paused);
        }
        self.spawn(&self.active, resource, input, None)
    }

    fn spawn(
        &self,
        generation: &Rc<Generation>,
        resource: ResourceId,
        input: &[u8],
        checkpoint: Option<Checkpoint>,
    ) -> Result<RequestTask, RouteError> {
        crate::environment::check_bindings(&generation.bindings, &self.resources)?;
        let service = self
            .resources
            .service(resource)
            .map_err(RouteError::Resource)?;
        if service != generation.api.service {
            return Err(RouteError::ContractMismatch {
                expected: generation.api.service,
                found: service,
            });
        }
        let count = generation
            .in_flight
            .get()
            .checked_add(1)
            .ok_or(RouteError::GenerationExhausted)?;
        let services = self.resources.services();
        let control = Control::new(
            generation.api.state_schema,
            checkpoint.clone(),
            self.options.max_checkpoint_bytes,
            generation.image.as_ref().map(GenerationOwner::lease),
            Rc::downgrade(&generation.scope),
        );
        let (state_schema, state) = checkpoint
            .as_ref()
            .map(|state| (state.schema, state.bytes.as_slice()))
            .unwrap_or((ServiceKey::new(0, 0), &[]));
        let request = HandlerRequest {
            generation: generation.id,
            resource,
            service,
            services: services.as_raw(),
            input: input.as_ptr(),
            input_len: input.len(),
            control: control.raw(),
            state_schema,
            state: state.as_ptr(),
            state_len: state.len(),
            bindings: generation.bindings.as_ptr(),
            bindings_len: generation.bindings.len(),
            init_schema: generation
                .environment
                .initial
                .as_ref()
                .map(|data| data.schema)
                .unwrap_or(ServiceKey::new(0, 0)),
            init: generation
                .environment
                .initial
                .as_ref()
                .map(|data| data.bytes.as_ptr())
                .unwrap_or(std::ptr::null()),
            init_len: generation
                .environment
                .initial
                .as_ref()
                .map(|data| data.bytes.len())
                .unwrap_or(0),
        };
        generation.in_flight.set(count);
        let mut admission = AdmissionCount {
            generation: generation.clone(),
            armed: true,
        };
        // The validated native contract requires copying input and retaining
        // services synchronously. Neither stack descriptor escapes this call.
        let raw = {
            let _call = generation.image.as_ref().map(GenerationOwner::enter);
            unsafe { (generation.api.make_request)(&request) }
        };
        let lease = generation.image.as_ref().map(GenerationOwner::lease);
        let task = unsafe { FfiFuture::with_optional_keep_alive(raw, lease) }
            .ok_or(RouteError::InvalidTask)?;
        admission.armed = false;
        let mut controls = generation.controls.borrow_mut();
        controls.retain(|control| control.strong_count() != 0);
        controls.push(Rc::downgrade(&control));
        Ok(RequestTask {
            task: Some(task),
            generation: generation.clone(),
            resource,
            status: None,
            control,
            root_status: None,
            detached: false,
        })
    }

    pub fn options(&self) -> &RouterOptions {
        &self.options
    }

    pub fn pause(&mut self) {
        self.accepting = false;
    }
    pub fn resume(&mut self) -> Result<(), RouteError> {
        if self.restarting {
            return Err(RouteError::RestartInProgress);
        }
        self.accepting = true;
        Ok(())
    }
    pub fn is_accepting(&self) -> bool {
        self.accepting
    }
    pub fn generation(&self) -> u64 {
        self.active.id
    }
    /// Native artifact identity without extending its lifetime. Static APIs have none.
    #[cfg(feature = "native")]
    pub fn artifact_id(&self) -> Option<crate::ArtifactId> {
        self.active.image.as_ref().map(|image| image.id)
    }

    /// Releases this router's ownership of completed, retired generations.
    /// Tasks, Wakers and registered cleanup may still retain their own leases.
    /// Activity failures must first be acknowledged; no library is closed here.
    pub fn release_retired_generations(&mut self) -> usize {
        let before = self.retired.len();
        self.retired.retain(|generation| {
            generation.in_flight.get() != 0 || generation.activity_failed.get()
        });
        before - self.retired.len()
    }
    pub fn service(&self) -> ServiceKey {
        self.active.api.service
    }

    /// Counts unfinished managed roots, including their registered activities
    /// and cancelled requests whose asynchronous cleanup has not joined yet.
    pub fn in_flight(&self) -> usize {
        self.retired
            .iter()
            .cloned()
            .fold(self.active.in_flight.get(), |count, generation| {
                count.saturating_add(generation.in_flight.get())
            })
    }

    fn check_service(&self, service: ServiceKey) -> Result<(), RouteError> {
        if service != self.active.api.service {
            return Err(RouteError::ContractMismatch {
                expected: self.active.api.service,
                found: service,
            });
        }
        Ok(())
    }

    /// Explicit application acknowledgement after observing/reconciling an activity
    /// failure. Refuses while that generation has unfinished roots or cleanup.
    /// Does not restart failed workers, resume admission or undo external effects.
    pub fn acknowledge_activity_failure(&mut self, generation: u64) -> bool {
        std::iter::once(&self.active)
            .chain(&self.retired)
            .find(|entry| entry.id == generation && entry.in_flight.get() == 0)
            .is_some_and(|entry| entry.activity_failed.replace(false))
    }
}

/// Thread-bound root execution, including registered cleanup.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<reknot::RequestTask>();
/// ```
pub struct RequestTask {
    task: Option<FfiFuture>,
    generation: Rc<Generation>,
    resource: ResourceId,
    status: Option<RunStatus>,
    control: Rc<Control>,
    root_status: Option<RunStatus>,
    detached: bool,
}

impl RequestTask {
    pub fn generation(&self) -> u64 {
        self.generation.id
    }
    pub fn resource(&self) -> ResourceId {
        self.resource
    }
    pub fn status(&self) -> Option<RunStatus> {
        self.status
    }

    /// Destroys the root future synchronously. Registered activities are stopped
    /// and joined by poll_cancelled_requests (also driven by the router/Restart).
    /// No request is replayed and external effects are not rolled back.
    pub fn cancel(self) {
        drop(self);
    }

    /// Destroy the root but retain this task so its registered cleanup can be
    /// polled to completion. Used by the typed bridge to retain admission credit
    /// until actual cleanup, rather than merely until the caller disappears.
    #[cfg(feature = "typed")]
    pub(crate) fn cancel_and_join(&mut self) {
        if self.status.is_none() {
            self.root_status = Some(RunStatus::Error);
            self.task = None;
            self.control.close();
        }
    }

    pub fn activity_count(&self) -> usize {
        self.control.activity_count()
    }
    pub fn activity_error(&self) -> Option<ServiceError> {
        self.control.failure.get()
    }

    fn finish(&mut self, status: RunStatus) {
        if self.status.is_none() {
            self.control.notification_gate.revoke();
            self.control.waker.borrow_mut().take();
            self.task = None;
            self.status = Some(status);
            if self.control.failure.get().is_some() {
                self.generation.activity_failed.set(true);
            }
            self.generation
                .in_flight
                .set(self.generation.in_flight.get() - 1);
            let wake = self.generation.completion.borrow().clone();
            if let Some(wake) = wake {
                wake.wake();
            }
        }
    }
}
impl Future for RequestTask {
    type Output = RunStatus;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<RunStatus> {
        if let Some(status) = self.status {
            return Poll::Ready(status);
        }
        *self.control.waker.borrow_mut() = Some(cx.waker().clone());
        self.control.notification_gate.update(cx.waker());
        if self.root_status.is_none() {
            if let Poll::Ready(status) = Pin::new(self.task.as_mut().unwrap()).poll(cx) {
                self.task = None;
                self.root_status = Some(status);
                self.control.close();
            }
        }
        let joined = self.control.poll_activities(cx);
        if let Some(root) = self.root_status {
            if joined {
                let status = if self.control.failure.get().is_some() {
                    RunStatus::Error
                } else {
                    root
                };
                self.finish(status);
                return Poll::Ready(status);
            }
        }
        Poll::Pending
    }
}
impl Drop for RequestTask {
    fn drop(&mut self) {
        if self.status.is_some() || self.detached {
            return;
        }
        self.task = None;
        self.control.close();
        self.control.waker.borrow_mut().take();
        if self.control.activity_count() == 0 {
            self.finish(RunStatus::Error);
        } else {
            let cancelled = CancelledRequest {
                generation: self.generation.clone(),
                control: self.control.clone(),
                finished: false,
            };
            // Try-with also covers cancellation during TLS destruction. Retain
            // the owner if this thread can no longer service its cleanup queue.
            let mut owned = Some(cancelled);
            let _ = CANCELLED.try_with(|queue| queue.borrow_mut().push(owned.take().unwrap()));
            if let Some(cancelled) = owned {
                std::mem::forget(cancelled);
            }
            self.detached = true;
            let waker = self.generation.completion.borrow().clone();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

thread_local! { static CANCELLED: RefCell<Vec<CancelledRequest>> = const { RefCell::new(Vec::new()) }; }
struct CancelledRequest {
    generation: Rc<Generation>,
    control: Rc<Control>,
    finished: bool,
}
impl Drop for CancelledRequest {
    fn drop(&mut self) {
        if !self.finished {
            // Thread exit/abandonment cannot prove join. Retain the control and
            // generation instead of destroying callback-owned state prematurely.
            std::mem::forget(self.control.clone());
            std::mem::forget(self.generation.clone());
        }
    }
}

struct CancelledBatch(Vec<CancelledRequest>);
impl Drop for CancelledBatch {
    fn drop(&mut self) {
        // Restore not-yet-joined requests even if an executor Waker unwinds.
        let _ = CANCELLED.try_with(|queue| queue.borrow_mut().append(&mut self.0));
    }
}

/// Drives owner-thread cleanup for cancelled/dropped requests, even after their
/// router is dropped. Pending cleanup remains counted and retains its generation.
/// Call on the original owner thread; no blocking join or automatic thread spawn.
pub fn poll_cancelled_requests(cx: &mut Context<'_>) -> Poll<()> {
    let mut batch =
        CancelledBatch(CANCELLED.with(|queue| std::mem::take(&mut *queue.borrow_mut())));
    batch.0.retain_mut(|request| {
        if request.finished {
            return false;
        }
        if request.control.poll_activities(cx) {
            request.control.notification_gate.revoke();
            if request.control.failure.get().is_some() {
                request.generation.activity_failed.set(true);
            }
            request
                .generation
                .in_flight
                .set(request.generation.in_flight.get() - 1);
            request.finished = true;
            let waker = request.generation.completion.borrow().clone();
            if let Some(waker) = waker {
                waker.wake();
            }
            false
        } else {
            true
        }
    });
    drop(batch);
    CANCELLED.with(|queue| {
        if queue.borrow().is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
}

pub(crate) fn validate(api: HandlerApi) -> Result<(), RouteError> {
    let header = api.header;
    if header.abi_version != HANDLER_ABI_VERSION
        || header.struct_size < reknot_abi::services::HANDLER_API_V5_SIZE as u32
    {
        return Err(RouteError::Abi {
            version: header.abi_version,
            size: header.struct_size,
        });
    }
    if api.service.id == 0 || api.service.version == 0 || api.service.reserved != 0 {
        return Err(RouteError::InvalidApi);
    }
    let schema = api.state_schema;
    if !((schema.id == 0 && schema.version == 0 && schema.reserved == 0)
        || (schema.id != 0 && schema.version != 0 && schema.reserved == 0))
    {
        return Err(RouteError::InvalidApi);
    }
    crate::environment::validate_manifest(&api)
}
