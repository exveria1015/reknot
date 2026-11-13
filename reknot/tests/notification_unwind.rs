//! Complete a request while its host notification still owns the executor Waker.
//! Native runs isolate any C-ABI abort; Miri runs the same race directly.
use reknot::*;
use std::future::Future;
use std::pin::Pin;
#[cfg(not(miri))]
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

const KEY: ServiceKey = ServiceKey::new(0x1311_ab17, 1);
const LIMIT: Duration = Duration::from_secs(if cfg!(miri) { 60 } else { 10 });

struct Io {
    finished: Arc<AtomicBool>,
    notification: mpsc::Sender<Waker>,
}
impl HostResource for Io {
    fn start_async(
        &mut self,
        _: u32,
        _: &[u8],
        _: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        let finished = self.finished.clone();
        let mut notification = Some(self.notification.clone());
        Ok(Box::pin(std::future::poll_fn(move |cx| {
            if finished.load(Ordering::Acquire) {
                Poll::Ready(Ok(Vec::new()))
            } else {
                if let Some(sender) = notification.take() {
                    sender.send(cx.waker().clone()).unwrap();
                }
                Poll::Pending
            }
        })))
    }
}
#[reload_handler(KEY)]
async fn run(context: RequestContext) -> Result<(), ServiceError> {
    context.call_async(0, &[], 0)?.await?;
    Ok(())
}

struct Executor {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    first: AtomicBool,
    panic_on_drop: bool,
    panic_on_wake: bool,
    drops: Arc<AtomicUsize>,
}
impl Wake for Executor {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if !self.first.swap(true, Ordering::SeqCst) {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv_timeout(LIMIT).unwrap();
        }
        assert!(!self.panic_on_wake, "executor Waker notification panicked");
    }
}
impl Drop for Executor {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_on_drop, "executor Waker destructor panicked");
    }
}

fn exercise(mode: &str) {
    let (notification_tx, notification_rx) = mpsc::channel::<Waker>();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let drops = Arc::new(AtomicUsize::new(0));
    let resources = HostResources::new();
    let resource = resources.register(
        KEY,
        Io {
            finished: finished.clone(),
            notification: notification_tx,
        },
    );
    let router = unsafe { RequestRouter::from_api(*hot_handler_api(), resources) }.unwrap();
    let mut task = router.dispatch(resource.id(), &[]).unwrap();
    let executor = Waker::from(Arc::new(Executor {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        first: AtomicBool::new(false),
        panic_on_drop: mode != "normal",
        panic_on_wake: mode == "wake-drop",
        drops: drops.clone(),
    }));
    assert!(Pin::new(&mut task)
        .poll(&mut Context::from_waker(&executor))
        .is_pending());
    let notifier = std::thread::spawn(move || {
        notification_rx.recv_timeout(LIMIT).unwrap().wake();
    });
    entered_rx.recv_timeout(LIMIT).unwrap();
    // I/O is complete. Only delivery of its host notification is still in progress,
    // which the documented gate explicitly allows to race with completion/revoke.
    finished.store(true, Ordering::Release);
    drop(executor);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
    assert_eq!(
        Pin::new(&mut task).poll(&mut cx),
        Poll::Ready(RunStatus::Done)
    );
    assert_eq!(router.in_flight(), 0);
    release_tx.send(()).unwrap();
    assert!(
        notifier.join().is_ok(),
        "notification allowed executor panic to escape"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[cfg(not(miri))]
#[test]
#[ignore = "subprocess entry used by the three parent tests"]
fn child_notification_destructor() {
    exercise(&std::env::var("REKNOT_TEST_NOTIFICATION_DROP").expect("parent subprocess only"));
}

#[cfg(not(miri))]
fn check(mode: &str) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_notification_destructor",
            "--ignored",
            "--nocapture",
        ])
        .env("REKNOT_TEST_NOTIFICATION_DROP", mode)
        .env("RUST_BACKTRACE", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "notification mode={mode} exit={}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(miri)]
fn check(mode: &str) {
    exercise(mode);
}

#[test]
fn notification_completion_race_without_destructor_panic_succeeds() {
    check("normal");
}

#[test]
fn notification_contains_executor_destructor_panic_at_c_boundary() {
    check("panic");
}

#[test]
fn notification_contains_both_wake_and_destructor_panics_at_c_boundary() {
    check("wake-drop");
}
