use crate::Checkpoint;
use crate::{HostWaker, RunStatus, Services, TaskHandle, TaskVTable};
use reknot_abi::services::{
    AccessGenerationLocalFn, HandlerHeader, HostTaskControl, HostTaskVTable, HostTaskVTableExt,
    MakeGenerationLocalFn, GENERATION_SERVICES_ABI_VERSION, TASK_CONTROL_ABI_VERSION,
};
use reknot_abi::services::{HandlerRequest, ResourceId, ServiceError, ServiceKey};
use std::ffi::c_void;
use std::future::Future;
use std::pin::Pin;

pub(crate) struct TaskControl(HostTaskControl);
impl TaskControl {
    unsafe fn retain(raw: HostTaskControl) -> Result<Self, ServiceError> {
        if raw.data.is_null() || raw.vtable.is_null() {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let header = raw.vtable.cast::<HandlerHeader>().read();
        if header.abi_version != TASK_CONTROL_ABI_VERSION
            || header.struct_size < std::mem::size_of::<HostTaskVTable>() as u32
        {
            return Err(ServiceError::CONTRACT_MISMATCH);
        }
        let data = ((*raw.vtable).retain)(raw.data);
        if data.is_null() {
            return Err(ServiceError::FAILED);
        }
        Ok(Self(HostTaskControl { data, ..raw }))
    }
    fn extension(&self) -> Result<&HostTaskVTableExt, ServiceError> {
        unsafe {
            let header = self.0.vtable.cast::<HandlerHeader>().read();
            if header.struct_size < std::mem::size_of::<HostTaskVTableExt>() as u32 {
                return Err(ServiceError::UNSUPPORTED_OPERATION);
            }
            let pointer = self.0.vtable.cast::<HostTaskVTableExt>();
            // An unknown extension can have a different payload, including bytes
            // that are not valid Rust function pointers. Read its header first.
            let extension = std::ptr::addr_of!((*pointer).extension).read();
            if extension.abi_version != GENERATION_SERVICES_ABI_VERSION
                || extension.struct_size < std::mem::size_of::<HostTaskVTableExt>() as u32
            {
                return Err(ServiceError::UNSUPPORTED_OPERATION);
            }
            Ok(&*pointer)
        }
    }
    pub(crate) fn notification(&self) -> Result<crate::HostNotification, ServiceError> {
        let extension = self.extension()?;
        unsafe { crate::HostNotification::owned((extension.notification)(self.0.data)) }
    }
    pub(crate) fn with_local(
        &self,
        key: *const c_void,
        make: MakeGenerationLocalFn,
        init: *mut c_void,
        access: AccessGenerationLocalFn,
        argument: *mut c_void,
    ) -> u32 {
        match self.extension() {
            Ok(table) => unsafe {
                (table.with_local)(self.0.data, key, make, init, access, argument)
            },
            Err(error) => error.0,
        }
    }
}
impl Drop for TaskControl {
    fn drop(&mut self) {
        unsafe { ((*self.0.vtable).release)(self.0.data) };
    }
}

/// DLL-local owned message and retained host services. No reference to the
/// host's request descriptor or input buffer survives construction.
pub struct RequestContext {
    generation: u64,
    resource: ResourceId,
    service: ServiceKey,
    services: Services,
    input: Vec<u8>,
    control: TaskControl,
    checkpoint: Option<Checkpoint>,
    bindings: Vec<crate::ServiceBinding>,
    initial: Option<crate::InitData>,
}

impl RequestContext {
    pub(crate) fn control(&self) -> &TaskControl {
        &self.control
    }

    /// A resident-host C notification tied to effective request completion,
    /// including registered cleanup. Retained raw clones may outlive the DLL;
    /// they become inert when the request has fully joined.
    pub fn host_notification(&self) -> Result<crate::HostNotification, ServiceError> {
        self.control.notification()
    }

    /// Registers a cooperative local child. Its factory runs only on first poll,
    /// after successful registration. The request cannot complete until it joins.
    pub fn spawn_child<F: Future<Output = Result<(), ServiceError>> + 'static>(
        &self,
        make: impl FnOnce(crate::ChildStop) -> F + 'static,
    ) -> Result<(), ServiceError> {
        let stop = crate::ChildStop::default();
        let child_stop = stop.clone();
        self.register_activity(crate::activity::Child {
            stop,
            future: Some(Box::pin(async move { make(child_stop).await })),
        })
    }

    /// Registers an owned adapter for a child join handle or callback registration.
    /// A rejected adapter is dropped before first poll; no ownership crosses on error.
    pub fn register_activity(
        &self,
        activity: impl crate::ManagedActivity,
    ) -> Result<(), ServiceError> {
        let raw = crate::activity::into_raw(activity);
        let status =
            unsafe { ((*self.control.0.vtable).register_activity)(self.control.0.data, raw) };
        if status == 0 {
            Ok(())
        } else {
            unsafe { crate::activity::reject(raw) };
            Err(ServiceError(status))
        }
    }
    /// # Safety
    /// request and its input must be readable for this call. Services must obey
    /// Services::clone_from_raw's lifetime, ABI and owner-thread contracts.
    pub unsafe fn from_raw(request: *const HandlerRequest) -> Result<Self, ServiceError> {
        if request.is_null() {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let request = &*request;
        if request.bindings_len > crate::MAX_SERVICE_REQUIREMENTS
            || (request.bindings_len != 0 && request.bindings.is_null())
            || request.init_len > crate::MAX_INIT_BYTES
            || (request.init_len != 0 && request.init.is_null())
        {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let bindings = if request.bindings_len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(request.bindings, request.bindings_len).to_vec()
        };
        let mut ids = Vec::new();
        for binding in &bindings {
            if binding.binding == 0
                || !crate::environment::valid_key(binding.service)
                || ids.contains(&binding.binding)
            {
                return Err(ServiceError::CONTRACT_MISMATCH);
            }
            ids.push(binding.binding);
        }
        let initial = if crate::environment::absent_key(request.init_schema) {
            if request.init_len != 0 {
                return Err(ServiceError::CONTRACT_MISMATCH);
            }
            None
        } else {
            if !crate::environment::valid_key(request.init_schema) {
                return Err(ServiceError::CONTRACT_MISMATCH);
            }
            let bytes = if request.init_len == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(request.init, request.init_len).to_vec()
            };
            Some(crate::InitData::new(request.init_schema, bytes))
        };
        let services = Services::clone_from_raw(request.services)?;
        let control = TaskControl::retain(request.control)?;
        let checkpoint = if request.state_schema.id == 0 {
            None
        } else {
            if request.state_len > 0 && request.state.is_null() {
                return Err(ServiceError::INVALID_ARGUMENT);
            }
            let bytes = if request.state_len == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(request.state, request.state_len).to_vec()
            };
            Some(Checkpoint::new(request.state_schema, bytes))
        };
        let input = if request.input_len == 0 {
            Vec::new()
        } else {
            if request.input.is_null() {
                return Err(ServiceError::INVALID_ARGUMENT);
            }
            std::slice::from_raw_parts(request.input, request.input_len).to_vec()
        };
        Ok(Self {
            generation: request.generation,
            resource: request.resource,
            service: request.service,
            services,
            input,
            control,
            checkpoint,
            bindings,
            initial,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn resource(&self) -> ResourceId {
        self.resource
    }
    pub fn service(&self) -> ServiceKey {
        self.service
    }
    pub fn services(&self) -> &Services {
        &self.services
    }
    pub fn binding(&self, binding: u64) -> Result<crate::ServiceBinding, ServiceError> {
        self.bindings
            .iter()
            .find(|entry| entry.binding == binding)
            .copied()
            .ok_or(ServiceError::REVOKED)
    }
    pub fn initial_data(&self) -> Option<&crate::InitData> {
        self.initial.as_ref()
    }
    pub fn initial<T: crate::InitPayload>(&self) -> Result<T, ServiceError> {
        self.initial
            .as_ref()
            .ok_or(ServiceError::CONTRACT_MISMATCH)?
            .decode::<T>()
    }
    pub fn call_service(
        &self,
        binding: u64,
        operation: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, ServiceError> {
        let target = self.binding(binding)?;
        self.services
            .call(target.resource, target.service, operation, input, output)
    }
    pub fn input(&self) -> &[u8] {
        &self.input
    }

    pub fn stop_requested(&self) -> bool {
        unsafe { ((*self.control.0.vtable).stop_requested)(self.control.0.data) }
    }

    /// Woken by the owning RequestTask when the router requests shutdown.
    pub async fn cancelled(&self) {
        std::future::poll_fn(|_| {
            if self.stop_requested() {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await
    }

    pub fn checkpoint(&self) -> Option<&Checkpoint> {
        self.checkpoint.as_ref()
    }

    /// Stores a bounded, committed checkpoint in the host. Not an IO rollback.
    pub fn save_checkpoint(&self, schema: ServiceKey, bytes: &[u8]) -> Result<(), ServiceError> {
        let status = unsafe {
            ((*self.control.0.vtable).checkpoint)(
                self.control.0.data,
                schema,
                bytes.as_ptr(),
                bytes.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(ServiceError(status))
        }
    }

    pub fn call(
        &self,
        operation: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, ServiceError> {
        self.services
            .call(self.resource, self.service, operation, input, output)
    }
}

struct HandlerFuture(Pin<Box<dyn Future<Output = RunStatus>>>);

/// SDK support for reload_handler. User code first runs when the task is polled.
#[doc(hidden)]
pub fn make_handler_future(
    future: Pin<Box<dyn Future<Output = Result<(), ServiceError>>>>,
) -> TaskHandle {
    let future = Box::pin(async move {
        match future.await {
            Ok(()) => RunStatus::Done,
            Err(_) => RunStatus::Error,
        }
    });
    TaskHandle {
        data: Box::into_raw(Box::new(HandlerFuture(future))).cast(),
        vtable: &VTABLE,
    }
}

unsafe extern "C" fn poll(data: *mut c_void, waker: *const HostWaker) -> RunStatus {
    let future = &mut *data.cast::<HandlerFuture>();
    crate::task::poll_result_future(future.0.as_mut(), waker)
}
unsafe extern "C" fn destroy(data: *mut c_void) {
    drop(Box::from_raw(data.cast::<HandlerFuture>()));
}
static VTABLE: TaskVTable = TaskVTable {
    poll,
    drop: destroy,
};
