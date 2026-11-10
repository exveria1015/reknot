//! cargo run -p reknot --example managed_children
use reknot::*;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

const SERVICE: ServiceKey = ServiceKey::new(0x4348_494c, 1);
const STATE: ServiceKey = ServiceKey::new(0x4348_5354, 1);
struct Session;
impl HostResource for Session {
    fn call(&mut self, _: u32, _: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        Ok(0)
    }
}

#[reload_handler(SERVICE, state = STATE)]
async fn worker(ctx: RequestContext) -> Result<(), ServiceError> {
    ctx.spawn_child(|stop| async move {
        println!("child started");
        stop.cancelled().await;
        // Simulate cleanup taking another owner-thread poll, without blocking it.
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
        println!("child joined");
        Ok(())
    })?;
    ctx.cancelled().await;
    ctx.save_checkpoint(STATE, &[1])?;
    println!("root body returned (request still waits for child)");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let resources = HostResources::new();
    let session = resources.register(SERVICE, Session);
    let api = unsafe { *hot_handler_api() };
    // Statically linked callbacks obey the SDK ABI and live for the process lifetime.
    let mut router = unsafe { RequestRouter::from_api(api, resources) }?;
    router.start_worker(
        WorkerId(1),
        session.id(),
        b"worker",
        Checkpoint::new(STATE, vec![0]),
    )?;
    let mut cx = Context::from_waker(Waker::noop());
    assert!(router.poll_workers(&mut cx).is_pending());
    let next = unsafe { router.prepare_api(api) }?;
    let mut restart = router.restart(next, |_, state, _| Ok(state.clone()))?;
    assert!(Pin::new(&mut restart).poll(&mut cx).is_pending());
    assert!(matches!(
        Pin::new(&mut restart).poll(&mut cx),
        Poll::Ready(Ok(2))
    ));
    drop(restart);
    println!(
        "generation {} installed after child joined",
        router.generation()
    );
    Ok(())
}
