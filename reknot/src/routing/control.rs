use crate::activity::Entry;
use crate::generation::{enter, CodeLease};
use crate::notification::WakeGate;
use crate::{ActivityHandle, MAX_MANAGED_ACTIVITIES};
use crate::{ServiceError, ServiceKey};
use reknot_abi::services::{
    AccessGenerationLocalFn, HandlerHeader, HostTaskControl, HostTaskVTable,
    HostTaskVTableCancellation, HostTaskVTableCleanup, HostTaskVTableExt, MakeGenerationLocalFn,
    CANCELLATION_ABI_VERSION, GENERATION_SERVICES_ABI_VERSION, TASK_CLEANUP_ABI_VERSION,
    TASK_CONTROL_ABI_VERSION,
};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::task::Context;
use std::task::Waker;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub schema: ServiceKey,
    pub bytes: Vec<u8>,
}
impl Checkpoint {
    pub fn new(schema: ServiceKey, bytes: Vec<u8>) -> Self {
        Self { schema, bytes }
    }
}

pub(super) struct Control {
    pub stop: Cell<bool>,
    pub cancel_requested: Cell<bool>,
    pub checkpoint: RefCell<Option<Checkpoint>>,
    pub schema: ServiceKey,
    pub max_bytes: usize,
    pub waker: RefCell<Option<Waker>>,
    pub closing: Cell<bool>,
    pub failure: Cell<Option<ServiceError>>,
    activities: RefCell<Vec<Entry>>,
    polling: Cell<usize>,
    cancellation: crate::CancellationToken,
    lease: Option<CodeLease>,
    generation: Weak<super::Generation>,
    pub(super) notification_gate: Arc<WakeGate>,
}
impl Control {
    pub fn new(
        schema: ServiceKey,
        checkpoint: Option<Checkpoint>,
        max_bytes: usize,
        lease: Option<CodeLease>,
        generation: Weak<super::Generation>,
    ) -> Rc<Self> {
        Rc::new(Self {
            stop: Cell::new(false),
            cancel_requested: Cell::new(false),
            checkpoint: RefCell::new(checkpoint),
            schema,
            max_bytes,
            waker: RefCell::new(None),
            closing: Cell::new(false),
            failure: Cell::new(None),
            activities: RefCell::new(Vec::new()),
            polling: Cell::new(0),
            cancellation: crate::CancellationToken::default(),
            lease,
            generation,
            notification_gate: Arc::new(WakeGate::default()),
        })
    }
    pub fn stop(&self) {
        self.stop.set(true);
        // A nested Future may have a different Waker from the root. Notify
        // those registrations as well, while their DLL remains guarded.
        self.notify_stop();
        let waker = self.waker.borrow().clone();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
    pub fn close(&self) {
        self.closing.set(true);
        // The root is already terminal/cancelled. Do not wake its executor
        // from completion or Drop; activity stop notifications run during polling.
        self.stop.set(true);
    }
    pub fn request_cancel(&self) -> bool {
        if self.closing.get() || self.cancel_requested.replace(true) {
            return false;
        }
        // Never destroy a future through a shared/reentrant control handle.
        // Its owning executor performs destruction on the next exclusive poll.
        self.close();
        let waker = self.waker.borrow().clone();
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }
    pub fn activity_count(&self) -> usize {
        self.activities.borrow().len() + self.polling.get()
    }
    fn notify_stop(&self) {
        let _call = enter(&self.lease);
        self.cancellation.cancel();
    }
    pub fn poll_activities(&self, cx: &mut Context<'_>) -> bool {
        self.notification_gate.update(cx.waker());
        if self.stop.get() {
            // close() defers notification until polling, including when the
            // root was dropped but a child still owns a RequestContext.
            self.notify_stop();
        }
        // Do not hold a RefCell borrow over foreign callbacks. Registration during
        // polling is bounded too; new entries are driven on the following poll.
        let entries = std::mem::take(&mut *self.activities.borrow_mut());
        self.polling.set(entries.len());
        let mut pending = Vec::new();
        let mut wake_root = false;
        for mut entry in entries {
            let (joined, error) = entry.poll(self.stop.get(), cx);
            if let Some(error) = error {
                if self.failure.get().is_none() {
                    self.failure.set(Some(error));
                    wake_root = true;
                }
                self.stop.set(true);
            }
            if !joined {
                pending.push(entry);
            }
        }
        self.polling.set(0);
        let added = {
            let mut activities = self.activities.borrow_mut();
            let added = !activities.is_empty();
            activities.extend(pending);
            added
        };
        // User Wakers can panic. Restore ownership before notifying the root so
        // recovery cannot accidentally lose unjoined registrations.
        if added {
            // New registrations have not been polled yet and cannot provide
            // their own wake. Schedule another bounded pass, not an inline loop.
            cx.waker().wake_by_ref();
        }
        if wake_root {
            self.stop();
        }
        self.activity_count() == 0
    }
    pub fn raw(self: &Rc<Self>) -> HostTaskControl {
        HostTaskControl {
            data: Rc::as_ptr(self).cast_mut().cast(),
            // Preserve provenance for the optional tail. A reference to only
            // base would grant read access to that prefix, not the extension.
            vtable: std::ptr::addr_of!(VTABLE).cast::<HostTaskVTable>(),
        }
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.notification_gate.revoke();
    }
}
unsafe extern "C" fn notification(data: *mut c_void) -> crate::HostWaker {
    (&*data.cast::<Control>()).notification_gate.raw()
}
unsafe extern "C" fn with_local(
    data: *mut c_void,
    key: *const c_void,
    make: MakeGenerationLocalFn,
    init: *mut c_void,
    access: AccessGenerationLocalFn,
    argument: *mut c_void,
) -> u32 {
    let control = &*data.cast::<Control>();
    match control.generation.upgrade() {
        Some(generation) => generation
            .scope
            .with_local(key, make, init, access, argument),
        None => ServiceError::REVOKED.0,
    }
}
unsafe extern "C" fn register_activity(data: *mut c_void, raw: ActivityHandle) -> u32 {
    let control = &*data.cast::<Control>();
    if control.stop.get() || control.closing.get() {
        return ServiceError::REVOKED.0;
    }
    if control.activity_count() >= MAX_MANAGED_ACTIVITIES {
        return ServiceError::BUSY.0;
    }
    match Entry::new(raw, control.lease.clone()) {
        Ok(entry) => {
            control.activities.borrow_mut().push(entry);
            0
        }
        Err(error) => error.0,
    }
}
unsafe extern "C" fn retain(data: *mut c_void) -> *mut c_void {
    Rc::increment_strong_count(data.cast::<Control>());
    data
}
unsafe extern "C" fn release(data: *mut c_void) {
    drop(Rc::from_raw(data.cast::<Control>()));
}
unsafe extern "C" fn stopped(data: *mut c_void) -> bool {
    (&*data.cast::<Control>()).stop.get()
}
unsafe extern "C" fn cancelled(data: *mut c_void) -> crate::TaskHandle {
    let control = &*data.cast::<Control>();
    let token = control.cancellation.clone();
    crate::request::make_handler_future(Box::pin(async move {
        token.cancelled().await;
        Ok(())
    }))
}
unsafe extern "C" fn cleanup_failed(data: *mut c_void, error: ServiceError) {
    let control = &*data.cast::<Control>();
    if control.failure.get().is_none() || error == ServiceError::ASYNC_CLEANUP_FAILED {
        control.failure.set(Some(error));
    }
    control.stop.set(true);
    if let Some(lease) = &control.lease {
        lease.pin_task_cleanup_failure(error);
    }
    // Host I/O with unconfirmed cleanup stays owned by the activity. Wake a
    // pending Restart so it can report the failure without waiting for join.
    if error != ServiceError::ASYNC_CLEANUP_FAILED {
        return;
    }
    let Some(generation) = control.generation.upgrade() else {
        return;
    };
    if !generation.unconfirmed_cleanup.replace(true) {
        let _ = crate::task::catch_callback(|| {
            let wake = generation.completion.borrow().clone();
            if let Some(wake) = wake {
                wake.wake();
            }
        });
    }
}
unsafe extern "C" fn checkpoint(
    data: *mut c_void,
    schema: ServiceKey,
    bytes: *const u8,
    len: usize,
) -> u32 {
    let control = &*data.cast::<Control>();
    if schema != control.schema || schema.id == 0 {
        return ServiceError::CONTRACT_MISMATCH.0;
    }
    if len > control.max_bytes || (len > 0 && bytes.is_null()) {
        return ServiceError::INVALID_ARGUMENT.0;
    }
    let bytes = if len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(bytes, len).to_vec()
    };
    *control.checkpoint.borrow_mut() = Some(Checkpoint { schema, bytes });
    0
}
static VTABLE: HostTaskVTableCleanup = HostTaskVTableCleanup {
    base: HostTaskVTableCancellation {
        base: HostTaskVTableExt {
            base: HostTaskVTable {
                header: HandlerHeader {
                    abi_version: TASK_CONTROL_ABI_VERSION,
                    struct_size: std::mem::size_of::<HostTaskVTableCleanup>() as u32,
                },
                retain,
                release,
                stop_requested: stopped,
                checkpoint,
                register_activity,
            },
            extension: HandlerHeader {
                abi_version: GENERATION_SERVICES_ABI_VERSION,
                struct_size: std::mem::size_of::<HostTaskVTableExt>() as u32,
            },
            notification,
            with_local,
        },
        cancellation: HandlerHeader {
            abi_version: CANCELLATION_ABI_VERSION,
            struct_size: std::mem::size_of::<HostTaskVTableCancellation>() as u32,
        },
        cancelled,
    },
    cleanup: HandlerHeader {
        abi_version: TASK_CLEANUP_ABI_VERSION,
        struct_size: std::mem::size_of::<HostTaskVTableCleanup>() as u32,
    },
    cleanup_failed,
};
