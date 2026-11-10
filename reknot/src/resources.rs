use reknot_abi::services::{
    HostServices, HostServicesVTable, ResourceId, ServiceError, ServiceKey, ServiceResult,
    SERVICES_ABI_VERSION,
};
use std::cell::{Ref, RefCell, RefMut};
use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
mod async_ops;
use crate::{AsyncLimits, CancellationToken, HostServiceFuture};

static NEXT_REGISTRY: AtomicU64 = AtomicU64::new(1);

/// A short, nonblocking operation on host-owned data. Never retain the input or
/// output slices. Check output capacity before applying an irreversible action.
/// Services do not automatically retry BUFFER_TOO_SMALL or any other error.
pub trait HostResource: 'static {
    fn call(
        &mut self,
        _operation: u32,
        _input: &[u8],
        _output: &mut [u8],
    ) -> Result<usize, ServiceError> {
        Err(ServiceError::UNSUPPORTED_OPERATION)
    }

    /// Copy input and return an owned future. Polling must be nonblocking;
    /// Ready certifies I/O quiescence, including after cooperative cancellation.
    fn start_async(
        &mut self,
        _operation: u32,
        _input: &[u8],
        _cancel: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        Err(ServiceError::UNSUPPORTED_OPERATION)
    }
}

trait ErasedResource {
    fn start_async(
        &self,
        operation: u32,
        input: &[u8],
        cancel: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError>;
    fn call(&self, operation: u32, input: &[u8], output: &mut [u8]) -> Result<usize, ServiceError>;
}

impl<T: HostResource> ErasedResource for RefCell<T> {
    fn start_async(
        &self,
        operation: u32,
        input: &[u8],
        cancel: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        self.try_borrow_mut()
            .map_err(|_| ServiceError::BUSY)?
            .start_async(operation, input, cancel)
    }
    fn call(&self, operation: u32, input: &[u8], output: &mut [u8]) -> Result<usize, ServiceError> {
        self.try_borrow_mut()
            .map_err(|_| ServiceError::BUSY)?
            .call(operation, input, output)
    }
}

struct Slot {
    generation: u64,
    value: Option<(ServiceKey, Rc<dyn ErasedResource>)>,
}

struct Registry {
    id: u64,
    slots: RefCell<Vec<Slot>>,
    async_ops: async_ops::Operations,
}

/// A thread-bound registry independent of any logic generation. Service clients
/// retain the registry; revocation invalidates an ID without exposing Rust data.
#[derive(Clone)]
pub struct HostResources(Rc<Registry>);

impl Default for HostResources {
    fn default() -> Self {
        Self::new()
    }
}

impl HostResources {
    pub fn new() -> Self {
        Self::with_async_limits(AsyncLimits::default()).expect("valid default async limits")
    }
    pub fn with_async_limits(limits: AsyncLimits) -> Result<Self, ServiceError> {
        let limits = limits.validate()?;
        let id = NEXT_REGISTRY
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("host registry ID space exhausted");
        Ok(Self(Rc::new(Registry {
            id,
            slots: RefCell::new(Vec::new()),
            async_ops: async_ops::Operations::new(limits),
        })))
    }

    pub fn register<T: HostResource>(&self, service: ServiceKey, value: T) -> Resource<T> {
        assert!(
            service.id != 0 && service.version != 0 && service.reserved == 0,
            "invalid service contract"
        );
        let value = Rc::new(RefCell::new(value));
        let mut slots = self.0.slots.borrow_mut();
        let index = slots
            .iter()
            .position(|slot| slot.value.is_none() && slot.generation != u64::MAX)
            .unwrap_or(slots.len());
        if index == slots.len() {
            slots.push(Slot {
                generation: 1,
                value: None,
            });
        }
        let slot = &mut slots[index];
        slot.value = Some((service, value.clone()));
        Resource {
            id: ResourceId {
                registry: self.0.id,
                slot: index as u64,
                generation: slot.generation,
            },
            value,
        }
    }

    /// Revokes future service calls. Existing host `Resource<T>` owners may keep
    /// the underlying connection/data alive until they too are dropped.
    pub fn revoke(&self, id: ResourceId) -> bool {
        if id.registry != self.0.id {
            return false;
        }
        let Ok(index) = usize::try_from(id.slot) else {
            return false;
        };
        let removed = {
            let mut slots = self.0.slots.borrow_mut();
            let Some(slot) = slots.get_mut(index) else {
                return false;
            };
            if slot.generation != id.generation {
                return false;
            }
            let removed = slot.value.take();
            if removed.is_some() {
                slot.generation = slot.generation.saturating_add(1);
            }
            removed
        };
        self.0.async_ops.revoke(id);
        // Destructors may reenter the registry; do not hold its RefCell borrow.
        removed.is_some()
    }

    pub fn service(&self, id: ResourceId) -> Result<ServiceKey, ServiceError> {
        lookup(&self.0, id).map(|(key, _)| key)
    }

    pub fn services(&self) -> Services {
        Services {
            raw: HostServices {
                data: Rc::into_raw(self.0.clone()).cast_mut().cast(),
                vtable: &VTABLE,
            },
            _local: PhantomData,
        }
    }
}

/// Typed access exists only in the host; DLLs receive id(), never T's layout.
pub struct Resource<T> {
    id: ResourceId,
    value: Rc<RefCell<T>>,
}

impl<T> Clone for Resource<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            value: self.value.clone(),
        }
    }
}

impl<T> Resource<T> {
    pub fn id(&self) -> ResourceId {
        self.id
    }
    pub fn try_borrow(&self) -> Result<Ref<'_, T>, std::cell::BorrowError> {
        self.value.try_borrow()
    }
    pub fn try_borrow_mut(&self) -> Result<RefMut<'_, T>, std::cell::BorrowMutError> {
        self.value.try_borrow_mut()
    }
}

/// Owning, thread-bound client for the host ABI. Clones retain host services,
/// so they remain valid even after the router or original registry handle drops.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<reknot::Services>();
/// ```
pub struct Services {
    raw: HostServices,
    _local: PhantomData<Rc<()>>,
}

impl Services {
    /// # Safety
    /// raw must be a live host descriptor with valid versioned callbacks. All
    /// operations including destruction must run on its owner thread. retain
    /// must return an owning reference; callbacks must not unwind over FFI.
    pub unsafe fn clone_from_raw(raw: HostServices) -> Result<Self, ServiceError> {
        if raw.data.is_null() || raw.vtable.is_null() {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let header = raw
            .vtable
            .cast::<reknot_abi::services::HandlerHeader>()
            .read();
        if header.abi_version != SERVICES_ABI_VERSION
            || header.struct_size < std::mem::size_of::<HostServicesVTable>() as u32
        {
            return Err(ServiceError::CONTRACT_MISMATCH);
        }
        let table = &*raw.vtable;
        let data = (table.retain)(raw.data);
        if data.is_null() {
            return Err(ServiceError::FAILED);
        }
        Ok(Self {
            raw: HostServices {
                data,
                vtable: raw.vtable,
            },
            _local: PhantomData,
        })
    }

    /// Borrowed descriptor: copying it does not transfer an owning reference.
    pub fn as_raw(&self) -> HostServices {
        self.raw
    }

    pub fn call(
        &self,
        resource: ResourceId,
        service: ServiceKey,
        operation: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, ServiceError> {
        let result = unsafe {
            ((*self.raw.vtable).call)(
                self.raw.data,
                resource,
                service,
                operation,
                input.as_ptr(),
                input.len(),
                output.as_mut_ptr(),
                output.len(),
            )
        };
        if result.status != 0 {
            return Err(ServiceError(result.status));
        }
        if result.written > output.len() {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        Ok(result.written)
    }
}

impl Clone for Services {
    fn clone(&self) -> Self {
        // This descriptor was already validated and remains owned by self.
        unsafe { Self::clone_from_raw(self.raw) }.expect("host failed to retain services")
    }
}

impl Drop for Services {
    fn drop(&mut self) {
        unsafe { ((*self.raw.vtable).release)(self.raw.data) };
    }
}

fn lookup(
    registry: &Registry,
    id: ResourceId,
) -> Result<(ServiceKey, Rc<dyn ErasedResource>), ServiceError> {
    if registry.id != id.registry {
        return Err(ServiceError::REVOKED);
    }
    let index = usize::try_from(id.slot).map_err(|_| ServiceError::REVOKED)?;
    let slots = registry
        .slots
        .try_borrow()
        .map_err(|_| ServiceError::BUSY)?;
    let slot = slots.get(index).ok_or(ServiceError::REVOKED)?;
    if slot.generation != id.generation {
        return Err(ServiceError::REVOKED);
    }
    slot.value.clone().ok_or(ServiceError::REVOKED)
}

unsafe extern "C" fn retain(data: *mut c_void) -> *mut c_void {
    Rc::increment_strong_count(data.cast::<Registry>());
    data
}
unsafe extern "C" fn release(data: *mut c_void) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(Rc::from_raw(data.cast::<Registry>()));
    }));
}
unsafe extern "C" fn call(
    data: *mut c_void,
    id: ResourceId,
    service: ServiceKey,
    operation: u32,
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    output_len: usize,
) -> ServiceResult {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if data.is_null()
            || (input_len != 0 && input.is_null())
            || (output_len != 0 && output.is_null())
        {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let registry = &*data.cast::<Registry>();
        let (key, resource) = lookup(registry, id)?;
        if key != service {
            return Err(ServiceError::CONTRACT_MISMATCH);
        }
        let input = if input_len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(input, input_len)
        };
        let output = if output_len == 0 {
            &mut []
        } else {
            std::slice::from_raw_parts_mut(output, output_len)
        };
        let written = resource.call(operation, input, output)?;
        if written > output.len() {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        Ok(written)
    }))
    .unwrap_or(Err(ServiceError::PANICKED));
    match result {
        Ok(written) => ServiceResult { status: 0, written },
        Err(error) => ServiceResult {
            status: error.0,
            written: 0,
        },
    }
}

static VTABLE: HostServicesVTable = HostServicesVTable {
    abi_version: SERVICES_ABI_VERSION,
    struct_size: std::mem::size_of::<HostServicesVTable>() as u32,
    retain,
    release,
    call,
    start_async: async_ops::start,
    poll_async: async_ops::poll,
    cancel_async: async_ops::cancel,
    release_async: async_ops::release,
};
