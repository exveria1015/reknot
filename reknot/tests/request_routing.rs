use reknot::{
    HandlerApi, HostResource, HostResources, RequestContext, RequestRouter, RequestTask,
    RouteError, RunStatus, ServiceError, ServiceKey,
};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

const KEY: ServiceKey = ServiceKey::new(7, 1);

#[derive(Default)]
struct Session(Vec<Vec<u8>>);
impl HostResource for Session {
    fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0.push(input.to_vec());
        Ok(0)
    }
}

#[reknot::reload_handler(KEY)]
async fn handle(request: RequestContext) -> Result<(), ServiceError> {
    let mut first = true;
    std::future::poll_fn(|cx| {
        if std::mem::take(&mut first) {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    let mut message = request.generation().to_string().into_bytes();
    message.push(b':');
    message.extend_from_slice(request.input());
    request.call(0, &message, &mut [])?;
    Ok(())
}

fn api() -> HandlerApi {
    unsafe { *hot_handler_api() }
}
fn poll(task: &mut RequestTask) -> Poll<RunStatus> {
    Pin::new(task).poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn drain_cutover_owns_input_and_keeps_old_requests_and_host_resources_alive() {
    let host = HostResources::new();
    let session = host.register(KEY, Session::default());
    let mut router = unsafe { RequestRouter::from_api(api(), host.clone()) }.unwrap();
    let mut input = b"old".to_vec();
    let mut old = router.dispatch(session.id(), &input).unwrap();
    input.fill(b'x'); // The handler owns its original bytes.
    assert_eq!(poll(&mut old), Poll::Pending);
    let prepared = unsafe { router.prepare_api(api()) }.unwrap();
    assert_eq!(router.generation(), 1);
    assert_eq!(prepared.based_on(), 1);
    assert_eq!(router.activate(prepared).unwrap(), 2);
    let mut new = router.dispatch(session.id(), b"new").unwrap();
    assert_eq!(old.generation(), 1);
    assert_eq!(new.generation(), 2);
    assert_eq!(router.in_flight(), 2);
    assert_eq!(poll(&mut new), Poll::Pending);
    drop(router);
    drop(host);
    assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
    assert_eq!(poll(&mut new), Poll::Ready(RunStatus::Done));
    assert_eq!(
        session.try_borrow().unwrap().0,
        [b"1:old".to_vec(), b"2:new".to_vec()]
    );
}

#[test]
fn invalid_stale_and_wrong_router_candidates_do_not_change_admission() {
    let host = HostResources::new();
    let session = host.register(KEY, Session::default());
    let mut router = unsafe { RequestRouter::from_api(api(), host.clone()) }.unwrap();
    let mut wrong = api();
    wrong.service.version += 1;
    assert!(matches!(
        unsafe { router.prepare_api(wrong) },
        Err(RouteError::ContractMismatch { .. })
    ));
    wrong = api();
    wrong.header.struct_size = 0;
    assert!(matches!(
        unsafe { router.prepare_api(wrong) },
        Err(RouteError::Abi { .. })
    ));
    let first = unsafe { router.prepare_api(api()) }.unwrap();
    let stale = unsafe { router.prepare_api(api()) }.unwrap();
    let other = unsafe { RequestRouter::from_api(api(), host) }.unwrap();
    let foreign = unsafe { other.prepare_api(api()) }.unwrap();
    assert!(matches!(
        router.activate(foreign),
        Err(RouteError::WrongRouter)
    ));
    assert_eq!(router.generation(), 1);
    router.pause();
    assert!(matches!(
        router.dispatch(session.id(), b"paused"),
        Err(RouteError::Paused)
    ));
    assert_eq!(router.activate(first).unwrap(), 2);
    assert!(!router.is_accepting());
    assert!(matches!(
        router.activate(stale),
        Err(RouteError::StaleCandidate)
    ));
    router.resume().unwrap();
    let task = router.dispatch(session.id(), b"cancelled").unwrap();
    assert_eq!(router.in_flight(), 1);
    task.cancel();
    assert_eq!(router.in_flight(), 0);
    assert!(session.try_borrow().unwrap().0.is_empty());
}

#[test]
fn revocation_after_admission_fails_without_replaying_the_request() {
    let host = HostResources::new();
    let session = host.register(KEY, Session::default());
    let router = unsafe { RequestRouter::from_api(api(), host.clone()) }.unwrap();
    let mut task = router.dispatch(session.id(), b"revoked").unwrap();
    assert_eq!(poll(&mut task), Poll::Pending);
    assert!(host.revoke(session.id()));
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Error));
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Error));
    assert_eq!(router.in_flight(), 0);
    assert!(matches!(
        router.dispatch(session.id(), b"later"),
        Err(RouteError::Resource(ServiceError::REVOKED))
    ));
    assert!(session.try_borrow().unwrap().0.is_empty());
}

#[test]
fn failed_task_creation_does_not_leave_a_phantom_in_flight_request() {
    unsafe extern "C" fn invalid(_: *const reknot::HandlerRequest) -> reknot::TaskHandle {
        reknot::TaskHandle {
            data: std::ptr::null_mut(),
            vtable: std::ptr::null(),
        }
    }
    let host = HostResources::new();
    let session = host.register(KEY, Session::default());
    let router = unsafe { RequestRouter::from_api(HandlerApi::new(KEY, invalid), host) }.unwrap();
    assert!(matches!(
        router.dispatch(session.id(), b"invalid"),
        Err(RouteError::InvalidTask)
    ));
    assert_eq!(router.in_flight(), 0);
}
