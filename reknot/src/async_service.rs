use crate::{
    AsyncOperationId, ManagedActivity, RequestContext, ResourceId, ServiceError, ServiceKey,
    Services,
};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::task::{Context, Poll, Waker};

pub const MAX_ASYNC_BUFFER: usize = 1024 * 1024;
#[derive(Clone, Copy, Debug)]
pub struct AsyncLimits {
    pub max_operations: usize,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
}
impl Default for AsyncLimits {
    fn default() -> Self {
        Self {
            max_operations: 64,
            max_input_bytes: 65536,
            max_output_bytes: 65536,
        }
    }
}
impl AsyncLimits {
    pub(crate) fn validate(self) -> Result<Self, ServiceError> {
        if self.max_operations == 0
            || self.max_input_bytes > MAX_ASYNC_BUFFER
            || self.max_output_bytes > MAX_ASYNC_BUFFER
        {
            Err(ServiceError::INVALID_ARGUMENT)
        } else {
            Ok(self)
        }
    }
}
pub type HostServiceFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>, ServiceError>>>>;

/// Host-owned Send + Sync signal, with no SDK/FFI pointers.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<CancelState>);
#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    waker: Mutex<Option<Waker>>,
}
impl CancellationToken {
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    pub async fn cancelled(&self) {
        std::future::poll_fn(|cx| {
            let new = cx.waker().clone();
            let old = self.0.waker.lock().unwrap().replace(new);
            drop(old);
            if self.is_cancelled() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
    pub(crate) fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        let waker = self.0.waker.lock().unwrap().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

#[derive(Default)]
struct CallState {
    result: Option<Result<Vec<u8>, ServiceError>>,
    abandoned: bool,
    waker: Option<Waker>,
}
/// Owner-thread await handle. Drop requests cancellation; registered cleanup
/// still waits for host I/O. This future and SDK handles are deliberately !Send.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<reknot::ServiceCall>();
/// ```
pub struct ServiceCall {
    state: Rc<RefCell<CallState>>,
    completed: bool,
}
impl Future for ServiceCall {
    type Output = Result<Vec<u8>, ServiceError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.completed, "completed ServiceCall polled again");
        let result = {
            let mut state = self.state.borrow_mut();
            state.waker = Some(cx.waker().clone());
            state.result.take()
        };
        if let Some(result) = result {
            self.completed = true;
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}
impl Drop for ServiceCall {
    fn drop(&mut self) {
        if !self.completed {
            let waker = {
                let mut state = self.state.borrow_mut();
                state.abandoned = true;
                state.waker.clone()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}
struct CallActivity {
    services: Services,
    resource: ResourceId,
    service: ServiceKey,
    operation: u32,
    input: Vec<u8>,
    output: Vec<u8>,
    id: Option<AsyncOperationId>,
    state: Rc<RefCell<CallState>>,
    stopping: bool,
    cancel_sent: bool,
    notification: Option<crate::HostNotification>,
}
impl CallActivity {
    fn deliver(&mut self, result: Result<Vec<u8>, ServiceError>) {
        let waker = {
            let mut state = self.state.borrow_mut();
            if !state.abandoned {
                state.result = Some(result);
            }
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}
// Before first poll no host operation exists; terminal host results certify
// quiescence. The adapter retains services until it releases that operation.
unsafe impl ManagedActivity for CallActivity {
    fn request_stop(mut self: Pin<&mut Self>) -> Result<(), ServiceError> {
        self.stopping = true;
        if let Some(id) = self.id {
            self.services.cancel_async(id)?;
        }
        self.cancel_sent = true;
        Ok(())
    }
    fn poll_join(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), ServiceError>> {
        let abandoned = self.state.borrow().abandoned;
        if ((abandoned && !self.stopping) || (self.stopping && !self.cancel_sent))
            && self.as_mut().request_stop().is_err()
        {
            return Poll::Pending;
        }
        if self.id.is_none() {
            if self.stopping {
                self.deliver(Err(ServiceError::CANCELLED));
                return Poll::Ready(Ok(()));
            }
            match self.services.start_async(
                self.resource,
                self.service,
                self.operation,
                &self.input,
                self.output.len(),
            ) {
                Ok(id) => {
                    self.id = Some(id);
                    self.input.clear();
                }
                Err(error) => {
                    self.deliver(Err(error));
                    return Poll::Ready(Ok(()));
                }
            }
        }
        let id = self.id.unwrap();
        let this = &mut *self;
        match this
            .services
            .poll_async(id, cx, &mut this.output, this.notification.as_ref())
        {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                this.services.release_async(id);
                this.id = None;
                let result = result.map(|len| this.output[..len].to_vec());
                this.deliver(result);
                Poll::Ready(Ok(())) // I/O errors are request results, not join failures.
            }
        }
    }
}
impl RequestContext {
    /// Copy input and register a lazy, bounded host operation. Cancellation and
    /// root Drop wait for host completion, even if this await handle is dropped.
    pub fn call_async(
        &self,
        operation: u32,
        input: &[u8],
        max_output_bytes: usize,
    ) -> Result<ServiceCall, ServiceError> {
        self.call_async_on(
            self.resource(),
            self.service(),
            operation,
            input,
            max_output_bytes,
        )
    }
    pub fn call_service_async(
        &self,
        binding: u64,
        operation: u32,
        input: &[u8],
        max_output_bytes: usize,
    ) -> Result<ServiceCall, ServiceError> {
        let target = self.binding(binding)?;
        self.call_async_on(
            target.resource,
            target.service,
            operation,
            input,
            max_output_bytes,
        )
    }
    fn call_async_on(
        &self,
        resource: ResourceId,
        service: ServiceKey,
        operation: u32,
        input: &[u8],
        max_output_bytes: usize,
    ) -> Result<ServiceCall, ServiceError> {
        if input.len() > MAX_ASYNC_BUFFER || max_output_bytes > MAX_ASYNC_BUFFER {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let state = Rc::new(RefCell::new(CallState::default()));
        let notification = match self.host_notification() {
            Ok(notification) => Some(notification),
            Err(ServiceError::UNSUPPORTED_OPERATION) => None,
            Err(error) => return Err(error),
        };
        self.register_activity(CallActivity {
            services: self.services().clone(),
            resource,
            service,
            operation,
            input: input.to_vec(),
            output: vec![0; max_output_bytes],
            id: None,
            state: state.clone(),
            stopping: false,
            cancel_sent: false,
            notification,
        })?;
        Ok(ServiceCall {
            state,
            completed: false,
        })
    }
}
