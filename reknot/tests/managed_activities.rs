use reknot::*;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

const KEY: ServiceKey = ServiceKey::new(0x4143_5449, 1);
const STATE: ServiceKey = ServiceKey::new(0x4143_5453, 1);
#[derive(Default)]
struct Probe {
    allow_join: Cell<bool>,
    stop_failures: Cell<usize>,
    fail_join: Cell<bool>,
    starts: Cell<usize>,
    stops: Cell<usize>,
    joins: Cell<usize>,
    drops: Cell<usize>,
    rejected: Cell<usize>,
    child_started: Cell<usize>,
    child_finished: Cell<usize>,
}
thread_local! { static PROBE: RefCell<Option<Rc<Probe>>> = const { RefCell::new(None) }; }
fn current_probe() -> Rc<Probe> {
    PROBE.with(|slot| slot.borrow().as_ref().unwrap().clone())
}
struct Callback {
    probe: Rc<Probe>,
    started: bool,
    stopped: bool,
    joined: bool,
}
impl Callback {
    fn new(probe: Rc<Probe>) -> Self {
        Self {
            probe,
            started: false,
            stopped: false,
            joined: false,
        }
    }
}
// This deterministic subscription owns its state. Ready certifies it is closed;
// Drop before first poll or after joining cannot leave a live callback behind.
unsafe impl ManagedActivity for Callback {
    fn request_stop(mut self: Pin<&mut Self>) -> Result<(), ServiceError> {
        self.probe.stops.set(self.probe.stops.get() + 1);
        if self.probe.stop_failures.get() > 0 {
            self.probe
                .stop_failures
                .set(self.probe.stop_failures.get() - 1);
            return Err(ServiceError::FAILED);
        }
        self.stopped = true;
        Ok(())
    }
    fn poll_join(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), ServiceError>> {
        if !self.started && !self.stopped {
            self.started = true;
            self.probe.starts.set(self.probe.starts.get() + 1);
        }
        if !self.stopped || !self.probe.allow_join.get() {
            return Poll::Pending;
        }
        self.joined = true;
        self.probe.joins.set(self.probe.joins.get() + 1);
        Poll::Ready(if self.probe.fail_join.get() {
            Err(ServiceError::FAILED)
        } else {
            Ok(())
        })
    }
}
impl Drop for Callback {
    fn drop(&mut self) {
        assert!(
            !self.started || self.joined,
            "live callback state freed before join"
        );
        self.probe.drops.set(self.probe.drops.get() + 1);
    }
}
struct Session;
impl HostResource for Session {
    fn call(&mut self, _: u32, _: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        Ok(0)
    }
}

#[reload_handler(KEY, state = STATE)]
async fn root(ctx: RequestContext) -> Result<(), ServiceError> {
    let probe = current_probe();
    if ctx.input() == b"children" || ctx.input() == b"child_panic" {
        let panic = ctx.input() == b"child_panic";
        ctx.spawn_child(move |stop| async move {
            probe.child_started.set(probe.child_started.get() + 1);
            assert!(!panic, "child panic");
            stop.cancelled().await;
            std::future::poll_fn(|_| {
                if probe.allow_join.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            probe.child_finished.set(probe.child_finished.get() + 1);
            Ok(())
        })?;
    } else if ctx.input() == b"limit" {
        for _ in 0..MAX_MANAGED_ACTIVITIES {
            let probe = probe.clone();
            ctx.spawn_child(move |stop| async move {
                probe.child_started.set(probe.child_started.get() + 1);
                stop.cancelled().await;
                Ok(())
            })?;
        }
        let probe = probe.clone();
        assert_eq!(
            ctx.spawn_child(move |_| async move {
                probe.child_started.set(9999);
                Ok(())
            }),
            Err(ServiceError::BUSY)
        );
    } else {
        ctx.register_activity(Callback::new(probe.clone()))?;
    }
    if ctx.input() == b"return" {
        return Ok(());
    }
    if ctx.input() == b"panic" {
        panic!("root panic");
    }
    ctx.cancelled().await;
    assert_eq!(
        ctx.register_activity(Callback::new(current_probe())),
        Err(ServiceError::REVOKED)
    );
    current_probe().rejected.set(1);
    if let Some(state) = ctx.checkpoint() {
        ctx.save_checkpoint(state.schema, &[9])?;
    }
    Ok(())
}
fn setup() -> (RequestRouter, Resource<Session>, Rc<Probe>) {
    let probe = Rc::new(Probe::default());
    PROBE.with(|slot| *slot.borrow_mut() = Some(probe.clone()));
    let resources = HostResources::new();
    let session = resources.register(KEY, Session);
    let api = unsafe { *hot_handler_api() };
    let router = unsafe { RequestRouter::from_api(api, resources) }.unwrap();
    (router, session, probe)
}
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
}
fn cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}
fn candidate(router: &RequestRouter) -> PreparedHandler {
    unsafe { router.prepare_api(*hot_handler_api()) }.unwrap()
}

#[test]
fn local_child_shutdown_delays_worker_restart_and_uses_committed_checkpoint() {
    let (mut router, session, probe) = setup();
    router
        .start_worker(
            WorkerId(1),
            session.id(),
            b"children",
            Checkpoint::new(STATE, vec![1]),
        )
        .unwrap();
    assert!(router.poll_workers(&mut cx()).is_pending());
    assert_eq!(probe.child_started.get(), 1);
    let next = candidate(&router);
    let mut restart = router
        .restart(next, |_, state, _| {
            assert_eq!(state.bytes, [9]);
            Ok(state.clone())
        })
        .unwrap();
    assert!(poll(&mut restart).is_pending());
    assert_eq!(probe.child_finished.get(), 0);
    drop(restart); // interruption must retain the unfinished child
    assert_eq!(router.in_flight(), 1);
    assert_eq!(router.generation(), 1);
    probe.allow_join.set(true);
    let next = candidate(&router);
    let mut restart = router
        .restart(next, |_, state, _| Ok(state.clone()))
        .unwrap();
    assert!(matches!(poll(&mut restart), Poll::Ready(Ok(2))));
    drop(restart);
    assert_eq!(probe.child_finished.get(), 1);
    // The staged new root has not yet started a child.
    assert_eq!(probe.child_started.get(), 1);
    router.remove_worker(WorkerId(1));
}

#[test]
fn normal_return_and_root_panic_wait_for_callback_quiescence() {
    for (input, expected) in [
        (b"return".as_slice(), RunStatus::Done),
        (b"panic", RunStatus::Error),
    ] {
        let (router, session, probe) = setup();
        let mut task = router.dispatch(session.id(), input).unwrap();
        assert!(poll(&mut task).is_pending());
        assert_eq!(task.status(), None);
        assert_eq!(router.in_flight(), 1);
        assert_eq!(probe.drops.get(), 0);
        probe.allow_join.set(true);
        assert_eq!(poll(&mut task), Poll::Ready(expected));
        assert_eq!(router.in_flight(), 0);
        assert_eq!(probe.joins.get(), 1);
        assert_eq!(probe.drops.get(), 1);
    }
}

#[test]
fn drop_in_old_drain_generation_retains_cleanup_across_router_drop() {
    let (mut router, session, probe) = setup();
    let mut task = router.dispatch(session.id(), b"callback").unwrap();
    assert!(poll(&mut task).is_pending());
    assert_eq!(probe.starts.get(), 1);
    let next = candidate(&router);
    router.activate(next).unwrap();
    drop(task);
    assert_eq!(router.in_flight(), 1);
    assert!(poll_cancelled_requests(&mut cx()).is_pending());
    assert_eq!(probe.drops.get(), 0);
    drop(router);
    drop(session);
    probe.allow_join.set(true);
    assert!(poll_cancelled_requests(&mut cx()).is_ready());
    assert_eq!(probe.joins.get(), 1);
    assert_eq!(probe.drops.get(), 1);
}

#[test]
fn failed_unsubscribe_is_not_join_and_blocks_cutover_even_for_dropped_old_root() {
    let (mut router, session, probe) = setup();
    let mut task = router.dispatch(session.id(), b"callback").unwrap();
    assert!(poll(&mut task).is_pending());
    let next = candidate(&router);
    router.activate(next).unwrap();
    probe.stop_failures.set(1);
    drop(task);
    let next = candidate(&router);
    let mut restart = router
        .restart(next, |_, state, _| Ok(state.clone()))
        .unwrap();
    assert!(poll(&mut restart).is_pending());
    assert_eq!(probe.stops.get(), 1);
    assert_eq!(probe.joins.get(), 0);
    assert_eq!(probe.drops.get(), 0);
    drop(restart);
    assert!(!router.acknowledge_activity_failure(1)); // still alive
    probe.allow_join.set(true);
    let next = candidate(&router);
    let mut restart = router
        .restart(next, |_, state, _| Ok(state.clone()))
        .unwrap();
    assert!(matches!(
        poll(&mut restart),
        Poll::Ready(Err(RouteError::ActivityFailed { generation: 1 }))
    ));
    drop(restart);
    assert_eq!(router.in_flight(), 0);
    assert_eq!(router.generation(), 2);
    assert!(!router.is_accepting());
    assert_eq!(probe.stops.get(), 2);
    assert_eq!(probe.joins.get(), 1);
    assert!(router.acknowledge_activity_failure(1));
    let next = candidate(&router);
    let mut restart = router
        .restart(next, |_, state, _| Ok(state.clone()))
        .unwrap();
    assert!(matches!(poll(&mut restart), Poll::Ready(Ok(3))));
}

#[test]
fn quiescent_join_failure_and_safe_child_panic_are_terminal_errors() {
    for input in [b"callback".as_slice(), b"child_panic"] {
        let (mut router, session, probe) = setup();
        probe.fail_join.set(true);
        let mut task = router.dispatch(session.id(), input).unwrap();
        assert!(poll(&mut task).is_pending());
        let next = candidate(&router);
        let mut restart = router
            .restart(next, |_, state, _| Ok(state.clone()))
            .unwrap();
        assert!(poll(&mut restart).is_pending());
        probe.allow_join.set(true);
        assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Error));
        assert!(matches!(
            poll(&mut restart),
            Poll::Ready(Err(RouteError::ActivityFailed { generation: 1 }))
        ));
    }
}

#[test]
fn registration_is_bounded_lazy_and_closed_after_stop() {
    let (mut router, session, probe) = setup();
    let mut task = router.dispatch(session.id(), b"limit").unwrap();
    assert!(poll(&mut task).is_pending());
    assert_eq!(task.activity_count(), MAX_MANAGED_ACTIVITIES);
    assert_eq!(probe.child_started.get(), MAX_MANAGED_ACTIVITIES);
    let next = candidate(&router);
    let mut restart = router
        .restart(next, |_, state, _| Ok(state.clone()))
        .unwrap();
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Done));
    assert_eq!(current_probe().rejected.get(), 1);
    assert!(matches!(poll(&mut restart), Poll::Ready(Ok(2))));
}

#[test]
fn wake_panic_after_activity_failure_preserves_pending_cleanup_for_recovery() {
    struct PanicWake;
    impl std::task::Wake for PanicWake {
        fn wake(self: std::sync::Arc<Self>) {
            panic!("waker panic");
        }
    }
    let (router, session, probe) = setup();
    probe.stop_failures.set(1);
    let mut task = router.dispatch(session.id(), b"return").unwrap();
    let waker = Waker::from(std::sync::Arc::new(PanicWake));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = Pin::new(&mut task).poll(&mut Context::from_waker(&waker));
    }));
    assert!(result.is_err());
    assert_eq!(task.activity_count(), 1);
    assert_eq!(router.in_flight(), 1);
    assert_eq!(probe.drops.get(), 0);
    probe.allow_join.set(true);
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Error));
    assert_eq!(router.in_flight(), 0);
    assert_eq!(probe.drops.get(), 1);
}

#[test]
fn wake_panic_while_draining_cancelled_queue_does_not_lose_other_requests() {
    struct PanicWake;
    impl std::task::Wake for PanicWake {
        fn wake(self: std::sync::Arc<Self>) {
            panic!("completion waker panic");
        }
    }
    let (mut router, session, probe) = setup();
    for _ in 0..2 {
        let mut task = router.dispatch(session.id(), b"callback").unwrap();
        assert!(poll(&mut task).is_pending());
        drop(task);
    }
    probe.allow_join.set(true);
    let next = candidate(&router);
    let mut restart = router
        .restart(next, |_, state, _| Ok(state.clone()))
        .unwrap();
    let waker = Waker::from(std::sync::Arc::new(PanicWake));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = Pin::new(&mut restart).poll(&mut Context::from_waker(&waker));
    }));
    assert!(result.is_err());
    drop(restart);
    assert_eq!(router.in_flight(), 1);
    assert!(poll_cancelled_requests(&mut cx()).is_ready());
    assert_eq!(router.in_flight(), 0);
    assert_eq!(probe.joins.get(), 2);
    assert_eq!(probe.drops.get(), 2);
}
