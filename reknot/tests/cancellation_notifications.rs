//! Regression tests must use selective Future polling, not only root polling.
#[path = "fixtures/cancellation_logic.rs"]
mod logic;
use logic::{KEY, STATE};
use reknot::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll, Wake, Waker};

#[derive(Default)]
struct Notified(AtomicBool);
impl Wake for Notified {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}
struct Session;
impl HostResource for Session {}
fn setup() -> (RequestRouter, Resource<Session>) {
    let resources = HostResources::new();
    let session = resources.register(KEY, Session);
    let router = unsafe { RequestRouter::from_api(logic::handler_api(), resources) }.unwrap();
    (router, session)
}
fn next_poll(notified: &Notified) {
    assert!(
        notified.0.swap(false, Ordering::SeqCst),
        "unfinished work has no wake to schedule its next poll"
    );
}
fn stops(mode: &[u8]) {
    let (mut router, session) = setup();
    let id = WorkerId(1);
    router
        .start_worker(id, session.id(), mode, Checkpoint::new(STATE, vec![]))
        .unwrap();
    let notified = Arc::new(Notified::default());
    let waker = Waker::from(notified.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(router.poll_workers(&mut cx).is_pending());
    notified.0.store(false, Ordering::SeqCst);
    assert!(router.stop_worker(id));
    for _ in 0..8 {
        next_poll(&notified);
        if router.poll_workers(&mut cx).is_ready() {
            break;
        }
    }
    assert_eq!(router.worker_status(id), Some(RunStatus::Done), "{mode:?}");
    assert_eq!(router.in_flight(), 0);
}

#[test]
fn composed_request_and_child_waits_finish_only_from_notifications() {
    for mode in [
        b"direct".as_slice(),
        b"request-one",
        b"request-many",
        b"child-one",
        b"child-many",
        b"full-capacity",
    ] {
        stops(mode);
    }
}
#[test]
fn request_waits_replace_and_release_only_their_own_wakers() {
    stops(b"registration-lifetime");
}
#[test]
fn panicking_nested_waker_does_not_cross_ffi_or_suppress_other_waiters() {
    stops(b"panic-wake");
}

#[test]
fn composed_cancellation_allows_restart_to_finish_and_reopen_admission() {
    for mode in [b"request-one".as_slice(), b"request-many", b"child-many"] {
        let (mut router, session) = setup();
        router
            .start_worker(
                WorkerId(1),
                session.id(),
                mode,
                Checkpoint::new(STATE, vec![]),
            )
            .unwrap();
        let notified = Arc::new(Notified::default());
        let waker = Waker::from(notified.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(router.poll_workers(&mut cx).is_pending());
        let candidate = unsafe { router.prepare_api(logic::handler_api()) }.unwrap();
        let mut restart = router
            .restart(candidate, |_, state, _| Ok(state.clone()))
            .unwrap();
        let mut complete = false;
        for _ in 0..8 {
            next_poll(&notified);
            if let Poll::Ready(result) = Pin::new(&mut restart).poll(&mut cx) {
                assert_eq!(result.unwrap(), 2);
                complete = true;
                break;
            }
        }
        drop(restart);
        assert!(complete, "{mode:?}");
        assert!(router.is_accepting());
    }
}

#[test]
fn root_completion_and_drop_notify_context_waits_owned_by_children() {
    for abandon in [false, true] {
        let (router, session) = setup();
        let mut task = router
            .dispatch(
                session.id(),
                if abandon {
                    b"drop-root"
                } else {
                    b"child-context"
                },
            )
            .unwrap();
        let notified = Arc::new(Notified::default());
        let waker = Waker::from(notified.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
        if abandon {
            drop(task);
            // First cleanup poll is mandatory after dropping a request.
            assert!(poll_cancelled_requests(&mut cx).is_ready());
        } else {
            next_poll(&notified);
            assert_eq!(
                Pin::new(&mut task).poll(&mut cx),
                Poll::Ready(RunStatus::Done)
            );
        }
        assert_eq!(router.in_flight(), 0);
    }
}

#[test]
fn activity_failure_notifies_nested_root_cancellation() {
    let (router, session) = setup();
    let mut task = router.dispatch(session.id(), b"activity-failure").unwrap();
    let notified = Arc::new(Notified::default());
    let waker = Waker::from(notified.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
    next_poll(&notified);
    assert_eq!(
        Pin::new(&mut task).poll(&mut cx),
        Poll::Ready(RunStatus::Error)
    );
    assert_eq!(router.in_flight(), 0);
}
