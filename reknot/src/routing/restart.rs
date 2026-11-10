use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkerId(pub u64);

pub(super) struct Worker {
    id: WorkerId,
    resource: ResourceId,
    input: Vec<u8>,
    task: RequestTask,
}

impl RequestRouter {
    /// Registers an explicitly restartable worker. Ordinary dispatch requests
    /// are never replayed. Seed state must match the active handler's schema.
    pub fn start_worker(
        &mut self,
        id: WorkerId,
        resource: ResourceId,
        input: &[u8],
        state: Checkpoint,
    ) -> Result<(), RouteError> {
        if !self.accepting {
            return Err(RouteError::Paused);
        }
        if self.workers.iter().any(|worker| worker.id == id) {
            return Err(RouteError::WorkerExists(id));
        }
        self.validate_checkpoint(&state, self.active.api.state_schema)?;
        let task = self.spawn(&self.active, resource, input, Some(state))?;
        self.workers.push(Worker {
            id,
            resource,
            input: input.to_vec(),
            task,
        });
        Ok(())
    }

    /// Drives registered workers on the owning executor; statuses remain
    /// available even when every worker has finished.
    pub fn poll_workers(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let _ = poll_cancelled_requests(cx);
        let mut pending = false;
        for worker in &mut self.workers {
            pending |= Pin::new(&mut worker.task).poll(cx).is_pending();
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    pub fn worker_status(&self, id: WorkerId) -> Option<RunStatus> {
        self.workers
            .iter()
            .find(|worker| worker.id == id)
            .and_then(|worker| worker.task.status())
    }
    pub fn worker_activity_count(&self, id: WorkerId) -> Option<usize> {
        self.workers
            .iter()
            .find(|worker| worker.id == id)
            .map(|worker| worker.task.activity_count())
    }
    pub fn worker_activity_error(&self, id: WorkerId) -> Option<ServiceError> {
        self.workers
            .iter()
            .find(|worker| worker.id == id)
            .and_then(|worker| worker.task.activity_error())
    }
    pub fn worker_checkpoint(&self, id: WorkerId) -> Option<Checkpoint> {
        self.workers
            .iter()
            .find(|worker| worker.id == id)
            .and_then(|worker| worker.task.control.checkpoint.borrow().clone())
    }
    pub fn stop_worker(&self, id: WorkerId) -> bool {
        if let Some(worker) = self.workers.iter().find(|worker| worker.id == id) {
            worker.task.control.stop();
            true
        } else {
            false
        }
    }
    /// Explicitly cancels and unregisters one worker, ending its future borrow.
    pub fn remove_worker(&mut self, id: WorkerId) -> bool {
        if let Some(index) = self.workers.iter().position(|worker| worker.id == id) {
            self.workers.remove(index);
            true
        } else {
            false
        }
    }

    /// Closes admission and signals all managed roots, including old Drain
    /// generations. The returned future drives registered workers and waits for
    /// externally owned RequestTasks to be driven to completion by their executor.
    /// Dropping it (e.g. a host timeout) leaves admission paused and futures alive.
    pub fn restart<'a>(
        &'a mut self,
        candidate: PreparedHandler,
        migrate: impl FnMut(WorkerId, &Checkpoint, ServiceKey) -> Result<Checkpoint, ServiceError> + 'a,
    ) -> Result<Restart<'a>, RouteError> {
        self.restart_with_commit(candidate, migrate, || Ok(()))
    }

    /// Stops/joins roots and stages every new worker before calling commit once.
    /// Success installs the staged generation without polling its handlers.
    ///
    /// The application owns exclusion, transactions, rollback and durable recovery.
    /// commit must leave resources recoverable on Err; unknown outcomes must keep
    /// resource access fenced. It must not invalidate staged service bindings.
    /// It is synchronous: do not assume frame latency or crash-atomic installation
    /// of both database state and the in-memory router. Interrupted/failed Restart
    /// leaves admission paused; an unpolled commit is never invoked on Drop.
    pub fn restart_with_commit<'a>(
        &'a mut self,
        candidate: PreparedHandler,
        migrate: impl FnMut(WorkerId, &Checkpoint, ServiceKey) -> Result<Checkpoint, ServiceError> + 'a,
        commit: impl FnOnce() -> Result<(), ServiceError> + 'a,
    ) -> Result<Restart<'a>, RouteError> {
        let state = self.begin_restart(candidate, migrate, commit)?;
        Ok(Restart {
            router: self,
            state,
        })
    }

    pub(crate) fn finish_restart(&mut self) {
        self.restarting = false;
        self.active.completion.borrow_mut().take();
    }

    pub(crate) fn begin_restart<'a>(
        &mut self,
        candidate: PreparedHandler,
        migrate: impl FnMut(WorkerId, &Checkpoint, ServiceKey) -> Result<Checkpoint, ServiceError> + 'a,
        commit: impl FnOnce() -> Result<(), ServiceError> + 'a,
    ) -> Result<RestartState<'a>, RouteError> {
        if self.restarting {
            return Err(RouteError::RestartInProgress);
        }
        if candidate.router != self.id {
            return Err(RouteError::WrongRouter);
        }
        if candidate.base != self.active.id {
            return Err(RouteError::StaleCandidate);
        }
        self.check_service(candidate.api.service)?;
        crate::environment::check_bindings(&candidate.bindings, &self.resources)?;
        let accepting = self.accepting;
        self.accepting = false;
        self.restarting = true;
        // Establish the interruption guard before invoking user-supplied Wakers.
        let restart = RestartState {
            candidate: Some(candidate),
            migrate: Box::new(migrate),
            commit: Some(Box::new(commit)),
            accepting,
            finished: false,
        };
        // A Waker may unwind while stop is signalled. Restore the control flag,
        // but retain paused admission and already delivered shutdown requests.
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let generations = std::iter::once(self.active.clone())
                .chain(self.retired.iter().cloned())
                .collect::<Vec<_>>();
            for generation in generations {
                let controls = generation
                    .controls
                    .borrow()
                    .iter()
                    .filter_map(Weak::upgrade)
                    .collect::<Vec<_>>();
                for control in controls {
                    control.stop();
                }
            }
        }));
        if let Err(payload) = stopped {
            self.restarting = false;
            std::panic::resume_unwind(payload);
        }
        Ok(restart)
    }

    fn validate_checkpoint(
        &self,
        state: &Checkpoint,
        schema: ServiceKey,
    ) -> Result<(), RouteError> {
        if schema.id == 0 || state.schema != schema {
            return Err(RouteError::StateSchemaMismatch);
        }
        if state.bytes.len() > self.options.max_checkpoint_bytes {
            return Err(RouteError::CheckpointTooLarge);
        }
        Ok(())
    }
}

type Migrator<'a> =
    dyn FnMut(WorkerId, &Checkpoint, ServiceKey) -> Result<Checkpoint, ServiceError> + 'a;

pub(crate) struct RestartState<'a> {
    candidate: Option<PreparedHandler>,
    migrate: Box<Migrator<'a>>,
    commit: Option<Box<dyn FnOnce() -> Result<(), ServiceError> + 'a>>,
    accepting: bool,
    finished: bool,
}

impl RestartState<'_> {
    fn cutover(&mut self, router: &mut RequestRouter) -> Result<u64, RouteError> {
        crate::environment::check_bindings(
            &self.candidate.as_ref().unwrap().bindings,
            &router.resources,
        )?;
        for worker in &router.workers {
            if worker.task.status() != Some(RunStatus::Done) {
                return Err(RouteError::WorkerFailed(worker.id));
            }
        }
        let candidate = self.candidate.as_ref().unwrap();
        let target = candidate.api.state_schema;
        // Stage all migrations and task factories before replacing the active
        // generation. Migrators must not mutate shared resources or perform IO.
        let mut states = Vec::new();
        for worker in &router.workers {
            let state = worker
                .task
                .control
                .checkpoint
                .borrow()
                .clone()
                .ok_or(RouteError::StateSchemaMismatch)?;
            let migrated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (self.migrate)(worker.id, &state, target)
            }))
            .unwrap_or(Err(ServiceError::PANICKED))
            .map_err(|error| RouteError::Migration {
                worker: worker.id,
                error,
            })?;
            router.validate_checkpoint(&migrated, target)?;
            states.push(migrated);
        }
        let candidate = self.candidate.take().unwrap();
        let next = Rc::new(Generation {
            id: router
                .active
                .id
                .checked_add(1)
                .ok_or(RouteError::GenerationExhausted)?,
            api: candidate.api,
            in_flight: Cell::new(0),
            scope: crate::generation_scope::GenerationScope::new(
                candidate.image.as_ref().map(GenerationOwner::lease),
            ),
            image: candidate.image,
            controls: RefCell::new(Vec::new()),
            completion: router.active.completion.clone(),
            activity_failed: Cell::new(false),
            environment: candidate.environment,
            bindings: candidate.bindings,
        });
        let mut workers = Vec::new();
        for (worker, state) in router.workers.iter().zip(states) {
            let task = router.spawn(&next, worker.resource, &worker.input, Some(state))?;
            workers.push(Worker {
                id: worker.id,
                resource: worker.resource,
                input: worker.input.clone(),
                task,
            });
        }
        crate::environment::check_bindings(&next.bindings, &router.resources)?;
        // No business future has been polled. All fallible SDK preparation is
        // finished. The application's transaction is the final fallible step.
        let commit = self.commit.take().expect("host commit called once");
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(commit))
            .unwrap_or(Err(ServiceError::PANICKED))
            .map_err(RouteError::HostCommit)?;
        if let Some(image) = &next.image {
            image.activate();
        }
        router.retired.retain(|generation| {
            generation.in_flight.get() != 0 || generation.activity_failed.get()
        });
        router.retired.push(router.active.clone());
        if let Some(image) = &router.active.image {
            image.retire();
        }
        router.active = next;
        router.workers = workers;
        router.accepting = self.accepting;
        Ok(router.active.id)
    }
}

impl RestartState<'_> {
    pub(crate) fn poll(
        &mut self,
        router: &mut RequestRouter,
        cx: &mut Context<'_>,
    ) -> Poll<Result<u64, RouteError>> {
        assert!(!self.finished, "completed Restart polled again");
        *router.active.completion.borrow_mut() = Some(cx.waker().clone());
        let _ = router.poll_workers(cx);
        if router.in_flight() != 0 {
            return Poll::Pending;
        }
        let failed = std::iter::once(router.active.clone())
            .chain(router.retired.iter().cloned())
            .find(|generation| generation.activity_failed.get());
        let result = if let Some(generation) = failed {
            Err(RouteError::ActivityFailed {
                generation: generation.id,
            })
        } else {
            self.cutover(router)
        };
        self.finished = true;
        self.finish(router);
        Poll::Ready(result)
    }
    pub(crate) fn finish(&mut self, router: &mut RequestRouter) {
        router.finish_restart();
        // Failure/interruption keeps admission paused; no rollback is implied.
    }
}

pub struct Restart<'a> {
    router: &'a mut RequestRouter,
    state: RestartState<'a>,
}
impl Future for Restart<'_> {
    type Output = Result<u64, RouteError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.state.poll(this.router, cx)
    }
}
impl Drop for Restart<'_> {
    fn drop(&mut self) {
        self.state.finish(self.router);
    }
}
