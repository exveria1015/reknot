// Versioned C ABI for message handlers, host services and owned tasks.
// Each allocation is destroyed by the side that created it. Independent DLL
// fixtures carry a frozen ABI snapshot instead of including this source.
use std::ffi::c_void;

pub mod services;

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Done = 0,
    Pending = 1,
    Error = 2,
}

#[repr(C)]
pub struct HostWakerVTable {
    pub clone: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub wake: unsafe extern "C" fn(*mut c_void),
    pub wake_by_ref: unsafe extern "C" fn(*mut c_void),
    pub drop: unsafe extern "C" fn(*mut c_void),
}

#[repr(C)]
pub struct HostWaker {
    pub data: *mut c_void,
    pub vtable: *const HostWakerVTable,
}

#[repr(C)]
pub struct TaskVTable {
    pub poll: unsafe extern "C" fn(*mut c_void, *const HostWaker) -> RunStatus,
    pub drop: unsafe extern "C" fn(*mut c_void),
}

#[repr(C)]
/// An owned foreign future handle. Ownership is transferred to one caller,
/// which must invoke `vtable.drop(data)` exactly once while the library is live.
/// A null data or vtable pointer denotes an invalid handle. Borrowed state and
/// callback context must remain valid until the foreign future is destroyed.
pub struct TaskHandle {
    pub data: *mut c_void,
    pub vtable: *const TaskVTable,
}
