use crate::{RequestContext, ServiceError};
use reknot_abi::services::{GenerationLocalCreated, GenerationLocalValue};
use std::cell::RefCell;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::atomic::AtomicU8;

/// A static key for registered owner-thread, generation-local data. Values are
/// isolated across routers and generations, lazily initialized, and destroyed
/// while their code is leased when the last generation owner is released.
/// This does not enumerate or clean arbitrary std::thread_local! values.
/// Access and initialization panics are reported as PANICKED. A destructor panic
/// retains native code permanently; the destructor is never retried.
///
/// ```compile_fail
/// use reknot::{GenerationLocal, RequestContext};
/// static VALUE: GenerationLocal<Vec<u8>> = GenerationLocal::new(Vec::new);
/// fn escape(ctx: &RequestContext) -> &'static Vec<u8> {
///     VALUE.with(ctx, |value| value).unwrap()
/// }
/// ```
pub struct GenerationLocal<T: 'static> {
    initialize: fn() -> T,
    // Give every key address identity, including otherwise identical initializers.
    _identity: AtomicU8,
}
impl<T: 'static> GenerationLocal<T> {
    pub const fn new(initialize: fn() -> T) -> Self {
        Self {
            initialize,
            _identity: AtomicU8::new(0),
        }
    }
    pub fn with<R>(
        &'static self,
        context: &RequestContext,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R, ServiceError> {
        self.with_cell(context, |cell| {
            let value = cell.try_borrow().map_err(|_| ServiceError::BUSY)?;
            Ok(f(&value))
        })
    }
    pub fn with_mut<R>(
        &'static self,
        context: &RequestContext,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, ServiceError> {
        self.with_cell(context, |cell| {
            let mut value = cell.try_borrow_mut().map_err(|_| ServiceError::BUSY)?;
            Ok(f(&mut value))
        })
    }
    fn with_cell<R, F>(&'static self, context: &RequestContext, f: F) -> Result<R, ServiceError>
    where
        F: FnOnce(&RefCell<T>) -> Result<R, ServiceError>,
    {
        struct Access<T, F, R> {
            f: Option<F>,
            result: Option<R>,
            _type: PhantomData<fn() -> T>,
        }
        unsafe extern "C" fn access<T, F, R>(data: *mut c_void, argument: *mut c_void) -> u32
        where
            F: FnOnce(&RefCell<T>) -> Result<R, ServiceError>,
        {
            let call = &mut *argument.cast::<Access<T, F, R>>();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (call.f.take().unwrap())(&*data.cast::<RefCell<T>>())
            }));
            match result {
                Ok(Ok(value)) => {
                    call.result = Some(value);
                    0
                }
                Ok(Err(error)) => error.0,
                Err(_) => ServiceError::PANICKED.0,
            }
        }
        let mut call = Access::<T, F, R> {
            f: Some(f),
            result: None,
            _type: PhantomData,
        };
        let status = context.control().with_local(
            (self as *const Self).cast(),
            make::<T>,
            (self as *const Self).cast_mut().cast(),
            access::<T, F, R>,
            (&mut call as *mut Access<T, F, R>).cast(),
        );
        if status != 0 {
            return Err(ServiceError(status));
        }
        call.result.ok_or(ServiceError::FAILED)
    }
}
unsafe extern "C" fn destroy<T>(data: *mut c_void) -> u32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(Box::from_raw(data.cast::<RefCell<T>>()))
    })) {
        Ok(()) => 0,
        Err(_) => ServiceError::PANICKED.0,
    }
}
unsafe extern "C" fn make<T: 'static>(argument: *mut c_void) -> GenerationLocalCreated {
    let key = &*argument.cast::<GenerationLocal<T>>();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Box::new(RefCell::new((key.initialize)()))
    }));
    match result {
        Ok(value) => GenerationLocalCreated {
            status: 0,
            value: GenerationLocalValue {
                data: Box::into_raw(value).cast(),
                destroy: Some(destroy::<T>),
            },
        },
        Err(_) => GenerationLocalCreated {
            status: ServiceError::PANICKED.0,
            value: GenerationLocalValue {
                data: std::ptr::null_mut(),
                destroy: None,
            },
        },
    }
}
