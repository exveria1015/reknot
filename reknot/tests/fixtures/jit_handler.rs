//! Finite, single-owner ABI fixture, not a general task allocator. Every loaded
//! image has one task slot. The harness never dispatches a second concurrent
//! request in the same generation, never reenters, and keeps all images loaded.
#![no_std]
#![allow(dead_code)]
extern crate core as std;
#[path = "abi_v5.rs"]
mod abi;
#[path = "jit_compute.rs"]
mod compute;
use abi::services::*;
use abi::{HostWaker, RunStatus, TaskHandle, TaskVTable};
use core::ffi::c_void;

const KEY: ServiceKey = ServiceKey::new(0x54494d45, 1);
struct Task {
    seed: u64,
    resource: ResourceId,
    services: HostServices,
    control: HostTaskControl,
    polled: bool,
}
static mut BUSY: bool = false;
static mut TASK: Task = Task {
    seed: 0,
    resource: ResourceId {
        registry: 0,
        slot: 0,
        generation: 0,
    },
    services: HostServices {
        data: core::ptr::null_mut(),
        vtable: core::ptr::null(),
    },
    control: HostTaskControl {
        data: core::ptr::null_mut(),
        vtable: core::ptr::null(),
    },
    polled: false,
};

unsafe extern "C" fn make(request: *const HandlerRequest) -> TaskHandle {
    if request.is_null() || BUSY {
        return TaskHandle {
            data: core::ptr::null_mut(),
            vtable: core::ptr::null(),
        };
    }
    let r = &*request;
    if r.input.is_null()
        || r.input_len != 8
        || r.state_len != 0
        || r.bindings_len != 0
        || r.init_len != 0
    {
        return TaskHandle {
            data: core::ptr::null_mut(),
            vtable: core::ptr::null(),
        };
    }
    let t = &mut *core::ptr::addr_of_mut!(TASK);
    t.seed = (r.input as *const u64).read_unaligned();
    t.resource = r.resource;
    t.services.vtable = r.services.vtable;
    t.services.data = ((*r.services.vtable).retain)(r.services.data);
    t.control.vtable = r.control.vtable;
    t.control.data = ((*r.control.vtable).retain)(r.control.data);
    t.polled = false;
    BUSY = true;
    TaskHandle {
        data: core::ptr::addr_of_mut!(TASK).cast(),
        vtable: &VTABLE,
    }
}

unsafe extern "C" fn poll(data: *mut c_void, waker: *const HostWaker) -> RunStatus {
    let t = &mut *data.cast::<Task>();
    if !t.polled {
        t.polled = true;
        let w = &*waker;
        ((*w.vtable).wake_by_ref)(w.data);
        return RunStatus::Pending;
    }
    let reply = compute::bench(t.seed, None);
    let status = ((*t.services.vtable).call)(
        t.services.data,
        t.resource,
        KEY,
        0,
        core::ptr::addr_of!(reply).cast(),
        8,
        core::ptr::null_mut(),
        0,
    )
    .status;
    if status == 0 {
        RunStatus::Done
    } else {
        RunStatus::Error
    }
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let t = &mut *data.cast::<Task>();
    ((*t.control.vtable).release)(t.control.data);
    ((*t.services.vtable).release)(t.services.data);
    BUSY = false;
}
static VTABLE: TaskVTable = TaskVTable {
    poll,
    drop: destroy,
};
static API: HandlerApi = HandlerApi {
    header: HandlerHeader {
        abi_version: if cfg!(bad_abi) {
            999
        } else {
            HANDLER_ABI_VERSION
        },
        struct_size: core::mem::size_of::<HandlerApi>() as u32,
    },
    ..HandlerApi::new(KEY, make)
};
#[cfg(unsupported_tls)]
#[used]
#[link_section = ".tls$AAA"]
static UNSUPPORTED_TLS: u8 = 1;
#[cfg(unresolved_import)]
extern "C" {
    fn reknot_missing_runtime();
}
#[no_mangle]
pub extern "C" fn hot_handler_api() -> *const HandlerApi {
    #[cfg(unresolved_import)]
    unsafe {
        reknot_missing_runtime();
    }
    &API
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
