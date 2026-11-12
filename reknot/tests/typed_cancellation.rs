#![cfg(feature = "typed")]
use futures_util::stream::{FuturesUnordered, Stream};
use reknot::typed::{Error, Handler, Module, ModuleHost};
use reknot::RequestContext;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

const WAIT: Handler<u8, ()> = Handler::new("cancel.wait", 1);
thread_local! {
    static JOIN_READY: Cell<bool> = const { Cell::new(false) };
    static JOIN_WAKE: RefCell<Option<Waker>> = const { RefCell::new(None) };
}
fn module() -> Module {
    Module::new("cancel.test", 1).with(WAIT)
}
#[reknot::handler(WAIT)]
async fn wait(context: RequestContext, mode: u8) {
    if mode == 2 {
        context
            .spawn_child(|stop| async move {
                stop.cancelled().await;
                std::future::poll_fn(|cx| {
                    if JOIN_READY.with(Cell::get) {
                        Poll::Ready(())
                    } else {
                        JOIN_WAKE.with(|slot| *slot.borrow_mut() = Some(cx.waker().clone()));
                        Poll::Pending
                    }
                })
                .await;
                Ok(())
            })
            .unwrap();
    }
    if mode == 0 {
        context.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}
reknot::export_handlers!(module(), [wait]);
fn host() -> ModuleHost {
    unsafe { module().builder().from_api(handler_api()) }.unwrap()
}
#[derive(Default)]
struct Signal(AtomicUsize);
impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn cancelled_call_is_requeued_by_futures_unordered() {
    let host = host();
    let mut calls = FuturesUnordered::new();
    calls.push(host.handler(WAIT).unwrap().call(0));
    let signal = Arc::new(Signal::default());
    let waker = Waker::from(signal.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut calls).poll_next(&mut cx).is_pending());
    signal.0.store(0, Ordering::SeqCst);
    calls.iter_mut().next().unwrap().cancel();
    assert!(signal.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        Pin::new(&mut calls).poll_next(&mut cx),
        Poll::Ready(Some(Err(Error::Cancelled)))
    ));
    assert_eq!(host.in_flight(), 0);
}

#[test]
fn cancelling_parked_uncooperative_call_unblocks_pending_restart() {
    let host = host();
    let mut calls = FuturesUnordered::new();
    calls.push(host.handler(WAIT).unwrap().call(1));
    let signal = Arc::new(Signal::default());
    let waker = Waker::from(signal.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut calls).poll_next(&mut cx).is_pending());
    let mut restart = unsafe { host.restart_api(handler_api()) }.unwrap();
    assert!(Pin::new(&mut restart).poll(&mut cx).is_pending());
    // Consume the initial cooperative stop wake before explicit cancellation.
    assert!(Pin::new(&mut calls).poll_next(&mut cx).is_pending());
    signal.0.store(0, Ordering::SeqCst);
    calls.iter_mut().next().unwrap().cancel();
    assert!(signal.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        Pin::new(&mut calls).poll_next(&mut cx),
        Poll::Ready(Some(Err(Error::Cancelled)))
    ));
    assert!(matches!(
        Pin::new(&mut restart).poll(&mut cx),
        Poll::Ready(Ok(2))
    ));
    assert!(host.is_accepting());
    assert_eq!(host.in_flight(), 0);
}

#[test]
fn cancellation_wakes_latest_executor_once_and_still_waits_for_child() {
    JOIN_READY.with(|ready| ready.set(false));
    let host = host();
    let mut call = host.handler(WAIT).unwrap().call(2);
    let old = Arc::new(Signal::default());
    let current = Arc::new(Signal::default());
    let waker = Waker::from(current.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut call)
        .poll(&mut Context::from_waker(&Waker::from(old.clone())))
        .is_pending());
    assert!(Pin::new(&mut call).poll(&mut cx).is_pending());
    old.0.store(0, Ordering::SeqCst);
    current.0.store(0, Ordering::SeqCst);
    call.cancel();
    assert_eq!(old.0.load(Ordering::SeqCst), 0);
    assert_eq!(current.0.load(Ordering::SeqCst), 1);
    assert!(Pin::new(&mut call).poll(&mut cx).is_pending());
    assert_eq!(host.in_flight(), 1);
    current.0.store(0, Ordering::SeqCst);
    for _ in 0..3 {
        call.cancel();
    }
    assert_eq!(
        current.0.load(Ordering::SeqCst),
        0,
        "repeated cancellation must not spin the driver"
    );
    JOIN_READY.with(|ready| ready.set(true));
    JOIN_WAKE.with(|slot| slot.borrow_mut().take().unwrap().wake());
    assert!(current.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        Pin::new(&mut call).poll(&mut cx),
        Poll::Ready(Err(Error::Cancelled))
    ));
    assert_eq!(host.in_flight(), 0);
}

#[test]
fn cancelling_before_first_poll_needs_no_registered_waker() {
    let host = host();
    let mut call = host.handler(WAIT).unwrap().call(1);
    call.cancel();
    call.cancel();
    assert!(matches!(
        Pin::new(&mut call).poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(Error::Cancelled))
    ));
    assert_eq!(host.in_flight(), 0);
}
