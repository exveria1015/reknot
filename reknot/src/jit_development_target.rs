//! Internal application boundary shared by raw and typed development sessions.
use super::*;
use crate::{LoadedHandler, PreparedHandler, RouteError, RouterOptions};
use std::task::Poll;

pub(crate) struct TargetFailure {
    pub message: String,
    pub budget: bool,
}
impl From<RouteError> for TargetFailure {
    fn from(error: RouteError) -> Self {
        Self {
            budget: matches!(
                error,
                RouteError::GenerationLimit { .. } | RouteError::GenerationExhausted
            ),
            message: error.to_string(),
        }
    }
}
pub(crate) enum Activation {
    Ready(u64),
    #[cfg(feature = "typed")]
    Restarting,
}

/// Preparation validates the application contract before the core's final source
/// revision check. Activation alone changes admission. Implementations never
/// detach the LoadedHandler's code owner from its callbacks.
pub(crate) trait DevelopmentTarget {
    type Candidate;
    fn has_active(&self) -> bool;
    fn active_image(&self) -> Option<CoffImageId>;
    fn prepare(&mut self, loaded: LoadedHandler) -> Result<Self::Candidate, TargetFailure>;
    fn activate(&mut self, candidate: Self::Candidate) -> Result<Activation, TargetFailure>;
    fn poll_restart(&mut self, cx: &mut Context<'_>) -> Poll<Result<u64, TargetFailure>>;
    fn maintain(&mut self, cx: &mut Context<'_>, managed: bool);
    fn release_retired(&mut self);
    fn cancel_retired(&self, image: CoffImageId);
}

pub(super) struct RouterTarget {
    pub router: Option<RequestRouter>,
    pub resources: HostResources,
    pub options: RouterOptions,
}
pub(super) enum RouterCandidate {
    Initial(RequestRouter),
    Update(Box<PreparedHandler>),
}
impl DevelopmentTarget for RouterTarget {
    type Candidate = RouterCandidate;
    fn has_active(&self) -> bool {
        self.router.is_some()
    }
    fn active_image(&self) -> Option<CoffImageId> {
        self.router.as_ref().and_then(RequestRouter::jit_image_id)
    }
    fn prepare(&mut self, loaded: LoadedHandler) -> Result<Self::Candidate, TargetFailure> {
        match &self.router {
            Some(router) => router
                .prepare_loaded(loaded)
                .map(|candidate| RouterCandidate::Update(Box::new(candidate))),
            None => RequestRouter::from_loaded_with_options(
                loaded,
                self.resources.clone(),
                self.options.clone(),
            )
            .map(RouterCandidate::Initial),
        }
        .map_err(Into::into)
    }
    fn activate(&mut self, candidate: Self::Candidate) -> Result<Activation, TargetFailure> {
        let generation = match candidate {
            RouterCandidate::Initial(router) => {
                let generation = router.generation();
                self.router = Some(router);
                generation
            }
            RouterCandidate::Update(candidate) => {
                self.router.as_mut().unwrap().activate(*candidate)?
            }
        };
        Ok(Activation::Ready(generation))
    }
    fn poll_restart(&mut self, _: &mut Context<'_>) -> Poll<Result<u64, TargetFailure>> {
        unreachable!("raw layered sessions only use Drain")
    }
    fn maintain(&mut self, cx: &mut Context<'_>, managed: bool) {
        if managed {
            let _ = crate::poll_cancelled_requests(cx);
            if let Some(router) = &mut self.router {
                let _ = router.poll_workers(cx);
            }
        }
    }
    fn release_retired(&mut self) {
        if let Some(router) = &mut self.router {
            router.release_retired_generations();
        }
    }
    fn cancel_retired(&self, image: CoffImageId) {
        if let Some(router) = &self.router {
            router.cancel_retired_image(image);
        }
    }
}
