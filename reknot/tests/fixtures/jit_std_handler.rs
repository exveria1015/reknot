//! Independent ABI fixture for dynamic std, emulated TLS and in-generation SEH.
#![allow(dead_code)]
#[path = "abi_v5.rs"]
mod abi;
use abi::services::*;
use abi::{HostWaker, RunStatus, TaskHandle, TaskVTable};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::panic::{catch_unwind, panic_any, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};

const VERSION: u64 = if cfg!(version_two) { 2 } else { 1 };
const KEY: ServiceKey = ServiceKey::new(0x54494d45, 1);
static TLS_DROPS: AtomicU64 = AtomicU64::new(0);
static LATE_DROPS: AtomicU64 = AtomicU64::new(0);
static STACK_DROPS: AtomicU64 = AtomicU64::new(0);
static PAYLOAD_DROPS: AtomicU64 = AtomicU64::new(0);

struct Local(Vec<u64>);
impl Drop for Local {
    fn drop(&mut self) {
        // A destructor may initialize another TLS variable. The backing memory
        // must remain available until every destructor round has finished.
        LATE.with(|_| {});
        TLS_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}
struct Late;
impl Drop for Late {
    fn drop(&mut self) {
        LATE_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}
#[repr(align(64))]
struct Aligned([u64; 8]);
thread_local! {
    static LOCAL: RefCell<Local> = RefCell::new(Local(Vec::new()));
    static COUNT: Cell<u64> = const { Cell::new(7) };
    static ALIGN: Aligned = const { Aligned([19; 8]) };
    static LATE: Late = const { Late };
}
struct StackDrop;
impl Drop for StackDrop {
    fn drop(&mut self) {
        STACK_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}
struct Payload(u64);
impl Drop for Payload {
    fn drop(&mut self) {
        PAYLOAD_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

#[inline(never)]
fn panic_leaf(seed: u64) {
    let _drop = StackDrop;
    panic_any(Payload(seed));
}
#[inline(never)]
fn panic_middle(seed: u64) {
    let _drop = StackDrop;
    let payload = catch_unwind(|| panic_leaf(seed)).unwrap_err();
    resume_unwind(payload);
}
fn exercise(seed: u64) -> u64 {
    let payload = catch_unwind(|| panic_middle(seed)).unwrap_err();
    assert_eq!(payload.downcast_ref::<Payload>().unwrap().0, seed);
    drop(payload);
    let mut values = vec![seed + VERSION, seed, seed + 2];
    values.sort_unstable();
    let text = format!("jit-{VERSION}-{}", values[0]);
    let mut map = std::collections::HashMap::new();
    map.insert(text.clone(), values);
    assert_eq!(map[&text][0], seed);
    let shared = std::sync::Arc::new(std::sync::Mutex::new(seed));
    *shared.lock().unwrap() += VERSION;
    let (send, receive) = std::sync::mpsc::channel();
    send.send(*shared.lock().unwrap()).unwrap();
    assert_eq!(receive.recv().unwrap(), seed + VERSION);
    assert!(std::time::Instant::now().elapsed().as_secs() < 60);
    let _ = std::env::var_os("PATH");
    seed + VERSION
}

#[no_mangle]
pub extern "C" fn jit_runtime_probe(seed: u64) -> u64 {
    catch_unwind(|| exercise(seed)).unwrap_or(u64::MAX)
}
#[no_mangle]
pub extern "C" fn jit_runtime_touch(seed: u64) -> u64 {
    catch_unwind(|| {
        ALIGN.with(|v| {
            assert_eq!((v as *const Aligned as usize) % 64, 0);
            assert_eq!(v.0, [19; 8]);
        });
        LOCAL.with(|l| l.borrow_mut().0.push(seed));
        COUNT.with(|n| {
            let old = n.get();
            n.set(old + 1);
            old + VERSION * 100
        })
    })
    .unwrap_or(u64::MAX)
}
#[no_mangle]
pub extern "C" fn jit_runtime_workers(count: u64) -> u64 {
    catch_unwind(|| {
        let workers: Vec<_> = (0..count)
            .map(|seed| {
                std::thread::spawn(move || {
                    assert_eq!(jit_runtime_touch(seed), 7 + VERSION * 100);
                    assert_eq!(jit_runtime_probe(seed), seed + VERSION);
                    assert_eq!(jit_runtime_touch(seed), 8 + VERSION * 100);
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        count
    })
    .unwrap_or(u64::MAX)
}
#[no_mangle]
pub extern "C" fn jit_runtime_stat(which: u32) -> u64 {
    match which {
        0 => TLS_DROPS.load(Ordering::SeqCst),
        1 => LATE_DROPS.load(Ordering::SeqCst),
        2 => STACK_DROPS.load(Ordering::SeqCst),
        3 => PAYLOAD_DROPS.load(Ordering::SeqCst),
        _ => u64::MAX,
    }
}
#[no_mangle]
pub extern "C" fn jit_runtime_quiet() {
    std::panic::set_hook(Box::new(|_| {}));
}

#[link(name = "kernel32")]
extern "system" {
    fn SetLastError(error: u32);
    fn GetLastError() -> u32;
}
#[no_mangle]
pub extern "C" fn jit_runtime_last_error() -> u64 {
    unsafe {
        SetLastError(12345);
        COUNT.with(|n| std::hint::black_box(n.get()));
        let first = GetLastError();
        SetLastError(54321);
        COUNT.with(|n| std::hint::black_box(n.get()));
        u64::from(first) * 100_000 + u64::from(GetLastError())
    }
}

struct Task {
    seed: u64,
    resource: ResourceId,
    services: HostServices,
    control: HostTaskControl,
    polled: bool,
}
impl Drop for Task {
    fn drop(&mut self) {
        unsafe {
            ((*self.control.vtable).release)(self.control.data);
            ((*self.services.vtable).release)(self.services.data);
        }
    }
}
unsafe extern "C" fn make(request: *const HandlerRequest) -> TaskHandle {
    let empty = || TaskHandle {
        data: std::ptr::null_mut(),
        vtable: std::ptr::null(),
    };
    if request.is_null() {
        return empty();
    }
    let r = &*request;
    if r.input.is_null()
        || r.input_len != 8
        || r.state_len != 0
        || r.bindings_len != 0
        || r.init_len != 0
    {
        return empty();
    }
    let t = Box::new(Task {
        seed: r.input.cast::<u64>().read_unaligned(),
        resource: r.resource,
        services: HostServices {
            data: ((*r.services.vtable).retain)(r.services.data),
            vtable: r.services.vtable,
        },
        control: HostTaskControl {
            data: ((*r.control.vtable).retain)(r.control.data),
            vtable: r.control.vtable,
        },
        polled: false,
    });
    TaskHandle {
        data: Box::into_raw(t).cast(),
        vtable: &VTABLE,
    }
}
unsafe extern "C" fn poll(data: *mut c_void, waker: *const HostWaker) -> RunStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let t = &mut *data.cast::<Task>();
        if !t.polled {
            t.polled = true;
            let w = &*waker;
            ((*w.vtable).wake_by_ref)(w.data);
            return RunStatus::Pending;
        }
        let reply = exercise(t.seed);
        let status = ((*t.services.vtable).call)(
            t.services.data,
            t.resource,
            KEY,
            0,
            std::ptr::addr_of!(reply).cast(),
            8,
            std::ptr::null_mut(),
            0,
        )
        .status;
        if status == 0 {
            RunStatus::Done
        } else {
            RunStatus::Error
        }
    }))
    .unwrap_or(RunStatus::Error)
}
unsafe extern "C" fn destroy(data: *mut c_void) {
    drop(Box::from_raw(data.cast::<Task>()));
}
static VTABLE: TaskVTable = TaskVTable {
    poll,
    drop: destroy,
};
static API: HandlerApi = HandlerApi::new(KEY, make);
#[no_mangle]
pub extern "C" fn hot_handler_api() -> *const HandlerApi {
    &API
}
