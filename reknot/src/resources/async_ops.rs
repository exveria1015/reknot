use super::*;
use crate::{AsyncOperationId, AsyncPolled, AsyncStarted, HostWaker};
use std::cell::Cell;
use std::collections::HashMap;
use std::task::{Context, Poll};

struct Operation {
    resource: ResourceId,
    cancel: CancellationToken,
    reason: Cell<Option<ServiceError>>,
    state: RefCell<OperationState>,
    limit: usize,
}
struct OperationState {
    future: Option<HostServiceFuture>,
    result: Option<Result<Vec<u8>, ServiceError>>,
    polling: bool,
    poisoned: bool,
}
pub(super) struct Operations {
    limits: AsyncLimits,
    next: Cell<u64>,
    reserved: Cell<usize>,
    entries: RefCell<HashMap<u64, Rc<Operation>>>,
}
impl Operations {
    pub fn new(limits: AsyncLimits) -> Self {
        Self {
            limits,
            next: Cell::new(1),
            reserved: Cell::new(0),
            entries: RefCell::new(HashMap::new()),
        }
    }
    pub fn revoke(&self, resource: ResourceId) {
        let entries: Vec<_> = self
            .entries
            .borrow()
            .values()
            .filter(|entry| entry.resource == resource)
            .cloned()
            .collect();
        for entry in entries {
            entry.reason.set(Some(ServiceError::REVOKED));
            entry.cancel.cancel();
        }
    }
    fn get(&self, registry: u64, id: AsyncOperationId) -> Result<Rc<Operation>, ServiceError> {
        if id.registry != registry {
            return Err(ServiceError::REVOKED);
        }
        self.entries
            .borrow()
            .get(&id.serial)
            .cloned()
            .ok_or(ServiceError::REVOKED)
    }
}
fn failed(error: ServiceError, ready: bool) -> AsyncPolled {
    AsyncPolled {
        ready: u32::from(ready),
        status: error.0,
        written: 0,
    }
}

pub(super) unsafe extern "C" fn start(
    data: *mut c_void,
    resource: ResourceId,
    service: ServiceKey,
    op: u32,
    input: *const u8,
    len: usize,
    output_limit: usize,
) -> AsyncStarted {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if data.is_null() || (len != 0 && input.is_null()) {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let registry = &*data.cast::<Registry>();
        let operations = &registry.async_ops;
        if len > operations.limits.max_input_bytes
            || output_limit > operations.limits.max_output_bytes
        {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        if operations.entries.borrow().len() + operations.reserved.get()
            >= operations.limits.max_operations
        {
            return Err(ServiceError::BUSY);
        }
        let (key, target) = lookup(registry, resource)?;
        if key != service {
            return Err(ServiceError::CONTRACT_MISMATCH);
        }
        let serial = operations.next.get();
        operations
            .next
            .set(serial.checked_add(1).ok_or(ServiceError::FAILED)?);
        struct Reservation<'a>(&'a Cell<usize>);
        impl Drop for Reservation<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() - 1);
            }
        }
        operations.reserved.set(operations.reserved.get() + 1);
        let _reservation = Reservation(&operations.reserved);
        let cancel = CancellationToken::default();
        let bytes = if len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(input, len)
        };
        let future = target.start_async(op, bytes, cancel.clone())?;
        let reason = lookup(registry, resource)
            .err()
            .map(|_| ServiceError::REVOKED);
        if reason.is_some() {
            cancel.cancel();
        }
        operations.entries.borrow_mut().insert(
            serial,
            Rc::new(Operation {
                resource,
                cancel,
                reason: Cell::new(reason),
                limit: output_limit,
                state: RefCell::new(OperationState {
                    future: Some(future),
                    result: None,
                    polling: false,
                    poisoned: false,
                }),
            }),
        );
        Ok(AsyncOperationId {
            registry: registry.id,
            serial,
        })
    }))
    .unwrap_or(Err(ServiceError::PANICKED));
    match result {
        Ok(id) => AsyncStarted { status: 0, id },
        Err(error) => AsyncStarted {
            status: error.0,
            id: AsyncOperationId::default(),
        },
    }
}
pub(super) unsafe extern "C" fn poll(
    data: *mut c_void,
    id: AsyncOperationId,
    host: *const HostWaker,
    output: *mut u8,
    capacity: usize,
) -> AsyncPolled {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if data.is_null() || host.is_null() || (capacity != 0 && output.is_null()) {
            return failed(ServiceError::INVALID_ARGUMENT, false);
        }
        let registry = &*data.cast::<Registry>();
        let entry = match registry.async_ops.get(registry.id, id) {
            Ok(entry) => entry,
            Err(error) => return failed(error, true),
        };
        let future = {
            let mut state = entry.state.borrow_mut();
            if state.poisoned {
                return failed(ServiceError::PANICKED, false);
            }
            if state.polling {
                return failed(ServiceError::BUSY, false);
            }
            if state.result.is_none() {
                state.polling = true;
                state.future.take()
            } else {
                None
            }
        };
        if let Some(mut future) = future {
            let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let waker = crate::task::waker_from_host(&*host);
                future.as_mut().poll(&mut Context::from_waker(&waker))
            }));
            let mut state = entry.state.borrow_mut();
            match polled {
                Ok(Poll::Ready(result)) => {
                    let result = result.and_then(|bytes| {
                        if bytes.len() <= entry.limit {
                            Ok(bytes)
                        } else {
                            Err(ServiceError::INVALID_ARGUMENT)
                        }
                    });
                    drop(state);
                    drop(future); // I/O future destruction precedes the quiescent response.
                    let mut state = entry.state.borrow_mut();
                    state.polling = false;
                    state.result = Some(result);
                }
                Ok(Poll::Pending) => {
                    state.polling = false;
                    state.future = Some(future);
                }
                Err(_) => {
                    state.polling = false;
                    state.future = Some(future);
                    state.poisoned = true;
                    drop(state);
                    entry.cancel.cancel();
                }
            }
        }
        let state = entry.state.borrow();
        let Some(result) = &state.result else {
            return failed(
                if state.poisoned {
                    ServiceError::PANICKED
                } else {
                    ServiceError(0)
                },
                false,
            );
        };
        if let Some(reason) = entry.reason.get() {
            return failed(reason, true);
        }
        match result {
            Err(error) => failed(*error, true),
            Ok(bytes) => {
                if capacity < bytes.len() {
                    return AsyncPolled {
                        ready: 1,
                        status: ServiceError::BUFFER_TOO_SMALL.0,
                        written: bytes.len(),
                    };
                }
                if !bytes.is_empty() {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
                }
                AsyncPolled {
                    ready: 1,
                    status: 0,
                    written: bytes.len(),
                }
            }
        }
    }));
    result.unwrap_or_else(|_| failed(ServiceError::PANICKED, false))
}
pub(super) unsafe extern "C" fn cancel(data: *mut c_void, id: AsyncOperationId) -> u32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let registry = &*data.cast::<Registry>();
        match registry.async_ops.get(registry.id, id) {
            Ok(entry) => {
                if entry.reason.get().is_none() {
                    entry.reason.set(Some(ServiceError::CANCELLED));
                }
                entry.cancel.cancel();
                0
            }
            Err(error) => error.0,
        }
    }))
    .unwrap_or(ServiceError::PANICKED.0)
}
pub(super) unsafe extern "C" fn release(data: *mut c_void, id: AsyncOperationId) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let registry = &*data.cast::<Registry>();
        if let Ok(entry) = registry.async_ops.get(registry.id, id) {
            let ready = {
                let state = entry.state.borrow();
                state.result.is_some() && !state.polling
            };
            if ready {
                let removed = registry.async_ops.entries.borrow_mut().remove(&id.serial);
                drop(removed);
            } else {
                let _ = cancel(data, id);
            }
        }
    }));
}

impl Services {
    pub(crate) fn start_async(
        &self,
        resource: ResourceId,
        service: ServiceKey,
        operation: u32,
        input: &[u8],
        max_output: usize,
    ) -> Result<AsyncOperationId, ServiceError> {
        let result = unsafe {
            ((*self.raw.vtable).start_async)(
                self.raw.data,
                resource,
                service,
                operation,
                input.as_ptr(),
                input.len(),
                max_output,
            )
        };
        if result.status == 0 {
            Ok(result.id)
        } else {
            Err(ServiceError(result.status))
        }
    }
    pub(crate) fn poll_async(
        &self,
        id: AsyncOperationId,
        cx: &mut Context<'_>,
        output: &mut [u8],
        notification: Option<&crate::HostNotification>,
    ) -> Poll<Result<usize, ServiceError>> {
        let fallback = notification
            .is_none()
            .then(|| crate::task::host_waker_from(cx.waker(), None));
        let waker = notification
            .map(crate::HostNotification::as_raw)
            .unwrap_or_else(|| {
                let raw = &fallback.as_ref().unwrap().0;
                HostWaker {
                    data: raw.data,
                    vtable: raw.vtable,
                }
            });
        let result = unsafe {
            ((*self.raw.vtable).poll_async)(
                self.raw.data,
                id,
                &waker,
                output.as_mut_ptr(),
                output.len(),
            )
        };
        if result.ready == 0 {
            return Poll::Pending;
        }
        if result.status != 0 {
            Poll::Ready(Err(ServiceError(result.status)))
        } else if result.written > output.len() {
            Poll::Ready(Err(ServiceError::INVALID_ARGUMENT))
        } else {
            Poll::Ready(Ok(result.written))
        }
    }
    pub(crate) fn cancel_async(&self, id: AsyncOperationId) -> Result<(), ServiceError> {
        let status = unsafe { ((*self.raw.vtable).cancel_async)(self.raw.data, id) };
        if status == 0 {
            Ok(())
        } else {
            Err(ServiceError(status))
        }
    }
    pub(crate) fn release_async(&self, id: AsyncOperationId) {
        unsafe {
            ((*self.raw.vtable).release_async)(self.raw.data, id);
        }
    }
}
