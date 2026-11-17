use super::{ReloadEvent, ReloadMode};
use crate::jit::development::{
    Activation, DevelopmentTarget, LayeredDevCore, LayeredDevOptions, LayeredDevSnapshot,
    LayeredDevState, TargetFailure,
};
use crate::jit::JitImageId;
use crate::typed::{
    BridgeDriver, BridgeOptions, Error, ModuleBuilder, ModuleClient, ModuleHost, ModuleRestart,
};
use crate::{LoadedHandler, PreparedHandler};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

pub(super) struct LayeredModule {
    core: LayeredDevCore,
    target: TypedTarget,
    bridges: Vec<BridgeDriver>,
}
impl LayeredModule {
    // Safety is accepted only through DevModule::start/start_managed.
    pub(super) fn start(
        builder: ModuleBuilder,
        options: LayeredDevOptions,
        mode: ReloadMode,
        managed: bool,
    ) -> Result<Self, Error> {
        let generations = builder.development_budget();
        let core =
            LayeredDevCore::start(options, managed, generations).map_err(Error::Development)?;
        let mut module = Self {
            core,
            target: TypedTarget {
                builder: Some(builder),
                host: None,
                mode,
                restart: None,
                error: None,
            },
            bridges: Vec::new(),
        };
        // Match native DevModule: host() and handler registration are available
        // when start returns. Watches are already installed during this build.
        loop {
            if let Some(event) = module
                .core
                .poll_update(&mut module.target, &mut Context::from_waker(Waker::noop()))
            {
                match event.state {
                    LayeredDevState::Activated { .. } => return Ok(module),
                    LayeredDevState::Building { .. } | LayeredDevState::Superseded => {}
                    _ => {
                        let error = module.target.error.take().unwrap_or_else(|| {
                            Error::Development(
                                event
                                    .diagnostic
                                    .unwrap_or_else(|| format!("{:?}", event.state)),
                            )
                        });
                        module.core.shutdown().map_err(Error::Development)?;
                        return Err(error);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    pub(super) fn host(&self) -> &ModuleHost {
        self.target.host.as_ref().unwrap()
    }
    pub(super) fn bridge(&mut self, options: BridgeOptions) -> Result<ModuleClient, Error> {
        let (client, driver) = self.host().bridge(options)?;
        self.bridges.push(driver);
        Ok(client)
    }
    pub(super) fn update(&mut self) -> Option<ReloadEvent> {
        let mut cx = Context::from_waker(Waker::noop());
        self.bridges
            .retain_mut(|driver| Pin::new(driver).poll(&mut cx).is_pending());
        let event = self.core.poll_update(&mut self.target, &mut cx)?;
        Some(match event.state {
            LayeredDevState::Building { .. } => ReloadEvent::Building,
            LayeredDevState::Restarting { .. } => ReloadEvent::Restarting,
            LayeredDevState::Activated { generation, .. } => ReloadEvent::Reloaded { generation },
            LayeredDevState::BuildFailed => ReloadEvent::BuildFailed {
                diagnostics: event.diagnostic.unwrap_or_default(),
            },
            LayeredDevState::Rejected => {
                ReloadEvent::Rejected {
                    error: self.target.error.take().unwrap_or_else(|| {
                        Error::Development(event.diagnostic.unwrap_or_default())
                    }),
                }
            }
            LayeredDevState::Superseded => ReloadEvent::Superseded,
            LayeredDevState::WatchUnavailable => ReloadEvent::WatchFailed {
                message: event.diagnostic.unwrap_or_default(),
            },
            _ => {
                self.target.error.take();
                ReloadEvent::Layered { event }
            }
        })
    }
    pub(super) fn snapshot(&self) -> LayeredDevSnapshot {
        self.core.snapshot(self.target.has_active())
    }
    pub(super) fn retry(&mut self) -> bool {
        if self.snapshot().state == LayeredDevState::Stopped {
            return false;
        }
        self.core.retry();
        true
    }
    pub(super) fn shutdown(&mut self) -> Result<(), Error> {
        self.target.restart.take();
        self.core.shutdown().map_err(Error::Development)
    }
}

struct TypedTarget {
    builder: Option<ModuleBuilder>,
    host: Option<ModuleHost>,
    mode: ReloadMode,
    restart: Option<ModuleRestart>,
    // Preserve structured contract differences through the common build pipeline.
    error: Option<Error>,
}
enum TypedCandidate {
    Initial(ModuleHost),
    Update(Box<PreparedHandler>),
}
impl TypedTarget {
    fn remember<T>(&mut self, result: Result<T, Error>) -> Result<T, TargetFailure> {
        result.map_err(|error| {
            let failure = TargetFailure {
                budget: matches!(
                    error,
                    Error::Route(
                        crate::RouteError::GenerationLimit { .. }
                            | crate::RouteError::GenerationExhausted
                    )
                ),
                message: error.to_string(),
            };
            self.error = Some(error);
            failure
        })
    }
}
impl DevelopmentTarget for TypedTarget {
    type Candidate = TypedCandidate;
    fn has_active(&self) -> bool {
        self.host.is_some()
    }
    fn active_image(&self) -> Option<JitImageId> {
        self.host
            .as_ref()
            .and_then(|host| host.0.router.borrow().jit_image_id())
    }
    fn prepare(&mut self, loaded: LoadedHandler) -> Result<Self::Candidate, TargetFailure> {
        let result = match &self.host {
            Some(host) => host
                .prepare_loaded(loaded)
                .map(|candidate| TypedCandidate::Update(Box::new(candidate))),
            None => self
                .builder
                .as_ref()
                .unwrap()
                .development_copy()
                .from_loaded(loaded)
                .map(TypedCandidate::Initial),
        };
        self.remember(result)
    }
    fn activate(&mut self, candidate: Self::Candidate) -> Result<Activation, TargetFailure> {
        match candidate {
            TypedCandidate::Initial(host) => {
                let generation = host.generation();
                self.host = Some(host);
                self.builder = None;
                Ok(Activation::Ready(generation))
            }
            TypedCandidate::Update(candidate) => {
                let host = self.host.as_ref().unwrap();
                match self.mode {
                    ReloadMode::Drain => {
                        let result = host.activate(*candidate);
                        self.remember(result).map(Activation::Ready)
                    }
                    ReloadMode::Restart => {
                        let result = host.restart_prepared(*candidate);
                        self.restart = Some(self.remember(result)?);
                        Ok(Activation::Restarting)
                    }
                }
            }
        }
    }
    fn poll_restart(&mut self, cx: &mut Context<'_>) -> Poll<Result<u64, TargetFailure>> {
        match Pin::new(self.restart.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.restart = None;
                Poll::Ready(self.remember(result))
            }
        }
    }
    fn maintain(&mut self, cx: &mut Context<'_>, _: bool) {
        if let Some(host) = &self.host {
            let _ = host.poll_cleanup(cx);
            let _ = host.poll_workers(cx);
        }
    }
    fn release_retired(&mut self) {
        if let Some(host) = &self.host {
            host.0.router.borrow_mut().release_retired_generations();
        }
    }
    fn cancel_retired(&self, image: JitImageId) {
        if let Some(host) = &self.host {
            host.0.router.borrow().cancel_retired_image(image);
        }
    }
}
