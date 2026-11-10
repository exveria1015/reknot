//! Message-oriented hot reload with host-owned resources and explicit task lifetimes.
mod activity;
mod async_service;
#[cfg(feature = "native")]
mod cache;
mod environment;
mod generation;
mod generation_local;
mod generation_scope;
#[cfg(feature = "native")]
mod native;
mod notification;
mod request;
mod resources;
#[cfg(feature = "native")]
mod retention;
mod routing;
mod task;
mod thread_service;
#[cfg(feature = "typed")]
pub mod typed;

pub use activity::{ChildStop, ManagedActivity, MAX_MANAGED_ACTIVITIES};
pub use async_service::{
    AsyncLimits, CancellationToken, HostServiceFuture, ServiceCall, MAX_ASYNC_BUFFER,
};
#[cfg(feature = "native")]
pub use cache::{clean_cache, inspect_cache, CacheEntry, CacheOptions, CacheReport, CacheStatus};
pub use environment::{HandlerEnvironment, InitData, InitPayload};
pub use generation_local::GenerationLocal;
pub use generation_scope::MAX_GENERATION_LOCALS;
#[cfg(feature = "native")]
pub use native::{reclaim_native_image, LoadError, ReclaimResult};
pub use notification::HostNotification;
pub use reknot_abi::services::{
    ActivityHandle, ActivityVTable, AsyncOperationId, AsyncPolled, AsyncStarted, HandlerApi,
    HandlerHeader, HandlerRequest, HostServices, HostServicesVTable, ResourceId, ServiceBinding,
    ServiceError, ServiceKey, ServiceRequirement, ServiceResult, ACTIVITY_ABI_VERSION,
    HANDLER_ABI_VERSION, MAX_INIT_BYTES, MAX_SERVICE_REQUIREMENTS, SERVICES_ABI_VERSION,
};
pub use reknot_abi::{HostWaker, HostWakerVTable, RunStatus, TaskHandle, TaskVTable};
pub use reknot_macros::reload_handler;
#[cfg(feature = "typed")]
pub use reknot_macros::{export_handlers, handler};
pub use request::RequestContext;
pub use resources::{HostResource, HostResources, Resource, Services};
#[cfg(feature = "native")]
pub use retention::{
    retention_snapshot, ArtifactId, CopyRemovalError, ImageOutcome, ImageRecord, ImageState,
    RetentionReason, RetentionSnapshot,
};
pub use routing::{
    poll_cancelled_requests, Checkpoint, GenerationBudget, PreparedHandler, RequestRouter,
    RequestTask, Restart, RouteError, RouterOptions, WorkerId,
};
pub use task::FfiFuture;
pub use thread_service::{ServiceClient, ServiceWorker, ThreadService, ThreadServiceLimits};

#[doc(hidden)]
pub mod internal;

#[cfg(feature = "dev-tools")]
pub mod development;
