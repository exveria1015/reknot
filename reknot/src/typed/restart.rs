use super::*;
use crate::routing::restart::RestartState;
use crate::{
    Checkpoint, HandlerApi, HostResource, InitData, PreparedHandler, RunStatus, ServiceError,
    WorkerId,
};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

type TypedMigrator<S> = dyn FnMut(WorkerId, &Checkpoint) -> Result<S, Error>;
type HostCommit = dyn FnOnce() -> Result<(), ServiceError>;

/// Pure checkpoint migration, with an optional final synchronous host commit.
pub struct StateMigration<S> {
    state: WorkerState<S>,
    migrate: Box<TypedMigrator<S>>,
    commit: Box<HostCommit>,
}
impl<S: Serialize + DeserializeOwned + JsonSchema + 'static> WorkerState<S> {
    pub fn migrate(
        self,
        migrate: impl FnMut(WorkerId, &Checkpoint) -> Result<S, Error> + 'static,
    ) -> StateMigration<S> {
        StateMigration {
            state: self,
            migrate: Box::new(migrate),
            commit: Box::new(|| Ok(())),
        }
    }
}
impl<S> StateMigration<S> {
    /// Runs after every old root/child has joined and new workers are staged,
    /// before any new body is polled. The application owns rollback/durability.
    pub fn with_commit(
        mut self,
        commit: impl FnOnce() -> Result<(), ServiceError> + 'static,
    ) -> Self {
        self.commit = Box::new(commit);
        self
    }
}

pub(super) struct WorkerReply {
    error: Option<String>,
}
impl HostResource for WorkerReply {
    fn call(&mut self, operation: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        if input.len() > MAX_MESSAGE_BYTES {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let result = match operation {
            0 => serde_json::from_slice::<Result<(), String>>(input)
                .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
            1 => Err(String::from_utf8(input.to_vec()).map_err(|_| ServiceError::INVALID_ARGUMENT)?),
            _ => return Err(ServiceError::UNSUPPORTED_OPERATION),
        };
        self.error = result.err();
        if self.error.is_some() {
            Err(ServiceError::FAILED)
        } else {
            Ok(0)
        }
    }
}

impl ModuleHost {
    pub(super) fn settle_abandoned_restart(&self) {
        if self.0.abandoned_restart.get() {
            if let Ok(mut router) = self.0.router.try_borrow_mut() {
                router.finish_restart();
                self.0.abandoned_restart.set(false);
            }
        }
    }
    pub(super) fn wait_for_admission(&self, waker: &std::task::Waker) {
        let mut waiters = self.0.admission_waiters.borrow_mut();
        if !waiters.iter().any(|old| old.will_wake(waker)) {
            waiters.push(waker.clone());
        }
    }
    pub(super) fn wake_admission(&self) {
        let waiters = std::mem::take(&mut *self.0.admission_waiters.borrow_mut());
        let mut failure = None;
        for waker in waiters {
            if let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake()))
            {
                failure.get_or_insert(payload);
            }
        }
        if let Some(payload) = failure {
            std::panic::resume_unwind(payload);
        }
    }
    pub fn is_accepting(&self) -> bool {
        self.0.router.try_borrow().is_ok_and(|r| r.is_accepting())
    }
    /// Explicit recovery after an interrupted/failed Restart. Does not replay work.
    pub fn resume(&self) -> Result<(), Error> {
        self.settle_abandoned_restart();
        self.0
            .router
            .try_borrow_mut()
            .map_err(|_| Error::Busy)?
            .resume()
            .map_err(Error::Route)?;
        self.wake_admission();
        Ok(())
    }
    /// Register a restartable root. Ordinary HandlerHandle/HandlerClient calls
    /// remain ordinary requests and are never registered or replayed implicitly.
    pub fn start_worker<I, S>(
        &self,
        id: WorkerId,
        handler: Handler<I, Result<(), String>>,
        input: I,
        state: WorkerState<S>,
        initial: &S,
    ) -> Result<(), Error>
    where
        I: Serialize + DeserializeOwned + JsonSchema + 'static,
        S: Serialize + DeserializeOwned + JsonSchema + 'static,
    {
        self.settle_abandoned_restart();
        let descriptor = handler.descriptor();
        let module = self.0.module.borrow();
        if !module.handlers.contains(&descriptor)
            || module.state.as_ref() != Some(&state.descriptor())
        {
            return Err(Error::Configuration(
                "worker handler or state schema is not registered in this module".into(),
            ));
        }
        let mut packet = descriptor.key().to_le_bytes().to_vec();
        packet.extend_from_slice(&encode(&input, MAX_MESSAGE_BYTES)?);
        let checkpoint = state.encode(initial)?;
        let mut router = self.0.router.try_borrow_mut().map_err(|_| Error::Busy)?;
        if router.in_flight() >= self.0.max_in_flight {
            return Err(Error::Busy);
        }
        let mailbox = self
            .0
            .resources
            .register(__private::SERVICE, WorkerReply { error: None });
        if let Err(error) = router.start_worker(id, mailbox.id(), &packet, checkpoint) {
            self.0.resources.revoke(mailbox.id());
            return Err(Error::Route(error));
        }
        self.0.workers.borrow_mut().insert(id, mailbox);
        Ok(())
    }
    pub fn poll_workers(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.settle_abandoned_restart();
        match self.0.router.try_borrow_mut() {
            Ok(mut router) => router.poll_workers(cx),
            Err(_) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
    pub fn stop_worker(&self, id: WorkerId) -> Result<bool, Error> {
        Ok(self
            .0
            .router
            .try_borrow()
            .map_err(|_| Error::Busy)?
            .stop_worker(id))
    }
    pub fn remove_worker(&self, id: WorkerId) -> Result<bool, Error> {
        let removed = self
            .0
            .router
            .try_borrow_mut()
            .map_err(|_| Error::Busy)?
            .remove_worker(id);
        if removed {
            if let Some(mailbox) = self.0.workers.borrow_mut().remove(&id) {
                self.0.resources.revoke(mailbox.id());
            }
        }
        Ok(removed)
    }
    pub fn worker_status(&self, id: WorkerId) -> Option<RunStatus> {
        self.0.router.borrow().worker_status(id)
    }
    pub fn worker_error(&self, id: WorkerId) -> Option<String> {
        self.0
            .workers
            .borrow()
            .get(&id)?
            .try_borrow()
            .ok()?
            .error
            .clone()
    }
    pub fn worker_checkpoint<S: Serialize + DeserializeOwned + JsonSchema + 'static>(
        &self,
        id: WorkerId,
        state: WorkerState<S>,
    ) -> Result<Option<S>, Error> {
        self.0
            .router
            .try_borrow()
            .map_err(|_| Error::Busy)?
            .worker_checkpoint(id)
            .map(|checkpoint| state.decode(&checkpoint))
            .transpose()
    }

    fn restart_environment(&self, module: &Module) -> Result<crate::HandlerEnvironment, Error> {
        module.validate()?;
        Ok(self.0.environment.clone().with_initial(InitData::new(
            __private::SCHEMA,
            encode(module, crate::MAX_INIT_BYTES)?,
        )))
    }
    pub(super) fn restart_prepared(
        &self,
        candidate: PreparedHandler,
    ) -> Result<ModuleRestart, Error> {
        let module = self.module();
        self.begin_restart(
            candidate,
            module,
            |_, old, _| Ok(old.clone()),
            Box::new(|| Ok(())),
            Rc::new(RefCell::new(None)),
        )
    }
    fn begin_restart(
        &self,
        candidate: PreparedHandler,
        next: Module,
        migrate: impl FnMut(WorkerId, &Checkpoint, ServiceKey) -> Result<Checkpoint, ServiceError>
            + 'static,
        commit: Box<HostCommit>,
        detail: Rc<RefCell<Option<String>>>,
    ) -> Result<ModuleRestart, Error> {
        self.settle_abandoned_restart();
        let state = self
            .0
            .router
            .try_borrow_mut()
            .map_err(|_| Error::Busy)?
            .begin_restart(candidate, migrate, commit)
            .map_err(Error::Route)?;
        Ok(ModuleRestart {
            host: self.clone(),
            state: Some(state),
            next,
            detail,
        })
    }
    /// Stop/join registered roots, preserve their typed checkpoints, then recreate
    /// only explicitly registered workers. Keep driving ordinary calls/bridges too.
    /// # Safety
    /// Same native code and callback lifetime contract as ModuleBuilder::load.
    #[cfg(feature = "native")]
    pub unsafe fn restart(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<ModuleRestart, Error> {
        self.settle_abandoned_restart();
        self.restart_prepared(self.prepare(path)?)
    }
    /// # Safety
    /// All callbacks remain resident, as required by ModuleBuilder::from_api.
    pub unsafe fn restart_api(&self, api: HandlerApi) -> Result<ModuleRestart, Error> {
        self.settle_abandoned_restart();
        let candidate = self
            .0
            .router
            .try_borrow()
            .map_err(|_| Error::Busy)?
            .prepare_api(api)
            .map_err(|e| route_error(&self.module(), e))?;
        self.restart_prepared(candidate)
    }
    /// Restart with explicit migration into the next versioned state contract.
    /// Handler input/output contracts remain unchanged; change those by restarting
    /// the host and rebuilding its clients.
    /// # Safety
    /// Same native code and callback lifetime contract as ModuleBuilder::load.
    #[cfg(feature = "native")]
    pub unsafe fn restart_with<S: Serialize + DeserializeOwned + JsonSchema + 'static>(
        &self,
        path: impl AsRef<std::path::Path>,
        migration: StateMigration<S>,
    ) -> Result<ModuleRestart, Error> {
        self.settle_abandoned_restart();
        let next = self.module().with_state(migration.state);
        let environment = self.restart_environment(&next)?;
        let candidate = self
            .0
            .router
            .try_borrow()
            .map_err(|_| Error::Busy)?
            .prepare_with_environment(path, environment)
            .map_err(|e| route_error(&next, e))?;
        self.migrate_prepared(candidate, next, migration)
    }
    /// # Safety
    /// All callbacks remain resident, as required by ModuleBuilder::from_api.
    pub unsafe fn restart_api_with<S: Serialize + DeserializeOwned + JsonSchema + 'static>(
        &self,
        api: HandlerApi,
        migration: StateMigration<S>,
    ) -> Result<ModuleRestart, Error> {
        self.settle_abandoned_restart();
        let next = self.module().with_state(migration.state);
        let environment = self.restart_environment(&next)?;
        let candidate = self
            .0
            .router
            .try_borrow()
            .map_err(|_| Error::Busy)?
            .prepare_api_with_environment(api, environment)
            .map_err(|e| route_error(&next, e))?;
        self.migrate_prepared(candidate, next, migration)
    }
    fn migrate_prepared<S: Serialize + DeserializeOwned + JsonSchema + 'static>(
        &self,
        candidate: PreparedHandler,
        next: Module,
        mut migration: StateMigration<S>,
    ) -> Result<ModuleRestart, Error> {
        let detail = Rc::new(RefCell::new(None));
        let error_detail = detail.clone();
        let migrate = move |id, old: &Checkpoint, _| {
            let value =
                (migration.migrate)(id, old).and_then(|value| migration.state.encode(&value));
            value.map_err(|error| {
                *error_detail.borrow_mut() = Some(error.to_string());
                ServiceError::FAILED
            })
        };
        self.begin_restart(candidate, next, migrate, migration.commit, detail)
    }
}

/// Owner-thread Restart future. Admission closes when this value is created.
/// Dropping it, timing out, or receiving Err leaves admission paused. No force
/// termination, automatic request replay, or external-effect rollback occurs.
pub struct ModuleRestart {
    host: ModuleHost,
    state: Option<RestartState<'static>>,
    next: Module,
    detail: Rc<RefCell<Option<String>>>,
}
impl Future for ModuleRestart {
    type Output = Result<u64, Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = {
            let mut router = match this.host.0.router.try_borrow_mut() {
                Ok(router) => router,
                Err(_) => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            };
            this.state
                .as_mut()
                .expect("completed ModuleRestart polled again")
                .poll(&mut router, cx)
        };
        result.map(|result| {
            this.state.take();
            match result {
                Ok(generation) => {
                    *this.host.0.module.borrow_mut() = this.next.clone();
                    if this.host.is_accepting() {
                        this.host.wake_admission();
                    }
                    Ok(generation)
                }
                Err(source) => Err(Error::Restart {
                    source,
                    detail: this.detail.borrow_mut().take(),
                }),
            }
        })
    }
}
impl Drop for ModuleRestart {
    fn drop(&mut self) {
        if self.state.is_some() {
            // Drop may run reentrantly from a worker. Defer flag cleanup until
            // the owner's next maintenance/resume call if its router is borrowed.
            self.host.0.abandoned_restart.set(true);
            self.host.settle_abandoned_restart();
        }
    }
}
