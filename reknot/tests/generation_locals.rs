use reknot::*;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

const KEY: ServiceKey = ServiceKey::new(0x4c4f_4341, 1);
#[derive(Default)]
struct Session(Vec<(u32, u64)>);
impl HostResource for Session {
    fn call(&mut self, op: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0
            .push((op, u64::from_le_bytes(input.try_into().unwrap())));
        Ok(0)
    }
}
#[derive(Default)]
struct Local {
    count: u64,
    target: Option<(Services, ResourceId, ServiceKey, u64)>,
}
impl Drop for Local {
    fn drop(&mut self) {
        if let Some((services, resource, service, generation)) = &self.target {
            services
                .call(*resource, *service, 9, &generation.to_le_bytes(), &mut [])
                .unwrap();
        }
    }
}
static LOCAL: GenerationLocal<Local> = GenerationLocal::new(Local::default);
static PANIC: GenerationLocal<()> = GenerationLocal::new(|| panic!("local initializer panic"));
#[reload_handler(KEY)]
async fn handle(ctx: RequestContext) -> Result<(), ServiceError> {
    if ctx.input() == b"borrow" {
        let result = LOCAL.with_mut(&ctx, |_| LOCAL.with(&ctx, |_| ()))?;
        assert_eq!(result, Err(ServiceError::BUSY));
        assert_eq!(
            LOCAL.with(&ctx, |_| panic!("access panic")),
            Err::<(), _>(ServiceError::PANICKED)
        );
        assert_eq!(PANIC.with(&ctx, |_| ()), Err(ServiceError::PANICKED));
    }
    let count = LOCAL.with_mut(&ctx, |local| {
        local.target = Some((
            ctx.services().clone(),
            ctx.resource(),
            ctx.service(),
            ctx.generation(),
        ));
        local.count += 1;
        local.count
    })?;
    ctx.call(0, &count.to_le_bytes(), &mut [])?;
    if ctx.input() == b"wait" {
        ctx.cancelled().await;
    }
    Ok(())
}
fn poll(task: &mut RequestTask) -> Poll<RunStatus> {
    Pin::new(task).poll(&mut Context::from_waker(Waker::noop()))
}
fn setup() -> (RequestRouter, Resource<Session>) {
    let resources = HostResources::new();
    let session = resources.register(KEY, Session::default());
    let router = unsafe { RequestRouter::from_api(handler_api(), resources) }.unwrap();
    (router, session)
}
fn run(router: &RequestRouter, session: &Resource<Session>, input: &[u8]) {
    let mut task = router.dispatch(session.id(), input).unwrap();
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Done));
}
#[test]
fn locals_are_shared_only_within_one_router_generation_and_destroyed_after_old_tasks() {
    let (mut router, session) = setup();
    let mut old = router.dispatch(session.id(), b"wait").unwrap();
    assert!(poll(&mut old).is_pending());
    run(&router, &session, b"now");
    assert_eq!(session.try_borrow().unwrap().0, [(0, 1), (0, 2)]);
    let candidate = unsafe { router.prepare_api(handler_api()) }.unwrap();
    router.activate(candidate).unwrap();
    run(&router, &session, b"now");
    assert_eq!(router.release_retired_generations(), 0);
    assert!(!session
        .try_borrow()
        .unwrap()
        .0
        .iter()
        .any(|(op, _)| *op == 9));
    drop(old);
    assert_eq!(router.release_retired_generations(), 1);
    assert_eq!(
        session.try_borrow().unwrap().0,
        [(0, 1), (0, 2), (0, 1), (9, 1)]
    );
    drop(router);
    assert_eq!(session.try_borrow().unwrap().0.last(), Some(&(9, 2)));
}
#[test]
fn routers_and_owner_threads_have_independent_registered_values() {
    for _ in 0..2 {
        let (router, session) = setup();
        run(&router, &session, b"now");
        assert_eq!(session.try_borrow().unwrap().0, [(0, 1)]);
    }
    std::thread::spawn(|| {
        let (router, session) = setup();
        run(&router, &session, b"now");
        assert_eq!(session.try_borrow().unwrap().0, [(0, 1)]);
    })
    .join()
    .unwrap();
}
#[test]
fn borrow_conflicts_and_initializer_and_access_panics_are_errors_without_poisoning_other_values() {
    let (router, session) = setup();
    run(&router, &session, b"borrow");
    run(&router, &session, b"now");
    assert_eq!(session.try_borrow().unwrap().0, [(0, 1), (0, 2)]);
}

#[test]
fn legacy_task_control_prefix_does_not_read_or_invoke_an_absent_extension() {
    use reknot_abi::services::{
        HandlerHeader, HostTaskControl, HostTaskVTable, TASK_CONTROL_ABI_VERSION,
    };
    use std::cell::Cell;
    use std::ffi::c_void;
    unsafe extern "C" fn retain(data: *mut c_void) -> *mut c_void {
        let count = &*data.cast::<Cell<usize>>();
        count.set(count.get() + 1);
        data
    }
    unsafe extern "C" fn release(data: *mut c_void) {
        let count = &*data.cast::<Cell<usize>>();
        count.set(count.get() - 1);
    }
    unsafe extern "C" fn stopped(_: *mut c_void) -> bool {
        false
    }
    unsafe extern "C" fn checkpoint(_: *mut c_void, _: ServiceKey, _: *const u8, _: usize) -> u32 {
        0
    }
    unsafe extern "C" fn register(_: *mut c_void, _: ActivityHandle) -> u32 {
        ServiceError::UNSUPPORTED_OPERATION.0
    }
    let table = HostTaskVTable {
        header: HandlerHeader {
            abi_version: TASK_CONTROL_ABI_VERSION,
            struct_size: std::mem::size_of::<HostTaskVTable>() as u32,
        },
        retain,
        release,
        stop_requested: stopped,
        checkpoint,
        register_activity: register,
    };
    let references = Cell::new(0usize);
    let resources = HostResources::new();
    let services = resources.services();
    let request = HandlerRequest {
        generation: 1,
        resource: ResourceId::default(),
        service: KEY,
        services: services.as_raw(),
        input: std::ptr::null(),
        input_len: 0,
        control: HostTaskControl {
            data: (&references as *const Cell<usize>).cast_mut().cast(),
            vtable: &table,
        },
        state_schema: ServiceKey::new(0, 0),
        state: std::ptr::null(),
        state_len: 0,
        bindings: std::ptr::null(),
        bindings_len: 0,
        init_schema: ServiceKey::new(0, 0),
        init: std::ptr::null(),
        init_len: 0,
    };
    let context = unsafe { RequestContext::from_raw(&request) }.unwrap();
    assert_eq!(references.get(), 1);
    assert_eq!(
        context.host_notification().err(),
        Some(ServiceError::UNSUPPORTED_OPERATION)
    );
    assert_eq!(
        LOCAL.with(&context, |_| ()),
        Err(ServiceError::UNSUPPORTED_OPERATION)
    );
    drop(context);
    assert_eq!(references.get(), 0);

    // A foreign extension's payload is not our function-pointer layout.
    // Its unknown version must be rejected before forming a reference to it.
    use reknot_abi::services::HostTaskVTableExt;
    let mut table = table;
    table.header.struct_size = std::mem::size_of::<HostTaskVTableExt>() as u32;
    let mut unknown = std::mem::MaybeUninit::<HostTaskVTableExt>::zeroed();
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&table as *const HostTaskVTable).cast::<u8>(),
            unknown.as_mut_ptr().cast::<u8>(),
            std::mem::size_of::<HostTaskVTable>(),
        );
        let pointer = unknown.as_mut_ptr().cast::<HostTaskVTableExt>();
        std::ptr::addr_of_mut!((*pointer).extension).write(HandlerHeader {
            abi_version: 999,
            struct_size: std::mem::size_of::<HostTaskVTableExt>() as u32,
        });
    }
    let request = HandlerRequest {
        control: HostTaskControl {
            data: (&references as *const Cell<usize>).cast_mut().cast(),
            vtable: unknown.as_ptr().cast(),
        },
        ..request
    };
    let context = unsafe { RequestContext::from_raw(&request) }.unwrap();
    assert_eq!(
        context.host_notification().err(),
        Some(ServiceError::UNSUPPORTED_OPERATION)
    );
    assert_eq!(
        LOCAL.with(&context, |_| ()),
        Err(ServiceError::UNSUPPORTED_OPERATION)
    );
    drop(context);
    assert_eq!(references.get(), 0);
}
