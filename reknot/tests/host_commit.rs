use reknot::*;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

const KEY: ServiceKey = ServiceKey::new(0x484f_5354, 1);
const STATE: ServiceKey = ServiceKey::new(0x4853_5441, 1);
struct Session {
    cleanup: Rc<Cell<bool>>,
    committed: Rc<Cell<bool>>,
    starts: Rc<RefCell<Vec<u64>>>,
}
impl HostResource for Session {
    fn call(
        &mut self,
        operation: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, ServiceError> {
        if operation == 0 {
            output[0] = u8::from(self.cleanup.get());
            return Ok(1);
        }
        let generation = u64::from_le_bytes(input.try_into().unwrap());
        if generation > 1 {
            assert!(self.committed.get(), "new body ran before host commit");
        }
        self.starts.borrow_mut().push(generation);
        Ok(0)
    }
}
#[reload_handler(KEY, state = STATE)]
async fn root(ctx: RequestContext) -> Result<(), ServiceError> {
    if ctx.input() == b"ordinary" {
        ctx.cancelled().await;
        return Ok(());
    }
    ctx.call(1, &ctx.generation().to_le_bytes(), &mut [])?;
    let services = ctx.services().clone();
    let id = ctx.resource();
    ctx.spawn_child(move |stop| async move {
        stop.cancelled().await;
        std::future::poll_fn(|_| {
            let mut gate = [0];
            services.call(id, KEY, 0, &[], &mut gate)?;
            if gate[0] == 1 {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
        .await
    })?;
    let state = ctx.checkpoint().unwrap();
    ctx.cancelled().await;
    ctx.save_checkpoint(state.schema, &[state.bytes[0] + 1])?;
    Ok(())
}
fn cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut cx())
}
struct Fixture {
    router: RequestRouter,
    session: Resource<Session>,
    cleanup: Rc<Cell<bool>>,
    committed: Rc<Cell<bool>>,
    starts: Rc<RefCell<Vec<u64>>>,
}
fn setup() -> Fixture {
    let host = HostResources::new();
    let cleanup = Rc::new(Cell::new(false));
    let committed = Rc::new(Cell::new(false));
    let starts = Rc::new(RefCell::new(Vec::new()));
    let session = host.register(
        KEY,
        Session {
            cleanup: cleanup.clone(),
            committed: committed.clone(),
            starts: starts.clone(),
        },
    );
    let mut router = unsafe { RequestRouter::from_api(*hot_handler_api(), host) }.unwrap();
    router
        .start_worker(
            WorkerId(1),
            session.id(),
            b"worker",
            Checkpoint::new(STATE, vec![1]),
        )
        .unwrap();
    assert!(router.poll_workers(&mut cx()).is_pending());
    Fixture {
        router,
        session,
        cleanup,
        committed,
        starts,
    }
}
fn next(router: &RequestRouter) -> PreparedHandler {
    unsafe { router.prepare_api(*hot_handler_api()) }.unwrap()
}

#[test]
fn commit_runs_once_after_all_roots_and_children_join_and_before_new_bodies() {
    let Fixture {
        mut router,
        session,
        cleanup,
        committed,
        starts,
    } = setup();
    let mut ordinary = router.dispatch(session.id(), b"ordinary").unwrap();
    assert!(poll(&mut ordinary).is_pending());
    let calls = Rc::new(Cell::new(0));
    let invoked = calls.clone();
    let published = committed.clone();
    let candidate = next(&router);
    let mut restart = router
        .restart_with_commit(
            candidate,
            |_, state, _| {
                assert_eq!(state.bytes, [2]);
                Ok(state.clone())
            },
            || {
                assert_eq!(*starts.borrow(), [1]);
                invoked.set(invoked.get() + 1);
                published.set(true);
                Ok(())
            },
        )
        .unwrap();
    assert!(poll(&mut restart).is_pending());
    assert_eq!(calls.get(), 0);
    assert_eq!(poll(&mut ordinary), Poll::Ready(RunStatus::Done));
    assert!(poll(&mut restart).is_pending());
    assert_eq!(calls.get(), 0);
    cleanup.set(true);
    assert!(matches!(poll(&mut restart), Poll::Ready(Ok(2))));
    drop(restart);
    assert_eq!(calls.get(), 1);
    assert_eq!(*starts.borrow(), [1]);
    assert!(router.poll_workers(&mut cx()).is_pending());
    assert_eq!(*starts.borrow(), [1, 2]);
    router.stop_worker(WorkerId(1));
    let _ = router.poll_workers(&mut cx());
}

#[test]
fn migration_and_factory_failures_and_interruption_never_commit_host_changes() {
    for phase in 0..3 {
        let Fixture {
            mut router,
            cleanup,
            committed,
            ..
        } = setup();
        let mut api = unsafe { *hot_handler_api() };
        unsafe extern "C" fn invalid(_: *const HandlerRequest) -> TaskHandle {
            TaskHandle {
                data: std::ptr::null_mut(),
                vtable: std::ptr::null(),
            }
        }
        if phase == 1 {
            api.make_request = invalid;
        }
        let candidate = unsafe { router.prepare_api(api) }.unwrap();
        if phase != 2 {
            cleanup.set(true);
        }
        let mut restart = router
            .restart_with_commit(
                candidate,
                |_, state, _| {
                    if phase == 0 {
                        Err(ServiceError::FAILED)
                    } else {
                        Ok(state.clone())
                    }
                },
                || {
                    committed.set(true);
                    Ok(())
                },
            )
            .unwrap();
        if phase == 2 {
            assert!(poll(&mut restart).is_pending());
        } else {
            assert!(matches!(poll(&mut restart), Poll::Ready(Err(_))));
        }
        drop(restart);
        assert!(!committed.get());
        assert_eq!(router.generation(), 1);
        assert!(!router.is_accepting());
        // Explicitly finish cleanup after the interrupted host wait.
        cleanup.set(true);
        let _ = router.poll_workers(&mut cx());
    }
}

#[test]
fn failed_host_commit_keeps_checkpoints_and_can_be_retried_without_replaying_roots() {
    let Fixture {
        mut router,
        cleanup,
        committed,
        starts,
        ..
    } = setup();
    cleanup.set(true);
    let candidate = next(&router);
    let mut restart = router
        .restart_with_commit(
            candidate,
            |_, state, _| Ok(state.clone()),
            || Err(ServiceError::BUSY),
        )
        .unwrap();
    assert!(matches!(
        poll(&mut restart),
        Poll::Ready(Err(RouteError::HostCommit(ServiceError::BUSY)))
    ));
    drop(restart);
    assert_eq!(router.generation(), 1);
    assert!(!router.is_accepting());
    assert_eq!(*starts.borrow(), [1]);
    assert_eq!(router.worker_checkpoint(WorkerId(1)).unwrap().bytes, [2]);
    let candidate = next(&router);
    let mut restart = router
        .restart_with_commit(
            candidate,
            |_, state, _| {
                assert_eq!(state.bytes, [2]);
                Ok(state.clone())
            },
            || {
                committed.set(true);
                Ok(())
            },
        )
        .unwrap();
    assert!(matches!(poll(&mut restart), Poll::Ready(Ok(2))));
    drop(restart);
    assert!(!router.is_accepting());
    router.resume().unwrap();
    assert_eq!(*starts.borrow(), [1]);
}

#[test]
fn host_panic_is_reported_but_application_side_effects_need_their_own_rollback() {
    let Fixture {
        mut router,
        cleanup,
        committed,
        starts,
        ..
    } = setup();
    cleanup.set(true);
    let candidate = next(&router);
    let mut restart = router
        .restart_with_commit(
            candidate,
            |_, state, _| Ok(state.clone()),
            || {
                committed.set(true);
                panic!("application commit panic");
            },
        )
        .unwrap();
    assert!(matches!(
        poll(&mut restart),
        Poll::Ready(Err(RouteError::HostCommit(ServiceError::PANICKED)))
    ));
    drop(restart);
    assert!(committed.get());
    assert_eq!(router.generation(), 1);
    assert!(!router.is_accepting());
    assert_eq!(*starts.borrow(), [1]);
}
