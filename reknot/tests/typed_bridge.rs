#![cfg(feature = "typed")]
use reknot::typed::{BridgeOptions, Error, Handler, Module, ModuleHost};
use reknot::RequestContext;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

const RUN: Handler<u64, (u64, u64)> = Handler::new("bridge.run", 1);
thread_local! {
    static CALLS: Cell<usize> = const { Cell::new(0) };
    static GATE: RefCell<Arc<AtomicBool>> = RefCell::new(Arc::new(AtomicBool::new(false)));
}
fn module() -> Module {
    Module::new("bridge.test", 1).with(RUN)
}
#[reknot::handler(RUN)]
async fn run(context: RequestContext, input: u64) -> (u64, u64) {
    CALLS.with(|calls| calls.set(calls.get() + 1));
    if input == 99 {
        let gate = GATE.with(|gate| gate.borrow().clone());
        context
            .spawn_child(move |stop| async move {
                stop.cancelled().await;
                std::future::poll_fn(|cx| {
                    if gate.load(Ordering::Acquire) {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                Ok(())
            })
            .unwrap();
        std::future::pending::<()>().await;
    }
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
    (input, context.generation())
}
reknot::export_handlers!(module(), [run]);
fn host() -> ModuleHost {
    CALLS.with(|calls| calls.set(0));
    GATE.with(|gate| *gate.borrow_mut() = Arc::new(AtomicBool::new(false)));
    unsafe { module().builder().from_api(handler_api()) }.unwrap()
}
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
}
fn ready<F: Future + Unpin>(mut future: F) -> F::Output {
    match poll(&mut future) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("expected ready"),
    }
}
fn options(capacity: usize, max_in_flight: usize) -> BridgeOptions {
    BridgeOptions {
        capacity,
        max_in_flight,
    }
}

#[test]
fn admission_pins_on_owner_fifo_and_completed_replies_keep_capacity() {
    let host = host();
    let (client, mut driver) = host.bridge(options(2, 1)).unwrap();
    let handle = client.handler(RUN).unwrap();
    let first = handle.call(1);
    let second = handle.call(2);
    assert_eq!(first.generation(), None);
    assert!(matches!(ready(handle.call(3)), Err(Error::Busy)));
    assert!(poll(&mut driver).is_pending());
    assert_eq!(first.generation(), Some(1));
    assert_eq!(second.generation(), None);
    unsafe { host.reload_api(handler_api()) }.unwrap();
    assert!(poll(&mut driver).is_pending());
    assert_eq!(client.outstanding(), 2);
    assert!(matches!(ready(handle.call(3)), Err(Error::Busy)));
    assert_eq!(ready(first).unwrap(), (1, 1));
    assert!(poll(&mut driver).is_pending());
    assert_eq!(second.generation(), Some(2));
    assert!(poll(&mut driver).is_pending());
    assert_eq!(ready(second).unwrap(), (2, 2));
    assert_eq!(client.outstanding(), 0);
    let closed = client.close();
    assert!(poll(&mut driver).is_ready());
    ready(closed).unwrap();
    assert!(matches!(ready(handle.call(4)), Err(Error::BridgeClosed)));
}

#[test]
fn cancellation_retains_running_credit_until_child_join_and_skips_queued_work() {
    let host = host();
    let (client, mut driver) = host.bridge(options(2, 1)).unwrap();
    let handle = client.handler(RUN).unwrap();
    let running = handle.call(99);
    assert!(poll(&mut driver).is_pending());
    let queued = handle.call(1);
    drop(queued);
    assert!(poll(&mut driver).is_pending());
    assert_eq!(
        client.outstanding(),
        1,
        "queued cancellation must not wait behind a full running slot"
    );
    assert_eq!(CALLS.with(Cell::get), 1);
    drop(running);
    assert!(poll(&mut driver).is_pending());
    assert_eq!(host.in_flight(), 1);
    assert_eq!(
        client.outstanding(),
        1,
        "dropped caller is not proof of joined cleanup"
    );
    let queued = handle.call(2);
    let mut shutdown1 = client.shutdown();
    let mut shutdown2 = client.shutdown();
    let w1 = Arc::new(CountWake(AtomicUsize::new(0)));
    let w2 = Arc::new(CountWake(AtomicUsize::new(0)));
    assert!(Pin::new(&mut shutdown1)
        .poll(&mut Context::from_waker(&Waker::from(w1.clone())))
        .is_pending());
    assert!(Pin::new(&mut shutdown2)
        .poll(&mut Context::from_waker(&Waker::from(w2.clone())))
        .is_pending());
    assert!(poll(&mut driver).is_pending());
    assert!(matches!(ready(queued), Err(Error::Cancelled)));
    assert_eq!(CALLS.with(Cell::get), 1);
    GATE.with(|gate| gate.borrow().store(true, Ordering::Release));
    assert!(poll(&mut driver).is_ready());
    assert_eq!(client.outstanding(), 0);
    assert_eq!(host.in_flight(), 0);
    assert!(w1.0.load(Ordering::Acquire) > 0);
    assert!(w2.0.load(Ordering::Acquire) > 0);
    ready(shutdown1).unwrap();
    ready(shutdown2).unwrap();
}

#[test]
fn explicit_cancel_can_be_awaited_and_last_client_drains_instead_of_replaying() {
    let host = host();
    let (client, mut driver) = host.bridge(options(2, 1)).unwrap();
    let handle = client.handler(RUN).unwrap();
    let cancelled = handle.call(1);
    cancelled.cancel();
    let accepted = handle.call(2);
    drop(handle);
    drop(client);
    assert!(poll(&mut driver).is_pending());
    assert!(matches!(ready(cancelled), Err(Error::Cancelled)));
    assert!(poll(&mut driver).is_ready());
    assert_eq!(ready(accepted).unwrap(), (2, 1));
    assert_eq!(CALLS.with(Cell::get), 1);
}

#[test]
fn driver_drop_reports_abandonment_and_cleanup_can_still_be_driven_locally() {
    let host = host();
    let (client, mut driver) = host.bridge(options(2, 1)).unwrap();
    let handle = client.handler(RUN).unwrap();
    let running = handle.call(99);
    assert!(poll(&mut driver).is_pending());
    let queued = handle.call(1);
    let shutdown = client.shutdown();
    drop(driver);
    assert!(matches!(ready(running), Err(Error::BridgeClosed)));
    assert!(matches!(ready(queued), Err(Error::BridgeClosed)));
    assert!(matches!(ready(shutdown), Err(Error::BridgeClosed)));
    assert_eq!(host.in_flight(), 1);
    GATE.with(|gate| gate.borrow().store(true, Ordering::Release));
    assert!(host
        .poll_cleanup(&mut Context::from_waker(Waker::noop()))
        .is_ready());
    assert_eq!(host.in_flight(), 0);
}

struct CountWake(AtomicUsize);
impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Release);
    }
}
struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}
#[test]
fn framework_thread_waits_on_send_future_while_only_owner_executes_logic() {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    send_sync::<reknot::typed::ModuleClient>();
    send_sync::<reknot::typed::HandlerClient<u64, (u64, u64)>>();
    send::<reknot::typed::RemoteCall<(u64, u64)>>();
    send::<reknot::typed::BridgeShutdown>();
    let host = host();
    let (client, driver) = host.bridge(options(8, 2)).unwrap();
    let worker = std::thread::spawn(move || {
        let handle = client.handler(RUN).unwrap();
        for input in 0..4 {
            assert_eq!(block_on(handle.call(input)).unwrap(), (input, 1));
        }
        assert_eq!(
            CALLS.with(Cell::get),
            0,
            "DLL/handler bodies must not run on framework threads"
        );
        block_on(client.close()).unwrap();
    });
    block_on(driver);
    worker.join().unwrap();
    assert_eq!(CALLS.with(Cell::get), 4);
}

#[test]
fn invalid_limits_wrong_contract_and_local_capacity_are_explicit_errors() {
    let host = host();
    for limits in [options(0, 0), options(2, 0), options(1, 2)] {
        assert!(host.bridge(limits).is_err());
    }
    let (client, driver) = host.bridge(options(1, 1)).unwrap();
    assert!(client
        .handler(Handler::<String, String>::new("bridge.run", 1))
        .is_err());
    drop(driver);
    assert!(client.is_closed());
    let host = unsafe { module().builder().max_in_flight(1).from_api(handler_api()) }.unwrap();
    let local = host.handler(RUN).unwrap().call(1);
    let (client, mut driver) = host.bridge(options(2, 1)).unwrap();
    let remote = client.handler(RUN).unwrap().call(2);
    assert!(poll(&mut driver).is_pending());
    assert!(matches!(ready(remote), Err(Error::Busy)));
    assert_eq!(CALLS.with(Cell::get), 0);
    drop(local);
}

#[test]
fn response_waker_panic_does_not_leave_a_consumed_call_in_the_driver() {
    struct PanicWake;
    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("executor wake failed")
        }
    }
    let host = host();
    let (client, mut driver) = host.bridge(options(2, 1)).unwrap();
    let handle = client.handler(RUN).unwrap();
    let mut call = handle.call(1);
    let waker = Waker::from(Arc::new(PanicWake));
    assert!(Pin::new(&mut call)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    assert!(poll(&mut driver).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll(&mut driver))).is_err());
    assert_eq!(ready(call).unwrap(), (1, 1));
    let next = handle.call(2);
    assert!(poll(&mut driver).is_pending());
    assert!(poll(&mut driver).is_pending());
    assert_eq!(ready(next).unwrap(), (2, 1));
}

#[test]
fn response_waker_panic_still_notifies_other_results_and_shutdown() {
    struct PanicWake;
    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("response notification failed");
        }
    }
    for abandon in [false, true] {
        let host = host();
        let (client, mut driver) = host.bridge(options(2, 2)).unwrap();
        let handle = client.handler(RUN).unwrap();
        let mut first = handle.call(1);
        let mut second = handle.call(2);
        let bad = Waker::from(Arc::new(PanicWake));
        let results = Arc::new(CountWake(AtomicUsize::new(0)));
        let good = Waker::from(results.clone());
        assert!(Pin::new(&mut first)
            .poll(&mut Context::from_waker(&bad))
            .is_pending());
        assert!(Pin::new(&mut second)
            .poll(&mut Context::from_waker(&good))
            .is_pending());
        let mut shutdown = client.close();
        let exits = Arc::new(CountWake(AtomicUsize::new(0)));
        let exit = Waker::from(exits.clone());
        assert!(Pin::new(&mut shutdown)
            .poll(&mut Context::from_waker(&exit))
            .is_pending());
        if abandon {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(driver))).is_err()
            );
            assert!(matches!(ready(first), Err(Error::BridgeClosed)));
            assert!(matches!(ready(second), Err(Error::BridgeClosed)));
            assert!(matches!(ready(shutdown), Err(Error::BridgeClosed)));
        } else {
            assert!(poll(&mut driver).is_pending());
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll(&mut driver)))
                    .is_err()
            );
            assert_eq!(ready(first).unwrap(), (1, 1));
            assert_eq!(ready(second).unwrap(), (2, 1));
            assert!(ready(shutdown).is_ok());
        }
        assert_eq!(results.0.load(Ordering::Acquire), 1);
        assert_eq!(exits.0.load(Ordering::Acquire), 1);
        assert_eq!(host.in_flight(), 0);
    }
}

#[test]
fn driver_scheduling_panic_does_not_hide_a_completed_reply() {
    struct PanicWake;
    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("owner scheduling failed");
        }
    }
    let host = host();
    let (client, mut driver) = host.bridge(options(2, 1)).unwrap();
    let handle = client.handler(RUN).unwrap();
    let mut first = handle.call(1);
    let second = handle.call(2);
    let count = Arc::new(CountWake(AtomicUsize::new(0)));
    let result_waker = Waker::from(count.clone());
    assert!(Pin::new(&mut first)
        .poll(&mut Context::from_waker(&result_waker))
        .is_pending());
    assert!(poll(&mut driver).is_pending());
    let bad = Waker::from(Arc::new(PanicWake));
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Pin::new(&mut driver).poll(&mut Context::from_waker(&bad))
    }))
    .is_err());
    assert_eq!(count.0.load(Ordering::Acquire), 1);
    assert_eq!(ready(first).unwrap(), (1, 1));
    assert!(poll(&mut driver).is_pending());
    assert!(poll(&mut driver).is_pending());
    assert_eq!(ready(second).unwrap(), (2, 1));
}
