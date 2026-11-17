//! Typed development facade; backend selection does not change handler clients.
use super::{BridgeOptions, Error, ModuleBuilder, ModuleClient, ModuleHost};
use std::{path::PathBuf, time::Duration};
#[cfg(feature = "jit-dev")]
#[path = "layered_development.rs"]
mod layered;
#[cfg(feature = "dev-tools")]
#[path = "native_development.rs"]
mod native;

/// Preserve in-flight work with Drain, or join it and migrate workers with Restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadMode {
    Drain,
    Restart,
}

/// Source location, Cargo package and save debounce. Cargo's metadata resolves
/// workspace targets and local dependencies; paths need not follow this repository.
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
#[non_exhaustive]
pub enum ReloadEvent {
    Building,
    Restarting,
    Reloaded {
        generation: u64,
    },
    BuildFailed {
        diagnostics: String,
    },
    Rejected {
        error: Error,
    },
    Superseded,
    WatchFailed {
        message: String,
    },
    /// JIT-specific capacity, foundation or recovery state. Ordinary build,
    /// contract, cutover and Restart events retain the common variants above.
    #[cfg(feature = "jit-dev")]
    Layered {
        event: crate::jit::development::LayeredDevEvent,
    },
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
            #[cfg(feature = "jit-dev")]
            Self::Layered { event } => write!(f, "Layered development: {:?}{}", event.state,
                event.diagnostic.as_ref().map_or(String::new(), |message| format!(": {message}"))),
        }
    }
}

/// Select the development producer. Existing `DevOptions` convert to Native;
/// `LayeredDevOptions` convert to COFF/ELF with Drain and KeepLoaded.
pub enum DevBackend {
    Native(DevOptions),
    #[cfg(feature = "jit-dev")]
    Layered {
        options: Box<crate::jit::development::LayeredDevOptions>,
        reload_mode: ReloadMode,
    },
}
impl From<DevOptions> for DevBackend {
    fn from(options: DevOptions) -> Self {
        Self::Native(options)
    }
}
#[cfg(feature = "jit-dev")]
impl From<crate::jit::development::LayeredDevOptions> for DevBackend {
    fn from(options: crate::jit::development::LayeredDevOptions) -> Self {
        Self::Layered {
            options: Box::new(options),
            reload_mode: ReloadMode::Drain,
        }
    }
}
impl DevBackend {
    pub fn with_reload_mode(mut self, mode: ReloadMode) -> Self {
        match &mut self {
            Self::Native(options) => options.reload_mode = mode,
            #[cfg(feature = "jit-dev")]
            Self::Layered { reload_mode, .. } => *reload_mode = mode,
        }
        self
    }
}

/// Typed save-time reload with interchangeable native or layered object builds.
/// `start` waits for the first validated generation. Later builds run on a worker;
/// call `update` regularly, including while requests run. Handles and clients stay
/// attached to the same ModuleHost across successful updates.
pub struct DevModule(Backend);
enum Backend {
    #[cfg(feature = "dev-tools")]
    Native(Box<native::NativeDevModule>),
    #[cfg(feature = "jit-dev")]
    Layered(Box<layered::LayeredModule>),
}
impl DevModule {
    /// # Safety
    /// Trust all future edits, dependencies, build scripts and native artifacts.
    /// Native builds obey `ModuleBuilder::load`; layered builds obey
    /// `LayeredArtifact::into_trusted`. Both default to KeepLoaded.
    pub unsafe fn start(
        builder: ModuleBuilder,
        options: impl Into<DevBackend>,
    ) -> Result<Self, Error> {
        match options.into() {
            DevBackend::Native(options) => {
                #[cfg(feature = "dev-tools")]
                {
                    Ok(Self(Backend::Native(Box::new(
                        native::NativeDevModule::start(builder, options)?,
                    ))))
                }
                #[cfg(not(feature = "dev-tools"))]
                {
                    let _ = (builder, options);
                    Err(Error::Configuration(
                        "native development requires dev-tools".into(),
                    ))
                }
            }
            #[cfg(feature = "jit-dev")]
            DevBackend::Layered {
                options,
                reload_mode,
            } => Ok(Self(Backend::Layered(Box::new(
                layered::LayeredModule::start(builder, *options, reload_mode, false)?,
            )))),
        }
    }
    /// Opt into Managed COFF/ELF collection; native settings are rejected.
    /// # Safety
    /// In addition to `start`, all future builds must obey
    /// `TrustedLayeredArtifact::into_managed`. External adapters must join through
    /// their final callback/Waker return; no unmanaged code references may escape.
    #[cfg(feature = "jit-dev")]
    pub unsafe fn start_managed(
        builder: ModuleBuilder,
        options: impl Into<DevBackend>,
    ) -> Result<Self, Error> {
        match options.into() {
            DevBackend::Layered {
                options,
                reload_mode,
            } => Ok(Self(Backend::Layered(Box::new(
                layered::LayeredModule::start(builder, *options, reload_mode, true)?,
            )))),
            DevBackend::Native(_) => Err(Error::Configuration(
                "Managed development requires a layered backend".into(),
            )),
        }
    }
    pub fn host(&self) -> &ModuleHost {
        match &self.0 {
            #[cfg(feature = "dev-tools")]
            Backend::Native(module) => module.host(),
            #[cfg(feature = "jit-dev")]
            Backend::Layered(module) => module.host(),
        }
    }
    /// `update` drives the owner-side requests and cancellation. For independent
    /// executor notification, use `host().bridge()` and poll its driver directly.
    pub fn bridge(&mut self, options: BridgeOptions) -> Result<ModuleClient, Error> {
        match &mut self.0 {
            #[cfg(feature = "dev-tools")]
            Backend::Native(module) => module.bridge(options),
            #[cfg(feature = "jit-dev")]
            Backend::Layered(module) => module.bridge(options),
        }
    }
    pub fn update(&mut self) -> Option<ReloadEvent> {
        match &mut self.0 {
            #[cfg(feature = "dev-tools")]
            Backend::Native(module) => module.update(),
            #[cfg(feature = "jit-dev")]
            Backend::Layered(module) => module.update(),
        }
    }
    /// Stop source publication and join build workers. This can wait for Cargo.
    /// Requests are not cancelled. An interrupted Restart keeps admission paused;
    /// continue polling cleanup and use the usual host recovery APIs.
    pub fn shutdown(&mut self) -> Result<(), Error> {
        match &mut self.0 {
            #[cfg(feature = "dev-tools")]
            Backend::Native(module) => module.shutdown(),
            #[cfg(feature = "jit-dev")]
            Backend::Layered(module) => module.shutdown(),
        }
    }
    /// Build metrics, foundation diagnostics and session-owned image retention.
    #[cfg(feature = "jit-dev")]
    pub fn layered_snapshot(&self) -> Option<crate::jit::development::LayeredDevSnapshot> {
        match &self.0 {
            #[cfg(feature = "dev-tools")]
            Backend::Native(_) => None,
            Backend::Layered(module) => Some(module.snapshot()),
        }
    }
    /// Recheck a layered project without replacing its engine or resetting budgets.
    /// Returns false for native development or a stopped session.
    #[cfg(feature = "jit-dev")]
    pub fn retry_layered(&mut self) -> bool {
        match &mut self.0 {
            #[cfg(feature = "dev-tools")]
            Backend::Native(_) => false,
            Backend::Layered(module) => module.retry(),
        }
    }
}
