use reknot::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::task::{Context, Poll, Wake, Waker};

const KEY: ServiceKey = ServiceKey::new(99, 1);
const V1: ServiceKey = ServiceKey::new(100, 1);
const V2: ServiceKey = ServiceKey::new(100, 2);
#[derive(Default)]
struct Session(Vec<Vec<u8>>);
impl HostResource for Session {
    fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0.push(input.to_vec());
        Ok(0)
    }
}
struct Guard<'a>(&'a RequestContext);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let _ = self.0.call(0, b"dropped", &mut []);
    }
}
#[reknot::reload_handler(KEY, state = V1)]
async fn worker(request: RequestContext) -> Result<(), ServiceError> {
    if request.input() == b"ordinary" {
        request.cancelled().await;
        return Ok(());
    }
    let guard = Guard(&request);
    let state = request.checkpoint().unwrap();
    request.call(0, &state.bytes, &mut [])?;
    if request.input() == b"oversized" {
        assert_eq!(
            request.save_checkpoint(state.schema, &[1, 2]),
            Err(ServiceError::INVALID_ARGUMENT)
        );
    }
    if request.input() == b"fail" {
        return Err(ServiceError::FAILED);
    }
    if request.input() == b"stubborn" {
        std::future::pending::<()>().await;
    }
    request.save_checkpoint(state.schema, &[state.bytes[0] + 1])?;
    request.cancelled().await;
    request.save_checkpoint(state.schema, &[state.bytes[0] + 2])?;
    drop(guard);
    Ok(())
}
fn api(schema: ServiceKey) -> HandlerApi {
    let mut api = unsafe { *hot_handler_api() };
    api.state_schema = schema;
    api
}
fn cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}
fn poll<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    Pin::new(f).poll(&mut cx())
}
fn setup() -> (RequestRouter, Resource<Session>) {
    let host = HostResources::new();
    let session = host.register(KEY, Session::default());
    let router = unsafe { RequestRouter::from_api(api(V1), host) }.unwrap();
    (router, session)
}

#[test]
fn restart_stops_all_workers_migrates_committed_state_then_starts_once() {
    let (mut router, session) = setup();
    for (id, value) in [(1, 1), (2, 5)] {
        router
            .start_worker(
                WorkerId(id),
                session.id(),
                b"worker",
                Checkpoint::new(V1, vec![value]),
            )
            .unwrap();
    }
    assert!(router.poll_workers(&mut cx()).is_pending());
    let incompatible = unsafe { router.prepare_api(api(V2)) }.unwrap();
    assert!(matches!(
        router.activate(incompatible),
        Err(RouteError::StateSchemaMismatch)
    ));
    let next = unsafe { router.prepare_api(api(V2)) }.unwrap();
    let mut restart = router
        .restart(next, |_, state, schema| {
            Ok(Checkpoint::new(schema, vec![state.bytes[0] + 10]))
        })
        .unwrap();
    assert!(matches!(poll(&mut restart), Poll::Ready(Ok(2))));
    drop(restart);
    assert!(router.is_accepting());
    assert_eq!(router.in_flight(), 2);
    assert_eq!(router.worker_checkpoint(WorkerId(1)).unwrap().bytes, [13]);
    assert_eq!(router.worker_checkpoint(WorkerId(2)).unwrap().bytes, [17]);
    assert_eq!(
        session.try_borrow().unwrap().0,
        [vec![1], vec![5], b"dropped".to_vec(), b"dropped".to_vec()]
    );
    assert!(router.poll_workers(&mut cx()).is_pending());
    assert_eq!(&session.try_borrow().unwrap().0[4..], &[vec![13], vec![17]]);
    assert_eq!(router.in_flight(), 2);
}

#[test]
fn migration_failure_keeps_old_checkpoints_and_pauses_admission_for_retry() {
    let (mut router, session) = setup();
    router
        .start_worker(
            WorkerId(1),
            session.id(),
            b"worker",
            Checkpoint::new(V1, vec![3]),
        )
        .unwrap();
    let _ = router.poll_workers(&mut cx());
    let next = unsafe { router.prepare_api(api(V2)) }.unwrap();
    let mut restart = router
        .restart(next, |_, _, _| Err(ServiceError::FAILED))
        .unwrap();
    assert!(matches!(
        poll(&mut restart),
        Poll::Ready(Err(RouteError::Migration { .. }))
    ));
    drop(restart);
    assert_eq!(router.generation(), 1);
    assert!(!router.is_accepting());
    assert_eq!(
        router.worker_checkpoint(WorkerId(1)).unwrap(),
        Checkpoint::new(V1, vec![5])
    );
    assert!(matches!(
        router.dispatch(session.id(), b"ordinary"),
        Err(RouteError::Paused)
    ));
    let retry = unsafe { router.prepare_api(api(V2)) }.unwrap();
    let mut restart = router
        .restart(retry, |_, state, schema| {
            Ok(Checkpoint::new(schema, state.bytes.clone()))
        })
        .unwrap();
    assert!(matches!(poll(&mut restart), Poll::Ready(Ok(2))));
    drop(restart);
    assert!(!router.is_accepting()); // Previous explicit pause is preserved.
    router.resume().unwrap();
}

#[test]
fn interrupted_restart_does_not_destroy_uncooperative_future() {
    let (mut router, session) = setup();
    router
        .start_worker(
            WorkerId(1),
            session.id(),
            b"stubborn",
            Checkpoint::new(V1, vec![1]),
        )
        .unwrap();
    let _ = router.poll_workers(&mut cx());
    let next = unsafe { router.prepare_api(api(V1)) }.unwrap();
    let mut restart = router
        .restart(next, |_, state, _| Ok(state.clone()))
        .unwrap();
    assert!(poll(&mut restart).is_pending());
    drop(restart); // Same behavior as the host dropping a timed-out future.
    assert_eq!(router.generation(), 1);
    assert!(!router.is_accepting());
    assert_eq!(router.in_flight(), 1);
    assert_eq!(session.try_borrow().unwrap().0, [vec![1]]);
    assert!(router.remove_worker(WorkerId(1)));
    assert_eq!(router.in_flight(), 0);
    assert_eq!(session.try_borrow().unwrap().0.last().unwrap(), b"dropped");
}

struct Signal(AtomicUsize);
impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
#[test]
fn restart_wakes_and_waits_for_external_requests_in_retired_generations() {
    let (mut router, session) = setup();
    let signal = Arc::new(Signal(AtomicUsize::new(0)));
    let waker = Waker::from(signal.clone());
    let mut context = Context::from_waker(&waker);
    let mut ordinary = router.dispatch(session.id(), b"ordinary").unwrap();
    assert!(Pin::new(&mut ordinary).poll(&mut context).is_pending());
    let drain = unsafe { router.prepare_api(api(V1)) }.unwrap();
    router.activate(drain).unwrap();
    let next = unsafe { router.prepare_api(api(V2)) }.unwrap();
    let mut restart = router
        .restart(next, |_, state, _| Ok(state.clone()))
        .unwrap();
    assert!(signal.0.load(Ordering::Relaxed) > 0);
    assert!(poll(&mut restart).is_pending());
    assert_eq!(
        Pin::new(&mut ordinary).poll(&mut context),
        Poll::Ready(RunStatus::Done)
    );
    assert!(matches!(poll(&mut restart), Poll::Ready(Ok(3))));
    drop(restart);
    assert_eq!(router.in_flight(), 0);
}

#[test]
fn budget_is_shared_and_never_refunded_for_discarded_candidates() {
    let host = HostResources::new();
    let budget = GenerationBudget::new(2).unwrap();
    let options = RouterOptions {
        generations: budget.clone(),
        max_checkpoint_bytes: 1,
    };
    let router =
        unsafe { RequestRouter::from_api_with_options(api(V1), host.clone(), options.clone()) }
            .unwrap();
    let candidate = unsafe { router.prepare_api(api(V1)) }.unwrap();
    drop(candidate);
    assert_eq!(budget.used(), 2);
    assert!(matches!(
        unsafe { router.prepare_api(api(V1)) },
        Err(RouteError::GenerationLimit { limit: 2 })
    ));
    drop(router);
    assert!(matches!(
        unsafe { RequestRouter::from_api_with_options(api(V1), host, options) },
        Err(RouteError::GenerationLimit { .. })
    ));
}

#[test]
fn invalid_migration_outputs_and_panics_never_start_new_workers() {
    for mode in 0..4 {
        let (mut router, session) = setup();
        router
            .start_worker(
                WorkerId(1),
                session.id(),
                b"worker",
                Checkpoint::new(V1, vec![1]),
            )
            .unwrap();
        let next = unsafe { router.prepare_api(api(V2)) }.unwrap();
        let mut restart = router
            .restart(next, move |_, state, target| {
                match mode {
                    0 => Ok(state.clone()), // Wrong schema.
                    1 => Ok(Checkpoint::new(target, vec![0; 1024 * 1024 + 1])),
                    2 => panic!("migrator panic"),
                    _ => Err(ServiceError::FAILED),
                }
            })
            .unwrap();
        assert!(matches!(poll(&mut restart), Poll::Ready(Err(_))));
        drop(restart);
        assert_eq!(router.generation(), 1);
        assert!(!router.is_accepting());
        assert_eq!(router.worker_checkpoint(WorkerId(1)).unwrap().bytes, [3]);
        assert_eq!(session.try_borrow().unwrap().0.len(), 2); // Start + drop, no new start.
    }
}

#[test]
fn failed_worker_and_failed_new_factory_leave_admission_paused() {
    let (mut router, session) = setup();
    router
        .start_worker(
            WorkerId(1),
            session.id(),
            b"fail",
            Checkpoint::new(V1, vec![1]),
        )
        .unwrap();
    let candidate = unsafe { router.prepare_api(api(V2)) }.unwrap();
    let mut restart = router
        .restart(candidate, |_, state, schema| {
            Ok(Checkpoint::new(schema, state.bytes.clone()))
        })
        .unwrap();
    assert!(matches!(
        poll(&mut restart),
        Poll::Ready(Err(RouteError::WorkerFailed(WorkerId(1))))
    ));
    drop(restart);
    assert_eq!(router.generation(), 1);
    assert!(!router.is_accepting());

    let (mut router, session) = setup();
    router
        .start_worker(
            WorkerId(1),
            session.id(),
            b"worker",
            Checkpoint::new(V1, vec![1]),
        )
        .unwrap();
    unsafe extern "C" fn invalid(_: *const HandlerRequest) -> TaskHandle {
        TaskHandle {
            data: std::ptr::null_mut(),
            vtable: std::ptr::null(),
        }
    }
    let mut broken = api(V2);
    broken.make_request = invalid;
    let candidate = unsafe { router.prepare_api(broken) }.unwrap();
    let mut restart = router
        .restart(candidate, |_, state, schema| {
            Ok(Checkpoint::new(schema, state.bytes.clone()))
        })
        .unwrap();
    assert!(matches!(
        poll(&mut restart),
        Poll::Ready(Err(RouteError::InvalidTask))
    ));
    drop(restart);
    assert_eq!(router.generation(), 1);
    assert_eq!(router.in_flight(), 0);
    assert_eq!(router.worker_checkpoint(WorkerId(1)).unwrap().bytes, [3]);
    assert!(!router.is_accepting());
}

#[test]
fn partially_staged_workers_are_destroyed_without_running_their_bodies() {
    let (mut router, session) = setup();
    for id in 1..=2 {
        router
            .start_worker(
                WorkerId(id),
                session.id(),
                &[id as u8],
                Checkpoint::new(V1, vec![id as u8]),
            )
            .unwrap();
    }
    unsafe extern "C" fn partial(raw: *const HandlerRequest) -> TaskHandle {
        if *(*raw).input == 2 {
            TaskHandle {
                data: std::ptr::null_mut(),
                vtable: std::ptr::null(),
            }
        } else {
            ((*hot_handler_api()).make_request)(raw)
        }
    }
    let mut broken = api(V2);
    broken.make_request = partial;
    let candidate = unsafe { router.prepare_api(broken) }.unwrap();
    let mut restart = router
        .restart(candidate, |_, state, schema| {
            Ok(Checkpoint::new(schema, state.bytes.clone()))
        })
        .unwrap();
    assert!(matches!(
        poll(&mut restart),
        Poll::Ready(Err(RouteError::InvalidTask))
    ));
    drop(restart);
    assert_eq!(router.generation(), 1);
    assert_eq!(router.in_flight(), 0);
    assert_eq!(
        session.try_borrow().unwrap().0,
        [vec![1], b"dropped".to_vec(), vec![2], b"dropped".to_vec()]
    );
    assert_eq!(router.worker_checkpoint(WorkerId(1)).unwrap().bytes, [3]);
    assert_eq!(router.worker_checkpoint(WorkerId(2)).unwrap().bytes, [4]);
}

#[test]
fn checkpoint_limits_apply_to_seed_and_dll_saves() {
    let host = HostResources::new();
    let session = host.register(KEY, Session::default());
    let options = RouterOptions {
        max_checkpoint_bytes: 1,
        ..RouterOptions::default()
    };
    let mut router =
        unsafe { RequestRouter::from_api_with_options(api(V1), host, options) }.unwrap();
    assert!(matches!(
        router.start_worker(
            WorkerId(1),
            session.id(),
            b"oversized",
            Checkpoint::new(V1, vec![1, 2])
        ),
        Err(RouteError::CheckpointTooLarge)
    ));
    router
        .start_worker(
            WorkerId(1),
            session.id(),
            b"oversized",
            Checkpoint::new(V1, vec![1]),
        )
        .unwrap();
    assert!(router.poll_workers(&mut cx()).is_pending());
    assert_eq!(router.worker_checkpoint(WorkerId(1)).unwrap().bytes, [2]);
}

#[test]
fn wake_panic_during_shutdown_keeps_admission_paused_but_allows_explicit_recovery() {
    struct PanicWake;
    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("waker panic");
        }
    }
    let (mut router, session) = setup();
    let mut task = router.dispatch(session.id(), b"ordinary").unwrap();
    let waker = Waker::from(Arc::new(PanicWake));
    assert!(Pin::new(&mut task)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    let candidate = unsafe { router.prepare_api(api(V1)) }.unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = router.restart(candidate, |_, state, _| Ok(state.clone()));
    }));
    assert!(result.is_err());
    assert!(!router.is_accepting());
    assert_eq!(router.in_flight(), 1);
    router.resume().unwrap();
    task.cancel();
}
