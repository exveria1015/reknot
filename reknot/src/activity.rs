use crate::generation::{enter, CodeLease};
use crate::{
    ActivityHandle, ActivityVTable, HandlerHeader, HostWaker, RunStatus, ServiceError,
    ACTIVITY_ABI_VERSION,
};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

pub const MAX_MANAGED_ACTIVITIES: usize = 64;

/// Adapter for a child join handle or an external callback registration.
///
/// # Safety
/// Own all data used outside the root; never borrow its future/stack. Dropping
/// before first poll must be safe (including rejected registration). After any
/// Ready result, the operation must be quiescent: no callback is executing or can
/// start, and all child threads/tasks have returned/joined. An unregister failure
/// without that proof must remain Pending, not Ready(Err). Destruction after
/// quiescence must be safe and must not unwind. All methods run on the owner thread;
/// external callers need their own synchronization and may not use thread-bound SDK state.
pub unsafe trait ManagedActivity: 'static {
    fn request_stop(self: Pin<&mut Self>) -> Result<(), ServiceError>;
    fn poll_join(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), ServiceError>>;
}

/// Cooperative cancellation for a locally polled child. Not a thread/spawn API.
#[derive(Clone, Default)]
pub struct ChildStop(Rc<StopState>);
#[derive(Default)]
struct StopState {
    stopped: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}
impl ChildStop {
    pub fn is_requested(&self) -> bool {
        self.0.stopped.get()
    }
    pub async fn cancelled(&self) {
        std::future::poll_fn(|cx| {
            *self.0.waker.borrow_mut() = Some(cx.waker().clone());
            if self.is_requested() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
    fn stop(&self) {
        self.0.stopped.set(true);
        let waker = self.0.waker.borrow().clone();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

type ChildFuture = Pin<Box<dyn Future<Output = Result<(), ServiceError>>>>;
pub(crate) struct Child {
    pub stop: ChildStop,
    pub future: Option<ChildFuture>,
}
// A 'static local future owns its state. A panic is terminal only after destroying it.
unsafe impl ManagedActivity for Child {
    fn request_stop(self: Pin<&mut Self>) -> Result<(), ServiceError> {
        self.stop.stop();
        Ok(())
    }
    fn poll_join(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), ServiceError>> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future.as_mut().unwrap().as_mut().poll(cx)
        }))
        .unwrap_or(Poll::Ready(Err(ServiceError::PANICKED)));
        if result.is_ready() {
            self.future = None;
        }
        result
    }
}

struct Adapter<T> {
    activity: Pin<Box<T>>,
    poisoned: bool,
}
pub(crate) fn into_raw<T: ManagedActivity>(activity: T) -> ActivityHandle {
    ActivityHandle {
        data: Box::into_raw(Box::new(Adapter {
            activity: Box::pin(activity),
            poisoned: false,
        }))
        .cast(),
        vtable: &Adapter::<T>::VTABLE,
    }
}
pub(crate) unsafe fn reject(raw: ActivityHandle) {
    (raw.vtable.as_ref().unwrap().destroy)(raw.data);
}
impl<T: ManagedActivity> Adapter<T> {
    const VTABLE: ActivityVTable = ActivityVTable {
        header: HandlerHeader {
            abi_version: ACTIVITY_ABI_VERSION,
            struct_size: std::mem::size_of::<ActivityVTable>() as u32,
        },
        request_stop: stop::<T>,
        poll_join: join::<T>,
        destroy: destroy::<T>,
    };
}
unsafe extern "C" fn stop<T: ManagedActivity>(data: *mut c_void) -> u32 {
    let adapter = &mut *data.cast::<Adapter<T>>();
    if adapter.poisoned {
        return ServiceError::PANICKED.0;
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        adapter.activity.as_mut().request_stop()
    })) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => error.0,
        Err(_) => {
            adapter.poisoned = true;
            ServiceError::PANICKED.0
        }
    }
}
unsafe extern "C" fn join<T: ManagedActivity>(
    data: *mut c_void,
    waker: *const HostWaker,
) -> RunStatus {
    let adapter = &mut *data.cast::<Adapter<T>>();
    if adapter.poisoned {
        return RunStatus::Pending;
    }
    // The existing bridge installs the host Waker and catches Rust unwinding.
    // A panic in an unsafe external adapter does not certify quiescence: poison
    // and retain it rather than return terminal Error or poll an invalid future again.
    let future = std::future::poll_fn(|cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            adapter.activity.as_mut().poll_join(cx)
        })) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(Ok(()))) => Poll::Ready(RunStatus::Done),
            Ok(Poll::Ready(Err(_))) => Poll::Ready(RunStatus::Error),
            Err(_) => {
                adapter.poisoned = true;
                Poll::Pending
            }
        }
    });
    crate::task::poll_result_future(std::pin::pin!(future), waker)
}
unsafe extern "C" fn destroy<T: ManagedActivity>(data: *mut c_void) {
    drop(Box::from_raw(data.cast::<Adapter<T>>()));
}

pub(crate) struct Entry {
    raw: ActivityHandle,
    stopped: bool,
    joined: bool,
    lease: Option<CodeLease>,
}
impl Entry {
    pub unsafe fn new(raw: ActivityHandle, lease: Option<CodeLease>) -> Result<Self, ServiceError> {
        if raw.data.is_null() || raw.vtable.is_null() {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let header = raw.vtable.cast::<HandlerHeader>().read();
        if header.abi_version != ACTIVITY_ABI_VERSION
            || header.struct_size < std::mem::size_of::<ActivityVTable>() as u32
        {
            return Err(ServiceError::CONTRACT_MISMATCH);
        }
        Ok(Self {
            raw,
            stopped: false,
            joined: false,
            lease,
        })
    }
    pub fn poll(&mut self, stopping: bool, cx: &mut Context<'_>) -> (bool, Option<ServiceError>) {
        let _call = enter(&self.lease);
        let mut error = None;
        if stopping && !self.stopped {
            let result = unsafe { ((*self.raw.vtable).request_stop)(self.raw.data) };
            if result == 0 {
                self.stopped = true;
            } else {
                error = Some(ServiceError(result));
            }
        }
        let (waker, _guard) = crate::task::host_waker_from(cx.waker(), self.lease.clone());
        let result = unsafe { ((*self.raw.vtable).poll_join)(self.raw.data, &waker) };
        self.joined = result != RunStatus::Pending;
        if result == RunStatus::Error {
            error = Some(ServiceError::FAILED);
        }
        (self.joined, error)
    }
}
impl Drop for Entry {
    fn drop(&mut self) {
        if self.joined {
            let _call = enter(&self.lease);
            unsafe { ((*self.raw.vtable).destroy)(self.raw.data) };
        } else {
            // Abandonment/thread exit cannot asynchronously join. Keep the raw owner
            // AND code lease alive; freeing external callback state here would be unsafe.
            std::mem::forget(self.lease.take());
        }
    }
}
