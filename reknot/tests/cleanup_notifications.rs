use reknot::*;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

const KEY: ServiceKey = ServiceKey::new(0x1311_c1ea, 1);
thread_local! {
    static READY: Cell<bool> = const { Cell::new(false) };
    static JOIN_WAKE: RefCell<Option<Waker>> = const { RefCell::new(None) };
}
#[derive(Default)]
struct Signal(AtomicUsize);
impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Owner {
    inner: Rc<RefCell<Option<RequestTask>>>,
    joins: Rc<Cell<usize>>,
}
impl HostResource for Owner {
    fn call(&mut self, operation: u32, _: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        if operation == 1 {
            let inner = self.inner.borrow_mut().take();
            drop(inner);
        }
        self.joins.set(self.joins.get() + 1);
        Ok(0)
    }
}
#[reload_handler(KEY)]
async fn run(context: RequestContext) -> Result<(), ServiceError> {
    let operation = if context.input() == b"outer" { 1 } else { 2 };
    let hold = context.input() == b"hold";
    let context = Rc::new(context);
    let child_context = context.clone();
    context.spawn_child(move |stop| async move {
        stop.cancelled().await;
        if hold {
            std::future::poll_fn(|cx| {
                if READY.with(Cell::get) {
                    Poll::Ready(())
                } else {
                    JOIN_WAKE.with(|slot| *slot.borrow_mut() = Some(cx.waker().clone()));
                    Poll::Pending
                }
            })
            .await;
        }
        child_context.call(operation, &[], &mut [])?;
        Ok(())
    })?;
    context.cancelled().await;
    Ok(())
}
fn setup(nested: bool) -> (RequestRouter, Rc<Cell<usize>>) {
    READY.with(|ready| ready.set(false));
    let inner = Rc::new(RefCell::new(None));
    let joins = Rc::new(Cell::new(0));
    let resources = HostResources::new();
    let resource = resources.register(
        KEY,
        Owner {
            inner: inner.clone(),
            joins: joins.clone(),
        },
    );
    let router = unsafe { RequestRouter::from_api(*hot_handler_api(), resources) }.unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    if nested {
        let mut task = router.dispatch(resource.id(), b"inner").unwrap();
        assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
        *inner.borrow_mut() = Some(task);
    }
    let mut outer = router
        .dispatch(resource.id(), if nested { b"outer" } else { b"hold" })
        .unwrap();
    assert!(Pin::new(&mut outer).poll(&mut cx).is_pending());
    drop(outer);
    (router, joins)
}

#[test]
fn newly_cancelled_root_gets_another_bounded_cleanup_poll() {
    let (router, joins) = setup(true);
    let signal = Arc::new(Signal::default());
    let waker = Waker::from(signal.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(poll_cancelled_requests(&mut cx).is_pending());
    assert_eq!(
        joins.get(),
        1,
        "one batch must not recursively drain unbounded new work"
    );
    assert_eq!(router.in_flight(), 1);
    assert!(signal.0.swap(0, Ordering::SeqCst) > 0);
    assert!(poll_cancelled_requests(&mut cx).is_ready());
    assert_eq!(router.in_flight(), 0);
    assert_eq!(joins.get(), 2);
    assert_eq!(signal.0.load(Ordering::SeqCst), 0);
}

#[test]
fn pending_cleanup_without_new_requests_does_not_self_wake() {
    let (router, joins) = setup(false);
    let signal = Arc::new(Signal::default());
    let waker = Waker::from(signal.clone());
    let mut cx = Context::from_waker(&waker);
    for _ in 0..2 {
        assert!(poll_cancelled_requests(&mut cx).is_pending());
        assert_eq!(signal.0.load(Ordering::SeqCst), 0);
    }
    READY.with(|ready| ready.set(true));
    JOIN_WAKE.with(|slot| slot.borrow_mut().take().unwrap().wake());
    assert!(signal.0.swap(0, Ordering::SeqCst) > 0);
    assert!(poll_cancelled_requests(&mut cx).is_ready());
    assert_eq!(router.in_flight(), 0);
    assert_eq!(joins.get(), 1);
}

#[test]
fn scheduling_panic_keeps_new_cleanup_owned_for_recovery() {
    struct PanicWake;
    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("executor notification failed");
        }
    }
    let (router, joins) = setup(true);
    let waker = Waker::from(Arc::new(PanicWake));
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        poll_cancelled_requests(&mut Context::from_waker(&waker))
    }))
    .is_err());
    assert_eq!(router.in_flight(), 1);
    assert_eq!(joins.get(), 1);
    assert!(poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_ready());
    assert_eq!(router.in_flight(), 0);
    assert_eq!(joins.get(), 2);
}

#[test]
fn scheduling_notification_can_reenter_cleanup_after_ownership_is_restored() {
    struct Reenter;
    impl Wake for Reenter {
        fn wake(self: Arc<Self>) {
            assert!(poll_cancelled_requests(&mut Context::from_waker(Waker::noop())).is_ready());
        }
    }
    let (router, joins) = setup(true);
    let waker = Waker::from(Arc::new(Reenter));
    assert!(poll_cancelled_requests(&mut Context::from_waker(&waker)).is_ready());
    assert_eq!(router.in_flight(), 0);
    assert_eq!(joins.get(), 2);
}
