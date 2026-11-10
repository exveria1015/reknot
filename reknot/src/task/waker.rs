use crate::generation::CodeLease;
use crate::{HostWaker, HostWakerVTable};
use std::ffi::c_void;
use std::future::Future;
use std::pin::Pin;
use std::task::Waker;
use std::task::{Context, Poll};

/// Polls a logic future with a host waker scoped to this call.
///
/// # Safety
/// `host` and all its callbacks must be valid during the call. Its clone callback
/// must produce an owned handle that remains valid until its drop callback runs.
pub unsafe fn poll_result_future(
    mut future: Pin<&mut dyn Future<Output = crate::RunStatus>>,
    host: *const HostWaker,
) -> crate::RunStatus {
    if host.is_null() {
        return crate::RunStatus::Error;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let waker = waker_from_host(&*host);
        let mut cx = Context::from_waker(&waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Pending => crate::RunStatus::Pending,
            Poll::Ready(status) => status,
        }
    }));
    result.unwrap_or(crate::RunStatus::Error)
}

pub(crate) unsafe fn waker_from_host(host: &HostWaker) -> Waker {
    if host.vtable.is_null() {
        return Waker::noop().clone();
    }

    #[repr(C)]
    struct HostProxy {
        data: *mut c_void,
        vtable: *const HostWakerVTable,
    }

    unsafe fn proxy_clone(data: *const ()) -> std::task::RawWaker {
        let proxy = &*(data as *const HostProxy);
        let data = ((*proxy.vtable).clone)(proxy.data);
        let proxy = HostProxy {
            data,
            vtable: proxy.vtable,
        };
        std::task::RawWaker::new(Box::into_raw(Box::new(proxy)) as *const (), &PROXY_VTABLE)
    }

    unsafe fn proxy_wake(data: *const ()) {
        let proxy = Box::from_raw(data as *mut HostProxy);
        ((*proxy.vtable).wake)(proxy.data);
    }

    unsafe fn proxy_wake_by_ref(data: *const ()) {
        let proxy = &*(data as *const HostProxy);
        ((*proxy.vtable).wake_by_ref)(proxy.data);
    }

    unsafe fn proxy_drop(data: *const ()) {
        let proxy = Box::from_raw(data as *mut HostProxy);
        ((*proxy.vtable).drop)(proxy.data);
    }

    static PROXY_VTABLE: std::task::RawWakerVTable =
        std::task::RawWakerVTable::new(proxy_clone, proxy_wake, proxy_wake_by_ref, proxy_drop);

    let data = ((*host.vtable).clone)(host.data);
    let proxy = HostProxy {
        data,
        vtable: host.vtable,
    };
    Waker::from_raw(std::task::RawWaker::new(
        Box::into_raw(Box::new(proxy)) as *const (),
        &PROXY_VTABLE,
    ))
}

struct HostWakerState {
    waker: Waker,
    // Must outlive the Waker's own destructor and every clone sent to a plugin.
    keep_alive: Option<CodeLease>,
}

unsafe extern "C" fn host_waker_clone(data: *mut c_void) -> *mut c_void {
    if data.is_null() {
        return std::ptr::null_mut();
    }
    let state = &*(data as *mut HostWakerState);
    let cloned = HostWakerState {
        waker: state.waker.clone(),
        keep_alive: state.keep_alive.clone(),
    };
    Box::into_raw(Box::new(cloned)) as *mut c_void
}

unsafe extern "C" fn host_waker_wake(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    let state = Box::from_raw(data as *mut HostWakerState);
    state.waker.wake();
}

unsafe extern "C" fn host_waker_wake_by_ref(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    let state = &*(data as *mut HostWakerState);
    state.waker.wake_by_ref();
}

unsafe extern "C" fn host_waker_drop(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    drop(Box::from_raw(data as *mut HostWakerState));
}

static HOST_WAKER_VTABLE: HostWakerVTable = HostWakerVTable {
    clone: host_waker_clone,
    wake: host_waker_wake,
    wake_by_ref: host_waker_wake_by_ref,
    drop: host_waker_drop,
};

pub(crate) struct HostWakerGuard {
    data: *mut c_void,
}

impl Drop for HostWakerGuard {
    fn drop(&mut self) {
        unsafe { (HOST_WAKER_VTABLE.drop)(self.data) };
    }
}

pub(crate) fn host_waker_from(
    waker: &Waker,
    keep_alive: Option<CodeLease>,
) -> (HostWaker, HostWakerGuard) {
    let state = HostWakerState {
        waker: waker.clone(),
        keep_alive,
    };
    let data = Box::into_raw(Box::new(state)) as *mut c_void;
    let host_waker = HostWaker {
        data,
        vtable: &HOST_WAKER_VTABLE,
    };
    (host_waker, HostWakerGuard { data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::task::Wake;

    struct WakeProbe(AtomicUsize);

    struct CodeLease(Arc<AtomicUsize>);

    impl Drop for CodeLease {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Wake for WakeProbe {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn poll_panic_releases_cloned_waker_and_null_host_does_not_poll() {
        let probe = Arc::new(WakeProbe(AtomicUsize::new(0)));
        let waker = Waker::from(probe.clone());
        {
            let (host, _guard) = host_waker_from(&waker, None);
            let mut panicking = Box::pin(async { panic!("logic poll panic") });
            assert_eq!(
                unsafe { poll_result_future(panicking.as_mut(), &host) },
                crate::RunStatus::Error
            );
            // Only the local Waker and the host handle retain the probe.
            assert_eq!(Arc::strong_count(&probe), 3);
        }
        assert_eq!(Arc::strong_count(&probe), 2);
        let mut unpolled = Box::pin(async { panic!("null host must not poll") });
        assert_eq!(
            unsafe { poll_result_future(unpolled.as_mut(), std::ptr::null()) },
            crate::RunStatus::Error
        );
    }

    #[test]
    fn cloned_waker_owns_host_handle_after_poll_scope_and_can_wake_on_another_thread() {
        let probe = Arc::new(WakeProbe(AtomicUsize::new(0)));
        let released = Arc::new(AtomicUsize::new(0));
        let lease = Arc::new(CodeLease(released.clone()));
        let mut retained = None;
        {
            let waker = Waker::from(probe.clone());
            let code =
                crate::generation::CodeLease::new(lease.clone(), Arc::new(AtomicUsize::new(0)));
            let (host, _host_guard) = host_waker_from(&waker, Some(code));
            let mut future = Box::pin(std::future::poll_fn(|cx| {
                let owned = cx.waker().clone();
                owned.wake_by_ref();
                retained = Some(owned.clone());
                owned.wake();
                Poll::Ready(crate::RunStatus::Done)
            }));
            assert_eq!(
                unsafe { poll_result_future(future.as_mut(), &host) },
                crate::RunStatus::Done
            );
        }
        drop(lease);
        assert_eq!(released.load(Ordering::Relaxed), 0);
        std::thread::spawn(move || retained.unwrap().wake())
            .join()
            .unwrap();
        assert_eq!(probe.0.load(Ordering::Relaxed), 3);
        assert_eq!(Arc::strong_count(&probe), 1);
        assert_eq!(released.load(Ordering::Relaxed), 1);
    }
}
