#![allow(dead_code)]
#[path = "abi_v5.rs"]
mod reknot_abi;
use reknot_abi::services::*;
use reknot_abi::{HostWaker, RunStatus, TaskHandle, TaskVTable};
use std::ffi::c_void;

const KEY: ServiceKey = ServiceKey::new(7, if cfg!(wrong_service) { 2 } else { 1 });
static API: HandlerApi = HandlerApi {
    header: HandlerHeader {
        abi_version: if cfg!(bad_abi) {
            999
        } else if cfg!(legacy_handler_abi) {
            2
        } else if cfg!(previous_handler_abi) {
            3
        } else {
            HANDLER_ABI_VERSION
        },
        struct_size: std::mem::size_of::<HandlerApi>() as u32,
    },
    service: KEY,
    make_request: make,
    state_schema: if cfg!(restart_worker) {
        ServiceKey::new(100, if cfg!(version_two) { 2 } else { 1 })
    } else {
        ServiceKey::new(0, 0)
    },
    required_services: [ServiceRequirement::EMPTY; MAX_SERVICE_REQUIREMENTS],
    required_count: 0,
    init_schema: ServiceKey::new(0, 0),
    validate_init: None,
};

#[no_mangle]
pub extern "C" fn hot_handler_api() -> *const HandlerApi {
    if cfg!(header_only_v4) {
        static OLD: HandlerHeader = HandlerHeader {
            abi_version: 4,
            struct_size: 8,
        };
        return (&OLD as *const HandlerHeader).cast();
    }
    &API
}

struct Task {
    host: HostServices,
    resource: ResourceId,
    service: ServiceKey,
    message: Vec<u8>,
    first: bool,
    control: HostTaskControl,
    state: Vec<u8>,
}

unsafe extern "C" fn make(raw: *const HandlerRequest) -> TaskHandle {
    let request = &*raw;
    let host = HostServices {
        data: ((*request.services.vtable).retain)(request.services.data),
        vtable: request.services.vtable,
    };
    let control = HostTaskControl {
        data: ((*request.control.vtable).retain)(request.control.data),
        vtable: request.control.vtable,
    };
    let state = if request.state_len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(request.state, request.state_len).to_vec()
    };
    let mut message = if cfg!(version_two) {
        b"v2:".to_vec()
    } else {
        b"v1:".to_vec()
    };
    if request.input_len != 0 {
        message.extend_from_slice(std::slice::from_raw_parts(request.input, request.input_len));
    }
    TaskHandle {
        data: Box::into_raw(Box::new(Task {
            host,
            resource: request.resource,
            service: request.service,
            message,
            first: true,
            control,
            state,
        }))
        .cast(),
        vtable: &VTABLE,
    }
}

unsafe extern "C" fn poll(data: *mut c_void, waker: *const HostWaker) -> RunStatus {
    let task = &mut *data.cast::<Task>();
    if cfg!(managed_activity) {
        if std::mem::take(&mut task.first) {
            let activity = native_activity::make(task.host, task.resource, task.service);
            let result = ((*task.control.vtable).register_activity)(task.control.data, activity);
            if result != 0 {
                ((*activity.vtable).destroy)(activity.data);
                return RunStatus::Error;
            }
            return RunStatus::Pending;
        }
        return RunStatus::Done;
    }
    if cfg!(restart_worker) {
        let value = if cfg!(version_two) {
            u16::from_le_bytes(task.state[..2].try_into().unwrap())
        } else {
            task.state[0] as u16
        };
        if std::mem::take(&mut task.first) {
            let reply = format!("v{}:{value}", if cfg!(version_two) { 2 } else { 1 });
            let result = ((*task.host.vtable).call)(
                task.host.data,
                task.resource,
                task.service,
                0,
                reply.as_ptr(),
                reply.len(),
                std::ptr::null_mut(),
                0,
            );
            return if result.status == 0 {
                RunStatus::Pending
            } else {
                RunStatus::Error
            };
        }
        if !((*task.control.vtable).stop_requested)(task.control.data) {
            return RunStatus::Pending;
        }
        let bytes = if cfg!(version_two) {
            (value + 1).to_le_bytes().to_vec()
        } else {
            vec![(value + 1) as u8]
        };
        let schema = API.state_schema;
        return if ((*task.control.vtable).checkpoint)(
            task.control.data,
            schema,
            bytes.as_ptr(),
            bytes.len(),
        ) == 0
        {
            RunStatus::Done
        } else {
            RunStatus::Error
        };
    }
    if std::mem::take(&mut task.first) {
        ((*(*waker).vtable).wake_by_ref)((*waker).data);
        return RunStatus::Pending;
    }
    let result = ((*task.host.vtable).call)(
        task.host.data,
        task.resource,
        task.service,
        0,
        task.message.as_ptr(),
        task.message.len(),
        std::ptr::null_mut(),
        0,
    );
    if result.status == 0 {
        RunStatus::Done
    } else {
        RunStatus::Error
    }
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let task = Box::from_raw(data.cast::<Task>());
    ((*task.control.vtable).release)(task.control.data);
    ((*task.host.vtable).release)(task.host.data);
}
static VTABLE: TaskVTable = TaskVTable {
    poll,
    drop: destroy,
};

mod native_activity {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread::{self, JoinHandle, ThreadId};

    struct Signal {
        entered: AtomicBool,
        release: AtomicBool,
        returned: AtomicBool,
    }
    struct Subscription {
        host: HostServices,
        resource: ResourceId,
        service: ServiceKey,
        owner: ThreadId,
        stop_attempted: bool,
        stopped: bool,
        signal: Arc<Signal>,
        thread: Option<JoinHandle<()>>,
    }
    pub unsafe fn make(
        host: HostServices,
        resource: ResourceId,
        service: ServiceKey,
    ) -> ActivityHandle {
        let host = HostServices {
            data: ((*host.vtable).retain)(host.data),
            ..host
        };
        ActivityHandle {
            data: Box::into_raw(Box::new(Subscription {
                host,
                resource,
                service,
                owner: thread::current().id(),
                stop_attempted: false,
                stopped: false,
                signal: Arc::new(Signal {
                    entered: AtomicBool::new(false),
                    release: AtomicBool::new(false),
                    returned: AtomicBool::new(false),
                }),
                thread: None,
            }))
            .cast(),
            vtable: &ACTIVITY,
        }
    }
    impl Subscription {
        unsafe fn log(&self, message: &[u8]) {
            assert_eq!(self.owner, thread::current().id());
            let result = ((*self.host.vtable).call)(
                self.host.data,
                self.resource,
                self.service,
                0,
                message.as_ptr(),
                message.len(),
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(result.status, 0);
        }
    }
    // Simulates an external event source executing a DLL callback. It only owns
    // synchronized DLL-local state, never a thread-bound SDK pointer/service.
    fn callback(signal: Arc<Signal>) {
        signal.entered.store(true, Ordering::Release);
        while !signal.release.load(Ordering::Acquire) {
            thread::yield_now();
        }
        signal.returned.store(true, Ordering::Release);
    }
    unsafe extern "C" fn stop(data: *mut c_void) -> u32 {
        let subscription = &mut *data.cast::<Subscription>();
        subscription.log(b"unsubscribe");
        if cfg!(activity_stop_fails) && !subscription.stop_attempted {
            subscription.stop_attempted = true;
            return ServiceError::FAILED.0;
        }
        subscription.stopped = true;
        0
    }
    unsafe extern "C" fn join(data: *mut c_void, _: *const HostWaker) -> RunStatus {
        let subscription = &mut *data.cast::<Subscription>();
        assert_eq!(subscription.owner, thread::current().id());
        if subscription.thread.is_none() {
            if subscription.stopped {
                return RunStatus::Done;
            }
            let signal = subscription.signal.clone();
            subscription.thread = Some(thread::spawn(move || callback(signal)));
        }
        if !subscription.signal.entered.load(Ordering::Acquire) {
            return RunStatus::Pending;
        }
        subscription.log(b"callback_active");
        let mut gate = [0];
        let result = ((*subscription.host.vtable).call)(
            subscription.host.data,
            subscription.resource,
            subscription.service,
            1,
            std::ptr::null(),
            0,
            gate.as_mut_ptr(),
            1,
        );
        assert_eq!(result.status, 0);
        if subscription.stopped && gate[0] != 0 {
            subscription.signal.release.store(true, Ordering::Release);
        }
        if !subscription.thread.as_ref().unwrap().is_finished() {
            return RunStatus::Pending;
        }
        let result = subscription.thread.take().unwrap().join();
        assert!(subscription.signal.returned.load(Ordering::Acquire));
        subscription.log(b"callback_joined");
        if result.is_ok() {
            RunStatus::Done
        } else {
            RunStatus::Error
        }
    }
    unsafe extern "C" fn destroy(data: *mut c_void) {
        let subscription = Box::from_raw(data.cast::<Subscription>());
        assert!(
            subscription.thread.is_none(),
            "destroyed before external callback joined"
        );
        subscription.log(b"activity_drop");
        ((*subscription.host.vtable).release)(subscription.host.data);
    }
    static ACTIVITY: ActivityVTable = ActivityVTable {
        header: HandlerHeader {
            abi_version: ACTIVITY_ABI_VERSION,
            struct_size: std::mem::size_of::<ActivityVTable>() as u32,
        },
        request_stop: stop,
        poll_join: join,
        destroy,
    };
}
