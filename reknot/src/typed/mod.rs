//! Typed, generation-bound handlers. Enable the `typed` feature in host and logic.
//!
//! Declare `Handler<Input, Output>` contracts in a shared protocol crate, mark
//! ordinary functions with [`crate::handler`], and export a single module with
//! [`crate::export_handlers`]. Only owned JSON bytes cross the native boundary.
//! Contract versions describe wire format **and meaning**: increment them when
//! changing either. Rust type names are an extra diagnostic, not schema inference.
//!
//! The declared contract checks a function's Rust signature at compile time:
//! ```compile_fail
//! const GREET: reknot::typed::Handler<String, String> =
//!     reknot::typed::Handler::new("bot.greet", 1);
//! #[reknot::handler(GREET)]
//! fn greet(input: u64) -> String { input.to_string() }
//! ```
//! Inputs must be owned:
//! ```compile_fail
//! const GREET: reknot::typed::Handler<String, String> =
//!     reknot::typed::Handler::new("bot.greet", 1);
//! #[reknot::handler(GREET)]
//! fn greet(input: &str) -> String { input.to_owned() }
//! ```
//! Owner-thread handles cannot be moved to a multithreaded executor:
//! ```compile_fail
//! fn require_send<T: Send>() {}
//! require_send::<reknot::typed::HandlerHandle<String, String>>();
//! ```
mod runtime;
pub use runtime::{Call, HandlerHandle, ModuleBuilder, ModuleHost};
mod bridge;
pub use bridge::{
    BridgeDriver, BridgeOptions, BridgeShutdown, HandlerClient, ModuleClient, RemoteCall,
};
#[cfg(feature = "dev-tools")]
mod development;
#[cfg(feature = "dev-tools")]
pub use development::{DevModule, DevOptions, ReloadEvent};

use crate::{RouteError, ServiceKey};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fmt;
use std::marker::PhantomData;

/// Maximum encoded input or result, independently on each side of the DLL.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_HANDLERS: usize = 64;

/// A stable, namespaced identity and explicit wire-contract version.
/// Use owned, Serde-compatible types; SDK objects remain in the host.
pub struct Handler<I, O> {
    name: &'static str,
    version: u32,
    marker: PhantomData<fn(I) -> O>,
}
impl<I, O> Copy for Handler<I, O> {}
impl<I, O> Clone for Handler<I, O> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<I, O> Handler<I, O> {
    pub const fn new(name: &'static str, version: u32) -> Self {
        Self {
            name,
            version,
            marker: PhantomData,
        }
    }
}
impl<I: Serialize + DeserializeOwned + 'static, O: Serialize + DeserializeOwned + 'static>
    Handler<I, O>
{
    pub fn descriptor(self) -> HandlerSpec {
        HandlerSpec {
            name: self.name.into(),
            version: self.version,
            input: std::any::type_name::<I>().into(),
            output: std::any::type_name::<O>().into(),
        }
    }
}

/// Serializable contract metadata; function names and display labels are not IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerSpec {
    pub name: String,
    pub version: u32,
    pub input: String,
    pub output: String,
}
impl HandlerSpec {
    pub fn key(&self) -> u64 {
        key(&self.name)
    }
}

/// One crate/DLL is one atomic Drain update unit. Keep its protocol in a shared crate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    pub name: String,
    pub version: u32,
    handlers: Vec<HandlerSpec>,
}
impl Module {
    pub fn new(name: impl Into<String>, version: u32) -> Self {
        Self {
            name: name.into(),
            version,
            handlers: Vec::new(),
        }
    }
    pub fn with<I, O>(mut self, handler: Handler<I, O>) -> Self
    where
        I: Serialize + DeserializeOwned + 'static,
        O: Serialize + DeserializeOwned + 'static,
    {
        self.handlers.push(handler.descriptor());
        self
    }
    pub fn handlers(&self) -> &[HandlerSpec] {
        &self.handlers
    }
    pub fn validate(&self) -> Result<(), Error> {
        if !valid_name(&self.name)
            || self.version == 0
            || self.handlers.is_empty()
            || self.handlers.len() > MAX_HANDLERS
        {
            return Err(Error::Configuration(
                "use a nonempty module name, positive version and 1..=64 handlers".into(),
            ));
        }
        for (index, handler) in self.handlers.iter().enumerate() {
            if !valid_name(&handler.name) || handler.version == 0 {
                return Err(Error::Configuration(format!(
                    "handler {:?} needs a nonempty namespaced name and positive version",
                    handler.name
                )));
            }
            if self.handlers[..index]
                .iter()
                .any(|other| other.key() == handler.key())
            {
                return Err(Error::Configuration(format!("duplicate handler key: {} (including hash collisions); choose distinct stable names", handler.name)));
            }
        }
        Ok(())
    }
    pub fn builder(self) -> ModuleBuilder {
        ModuleBuilder::new(self)
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.chars().any(|c| c.is_control() || c.is_whitespace())
}
fn key(name: &str) -> u64 {
    name.bytes().fold(0xcbf29ce484222325, |hash, b| {
        (hash ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

#[derive(Debug)]
pub enum Error {
    Configuration(String),
    Contract {
        module: String,
        expected: Vec<HandlerSpec>,
        source: RouteError,
    },
    Route(RouteError),
    Codec(String),
    TooLarge {
        limit: usize,
    },
    Busy,
    Cancelled,
    BridgeClosed,
    HandlerFailed {
        handler: String,
        generation: u64,
    },
    MissingReply,
    Development(String),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) | Self::Development(message) => f.write_str(message),
            Self::Contract { module, expected, source } => {
                write!(f, "module {module} rejected: {source}. Expected")?;
                for handler in expected { write!(f, " {} v{} ({} -> {});", handler.name, handler.version, handler.input, handler.output)?; }
                write!(f, " check export_handlers and shared protocol versions; restart the host after a protocol change")
            }
            Self::Route(RouteError::GenerationLimit { limit }) => write!(f, "load-attempt budget reached ({limit}); restart the development host (DLL reclamation does not refund attempts)"),
            Self::Route(error) => fmt::Display::fmt(error, f),
            Self::Codec(error) => write!(f, "typed message conversion failed: {error}"),
            Self::TooLarge { limit } => write!(f, "encoded message exceeds {limit} bytes"),
            Self::Busy => write!(f, "module is busy or its in-flight request limit was reached; try a later event"),
            Self::Cancelled => write!(f, "request cancelled; registered cleanup has finished; external effects were not rolled back"),
            Self::BridgeClosed => write!(f, "handler bridge is closed or its owner driver was dropped"),
            Self::HandlerFailed { handler, generation } => write!(f, "handler {handler} failed in generation {generation} (panic or registered cleanup failure); the event was not replayed"),
            Self::MissingReply => write!(f, "handler completed without a typed reply"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Route(source) | Self::Contract { source, .. } => Some(source),
            _ => None,
        }
    }
}
impl From<RouteError> for Error {
    fn from(error: RouteError) -> Self {
        Self::Route(error)
    }
}
fn route_error(module: &Module, source: RouteError) -> Error {
    if matches!(
        source,
        RouteError::InitData(_)
            | RouteError::InitSchemaMismatch { .. }
            | RouteError::ContractMismatch { .. }
    ) {
        Error::Contract {
            module: module.name.clone(),
            expected: module.handlers.clone(),
            source,
        }
    } else {
        Error::Route(source)
    }
}

// Limit while serializing, before allocating an arbitrarily large second buffer.
fn encode<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, Error> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
        full: bool,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.full = true;
                return Err(std::io::Error::other("message too large"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
        full: false,
    };
    let result = serde_json::to_writer(&mut writer, value);
    if writer.full {
        return Err(Error::TooLarge { limit });
    }
    result.map_err(|error| Error::Codec(error.to_string()))?;
    Ok(writer.bytes)
}

/// Implementation support for generated code; not an application API.
#[doc(hidden)]
pub mod __private {
    use super::*;
    use crate::{RequestContext, ServiceError, TaskHandle};
    use std::future::Future;
    use std::pin::Pin;
    pub const SERVICE: ServiceKey = ServiceKey::new(0x464c_4f57_5459_5045, 1);
    pub const SCHEMA: ServiceKey = ServiceKey::new(0x464c_4f57_4d4f_4455, 1);
    pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<(), ServiceError>>>>;

    /// # Safety
    /// `bytes` is readable for `len` bytes on the owner thread. The factory is pure.
    pub unsafe fn validate(
        bytes: *const u8,
        len: usize,
        manifest: impl FnOnce() -> (Module, Vec<HandlerSpec>),
    ) -> u32 {
        if bytes.is_null() || len > crate::MAX_INIT_BYTES {
            return ServiceError::INVALID_ARGUMENT.0;
        }
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let result = (|| {
                let expected: Module =
                    serde_json::from_slice(std::slice::from_raw_parts(bytes, len)).ok()?;
                let (mut declared, mut actual) = manifest();
                expected.validate().ok()?;
                declared.validate().ok()?;
                let mut expected = expected;
                expected.handlers.sort_by(|a, b| a.name.cmp(&b.name));
                declared.handlers.sort_by(|a, b| a.name.cmp(&b.name));
                actual.sort_by(|a, b| a.name.cmp(&b.name));
                (expected == declared && declared.handlers == actual).then_some(())
            })();
            if result.is_some() {
                0
            } else {
                ServiceError::CONTRACT_MISMATCH.0
            }
        }))
        .unwrap_or(ServiceError::PANICKED.0)
    }
    /// # Safety
    /// Same readable descriptor and ownership contract as `RequestContext::from_raw`.
    pub unsafe fn make(
        raw: *const crate::HandlerRequest,
        dispatch: impl FnOnce(RequestContext) -> Result<HandlerFuture, ServiceError>,
    ) -> TaskHandle {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let context = RequestContext::from_raw(raw)?;
            dispatch(context).map(crate::internal::make_handler_future)
        }))
        .ok()
        .and_then(Result::ok)
        .unwrap_or(TaskHandle {
            data: std::ptr::null_mut(),
            vtable: std::ptr::null(),
        })
    }
    pub fn request_key(context: &RequestContext) -> Result<u64, ServiceError> {
        let bytes = context.input();
        if bytes.len() > MAX_MESSAGE_BYTES + 8 {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let prefix = bytes.get(..8).ok_or(ServiceError::INVALID_ARGUMENT)?;
        Ok(u64::from_le_bytes(prefix.try_into().unwrap()))
    }
    pub async fn invoke<I, O, F>(
        _: Handler<I, O>,
        context: RequestContext,
        call: impl FnOnce(RequestContext, I) -> F,
    ) -> Result<(), ServiceError>
    where
        I: Serialize + DeserializeOwned + 'static,
        O: Serialize + DeserializeOwned + 'static,
        F: Future<Output = O>,
    {
        let services = context.services().clone();
        let resource = context.resource();
        let input = context
            .input()
            .get(8..)
            .ok_or(ServiceError::INVALID_ARGUMENT)?;
        let result = match serde_json::from_slice::<I>(input) {
            Ok(input) => encode(&call(context, input).await, MAX_MESSAGE_BYTES),
            Err(error) => Err(Error::Codec(error.to_string())),
        };
        let (operation, bytes) = match result {
            Ok(bytes) => (0, bytes),
            Err(error) => {
                let message = error.to_string();
                (
                    1,
                    message.chars().take(4096).collect::<String>().into_bytes(),
                )
            }
        };
        services
            .call(resource, SERVICE, operation, &bytes, &mut [])
            .map(|_| ())
    }
}
