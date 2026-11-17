use super::*;
use crate::async_service::AsyncPoll;
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
enum OperationState {
    Pending(HostServiceFuture),
    Polling,
    Complete(Result<Vec<u8>, ServiceError>),
    PollFailed { _future: HostServiceFuture },
    CleanupFailed,
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
    let result = crate::task::catch_callback(|| {
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
                state: RefCell::new(OperationState::Pending(future)),
            }),
        );
        Ok(AsyncOperationId {
            registry: registry.id,
            serial,
        })
    })
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
    let result = crate::task::catch_callback(|| {
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
            if matches!(*state, OperationState::Pending(_)) {
                let OperationState::Pending(future) =
                    std::mem::replace(&mut *state, OperationState::Polling)
                else {
                    unreachable!();
                };
                Some(future)
            } else {
                None
            }
        };
        if let Some(mut future) = future {
            let polled = crate::task::catch_callback(|| {
                let waker = crate::task::waker_from_host(&*host);
                future.as_mut().poll(&mut Context::from_waker(&waker))
            });
            let next = match polled {
                Ok(Poll::Ready(result)) => {
                    let result = result.and_then(|bytes| {
                        if bytes.len() <= entry.limit {
                            Ok(bytes)
                        } else {
                            Err(ServiceError::INVALID_ARGUMENT)
                        }
                    });
                    // The destructor ran at most once. A failed return cannot
                    // certify quiescence, even though the Future was consumed.
                    if crate::task::catch_callback(|| drop(future)).is_ok() {
                        OperationState::Complete(result)
                    } else {
                        OperationState::CleanupFailed
                    }
                }
                Ok(Poll::Pending) => OperationState::Pending(future),
                Err(()) => OperationState::PollFailed { _future: future },
            };
            let poisoned = matches!(
                next,
                OperationState::PollFailed { .. } | OperationState::CleanupFailed
            );
            *entry.state.borrow_mut() = next;
            if poisoned {
                let _ = crate::task::catch_callback(|| entry.cancel.cancel());
            }
        }
        let state = entry.state.borrow();
        let result = match &*state {
            OperationState::Pending(_) => return failed(ServiceError(0), false),
            OperationState::Polling => return failed(ServiceError::BUSY, false),
            OperationState::PollFailed { .. } => return failed(ServiceError::PANICKED, false),
            OperationState::CleanupFailed => {
                return failed(ServiceError::ASYNC_CLEANUP_FAILED, false);
            }
            OperationState::Complete(result) => result,
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
    });
    result.unwrap_or_else(|_| failed(ServiceError::PANICKED, false))
}
pub(super) unsafe extern "C" fn cancel(data: *mut c_void, id: AsyncOperationId) -> u32 {
    crate::task::catch_callback(|| {
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
    })
    .unwrap_or(ServiceError::PANICKED.0)
}
pub(super) unsafe extern "C" fn release(data: *mut c_void, id: AsyncOperationId) {
    let _ = crate::task::catch_callback(|| {
        let registry = &*data.cast::<Registry>();
        if let Ok(entry) = registry.async_ops.get(registry.id, id) {
            let ready = matches!(*entry.state.borrow(), OperationState::Complete(_));
            if ready {
                let removed = registry.async_ops.entries.borrow_mut().remove(&id.serial);
                drop(removed);
            } else {
                let _ = cancel(data, id);
            }
        }
    });
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
    ) -> AsyncPoll {
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
        let status = ServiceError(result.status);
        if result.ready == 0 {
            if status == ServiceError::ASYNC_CLEANUP_FAILED {
                AsyncPoll::CleanupFailed
            } else {
                AsyncPoll::Pending
            }
        } else if result.status != 0 {
            AsyncPoll::Ready(Err(status))
        } else if result.written > output.len() {
            AsyncPoll::Ready(Err(ServiceError::INVALID_ARGUMENT))
        } else {
            AsyncPoll::Ready(Ok(result.written))
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
