//! Notifications escaping SDK host I/O own only resident host code and data.
use crate::{HostWaker, HostWakerVTable, ServiceError};
use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Waker;

#[derive(Default)]
struct State {
    closed: bool,
    waker: Option<Waker>,
}
#[derive(Default)]
pub(crate) struct WakeGate(Mutex<State>);
impl WakeGate {
    pub(crate) fn update(&self, waker: &Waker) {
        // No executor clone/drop/wake runs while the mutex is held.
        // The wrapper's clones only retain an Arc. Its wake and final Drop
        // contain executor panics, including a last release racing with revoke.
        let new = crate::task::unwind_safe_waker(waker);
        let old = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return;
            }
            state.waker.replace(new)
        };
        drop(old);
    }
    pub(crate) fn revoke(&self) {
        let old = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            state.waker.take()
        };
        drop(old);
    }
    fn wake(&self) {
        let waker = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waker
            .clone();
        if let Some(waker) = waker {
            // Both notification and final destruction are protected by the
            // stored wrapper; neither can unwind across this C ABI callback.
            waker.wake();
        }
    }
    pub(crate) fn raw(self: &Arc<Self>) -> HostWaker {
        HostWaker {
            data: Arc::into_raw(self.clone()).cast_mut().cast(),
            vtable: &VTABLE,
        }
    }
}
unsafe extern "C" fn clone_raw(data: *mut c_void) -> *mut c_void {
    Arc::increment_strong_count(data.cast::<WakeGate>());
    data
}
unsafe extern "C" fn wake(data: *mut c_void) {
    let gate = Arc::from_raw(data.cast::<WakeGate>());
    gate.wake();
}
unsafe extern "C" fn wake_by_ref(data: *mut c_void) {
    (&*data.cast::<WakeGate>()).wake();
}
unsafe extern "C" fn release(data: *mut c_void) {
    drop(Arc::from_raw(data.cast::<WakeGate>()));
}
static VTABLE: HostWakerVTable = HostWakerVTable {
    clone: clone_raw,
    wake,
    wake_by_ref,
    drop: release,
};

/// Owner-thread wrapper for a host-resident C notification. Foreign consumers
/// must clone as_raw() through its vtable before retaining it. After effective
/// request completion, wake/clone/drop remain valid and wake has no effect.
/// This is not a DLL-owned Rust Waker to move into arbitrary external runtimes.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<reknot::HostNotification>();
/// ```
pub struct HostNotification {
    raw: HostWaker,
    _local: PhantomData<Rc<()>>,
}
impl HostNotification {
    pub(crate) unsafe fn owned(raw: HostWaker) -> Result<Self, ServiceError> {
        if raw.data.is_null() || raw.vtable.is_null() {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        Ok(Self {
            raw,
            _local: PhantomData,
        })
    }
    /// A borrowed C descriptor, valid while this wrapper lives. A retained copy
    /// needs its own vtable.clone; each owned handle needs exactly one drop/wake.
    pub fn as_raw(&self) -> HostWaker {
        HostWaker {
            data: self.raw.data,
            vtable: self.raw.vtable,
        }
    }
    pub fn notify(&self) {
        unsafe { ((*self.raw.vtable).wake_by_ref)(self.raw.data) };
    }
}
impl Clone for HostNotification {
    fn clone(&self) -> Self {
        let data = unsafe { ((*self.raw.vtable).clone)(self.raw.data) };
        Self {
            raw: HostWaker { data, ..self.raw },
            _local: PhantomData,
        }
    }
}
impl Drop for HostNotification {
    fn drop(&mut self) {
        unsafe { ((*self.raw.vtable).drop)(self.raw.data) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;
    struct Probe(AtomicUsize);
    impl Wake for Probe {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[test]
    fn revoked_notification_clones_do_not_retain_executor_or_call_it_after_request_end() {
        let probe = Arc::new(Probe(AtomicUsize::new(0)));
        let executor = Waker::from(probe.clone());
        let gate = Arc::new(WakeGate::default());
        gate.update(&executor);
        let notification = unsafe { HostNotification::owned(gate.raw()) }.unwrap();
        let external = unsafe { crate::task::waker_from_host(&notification.as_raw()) };
        external.wake_by_ref();
        assert_eq!(probe.0.load(Ordering::Relaxed), 1);
        gate.revoke();
        assert_eq!(Arc::strong_count(&probe), 2);
        drop(notification);
        drop(gate);
        std::thread::spawn(move || {
            let cloned = external.clone();
            external.wake_by_ref();
            cloned.wake();
            drop(external);
        })
        .join()
        .unwrap();
        assert_eq!(probe.0.load(Ordering::Relaxed), 1);
        assert_eq!(Arc::strong_count(&probe), 2);
    }
    #[test]
    fn notification_racing_with_revoke_never_keeps_executor_ownership_after_join() {
        let probe = Arc::new(Probe(AtomicUsize::new(0)));
        let gate = Arc::new(WakeGate::default());
        gate.update(&Waker::from(probe.clone()));
        let origin = unsafe { HostNotification::owned(gate.raw()) }.unwrap();
        let external = unsafe { crate::task::waker_from_host(&origin.as_raw()) };
        let thread = std::thread::spawn(move || {
            for _ in 0..8 {
                external.wake_by_ref();
            }
        });
        gate.revoke();
        thread.join().unwrap();
        assert_eq!(Arc::strong_count(&probe), 1);
        origin.notify();
        let before = probe.0.load(Ordering::Relaxed);
        origin.notify();
        assert_eq!(probe.0.load(Ordering::Relaxed), before);
    }
}
