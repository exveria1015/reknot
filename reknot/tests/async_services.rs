use reknot::*;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

const KEY: ServiceKey = ServiceKey::new(0x4153_594e, 1);
#[derive(Default)]
struct Probe {
    ready: Cell<bool>,
    cancelled: Cell<bool>,
    starts: Cell<usize>,
    drops: Cell<usize>,
    replies: RefCell<Vec<Vec<u8>>>,
}
struct Session(Rc<Probe>);
struct Io {
    probe: Rc<Probe>,
    input: Vec<u8>,
    cancel: CancellationToken,
}
impl Future for Io {
    type Output = Result<Vec<u8>, ServiceError>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.probe.cancelled.set(self.cancel.is_cancelled());
        if self.probe.ready.get() {
            Poll::Ready(Ok(self.input.clone()))
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Io {
    fn drop(&mut self) {
        self.probe.drops.set(self.probe.drops.get() + 1);
    }
}
impl HostResource for Session {
    fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0.replies.borrow_mut().push(input.to_vec());
        Ok(0)
    }
    fn start_async(
        &mut self,
        _: u32,
        input: &[u8],
        cancel: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        self.0.starts.set(self.0.starts.get() + 1);
        Ok(Box::pin(Io {
            probe: self.0.clone(),
            input: input.to_vec(),
            cancel,
        }))
    }
}
#[reload_handler(KEY)]
async fn handler(ctx: RequestContext) -> Result<(), ServiceError> {
    let call = ctx.call_async(0, ctx.input(), 16)?;
    if ctx.input() == b"drop" {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if std::mem::replace(&mut yielded, true) {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
        drop(call);
    } else {
        let reply = call.await?;
        ctx.call(1, &reply, &mut [])?;
    }
    Ok(())
}
fn setup(limits: AsyncLimits) -> (RequestRouter, HostResources, Resource<Session>, Rc<Probe>) {
    let probe = Rc::new(Probe::default());
    let resources = HostResources::with_async_limits(limits).unwrap();
    let session = resources.register(KEY, Session(probe.clone()));
    let router = unsafe { RequestRouter::from_api(*hot_handler_api(), resources.clone()) }.unwrap();
    (router, resources, session, probe)
}
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
}
fn cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}

#[test]
fn host_waits_and_copies_input_and_destroys_io_before_request_completion() {
    let (router, _, session, probe) = setup(AsyncLimits::default());
    let mut input = b"hello".to_vec();
    let mut task = router.dispatch(session.id(), &input).unwrap();
    input.fill(0);
    assert!(poll(&mut task).is_pending());
    assert_eq!(probe.starts.get(), 1);
    assert_eq!(probe.drops.get(), 0);
    probe.ready.set(true);
    assert!(poll(&mut task).is_pending()); // result wakes root; next poll consumes it
    assert_eq!(probe.drops.get(), 1);
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Done));
    assert_eq!(*probe.replies.borrow(), [b"hello".to_vec()]);
    assert_eq!(router.in_flight(), 0);
}

#[test]
fn dropped_root_and_dropped_await_handle_wait_for_real_io_acknowledgement() {
    for cancel_root in [true, false] {
        let (mut router, resources, session, probe) = setup(AsyncLimits::default());
        let mut task = router
            .dispatch(session.id(), if cancel_root { b"hello" } else { b"drop" })
            .unwrap();
        assert!(poll(&mut task).is_pending());
        if cancel_root {
            drop(task);
        } else {
            assert!(poll(&mut task).is_pending());
            assert_eq!(task.status(), None);
            drop(task);
        }
        let next = unsafe { router.prepare_api(*hot_handler_api()) }.unwrap();
        let mut restart = router
            .restart(next, |_, state, _| Ok(state.clone()))
            .unwrap();
        assert!(poll(&mut restart).is_pending());
        assert!(probe.cancelled.get());
        assert_eq!(probe.drops.get(), 0);
        drop(restart);
        assert_eq!(router.in_flight(), 1);
        drop(router);
        drop(resources);
        drop(session);
        probe.ready.set(true);
        assert!(poll_cancelled_requests(&mut cx()).is_ready());
        assert_eq!(probe.drops.get(), 1);
        assert!(probe.replies.borrow().is_empty());
    }
}

#[test]
fn revoke_cancels_but_does_not_erase_pending_operation_or_replay_result() {
    let (router, resources, session, probe) = setup(AsyncLimits::default());
    let mut task = router.dispatch(session.id(), b"hello").unwrap();
    assert!(poll(&mut task).is_pending());
    assert!(resources.revoke(session.id()));
    assert!(poll(&mut task).is_pending());
    assert!(probe.cancelled.get());
    assert_eq!(router.in_flight(), 1);
    probe.ready.set(true);
    assert!(poll(&mut task).is_pending());
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Error));
    assert_eq!(probe.drops.get(), 1);
    assert!(probe.replies.borrow().is_empty());
}

#[test]
fn admission_and_message_limits_reject_without_starting_io() {
    let (router, _, session, probe) = setup(AsyncLimits {
        max_operations: 1,
        max_input_bytes: 8,
        max_output_bytes: 16,
    });
    let mut first = router.dispatch(session.id(), b"a").unwrap();
    assert!(poll(&mut first).is_pending());
    let mut second = router.dispatch(session.id(), b"b").unwrap();
    assert!(poll(&mut second).is_pending());
    assert_eq!(poll(&mut second), Poll::Ready(RunStatus::Error));
    assert_eq!(probe.starts.get(), 1);
    probe.ready.set(true);
    assert!(poll(&mut first).is_pending());
    assert_eq!(poll(&mut first), Poll::Ready(RunStatus::Done));
    let mut long = router.dispatch(session.id(), b"123456789").unwrap();
    assert!(poll(&mut long).is_pending());
    assert_eq!(poll(&mut long), Poll::Ready(RunStatus::Error));
    assert_eq!(probe.starts.get(), 1);
}

#[test]
fn raw_abi_retains_completed_reply_for_small_buffer_and_rejects_stale_ids() {
    let (_, resources, session, probe) = setup(AsyncLimits::default());
    probe.ready.set(true);
    let services = resources.services();
    let raw = services.as_raw();
    let waker = HostWaker {
        data: std::ptr::null_mut(),
        vtable: std::ptr::null(),
    };
    unsafe {
        let start =
            ((*raw.vtable).start_async)(raw.data, session.id(), KEY, 0, b"abc".as_ptr(), 3, 16);
        assert_eq!(start.status, 0);
        let mut out = [0; 3];
        let result = ((*raw.vtable).poll_async)(raw.data, start.id, &waker, out.as_mut_ptr(), 1);
        assert_eq!(
            (result.ready, result.status, result.written),
            (1, ServiceError::BUFFER_TOO_SMALL.0, 3)
        );
        let result = ((*raw.vtable).poll_async)(raw.data, start.id, &waker, out.as_mut_ptr(), 3);
        assert_eq!((result.ready, result.status), (1, 0));
        assert_eq!(&out, b"abc");
        let other = HostResources::new().services();
        let other_raw = other.as_raw();
        let wrong =
            ((*other_raw.vtable).poll_async)(other_raw.data, start.id, &waker, out.as_mut_ptr(), 3);
        assert_eq!(wrong.status, ServiceError::REVOKED.0);
        ((*raw.vtable).release_async)(raw.data, start.id);
        let stale = ((*raw.vtable).poll_async)(raw.data, start.id, &waker, out.as_mut_ptr(), 3);
        assert_eq!(stale.status, ServiceError::REVOKED.0);
        assert_eq!(probe.starts.get(), 1);
        assert_eq!(probe.drops.get(), 1);
    }
}

#[test]
fn output_bound_and_wrong_service_contract_do_not_truncate_or_start_wrong_code() {
    let (router, resources, session, probe) = setup(AsyncLimits::default());
    probe.ready.set(true);
    let mut task = router
        .dispatch(session.id(), b"longer than sixteen bytes")
        .unwrap();
    assert!(poll(&mut task).is_pending());
    assert_eq!(poll(&mut task), Poll::Ready(RunStatus::Error));
    assert_eq!(probe.drops.get(), 1);
    let raw = resources.services();
    let raw = raw.as_raw();
    // Keep a service owner alive via resources during the short raw call.
    let bad = unsafe {
        ((*raw.vtable).start_async)(
            raw.data,
            session.id(),
            ServiceKey::new(KEY.id, 99),
            0,
            b"x".as_ptr(),
            1,
            16,
        )
    };
    assert_eq!(bad.status, ServiceError::CONTRACT_MISMATCH.0);
    assert_eq!(probe.starts.get(), 1);
}
