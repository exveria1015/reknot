use super::*;
use std::sync::atomic::AtomicUsize;
use std::sync::Barrier;
use std::task::Wake;

#[derive(Default)]
struct Counter(AtomicUsize);
impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn counter() -> (Arc<Counter>, Waker) {
    let count = Arc::new(Counter::default());
    let waker = Waker::from(count.clone());
    (count, waker)
}

#[test]
fn cancellation_wakes_all_waiters_on_same_token_and_clones() {
    let token = CancellationToken::default();
    let other = token.clone();
    let mut first = Box::pin(token.cancelled());
    let mut second = Box::pin(token.cancelled());
    let mut third = Box::pin(other.cancelled());
    let (a, wa) = counter();
    let (b, wb) = counter();
    let (c, wc) = counter();
    assert!(first
        .as_mut()
        .poll(&mut Context::from_waker(&wa))
        .is_pending());
    assert!(second
        .as_mut()
        .poll(&mut Context::from_waker(&wb))
        .is_pending());
    assert!(third
        .as_mut()
        .poll(&mut Context::from_waker(&wc))
        .is_pending());
    token.cancel();
    for count in [&a, &b, &c] {
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
    }
    for waiter in [&mut first, &mut second, &mut third] {
        assert!(waiter
            .as_mut()
            .poll(&mut Context::from_waker(&wa))
            .is_ready());
    }
    token.cancel();
    assert_eq!(a.0.load(Ordering::SeqCst), 1);
    assert_eq!(b.0.load(Ordering::SeqCst), 1);
    assert_eq!(c.0.load(Ordering::SeqCst), 1);
    assert!(token.0.waiters.lock().unwrap().is_empty());
}

#[test]
fn repoll_updates_only_its_waiter_and_drop_releases_its_waker() {
    let token = CancellationToken::default();
    let mut first = Box::pin(token.cancelled());
    let mut second = Box::pin(token.cancelled());
    let (old, old_waker) = counter();
    let (new, new_waker) = counter();
    let (other, other_waker) = counter();
    assert!(first
        .as_mut()
        .poll(&mut Context::from_waker(&old_waker))
        .is_pending());
    assert!(second
        .as_mut()
        .poll(&mut Context::from_waker(&other_waker))
        .is_pending());
    assert!(first
        .as_mut()
        .poll(&mut Context::from_waker(&new_waker))
        .is_pending());
    assert_eq!(Arc::strong_count(&old), 2);
    drop(first);
    assert_eq!(Arc::strong_count(&new), 2);
    // Reuse a dropped registration while a later slot remains occupied.
    for _ in 0..16 {
        let mut temporary = Box::pin(token.cancelled());
        assert!(temporary
            .as_mut()
            .poll(&mut Context::from_waker(&old_waker))
            .is_pending());
        assert_eq!(token.0.waiters.lock().unwrap().len(), 2);
    }
    let mut replacement = Box::pin(token.cancelled());
    assert!(replacement
        .as_mut()
        .poll(&mut Context::from_waker(&new_waker))
        .is_pending());
    token.cancel();
    assert_eq!(old.0.load(Ordering::SeqCst), 0);
    assert_eq!(new.0.load(Ordering::SeqCst), 1);
    assert_eq!(other.0.load(Ordering::SeqCst), 1);
    assert!(replacement
        .as_mut()
        .poll(&mut Context::from_waker(&new_waker))
        .is_ready());
    assert!(second
        .as_mut()
        .poll(&mut Context::from_waker(&other_waker))
        .is_ready());
    assert_eq!(Arc::strong_count(&new), 2);
    assert_eq!(Arc::strong_count(&other), 2);
}

#[test]
fn dropping_one_waiter_with_a_shared_waker_keeps_the_other_registered() {
    let token = CancellationToken::default();
    let mut first = Box::pin(token.cancelled());
    let mut second = Box::pin(token.cancelled());
    let (count, waker) = counter();
    assert!(first
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    assert!(second
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    drop(first);
    token.cancel();
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(second
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_ready());
}

#[test]
fn unpolled_dropped_and_already_cancelled_waits_retain_no_registration() {
    let token = CancellationToken::default();
    let (count, waker) = counter();
    drop(token.cancelled());
    assert!(token.0.waiters.lock().unwrap().is_empty());
    let mut wait = Box::pin(token.cancelled());
    assert!(wait
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    drop(wait);
    assert!(token.0.waiters.lock().unwrap().is_empty());
    assert_eq!(Arc::strong_count(&count), 2);
    token.cancel();
    let mut late = Box::pin(token.cancelled());
    assert!(late
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_ready());
    assert_eq!(count.0.load(Ordering::SeqCst), 0);
    assert!(token.0.waiters.lock().unwrap().is_empty());
    assert_eq!(Arc::strong_count(&count), 2);
}

#[test]
fn cancellation_notifies_other_waiters_after_a_reentrant_wake_panics() {
    struct ReentrantPanic(CancellationToken);
    impl Wake for ReentrantPanic {
        fn wake(self: Arc<Self>) {
            self.0.cancel();
            panic!("executor wake panicked");
        }
    }
    let token = CancellationToken::default();
    let mut bad = Box::pin(token.cancelled());
    let mut good = Box::pin(token.cancelled());
    let bad_waker = Waker::from(Arc::new(ReentrantPanic(token.clone())));
    let (count, waker) = counter();
    assert!(bad
        .as_mut()
        .poll(&mut Context::from_waker(&bad_waker))
        .is_pending());
    assert!(good
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    assert!(std::panic::catch_unwind(|| token.cancel()).is_err());
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(good
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_ready());
    assert!(bad
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_ready());
    assert!(token.0.waiters.lock().unwrap().is_empty());
}

#[test]
fn concurrent_registration_drop_and_cancellation_do_not_lose_wakes() {
    for _ in 0..if cfg!(miri) { 8 } else { 64 } {
        let token = CancellationToken::default();
        let race = Barrier::new(3);
        let cancelled = Barrier::new(3);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let token = &token;
                let race = &race;
                let cancelled = &cancelled;
                scope.spawn(move || {
                    let (count, waker) = counter();
                    let mut wait = Box::pin(token.cancelled());
                    let mut dropped = Box::pin(token.cancelled());
                    let _ = dropped
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()));
                    race.wait();
                    drop(dropped);
                    let pending = wait
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending();
                    cancelled.wait();
                    if pending {
                        assert!(count.0.load(Ordering::SeqCst) > 0);
                        assert!(wait
                            .as_mut()
                            .poll(&mut Context::from_waker(&waker))
                            .is_ready());
                    }
                });
            }
            race.wait();
            token.cancel();
            cancelled.wait();
        });
        assert!(token.0.waiters.lock().unwrap().is_empty());
    }
}
