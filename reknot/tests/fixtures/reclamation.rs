//! Constrained Handler ABI 5 fixture, using the frozen ABI and real Waker bridge.
//! No application TLS or spawned threads. External execution is explicitly
//! returned/joined by the test host before reclamation.
#![allow(dead_code)]
#[path = "abi_v5.rs"]
mod reknot_abi;
use reknot_abi::services::*;
use reknot_abi::{HostWaker, HostWakerVTable, RunStatus, TaskHandle, TaskVTable};
#[path = "../../src/task/waker.rs"]
mod bridge;
#[path = "../../src/generation.rs"]
mod generation;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Waker;

static API: HandlerApi = HandlerApi::new(ServiceKey::new(7, 1), make);
static SAVED_DATA: AtomicUsize = AtomicUsize::new(0);

#[no_mangle]
pub extern "C" fn hot_handler_api() -> *const HandlerApi {
    &API
}

struct Task {
    host: HostServices,
    resource: ResourceId,
    control: HostTaskControl,
    mode: u8,
    first: bool,
}
impl Task {
    unsafe fn report(&self, phase: u32) {
        if self.mode == 2 {
            let result = ((*self.host.vtable).call)(
                self.host.data,
                self.resource,
                API.service,
                phase,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(result.status, 0);
        }
    }
}
unsafe extern "C" fn make(raw: *const HandlerRequest) -> TaskHandle {
    let request = &*raw;
    let task = Box::new(Task {
        host: HostServices {
            data: ((*request.services.vtable).retain)(request.services.data),
            vtable: request.services.vtable,
        },
        control: HostTaskControl {
            data: ((*request.control.vtable).retain)(request.control.data),
            vtable: request.control.vtable,
        },
        resource: request.resource,
        mode: if request.input_len == 0 {
            0
        } else {
            *request.input
        },
        first: true,
    });
    task.report(1);
    TaskHandle {
        data: Box::into_raw(task).cast(),
        vtable: &TASK,
    }
}
unsafe extern "C" fn poll(data: *mut c_void, host: *const HostWaker) -> RunStatus {
    let task = &mut *data.cast::<Task>();
    task.report(2);
    if task.mode == 3 {
        if std::mem::take(&mut task.first) {
            let host = HostServices {
                data: ((*task.host.vtable).retain)(task.host.data),
                vtable: task.host.vtable,
            };
            let activity = ActivityHandle {
                data: Box::into_raw(Box::new(Activity {
                    host,
                    resource: task.resource,
                }))
                .cast(),
                vtable: &ACTIVITY,
            };
            assert_eq!(
                ((*task.control.vtable).register_activity)(task.control.data, activity),
                0
            );
            return RunStatus::Pending;
        }
        return RunStatus::Done;
    }
    if task.mode == 1 {
        let data = Box::into_raw(Box::new(bridge::waker_from_host(&*host)));
        assert_eq!(SAVED_DATA.swap(data as usize, Ordering::AcqRel), 0);
        return RunStatus::Done;
    }
    if task.mode == 2 || !std::mem::take(&mut task.first) {
        RunStatus::Done
    } else {
        RunStatus::Pending
    }
}
unsafe extern "C" fn destroy(data: *mut c_void) {
    let task = Box::from_raw(data.cast::<Task>());
    task.report(3);
    ((*task.control.vtable).release)(task.control.data);
    ((*task.host.vtable).release)(task.host.data);
}
static TASK: TaskVTable = TaskVTable {
    poll,
    drop: destroy,
};

struct Activity {
    host: HostServices,
    resource: ResourceId,
}
impl Activity {
    unsafe fn report(&self, phase: u32, output: &mut [u8]) {
        let result = ((*self.host.vtable).call)(
            self.host.data,
            self.resource,
            API.service,
            phase,
            std::ptr::null(),
            0,
            output.as_mut_ptr(),
            output.len(),
        );
        assert_eq!(result.status, 0);
    }
}
unsafe extern "C" fn stop(data: *mut c_void) -> u32 {
    (&*data.cast::<Activity>()).report(4, &mut []);
    0
}
unsafe extern "C" fn join(data: *mut c_void, _: *const HostWaker) -> RunStatus {
    let mut gate = [0];
    (&*data.cast::<Activity>()).report(5, &mut gate);
    if gate[0] == 0 {
        RunStatus::Pending
    } else {
        RunStatus::Done
    }
}
unsafe extern "C" fn destroy_activity(data: *mut c_void) {
    let activity = Box::from_raw(data.cast::<Activity>());
    activity.report(6, &mut []);
    ((*activity.host.vtable).release)(activity.host.data);
}
static ACTIVITY: ActivityVTable = ActivityVTable {
    header: HandlerHeader {
        abi_version: ACTIVITY_ABI_VERSION,
        struct_size: std::mem::size_of::<ActivityVTable>() as u32,
    },
    request_stop: stop,
    poll_join: join,
    destroy: destroy_activity,
};

/// Drops the last host lease, then remains inside the DLL while the host barrier
/// runs. The test must join the caller before releasing the loader reference.
#[no_mangle]
pub unsafe extern "C" fn hot_release_waker(
    barrier: unsafe extern "C" fn(*mut c_void),
    context: *mut c_void,
) {
    let data = SAVED_DATA.swap(0, Ordering::AcqRel) as *mut Waker;
    assert!(!data.is_null());
    drop(Box::from_raw(data));
    barrier(context);
    // Keep a DLL instruction after the barrier even in optimized builds, so a
    // tail call cannot erase the return frame this experiment is observing.
    SAVED_DATA.store(0, Ordering::SeqCst);
}
