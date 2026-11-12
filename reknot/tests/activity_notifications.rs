//! Drive requests only when notified; periodic polling must not hide lost wakes.
use reknot::*;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

const KEY: ServiceKey = ServiceKey::new(0x4e4f_5449, 1);

#[derive(Default)]
struct Counter(AtomicUsize);
impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Event {
    ready: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}
impl Event {
    async fn wait(&self) {
        std::future::poll_fn(|cx| {
            if self.ready.get() {
                Poll::Ready(())
            } else {
                *self.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }
    fn set(&self) {
        self.ready.set(true);
        let waker = self.waker.borrow_mut().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct Session;
impl HostResource for Session {
    fn start_async(
        &mut self,
        _: u32,
        input: &[u8],
        _: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        let reply = input.to_vec();
        Ok(Box::pin(async move { Ok(reply) }))
    }
}

#[reload_handler(KEY)]
async fn nested(ctx: RequestContext) -> Result<(), ServiceError> {
    if ctx.input() == b"idle" {
        ctx.spawn_child(|stop| async move {
            stop.cancelled().await;
            Ok(())
        })?;
        ctx.cancelled().await;
        return Ok(());
    }
    let service = ctx.input() == b"service";
    let context = Rc::new(ctx);
    let parent_context = context.clone();
    let root_done = Rc::new(Event::default());
    let root_signal = root_done.clone();
    context.spawn_child(move |_| async move {
        if service {
            let reply = parent_context.call_async(0, b"reply", 16)?.await?;
            assert_eq!(reply, b"reply");
        } else {
            let child_done = Rc::new(Event::default());
            let child_signal = child_done.clone();
            parent_context.spawn_child(move |_| async move {
                child_signal.set();
                Ok(())
            })?;
            child_done.wait().await;
        }
        root_signal.set();
        Ok(())
    })?;
    root_done.wait().await;
    Ok(())
}

fn setup(input: &[u8]) -> (RequestRouter, Resource<Session>, RequestTask) {
    let resources = HostResources::new();
    let session = resources.register(KEY, Session);
    let router = unsafe { RequestRouter::from_api(*hot_handler_api(), resources) }.unwrap();
    let task = router.dispatch(session.id(), input).unwrap();
    (router, session, task)
}

fn finish_notified(task: &mut RequestTask, counter: &Arc<Counter>) {
    let waker = Waker::from(counter.clone());
    for _ in 0..8 {
        assert!(
            counter.0.swap(0, Ordering::SeqCst) > 0,
            "pending work has no notification to schedule its next poll"
        );
        if let Poll::Ready(result) = Pin::new(&mut *task).poll(&mut Context::from_waker(&waker)) {
            assert_eq!(result, RunStatus::Done);
            assert_eq!(task.activity_count(), 0);
            return;
        }
    }
    panic!("notified request did not finish within its bounded work");
}

fn nested_registration_completes(input: &[u8]) {
    let (router, _session, mut task) = setup(input);
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    assert!(Pin::new(&mut task)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    // The new child/service is deferred to the next pass, preserving fairness.
    assert_eq!(task.activity_count(), 2);
    finish_notified(&mut task, &counter);
    assert_eq!(router.in_flight(), 0);
}

#[test]
fn child_registered_by_a_child_completes_without_periodic_polling() {
    nested_registration_completes(b"child");
}

#[test]
fn async_service_started_by_a_child_completes_without_periodic_polling() {
    nested_registration_completes(b"service");
}

#[test]
fn pending_activities_without_new_registrations_do_not_self_wake() {
    let (router, _session, mut task) = setup(b"idle");
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    assert!(Pin::new(&mut task)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    assert_eq!(task.activity_count(), 1);
    drop(task);
    assert!(poll_cancelled_requests(&mut Context::from_waker(&waker)).is_ready());
    assert_eq!(router.in_flight(), 0);
}

#[test]
fn new_activity_wake_panic_preserves_registrations_for_recovery() {
    struct PanicOnce(AtomicBool);
    impl Wake for PanicOnce {
        fn wake(self: Arc<Self>) {
            assert!(
                self.0.swap(true, Ordering::SeqCst),
                "executor notification failed"
            );
        }
    }
    let (router, _session, mut task) = setup(b"child");
    let waker = Waker::from(Arc::new(PanicOnce(AtomicBool::new(false))));
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Pin::new(&mut task).poll(&mut Context::from_waker(&waker))
    }))
    .is_err());
    assert_eq!(task.activity_count(), 2);
    assert_eq!(router.in_flight(), 1);
    // The executor recovers with a fresh context; subsequent polls require wakes.
    let counter = Arc::new(Counter(AtomicUsize::new(1)));
    finish_notified(&mut task, &counter);
    assert_eq!(router.in_flight(), 0);
}
