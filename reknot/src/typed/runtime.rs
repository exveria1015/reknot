use super::*;
use crate::{
    HandlerApi, HandlerEnvironment, HostResource, HostResources, InitData, PreparedHandler,
    RequestRouter, RequestTask, Resource, ResourceId, RouterOptions, RunStatus, ServiceError,
};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

pub struct ModuleBuilder {
    module: Module,
    resources: HostResources,
    environment: HandlerEnvironment,
    options: RouterOptions,
    max_in_flight: usize,
}
impl ModuleBuilder {
    pub(super) fn new(module: Module) -> Self {
        Self {
            module,
            resources: HostResources::new(),
            environment: HandlerEnvironment::default(),
            options: RouterOptions::default(),
            max_in_flight: 256,
        }
    }
    pub fn resources(mut self, resources: HostResources) -> Self {
        self.resources = resources;
        self
    }
    /// Bind an existing host service. `export_handlers!(..., requires = ...)` declares its contract.
    pub fn bind(mut self, binding: u64, resource: ResourceId) -> Self {
        self.environment = self.environment.bind(binding, resource);
        self
    }
    pub fn router_options(mut self, options: RouterOptions) -> Self {
        self.options = options;
        self
    }
    pub fn max_in_flight(mut self, limit: usize) -> Self {
        self.max_in_flight = limit;
        self
    }
    fn environment(&self) -> Result<HandlerEnvironment, Error> {
        self.module.validate()?;
        if self.max_in_flight == 0 {
            return Err(Error::Configuration(
                "max_in_flight must be positive".into(),
            ));
        }
        Ok(self.environment.clone().with_initial(InitData::new(
            __private::SCHEMA,
            encode(&self.module, crate::MAX_INIT_BYTES)?,
        )))
    }
    fn finish(self, router: RequestRouter) -> ModuleHost {
        ModuleHost(Rc::new(State {
            router: RefCell::new(router),
            module: self.module,
            resources: self.resources,
            max_in_flight: self.max_in_flight,
        }))
    }
    /// # Safety
    /// The native code, including all initialization and callbacks, must satisfy
    /// `RequestRouter::load`'s trusted-code and owner-thread contracts.
    #[cfg(feature = "native")]
    pub unsafe fn load(self, path: impl AsRef<std::path::Path>) -> Result<ModuleHost, Error> {
        let environment = self.environment()?;
        let router = RequestRouter::load_with_environment(
            path,
            self.resources.clone(),
            environment,
            self.options.clone(),
        )
        .map_err(|e| route_error(&self.module, e))?;
        Ok(self.finish(router))
    }
    /// # Safety
    /// Every callback must remain resident and satisfy `RequestRouter::from_api`.
    pub unsafe fn from_api(self, api: HandlerApi) -> Result<ModuleHost, Error> {
        let environment = self.environment()?;
        let router = RequestRouter::from_api_with_environment(
            api,
            self.resources.clone(),
            environment,
            self.options.clone(),
        )
        .map_err(|e| route_error(&self.module, e))?;
        Ok(self.finish(router))
    }
}
struct State {
    router: RefCell<RequestRouter>,
    module: Module,
    resources: HostResources,
    max_in_flight: usize,
}

/// Owner-thread controller. Cloned handles stay registered across successful reloads.
/// A request selects its generation when `HandlerHandle::call` is invoked.
#[derive(Clone)]
pub struct ModuleHost(Rc<State>);
impl ModuleHost {
    pub(super) fn module(&self) -> &Module {
        &self.0.module
    }
    pub fn handler<I, O>(&self, handler: Handler<I, O>) -> Result<HandlerHandle<I, O>, Error>
    where
        I: Serialize + DeserializeOwned + 'static,
        O: Serialize + DeserializeOwned + 'static,
    {
        if !self.0.module.handlers.contains(&handler.descriptor()) {
            return Err(Error::Configuration(format!(
                "handler {} v{} is not registered in module {} with these input/output types",
                handler.name, handler.version, self.0.module.name
            )));
        }
        Ok(HandlerHandle {
            host: self.clone(),
            handler,
        })
    }
    pub fn generation(&self) -> u64 {
        self.0.router.borrow().generation()
    }
    pub fn in_flight(&self) -> usize {
        self.0.router.borrow().in_flight()
    }
    pub(super) fn call_raw(&self, handler: &HandlerSpec, bytes: Vec<u8>) -> RawCall {
        let admission = (|| {
            if !self.0.module.handlers.contains(handler) {
                return Err(Error::Configuration(format!(
                    "handler {} is not registered with this contract",
                    handler.name
                )));
            }
            let router = self.0.router.try_borrow().map_err(|_| Error::Busy)?;
            if router.in_flight() >= self.0.max_in_flight {
                return Err(Error::Busy);
            }
            if bytes.len() > MAX_MESSAGE_BYTES {
                return Err(Error::TooLarge {
                    limit: MAX_MESSAGE_BYTES,
                });
            }
            let mut packet = handler.key().to_le_bytes().to_vec();
            packet.extend_from_slice(&bytes);
            let mailbox = self.0.resources.register(__private::SERVICE, Mailbox(None));
            match router.dispatch(mailbox.id(), &packet) {
                Ok(task) => Ok(ActiveCall {
                    task: Some(task),
                    mailbox,
                    host: self.clone(),
                    name: handler.name.clone(),
                    cancelled: false,
                }),
                Err(error) => {
                    self.0.resources.revoke(mailbox.id());
                    Err(Error::Route(error))
                }
            }
        })();
        RawCall {
            admission: Some(admission),
        }
    }
    #[cfg(feature = "native")]
    pub fn artifact_id(&self) -> Option<crate::ArtifactId> {
        self.0.router.borrow().artifact_id()
    }
    /// Drive dropped-request cleanup on this thread, even while no new events arrive.
    /// No DLL is unloaded here. Callers must keep polling if this returns Pending.
    pub fn poll_cleanup(&self, cx: &mut Context<'_>) -> Poll<()> {
        let status = crate::poll_cancelled_requests(cx);
        if let Ok(mut router) = self.0.router.try_borrow_mut() {
            router.release_retired_generations();
        }
        status
    }
    pub fn acknowledge_activity_failure(&self, generation: u64) -> Result<bool, Error> {
        Ok(self
            .0
            .router
            .try_borrow_mut()
            .map_err(|_| Error::Busy)?
            .acknowledge_activity_failure(generation))
    }
    /// # Safety
    /// Same trusted native-code contract as `ModuleBuilder::load`.
    #[cfg(feature = "native")]
    pub unsafe fn reload(&self, path: impl AsRef<std::path::Path>) -> Result<u64, Error> {
        let candidate = self.prepare(path)?;
        self.activate(candidate)
    }
    /// # Safety
    /// Same resident callback contract as `ModuleBuilder::from_api`.
    pub unsafe fn reload_api(&self, api: HandlerApi) -> Result<u64, Error> {
        let candidate = self
            .0
            .router
            .try_borrow()
            .map_err(|_| Error::Busy)?
            .prepare_api(api)
            .map_err(|e| route_error(&self.0.module, e))?;
        self.activate(candidate)
    }
    #[cfg(feature = "native")]
    pub(super) unsafe fn prepare(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<PreparedHandler, Error> {
        self.0
            .router
            .try_borrow()
            .map_err(|_| Error::Busy)?
            .prepare(path)
            .map_err(|e| route_error(&self.0.module, e))
    }
    pub(super) fn activate(&self, candidate: PreparedHandler) -> Result<u64, Error> {
        let mut router = self.0.router.try_borrow_mut().map_err(|_| Error::Busy)?;
        let generation = router
            .activate(candidate)
            .map_err(|e| route_error(&self.0.module, e))?;
        router.release_retired_generations();
        Ok(generation)
    }
}

/// Stable application-facing entry point. `Call` and this handle are deliberately !Send.
pub struct HandlerHandle<I, O> {
    host: ModuleHost,
    handler: Handler<I, O>,
}
impl<I, O> Clone for HandlerHandle<I, O> {
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            handler: self.handler,
        }
    }
}
impl<I: Serialize + DeserializeOwned + 'static, O: Serialize + DeserializeOwned + 'static>
    HandlerHandle<I, O>
{
    /// Admit immediately, then await the returned future on the same thread.
    /// This never retries the handler. Application `Result<T,E>` remains an ordinary result value.
    pub fn call(&self, input: I) -> Call<O> {
        let raw = match encode(&input, MAX_MESSAGE_BYTES) {
            Ok(bytes) => self.host.call_raw(&self.handler.descriptor(), bytes),
            Err(error) => RawCall {
                admission: Some(Err(error)),
            },
        };
        Call {
            raw,
            marker: PhantomData,
        }
    }
}
struct Mailbox(Option<Result<Vec<u8>, String>>);
impl HostResource for Mailbox {
    fn call(&mut self, operation: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        if input.len() > MAX_MESSAGE_BYTES || self.0.is_some() {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        self.0 = Some(match operation {
            0 => Ok(input.to_vec()),
            1 => Err(String::from_utf8(input.to_vec()).map_err(|_| ServiceError::INVALID_ARGUMENT)?),
            _ => return Err(ServiceError::UNSUPPORTED_OPERATION),
        });
        Ok(0)
    }
}
struct ActiveCall {
    task: Option<RequestTask>,
    mailbox: Resource<Mailbox>,
    host: ModuleHost,
    name: String,
    cancelled: bool,
}
impl Drop for ActiveCall {
    fn drop(&mut self) {
        // Cancellation first: task destruction may still use its response resource.
        self.task.take();
        self.host.0.resources.revoke(self.mailbox.id());
        if let Ok(mut router) = self.host.0.router.try_borrow_mut() {
            router.release_retired_generations();
        }
    }
}
/// An admitted, generation-bound request, including registered child cleanup.
/// Drop cancels it; the host's cleanup driver must continue polling afterwards.
pub struct Call<O> {
    raw: RawCall,
    marker: PhantomData<fn() -> O>,
}
impl<O> Call<O> {
    pub fn generation(&self) -> Option<u64> {
        self.raw.generation()
    }
    /// Destroy the root on its owner thread; keep awaiting this call until its
    /// registered cleanup finishes. Dropping it instead needs `poll_cleanup`.
    pub fn cancel(&mut self) {
        self.raw.cancel();
    }
}
impl<O: DeserializeOwned> Future for Call<O> {
    type Output = Result<O, Error>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.raw).poll(cx).map(|result| {
            result.and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|e| Error::Codec(e.to_string()))
            })
        })
    }
}
pub(super) struct RawCall {
    admission: Option<Result<ActiveCall, Error>>,
}
impl RawCall {
    pub(super) fn generation(&self) -> Option<u64> {
        self.admission
            .as_ref()?
            .as_ref()
            .ok()?
            .task
            .as_ref()
            .map(RequestTask::generation)
    }
    pub(super) fn cancel(&mut self) {
        if let Some(Ok(active)) = &mut self.admission {
            active.cancelled = true;
            active.task.as_mut().unwrap().cancel_and_join();
        }
    }
}
impl Future for RawCall {
    type Output = Result<Vec<u8>, Error>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let admission = self
            .admission
            .as_mut()
            .expect("typed Call polled after completion");
        if let Ok(active) = admission {
            if Pin::new(active.task.as_mut().unwrap())
                .poll(cx)
                .is_pending()
            {
                return Poll::Pending;
            }
        }
        let result = self.admission.take().unwrap().and_then(|active| {
            let task = active.task.as_ref().unwrap();
            if active.cancelled && task.activity_error().is_none() {
                return Err(Error::Cancelled);
            }
            if task.status() != Some(RunStatus::Done) {
                return Err(Error::HandlerFailed {
                    handler: active.name.clone(),
                    generation: task.generation(),
                });
            }
            let reply = active
                .mailbox
                .try_borrow_mut()
                .map_err(|_| Error::Busy)?
                .0
                .take()
                .ok_or(Error::MissingReply)?;
            reply.map_err(Error::Codec)
        });
        Poll::Ready(result)
    }
}
