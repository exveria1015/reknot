use super::waker::host_waker_from;
use crate::generation::{enter, CodeLease};
use crate::{HostWaker, RunStatus, TaskHandle};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Owns one FFI future. Only this type calls its foreign destructor.
pub(super) struct OwnedTask(TaskHandle);

impl OwnedTask {
    /// The caller transfers ownership and keeps the callbacks and borrowed data
    /// alive until this owner is dropped. Both pointers must be valid or null.
    pub(super) unsafe fn new(future: TaskHandle) -> Option<Self> {
        if future.data.is_null() || future.vtable.is_null() {
            return None;
        }
        Some(Self(future))
    }

    pub(super) fn poll(&mut self, waker: &HostWaker) -> RunStatus {
        // SAFETY: construction requires live data and callbacks; &mut self
        // prevents concurrent polling and the waker is live for this call.
        unsafe { ((*self.0.vtable).poll)(self.0.data, waker) }
    }
}

impl Drop for OwnedTask {
    fn drop(&mut self) {
        // SAFETY: this owner uniquely owns the data and runs its destructor once.
        unsafe { ((*self.0.vtable).drop)(self.0.data) };
    }
}

/// Polls a foreign future and releases it on completion, error, or cancellation.
pub struct FfiFuture {
    future: Option<OwnedTask>,
    completed: Option<RunStatus>,
    // Foreign future is destroyed before the code lease, even on cancellation.
    keep_alive: Option<CodeLease>,
}

impl FfiFuture {
    /// Takes ownership of an FFI future, rejecting null handles.
    ///
    /// # Safety
    /// The handle must uniquely own valid data with a matching vtable. The vtable,
    /// its library, and all data borrowed by the future must outlive the task.
    /// Callbacks must obey the poll/drop contract and must not unwind over FFI.
    pub unsafe fn new(future: TaskHandle) -> Option<Self> {
        Some(Self {
            future: Some(OwnedTask::new(future)?),
            completed: None,
            keep_alive: None,
        })
    }

    pub(crate) unsafe fn with_optional_keep_alive(
        future: TaskHandle,
        lease: Option<CodeLease>,
    ) -> Option<Self> {
        let mut task = Self::new(future)?;
        task.keep_alive = lease;
        Some(task)
    }
}

impl Drop for FfiFuture {
    fn drop(&mut self) {
        let _call = enter(&self.keep_alive);
        self.future = None;
    }
}

impl Future for FfiFuture {
    type Output = RunStatus;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(status) = self.completed {
            return Poll::Ready(status);
        }
        let _call = enter(&self.keep_alive);
        let (host_waker, _guard) = host_waker_from(cx.waker(), self.keep_alive.clone());
        let status = self
            .future
            .as_mut()
            .expect("active future missing")
            .poll(&host_waker);
        if status == RunStatus::Pending {
            return Poll::Pending;
        }
        // Destroy borrowed foreign state before returning control to the caller.
        self.future = None;
        self.completed = Some(status);
        Poll::Ready(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskVTable;
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::rc::Rc;

    struct Probe {
        polls: Rc<Cell<usize>>,
        drops: Rc<Cell<usize>>,
        status: RunStatus,
    }

    unsafe extern "C" fn poll_probe(data: *mut c_void, _: *const HostWaker) -> RunStatus {
        let probe = &mut *data.cast::<Probe>();
        probe.polls.set(probe.polls.get() + 1);
        probe.status
    }

    unsafe extern "C" fn drop_probe(data: *mut c_void) {
        let probe = Box::from_raw(data.cast::<Probe>());
        probe.drops.set(probe.drops.get() + 1);
    }

    static VTABLE: TaskVTable = TaskVTable {
        poll: poll_probe,
        drop: drop_probe,
    };

    #[test]
    fn completion_error_and_cancellation_drop_exactly_once() {
        for status in [RunStatus::Done, RunStatus::Error, RunStatus::Pending] {
            let polls = Rc::new(Cell::new(0));
            let drops = Rc::new(Cell::new(0));
            let raw = TaskHandle {
                data: Box::into_raw(Box::new(Probe {
                    polls: polls.clone(),
                    drops: drops.clone(),
                    status,
                }))
                .cast(),
                vtable: &VTABLE,
            };
            let mut task = unsafe { FfiFuture::new(raw) }.unwrap();
            let mut cx = Context::from_waker(std::task::Waker::noop());
            let result = Pin::new(&mut task).poll(&mut cx);
            if status == RunStatus::Pending {
                assert_eq!(result, Poll::Pending);
                assert_eq!(drops.get(), 0);
            } else {
                assert_eq!(result, Poll::Ready(status));
                assert_eq!(drops.get(), 1);
                assert_eq!(Pin::new(&mut task).poll(&mut cx), result);
                assert_eq!(polls.get(), 1, "completed FFI future was polled again");
            }
            drop(task);
            assert_eq!(drops.get(), 1);
        }
    }

    #[test]
    fn null_data_or_vtable_is_rejected() {
        assert!(unsafe {
            FfiFuture::new(TaskHandle {
                data: std::ptr::null_mut(),
                vtable: &VTABLE,
            })
        }
        .is_none());
        assert!(unsafe {
            FfiFuture::new(TaskHandle {
                data: std::ptr::null_mut(),
                vtable: std::ptr::null(),
            })
        }
        .is_none());
    }

    #[test]
    fn host_guards_cover_poll_and_foreign_drop_for_completion_and_cancellation() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        struct Observed {
            calls: Arc<AtomicUsize>,
            drops: Arc<AtomicUsize>,
            status: RunStatus,
        }
        unsafe extern "C" fn poll(data: *mut c_void, _: *const HostWaker) -> RunStatus {
            let observed = &*data.cast::<Observed>();
            assert!(observed.calls.load(Ordering::Acquire) > 0);
            observed.status
        }
        unsafe extern "C" fn destroy(data: *mut c_void) {
            let observed = Box::from_raw(data.cast::<Observed>());
            assert!(observed.calls.load(Ordering::Acquire) > 0);
            observed.drops.fetch_add(1, Ordering::AcqRel);
        }
        static VTABLE: TaskVTable = TaskVTable {
            poll,
            drop: destroy,
        };
        for status in [RunStatus::Done, RunStatus::Error, RunStatus::Pending] {
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let raw = TaskHandle {
                data: Box::into_raw(Box::new(Observed {
                    calls: calls.clone(),
                    drops: drops.clone(),
                    status,
                }))
                .cast(),
                vtable: &VTABLE,
            };
            let lease = CodeLease::new(Arc::new(()), calls.clone());
            let mut task =
                unsafe { FfiFuture::with_optional_keep_alive(raw, Some(lease)) }.unwrap();
            let _ = Pin::new(&mut task).poll(&mut Context::from_waker(std::task::Waker::noop()));
            assert_eq!(calls.load(Ordering::Acquire), 0);
            drop(task);
            assert_eq!(calls.load(Ordering::Acquire), 0);
            assert_eq!(drops.load(Ordering::Acquire), 1);
        }
    }
}
