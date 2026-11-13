use futures_util::{stream::FuturesUnordered, StreamExt};
use reknot::*;
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::task::{Context, Poll, Wake, Waker};

pub const KEY: ServiceKey = ServiceKey::new(0x5245_5052, 1);
pub const STATE: ServiceKey = ServiceKey::new(0x5245_5053, 1);

struct Counter(AtomicUsize);
impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[reload_handler(KEY, state = STATE)]
async fn worker(ctx: RequestContext) -> Result<(), ServiceError> {
    match ctx.input() {
        b"request-one" | b"request-many" | b"activity-failure" => {
            if ctx.input() == b"activity-failure" {
                ctx.spawn_child(|_| async { Err(ServiceError::FAILED) })?;
            }
            let mut waits = FuturesUnordered::new();
            let count = if ctx.input() == b"request-many" {
                128
            } else {
                1
            };
            for _ in 0..count {
                waits.push(ctx.cancelled());
            }
            while waits.next().await.is_some() {}
        }
        b"child-one" | b"child-many" | b"full-capacity" => {
            let children = if ctx.input() == b"full-capacity" {
                MAX_MANAGED_ACTIVITIES
            } else {
                1
            };
            let waits = if ctx.input() == b"child-one" { 1 } else { 2 };
            for _ in 0..children {
                ctx.spawn_child(move |stop| async move {
                    let other = stop.clone();
                    let mut pending = FuturesUnordered::new();
                    pending.push(stop.cancelled());
                    if waits == 2 {
                        pending.push(other.cancelled());
                    }
                    while pending.next().await.is_some() {}
                    Ok(())
                })?;
            }
            let mut pending = FuturesUnordered::new();
            pending.push(ctx.cancelled());
            while pending.next().await.is_some() {}
        }
        b"child-context" | b"drop-root" => {
            let drop_root = ctx.input() == b"drop-root";
            let context = Rc::new(ctx);
            let child_context = context.clone();
            let started = Rc::new(Cell::new(false));
            let child_started = started.clone();
            context.spawn_child(move |_| async move {
                child_started.set(true);
                let mut pending = FuturesUnordered::new();
                pending.push(child_context.cancelled());
                while pending.next().await.is_some() {}
                Ok(())
            })?;
            if drop_root {
                std::future::pending::<()>().await;
            }
            // Yield once so the child's nested cancellation wait is registered
            // before the root returns and Control::close() requests cleanup.
            std::future::poll_fn(|cx| {
                if started.get() {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
        }
        b"registration-lifetime" => {
            let old = Arc::new(Counter(AtomicUsize::new(0)));
            let new = Arc::new(Counter(AtomicUsize::new(0)));
            let old_waker = Waker::from(old.clone());
            let new_waker = Waker::from(new.clone());
            let mut wait = Box::pin(ctx.cancelled());
            assert!(wait
                .as_mut()
                .poll(&mut Context::from_waker(&old_waker))
                .is_pending());
            assert!(wait
                .as_mut()
                .poll(&mut Context::from_waker(&new_waker))
                .is_pending());
            assert_eq!(Arc::strong_count(&old), 2);
            for _ in 0..128 {
                let mut dropped = Box::pin(ctx.cancelled());
                assert!(dropped
                    .as_mut()
                    .poll(&mut Context::from_waker(&old_waker))
                    .is_pending());
                drop(dropped);
                assert_eq!(Arc::strong_count(&old), 2);
            }
            ctx.cancelled().await;
            assert_eq!(old.0.load(Ordering::SeqCst), 0);
            assert_eq!(new.0.load(Ordering::SeqCst), 1);
            assert!(Pin::new(&mut wait)
                .poll(&mut Context::from_waker(&new_waker))
                .is_ready());
            assert_eq!(Arc::strong_count(&new), 2);
            // Already stopped waits do not retain a notification registration.
            assert!(Box::pin(ctx.cancelled())
                .as_mut()
                .poll(&mut Context::from_waker(&new_waker))
                .is_ready());
            assert_eq!(Arc::strong_count(&new), 2);
        }
        b"panic-wake" => {
            struct PanicWake;
            impl Wake for PanicWake {
                fn wake(self: Arc<Self>) {
                    panic!("nested executor wake");
                }
            }
            let waker = Waker::from(Arc::new(PanicWake));
            let mut bad = Box::pin(ctx.cancelled());
            assert!(bad
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
            let mut good = FuturesUnordered::new();
            good.push(ctx.cancelled());
            while good.next().await.is_some() {}
            assert!(bad
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_ready());
        }
        _ => ctx.cancelled().await,
    }
    Ok(())
}
