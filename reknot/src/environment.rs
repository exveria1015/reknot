//! Explicit service binding and owned, codec-checked generation initialization.
use crate::{
    HandlerApi, HostResources, ResourceId, RouteError, ServiceBinding, ServiceError, ServiceKey,
    MAX_INIT_BYTES, MAX_SERVICE_REQUIREMENTS,
};

/// Stable wire schema, never Rust layout/type identity. Codecs and destructors
/// used in validation must be pure, bounded and free of application side effects.
pub trait InitPayload: Sized + 'static {
    const SCHEMA: ServiceKey;
    fn encode(&self) -> Result<Vec<u8>, ServiceError>;
    fn decode(bytes: &[u8]) -> Result<Self, ServiceError>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitData {
    pub schema: ServiceKey,
    pub bytes: Vec<u8>,
}
impl InitData {
    pub fn new(schema: ServiceKey, bytes: Vec<u8>) -> Self {
        Self { schema, bytes }
    }
    pub fn from_value<T: InitPayload>(value: &T) -> Result<Self, ServiceError> {
        let bytes = value.encode()?;
        if !valid_key(T::SCHEMA) || bytes.len() > MAX_INIT_BYTES {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        Ok(Self::new(T::SCHEMA, bytes))
    }
    pub fn decode<T: InitPayload>(&self) -> Result<T, ServiceError> {
        if self.schema != T::SCHEMA {
            return Err(ServiceError::CONTRACT_MISMATCH);
        }
        T::decode(&self.bytes)
    }
}
/// Generation-owned configuration. Unrequested bindings are not exposed to logic.
#[derive(Clone, Debug, Default)]
pub struct HandlerEnvironment {
    bindings: Vec<(u64, ResourceId)>,
    pub initial: Option<InitData>,
}
impl HandlerEnvironment {
    pub fn bind(mut self, binding: u64, resource: ResourceId) -> Self {
        self.bindings.push((binding, resource));
        self
    }
    pub fn with_initial(mut self, initial: InitData) -> Self {
        self.initial = Some(initial);
        self
    }
}

pub(crate) fn valid_key(key: ServiceKey) -> bool {
    key.id != 0 && key.version != 0 && key.reserved == 0
}
pub(crate) fn absent_key(key: ServiceKey) -> bool {
    key == ServiceKey::new(0, 0)
}
pub(crate) fn validate_manifest(api: &HandlerApi) -> Result<(), RouteError> {
    let count = api.required_count as usize;
    if count > MAX_SERVICE_REQUIREMENTS {
        return Err(RouteError::InvalidApi);
    }
    let mut seen = Vec::new();
    for requirement in &api.required_services[..count] {
        if requirement.binding == 0
            || !valid_key(requirement.service)
            || seen.contains(&requirement.binding)
        {
            return Err(RouteError::InvalidApi);
        }
        seen.push(requirement.binding);
    }
    if (absent_key(api.init_schema) && api.validate_init.is_none())
        || (valid_key(api.init_schema) && api.validate_init.is_some())
    {
        Ok(())
    } else {
        Err(RouteError::InvalidApi)
    }
}

pub(crate) fn negotiate(
    api: &HandlerApi,
    environment: &HandlerEnvironment,
    resources: &HostResources,
) -> Result<Vec<ServiceBinding>, RouteError> {
    if environment.bindings.len() > MAX_SERVICE_REQUIREMENTS {
        return Err(RouteError::InvalidEnvironment);
    }
    let mut seen = Vec::new();
    for &(binding, _) in &environment.bindings {
        if binding == 0 || seen.contains(&binding) {
            return Err(RouteError::InvalidEnvironment);
        }
        seen.push(binding);
    }
    let actual = environment
        .initial
        .as_ref()
        .map(|init| init.schema)
        .unwrap_or(ServiceKey::new(0, 0));
    if environment.initial.is_some() && absent_key(api.init_schema) {
        return Err(RouteError::InitData(ServiceError::INVALID_ARGUMENT));
    }
    if actual != api.init_schema {
        return Err(RouteError::InitSchemaMismatch {
            expected: api.init_schema,
            found: actual,
        });
    }
    if let Some(initial) = &environment.initial {
        if initial.bytes.len() > MAX_INIT_BYTES {
            return Err(RouteError::InitData(ServiceError::INVALID_ARGUMENT));
        }
        // Native callbacks are trusted; the SDK-generated validator catches its own
        // Rust unwinds. Validation runs before any request factory or stop signal.
        let result = unsafe {
            api.validate_init.ok_or(RouteError::InvalidApi)?(
                initial.bytes.as_ptr(),
                initial.bytes.len(),
            )
        };
        if result != 0 {
            return Err(RouteError::InitData(ServiceError(result)));
        }
    }
    let mut bindings = Vec::new();
    for required in &api.required_services[..api.required_count as usize] {
        let resource = environment
            .bindings
            .iter()
            .find(|(binding, _)| *binding == required.binding)
            .map(|(_, id)| *id)
            .ok_or(RouteError::MissingService {
                binding: required.binding,
                expected: required.service,
            })?;
        let found = resources.service(resource).map_err(RouteError::Resource)?;
        if found != required.service {
            return Err(RouteError::ContractMismatch {
                expected: required.service,
                found,
            });
        }
        bindings.push(ServiceBinding {
            binding: required.binding,
            resource,
            service: found,
        });
    }
    Ok(bindings)
}
pub(crate) fn check_bindings(
    bindings: &[ServiceBinding],
    resources: &HostResources,
) -> Result<(), RouteError> {
    for binding in bindings {
        let found = resources
            .service(binding.resource)
            .map_err(RouteError::Resource)?;
        if found != binding.service {
            return Err(RouteError::ContractMismatch {
                expected: binding.service,
                found,
            });
        }
    }
    Ok(())
}

/// SDK callback used by reload_handler's init option.
///
/// # Safety
/// The buffer is readable for this call; the caller must run this on the module's owner thread.
#[doc(hidden)]
pub unsafe extern "C" fn validate_init_data<T: InitPayload>(bytes: *const u8, len: usize) -> u32 {
    if len > MAX_INIT_BYTES || (len != 0 && bytes.is_null()) {
        return ServiceError::INVALID_ARGUMENT.0;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let bytes = if len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(bytes, len)
        };
        match T::decode(bytes) {
            Ok(_) => 0,
            Err(error) => error.0,
        }
    }))
    .unwrap_or(ServiceError::PANICKED.0)
}
