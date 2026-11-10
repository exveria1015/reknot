//! Byte-oriented, allocator-independent host services and message handlers.
use crate::TaskHandle;
use std::ffi::c_void;

pub const SERVICES_ABI_VERSION: u32 = 2;
pub const HANDLER_ABI_VERSION: u32 = 5;
pub const TASK_CONTROL_ABI_VERSION: u32 = 2;
pub const ACTIVITY_ABI_VERSION: u32 = 1;
pub const GENERATION_SERVICES_ABI_VERSION: u32 = 1;
pub const MAX_SERVICE_REQUIREMENTS: usize = 32;
pub const MAX_INIT_BYTES: usize = 1024 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceRequirement {
    pub binding: u64,
    pub service: ServiceKey,
}
impl ServiceRequirement {
    pub const EMPTY: Self = Self {
        binding: 0,
        service: ServiceKey::new(0, 0),
    };
    pub const fn new(binding: u64, service: ServiceKey) -> Self {
        Self { binding, service }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceBinding {
    pub binding: u64,
    pub resource: ResourceId,
    pub service: ServiceKey,
}
pub type ValidateInitFn = unsafe extern "C" fn(*const u8, usize) -> u32;

/// Owned on successful registration only. All calls stay on the request owner thread.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ActivityHandle {
    pub data: *mut c_void,
    pub vtable: *const ActivityVTable,
}

#[repr(C)]
pub struct ActivityVTable {
    pub header: HandlerHeader,
    /// Zero acknowledges the stop request; errors are retained and retried.
    pub request_stop: unsafe extern "C" fn(*mut c_void) -> u32,
    /// Done AND Error certify quiescence: join/unregistration and all callbacks
    /// have returned. An unconfirmed shutdown MUST remain Pending.
    pub poll_join: unsafe extern "C" fn(*mut c_void, *const crate::HostWaker) -> crate::RunStatus,
    /// Called exactly once, only after terminal poll_join. Must not unwind.
    pub destroy: unsafe extern "C" fn(*mut c_void),
}

#[repr(C)]
pub struct HostTaskVTable {
    pub header: HandlerHeader,
    pub retain: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub release: unsafe extern "C" fn(*mut c_void),
    pub stop_requested: unsafe extern "C" fn(*mut c_void) -> bool,
    pub checkpoint: unsafe extern "C" fn(*mut c_void, ServiceKey, *const u8, usize) -> u32,
    pub register_activity: unsafe extern "C" fn(*mut c_void, ActivityHandle) -> u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostTaskControl {
    pub data: *mut c_void,
    pub vtable: *const HostTaskVTable,
}

/// Optional tail of task-control ABI 2. The base prefix keeps its original
/// layout; readers must check BOTH sizes and the extension version before use.
#[repr(C)]
pub struct HostTaskVTableExt {
    pub base: HostTaskVTable,
    pub extension: HandlerHeader,
    /// Returns one owned host-resident notification handle. Its callbacks/data
    /// must remain valid after the request and plugin have been destroyed.
    pub notification: unsafe extern "C" fn(*mut c_void) -> crate::HostWaker,
    /// key identifies one DLL-local static key. Factory/access arguments are
    /// borrowed for this call only. Access cannot retain a pointer to the value.
    pub with_local: unsafe extern "C" fn(
        *mut c_void,
        *const c_void,
        MakeGenerationLocalFn,
        *mut c_void,
        AccessGenerationLocalFn,
        *mut c_void,
    ) -> u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GenerationLocalValue {
    pub data: *mut c_void,
    /// Zero certifies successful destruction. Any error makes the state
    /// uncertain; the host retains code and never retries this destructor.
    /// Must be Some for a successfully created, non-null value.
    pub destroy: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
}
#[repr(C)]
pub struct GenerationLocalCreated {
    pub status: u32,
    /// On an error, data must be null and no ownership is transferred.
    pub value: GenerationLocalValue,
}
pub type MakeGenerationLocalFn = unsafe extern "C" fn(*mut c_void) -> GenerationLocalCreated;
pub type AccessGenerationLocalFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ResourceId {
    pub registry: u64,
    pub slot: u64,
    pub generation: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ServiceKey {
    pub id: u64,
    pub version: u32,
    pub reserved: u32,
}

impl ServiceKey {
    pub const fn new(id: u64, version: u32) -> Self {
        Self {
            id,
            version,
            reserved: 0,
        }
    }
}

/// Open status codes permit unknown future errors without invalid enum values.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceError(pub u32);

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host service error {}", self.0)
    }
}

impl std::error::Error for ServiceError {}

impl ServiceError {
    pub const REVOKED: Self = Self(1);
    pub const CONTRACT_MISMATCH: Self = Self(2);
    pub const BUSY: Self = Self(3);
    pub const BUFFER_TOO_SMALL: Self = Self(4);
    pub const INVALID_ARGUMENT: Self = Self(5);
    pub const PANICKED: Self = Self(6);
    pub const UNSUPPORTED_OPERATION: Self = Self(7);
    pub const FAILED: Self = Self(8);
    pub const CANCELLED: Self = Self(9);
    pub const CLOSED: Self = Self(10);
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AsyncOperationId {
    pub registry: u64,
    pub serial: u64,
}
#[repr(C)]
pub struct AsyncStarted {
    pub status: u32,
    pub id: AsyncOperationId,
}
#[repr(C)]
pub struct AsyncPolled {
    /// 0: pending, 1: quiescent. Pending errors do not certify completion.
    pub ready: u32,
    pub status: u32,
    /// Required size on BUFFER_TOO_SMALL; completed results are retained.
    pub written: usize,
}

#[repr(C)]
pub struct ServiceResult {
    /// Zero means success; otherwise a ServiceError code.
    pub status: u32,
    pub written: usize,
}

#[repr(C)]
pub struct HostServicesVTable {
    pub abi_version: u32,
    pub struct_size: u32,
    pub retain: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub release: unsafe extern "C" fn(*mut c_void),
    pub call: unsafe extern "C" fn(
        *mut c_void,
        ResourceId,
        ServiceKey,
        u32,
        *const u8,
        usize,
        *mut u8,
        usize,
    ) -> ServiceResult,
    pub start_async: unsafe extern "C" fn(
        *mut c_void,
        ResourceId,
        ServiceKey,
        u32,
        *const u8,
        usize,
        usize,
    ) -> AsyncStarted,
    pub poll_async: unsafe extern "C" fn(
        *mut c_void,
        AsyncOperationId,
        *const crate::HostWaker,
        *mut u8,
        usize,
    ) -> AsyncPolled,
    pub cancel_async: unsafe extern "C" fn(*mut c_void, AsyncOperationId) -> u32,
    /// Release after quiescence. Pending operations are cancelled and retained.
    pub release_async: unsafe extern "C" fn(*mut c_void, AsyncOperationId),
}

/// One owning host reference when retained; descriptor copies alone do not
/// retain it. Calls, retain and release are restricted to the owner thread.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostServices {
    pub data: *mut c_void,
    pub vtable: *const HostServicesVTable,
}

#[repr(C)]
pub struct HandlerRequest {
    pub generation: u64,
    pub resource: ResourceId,
    pub service: ServiceKey,
    pub services: HostServices,
    pub input: *const u8,
    pub input_len: usize,
    pub control: HostTaskControl,
    pub state_schema: ServiceKey,
    pub state: *const u8,
    pub state_len: usize,
    pub bindings: *const ServiceBinding,
    pub bindings_len: usize,
    pub init_schema: ServiceKey,
    pub init: *const u8,
    pub init_len: usize,
}

/// Must copy the input/descriptor and retain host services before returning.
/// The returned task may not borrow HandlerRequest or its input allocation.
/// It must also retain task control and copy seed state, resolved bindings and
/// initialization bytes. Factories only prepare
/// inert futures: business logic, IO and child-task startup begin at first poll.
/// Destroying an unpolled staged task must not apply application state changes.
pub type MakeRequestFn = unsafe extern "C" fn(*const HandlerRequest) -> TaskHandle;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HandlerHeader {
    pub abi_version: u32,
    pub struct_size: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HandlerApi {
    pub header: HandlerHeader,
    pub service: ServiceKey,
    pub make_request: MakeRequestFn,
    pub state_schema: ServiceKey,
    /// Inline bounded manifest: no foreign slice or allocation outlives loading.
    pub required_services: [ServiceRequirement; MAX_SERVICE_REQUIREMENTS],
    pub required_count: u32,
    pub init_schema: ServiceKey,
    /// Pure validator; must not run business logic or retain the borrowed bytes.
    pub validate_init: Option<ValidateInitFn>,
}

impl HandlerApi {
    pub const fn new(service: ServiceKey, make_request: MakeRequestFn) -> Self {
        Self {
            header: HandlerHeader {
                abi_version: HANDLER_ABI_VERSION,
                struct_size: std::mem::size_of::<Self>() as u32,
            },
            service,
            make_request,
            state_schema: ServiceKey::new(0, 0),
            required_services: [ServiceRequirement::EMPTY; MAX_SERVICE_REQUIREMENTS],
            required_count: 0,
            init_schema: ServiceKey::new(0, 0),
            validate_init: None,
        }
    }

    pub const fn with_state(mut self, schema: ServiceKey) -> Self {
        self.state_schema = schema;
        self
    }
    pub const fn with_services(mut self, requirements: &[ServiceRequirement]) -> Self {
        assert!(
            requirements.len() <= MAX_SERVICE_REQUIREMENTS,
            "too many service requirements"
        );
        self.required_count = requirements.len() as u32;
        let mut i = 0;
        while i < requirements.len() {
            self.required_services[i] = requirements[i];
            i += 1;
        }
        self
    }
    pub const fn with_init(mut self, schema: ServiceKey, validate: ValidateInitFn) -> Self {
        self.init_schema = schema;
        self.validate_init = Some(validate);
        self
    }
}

pub type HandlerApiFn = unsafe extern "C" fn() -> *const HandlerApi;
