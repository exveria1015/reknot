use crate::generation::{enter, CodeLease};
use crate::ServiceError;
use reknot_abi::services::{AccessGenerationLocalFn, GenerationLocalValue, MakeGenerationLocalFn};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::rc::Rc;

pub const MAX_GENERATION_LOCALS: usize = 64;
enum Slot {
    Initializing,
    Ready(LocalEntry),
}
struct LocalEntry {
    value: GenerationLocalValue,
    lease: Option<CodeLease>,
}
pub(crate) struct GenerationScope {
    values: RefCell<BTreeMap<usize, Slot>>,
    order: RefCell<Vec<usize>>,
    lease: Option<CodeLease>,
}
impl GenerationScope {
    pub(crate) fn new(lease: Option<CodeLease>) -> Rc<Self> {
        Rc::new(Self {
            values: RefCell::new(BTreeMap::new()),
            order: RefCell::new(Vec::new()),
            lease,
        })
    }
    /// The Rc on the host stack keeps every local alive throughout foreign
    /// initialization/access, even if a reentrant callback releases the router.
    pub(crate) unsafe fn with_local(
        self: &Rc<Self>,
        key: *const c_void,
        make: MakeGenerationLocalFn,
        init: *mut c_void,
        access: AccessGenerationLocalFn,
        argument: *mut c_void,
    ) -> u32 {
        if key.is_null() {
            return ServiceError::INVALID_ARGUMENT.0;
        }
        let key = key as usize;
        let existing = {
            let values = self.values.borrow();
            match values.get(&key) {
                Some(Slot::Initializing) => return ServiceError::BUSY.0,
                Some(Slot::Ready(entry)) => Some(entry.value.data),
                None if values.len() >= MAX_GENERATION_LOCALS => return ServiceError::BUSY.0,
                None => None,
            }
        };
        let _call = enter(&self.lease);
        let data = if let Some(data) = existing {
            data
        } else {
            self.values.borrow_mut().insert(key, Slot::Initializing);
            let created = make(init);
            if created.status != 0
                || created.value.data.is_null()
                || created.value.destroy.is_none()
            {
                self.values.borrow_mut().remove(&key);
                // The FFI factory contract requires null data on failure.
                // Unknown foreign ownership still must not allow reclamation.
                if !created.value.data.is_null() {
                    registered(&self.lease);
                    failed(&self.lease);
                    std::mem::forget(self.lease.clone());
                }
                return if created.status == 0 {
                    ServiceError::INVALID_ARGUMENT.0
                } else {
                    created.status
                };
            }
            let data = created.value.data;
            self.values.borrow_mut().insert(
                key,
                Slot::Ready(LocalEntry {
                    value: created.value,
                    lease: self.lease.clone(),
                }),
            );
            self.order.borrow_mut().push(key);
            registered(&self.lease);
            data
        };
        access(data, argument)
    }
}
impl Drop for GenerationScope {
    fn drop(&mut self) {
        // Control holds only a Weak scope. At this point further access cannot
        // reinitialize values or build a scope -> local -> control ownership cycle.
        let mut values = std::mem::take(self.values.get_mut());
        for key in self.order.get_mut().drain(..).rev() {
            if let Some(Slot::Ready(entry)) = values.remove(&key) {
                let _call = enter(&entry.lease);
                let status = match entry.value.destroy {
                    Some(destroy) => unsafe { destroy(entry.value.data) },
                    None => ServiceError::INVALID_ARGUMENT.0,
                };
                if status == 0 {
                    destroyed(&entry.lease);
                } else {
                    failed(&entry.lease);
                    // The destructor may have partially destroyed its state.
                    // Never invoke it again, including during host TLS teardown.
                    std::mem::forget(entry);
                }
            }
        }
    }
}
fn registered(lease: &Option<CodeLease>) {
    #[cfg(feature = "native")]
    if let Some(id) = lease.as_ref().and_then(CodeLease::native_id) {
        crate::retention::update(id, |r| {
            r.registered_locals = r.registered_locals.saturating_add(1)
        });
    }
    let _ = lease;
}
fn destroyed(lease: &Option<CodeLease>) {
    #[cfg(feature = "native")]
    if let Some(id) = lease.as_ref().and_then(CodeLease::native_id) {
        crate::retention::update(id, |r| {
            r.registered_locals = r.registered_locals.saturating_sub(1)
        });
    }
    let _ = lease;
}
fn failed(lease: &Option<CodeLease>) {
    #[cfg(feature = "native")]
    if let Some(id) = lease.as_ref().and_then(CodeLease::native_id) {
        crate::retention::update(id, |r| {
            r.local_cleanup_failures = r.local_cleanup_failures.saturating_add(1)
        });
    }
    let _ = lease;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Make {
        scope: Rc<GenerationScope>,
        key: *const c_void,
        id: usize,
        events: Rc<RefCell<Vec<usize>>>,
        calls: Arc<AtomicUsize>,
        recurse: bool,
    }
    struct Value {
        id: usize,
        events: Rc<RefCell<Vec<usize>>>,
        calls: Arc<AtomicUsize>,
    }
    unsafe extern "C" fn destroy(data: *mut c_void) -> u32 {
        let value = Box::from_raw(data.cast::<Value>());
        assert!(value.calls.load(Ordering::Acquire) > 0);
        value.events.borrow_mut().push(value.id);
        0
    }
    unsafe extern "C" fn access(_: *mut c_void, _: *mut c_void) -> u32 {
        0
    }
    unsafe extern "C" fn make(
        argument: *mut c_void,
    ) -> reknot_abi::services::GenerationLocalCreated {
        let call = &*argument.cast::<Make>();
        assert!(call.calls.load(Ordering::Acquire) > 0);
        if call.recurse {
            assert_eq!(
                call.scope
                    .with_local(call.key, make, argument, access, std::ptr::null_mut()),
                ServiceError::BUSY.0
            );
        }
        reknot_abi::services::GenerationLocalCreated {
            status: 0,
            value: GenerationLocalValue {
                data: Box::into_raw(Box::new(Value {
                    id: call.id,
                    events: call.events.clone(),
                    calls: call.calls.clone(),
                }))
                .cast(),
                destroy: Some(destroy),
            },
        }
    }
    #[test]
    fn initialization_is_bounded_reentrancy_is_busy_and_cleanup_reverses_creation_order() {
        let calls = Arc::new(AtomicUsize::new(0));
        let scope = GenerationScope::new(Some(CodeLease::new(Arc::new(()), calls.clone())));
        let keys = [0u8; MAX_GENERATION_LOCALS + 1];
        let events = Rc::new(RefCell::new(Vec::new()));
        for id in 0..keys.len() {
            // Mix address order so cleanup order cannot accidentally follow the map.
            let key = &keys[if id == MAX_GENERATION_LOCALS {
                id
            } else {
                (id * 17) % MAX_GENERATION_LOCALS
            }];
            let mut argument = Make {
                scope: scope.clone(),
                key: (key as *const u8).cast(),
                id,
                events: events.clone(),
                calls: calls.clone(),
                recurse: true,
            };
            let status = unsafe {
                scope.with_local(
                    argument.key,
                    make,
                    (&mut argument as *mut Make).cast(),
                    access,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(
                status,
                if id == MAX_GENERATION_LOCALS {
                    ServiceError::BUSY.0
                } else {
                    0
                }
            );
        }
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert!(events.borrow().is_empty());
        drop(scope);
        assert_eq!(
            *events.borrow(),
            (0..MAX_GENERATION_LOCALS).rev().collect::<Vec<_>>()
        );
        assert_eq!(calls.load(Ordering::Acquire), 0);
    }
}
