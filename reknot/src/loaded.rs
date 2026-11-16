use crate::generation::GenerationOwner;
use crate::{HandlerApi, RouteError, ServiceKey};
use std::marker::PhantomData;
use std::rc::Rc;

/// A validated handler table and the code generation that owns its callbacks.
///
/// Loading trusted machine code is an unsafe boundary. Once loaded, move this
/// value into [`crate::RequestRouter::from_loaded`] or
/// [`crate::RequestRouter::prepare_loaded`]; no raw callbacks escape this API.
/// Tasks, their destruction, host call-return guards and escaped host Wakers
/// retain the original generation. Dropping or forgetting this handle never
/// automatically unloads code. JIT images default to process residency; explicitly
/// managed Rust COFF images can be collected through owner-thread maintenance.
///
/// This is a single-use, owner-thread value, not a shared function table.
/// ```compile_fail
/// fn duplicate(handler: reknot::LoadedHandler) {
///     let first = handler;
///     let second = handler; // the generation was moved
/// }
/// ```
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<reknot::LoadedHandler>();
/// ```
#[must_use = "move the loaded handler into a router or explicitly abandon it"]
pub struct LoadedHandler {
    pub(crate) api: HandlerApi,
    pub(crate) image: Option<GenerationOwner>,
    _owner_thread: PhantomData<Rc<()>>,
}

impl LoadedHandler {
    /// Wrap a process-resident API at the trust boundary.
    ///
    /// # Safety
    /// Every callback, vtable, dependency and escaped callback must remain valid
    /// until process exit and obey [`crate::RequestRouter::from_api`]'s contracts.
    /// This constructor does not turn arbitrary raw function pointers into safe
    /// code or establish the lifetime of a separately managed dynamic library.
    pub unsafe fn from_static_api(api: HandlerApi) -> Result<Self, RouteError> {
        crate::routing::validate(api)?;
        Ok(Self::new(api, None))
    }

    pub fn service(&self) -> ServiceKey {
        self.api.service
    }

    pub fn state_schema(&self) -> ServiceKey {
        self.api.state_schema
    }

    /// Physical COFF identity. This diagnostic ID does not retain or reacquire code.
    #[cfg(feature = "jit")]
    pub fn coff_image_id(&self) -> Option<crate::jit::CoffImageId> {
        self.image
            .as_ref()
            .and_then(|g| g.code.coff.as_ref())
            .map(|c| c.id)
    }

    #[cfg(feature = "jit")]
    pub fn coff_policy(&self) -> Option<crate::jit::CoffPolicy> {
        self.image
            .as_ref()
            .and_then(|g| g.code.coff.as_ref())
            .map(|c| c.policy)
    }

    pub(crate) fn needs_generation_budget(&self) -> bool {
        #[cfg(feature = "jit")]
        if self.coff_policy() == Some(crate::jit::CoffPolicy::Managed) {
            return false;
        }
        true
    }

    /// Native DLL identity, if any. This identifier does not retain code.
    #[cfg(feature = "native")]
    pub fn artifact_id(&self) -> Option<crate::ArtifactId> {
        self.image.as_ref().and_then(|image| image.id)
    }

    pub(crate) fn new(api: HandlerApi, image: Option<GenerationOwner>) -> Self {
        Self {
            api,
            image,
            _owner_thread: PhantomData,
        }
    }

    pub(crate) fn into_parts(self) -> (HandlerApi, Option<GenerationOwner>) {
        (self.api, self.image)
    }

    /// The loader keeps the complete image and dependencies resident, and the
    /// trusted entry obeys the HandlerApiFn ABI including readable header size.
    #[cfg(any(feature = "native", feature = "jit"))]
    pub(crate) unsafe fn from_entry(
        entry: reknot_abi::services::HandlerApiFn,
        image: GenerationOwner,
    ) -> Result<Self, RouteError> {
        let _call = image.enter();
        let pointer = entry();
        if pointer.is_null() {
            return Err(RouteError::InvalidApi);
        }
        let header = pointer.cast::<crate::HandlerHeader>().read();
        if header.abi_version != crate::HANDLER_ABI_VERSION
            || header.struct_size < reknot_abi::services::HANDLER_API_V5_SIZE as u32
        {
            return Err(RouteError::Abi {
                version: header.abi_version,
                size: header.struct_size,
            });
        }
        let api = HandlerApi::read_compatible(pointer);
        crate::routing::validate(api)?;
        Ok(Self::new(api, Some(image)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        HandlerRequest, HostResource, HostResources, HostServices, HostWaker, RequestRouter,
        RunStatus, ServiceError, TaskHandle, TaskVTable,
    };
    use reknot_abi::services::HostTaskControl;
    use std::cell::{Cell, RefCell};
    use std::ffi::c_void;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Weak};
    use std::task::{Context, Poll, Waker};

    const KEY: ServiceKey = ServiceKey::new(713, 1);
    struct Code;
    struct Probe {
        code: Weak<Code>,
        waker: RefCell<Option<HostWaker>>,
        polls: Cell<usize>,
        drops: Cell<usize>,
    }
    thread_local! { static NEXT: RefCell<Option<Rc<Probe>>> = const { RefCell::new(None) }; }
    struct Task {
        probe: Rc<Probe>,
        services: HostServices,
        control: HostTaskControl,
    }
    unsafe extern "C" fn make(request: *const HandlerRequest) -> TaskHandle {
        let request = &*request;
        let mut services = request.services;
        services.data = ((*services.vtable).retain)(services.data);
        let mut control = request.control;
        control.data = ((*control.vtable).retain)(control.data);
        let probe = NEXT.with(|slot| slot.borrow().as_ref().unwrap().clone());
        assert!(probe.code.upgrade().is_some());
        TaskHandle {
            data: Box::into_raw(Box::new(Task {
                probe,
                services,
                control,
            }))
            .cast(),
            vtable: &VTABLE,
        }
    }
    unsafe extern "C" fn poll(data: *mut c_void, waker: *const HostWaker) -> RunStatus {
        let task = &*data.cast::<Task>();
        assert!(task.probe.code.upgrade().is_some());
        let n = task.probe.polls.get();
        task.probe.polls.set(n + 1);
        if n == 0 {
            let waker = &*waker;
            *task.probe.waker.borrow_mut() = Some(HostWaker {
                data: ((*waker.vtable).clone)(waker.data),
                vtable: waker.vtable,
            });
            RunStatus::Pending
        } else {
            RunStatus::Done
        }
    }
    unsafe extern "C" fn destroy(data: *mut c_void) {
        let task = Box::from_raw(data.cast::<Task>());
        assert!(
            task.probe.code.upgrade().is_some(),
            "destructor lost its original code"
        );
        task.probe.drops.set(task.probe.drops.get() + 1);
        ((*task.control.vtable).release)(task.control.data);
        ((*task.services.vtable).release)(task.services.data);
    }
    static VTABLE: TaskVTable = TaskVTable {
        poll,
        drop: destroy,
    };
    struct Noop;
    impl HostResource for Noop {
        fn call(&mut self, _: u32, _: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
            Ok(0)
        }
    }
    fn handler() -> (LoadedHandler, Rc<Probe>) {
        let code = Arc::new(Code);
        let probe = Rc::new(Probe {
            code: Arc::downgrade(&code),
            waker: RefCell::new(None),
            polls: Cell::new(0),
            drops: Cell::new(0),
        });
        let owner = GenerationOwner::resident(code);
        (
            LoadedHandler::new(HandlerApi::new(KEY, make), Some(owner)),
            probe,
        )
    }
    #[test]
    fn task_and_late_waker_keep_original_code_after_router_drop() {
        let (loaded, probe) = handler();
        let resources = HostResources::new();
        let resource = resources.register(KEY, Noop);
        let router = RequestRouter::from_loaded(loaded, resources).unwrap();
        NEXT.with(|slot| *slot.borrow_mut() = Some(probe.clone()));
        let mut task = router.dispatch(resource.id(), &[]).unwrap();
        NEXT.with(|slot| slot.borrow_mut().take());
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Pending);
        drop(router);
        assert!(probe.code.upgrade().is_some());
        assert_eq!(
            Pin::new(&mut task).poll(&mut cx),
            Poll::Ready(RunStatus::Done)
        );
        drop(task);
        assert_eq!(probe.drops.get(), 1);
        assert!(
            probe.code.upgrade().is_some(),
            "escaped Waker lost the generation"
        );
        let waker = probe.waker.borrow_mut().take().unwrap();
        unsafe {
            ((*waker.vtable).wake_by_ref)(waker.data);
            let clone = ((*waker.vtable).clone)(waker.data);
            ((*waker.vtable).drop)(waker.data);
            assert!(probe.code.upgrade().is_some());
            ((*waker.vtable).wake)(clone);
        }
        assert!(probe.code.upgrade().is_none());
    }
    #[test]
    fn abandoned_and_rejected_candidates_do_not_rebind_the_active_owner() {
        let (first, old) = handler();
        let router = RequestRouter::from_loaded(first, HostResources::new()).unwrap();
        let (next, abandoned) = handler();
        let prepared = router.prepare_loaded(next).unwrap();
        assert!(abandoned.code.upgrade().is_some());
        drop(prepared);
        assert!(abandoned.code.upgrade().is_none());
        let (mut wrong, rejected) = handler();
        wrong.api.service = ServiceKey::new(999, 1);
        assert!(matches!(
            router.prepare_loaded(wrong),
            Err(RouteError::ContractMismatch { .. })
        ));
        assert!(rejected.code.upgrade().is_none());
        assert!(old.code.upgrade().is_some());
        assert_eq!(router.generation(), 1);
        drop(router);
        assert!(old.code.upgrade().is_none());
    }
    #[test]
    #[cfg(not(miri))] // This test intentionally leaks one small owner.
    fn forgetting_a_loaded_owner_retains_code() {
        let (loaded, probe) = handler();
        std::mem::forget(loaded);
        assert!(probe.code.upgrade().is_some());
    }
}
