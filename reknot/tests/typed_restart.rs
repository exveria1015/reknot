#![cfg(feature = "typed")]
use reknot::typed::{BridgeOptions, Error, Handler, Module, ModuleHost, WorkerState};
use reknot::{HandlerApi, RequestContext, WorkerId};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

const WORK: Handler<u8, Result<(), String>> = Handler::new("restart.worker", 1);
const REQUEST: Handler<String, u64> = Handler::new("restart.request", 1);
const OLD: WorkerState<u64> = WorkerState::new("restart.counter", 1);
const NEW: WorkerState<Counter> = WorkerState::new("restart.counter", 2);
#[reknot::schema]
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Counter {
    total: u64,
}
thread_local! { static STARTS: RefCell<Vec<(u64, u64)>> = const { RefCell::new(Vec::new()) }; }
thread_local! { static DROP_RESTART: RefCell<Option<reknot::typed::ModuleRestart>> = const { RefCell::new(None) }; }
fn module() -> Module {
    Module::new("restart-test", 1)
        .with(WORK)
        .with(REQUEST)
        .with_state(OLD)
}
async fn yield_once() {
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
}
#[reknot::handler(WORK)]
async fn worker(context: RequestContext, behavior: u8) -> Result<(), String> {
    if behavior == 1 {
        return Err("worker refused its configuration".into());
    }
    if behavior == 3 {
        DROP_RESTART.with(|slot| drop(slot.borrow_mut().take()));
    }
    let old = context.checkpoint().unwrap().schema.version == 1;
    let initial = if old {
        OLD.restore(&context).map_err(|e| e.to_string())?
    } else {
        NEW.restore(&context).map_err(|e| e.to_string())?.total
    };
    STARTS.with(|log| log.borrow_mut().push((context.generation(), initial)));
    let save = |value| {
        if old {
            OLD.save(&context, &value)
        } else {
            NEW.save(&context, &Counter { total: value })
        }
    };
    save(initial + 1).map_err(|e| e.to_string())?;
    context
        .spawn_child(|stop| async move {
            stop.cancelled().await;
            yield_once().await;
            Ok(())
        })
        .unwrap();
    if behavior == 2 {
        std::future::pending::<()>().await;
    }
    context.cancelled().await;
    save(initial + 2).map_err(|e| e.to_string())?;
    Ok(())
}
#[reknot::handler(REQUEST)]
async fn request(context: RequestContext, input: String) -> u64 {
    if input == "wait" {
        context.cancelled().await;
    }
    context.generation()
}
reknot::export_handlers!(module(), [worker, request]);
fn new_api() -> HandlerApi {
    unsafe extern "C" fn validate(bytes: *const u8, len: usize) -> u32 {
        reknot::typed::__private::validate(bytes, len, || {
            (
                module().with_state(NEW),
                vec![WORK.descriptor(), REQUEST.descriptor()],
            )
        })
    }
    unsafe extern "C" fn describe(bytes: *mut u8, len: usize) -> usize {
        reknot::typed::__private::describe(bytes, len, || {
            (
                module().with_state(NEW),
                vec![WORK.descriptor(), REQUEST.descriptor()],
            )
        })
    }
    let mut api = handler_api()
        .with_state(NEW.descriptor().key())
        .with_metadata(describe);
    api.validate_init = Some(validate);
    api
}
fn host() -> ModuleHost {
    unsafe { module().builder().from_api(handler_api()) }.unwrap()
}
fn cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut cx())
}
fn ready<F: Future + Unpin>(future: &mut F) -> F::Output {
    for _ in 0..100 {
        if let Poll::Ready(result) = poll(future) {
            return result;
        }
    }
    panic!("operation did not finish");
}
fn cleanup(host: &ModuleHost) {
    host.remove_worker(WorkerId(1)).unwrap();
    for _ in 0..100 {
        if host.poll_cleanup(&mut cx()).is_ready() {
            return;
        }
    }
    panic!("registered children did not join");
}

#[test]
fn typed_restart_migrates_after_join_and_preserves_clients_and_ordinary_request_identity() {
    let host = host();
    host.start_worker(WorkerId(1), WORK, 0, OLD, &5).unwrap();
    assert!(host.poll_workers(&mut cx()).is_pending());
    assert_eq!(host.worker_checkpoint(WorkerId(1), OLD).unwrap(), Some(6));
    let mut old_call = host.handler(REQUEST).unwrap().call("wait".into());
    assert!(poll(&mut old_call).is_pending());
    let (client, mut driver) = host.bridge(BridgeOptions::default()).unwrap();
    let commits = Rc::new(Cell::new(0));
    let counter = commits.clone();
    let mut restart = unsafe {
        host.restart_api_with(
            new_api(),
            NEW.migrate(|_, checkpoint| {
                Ok(Counter {
                    total: OLD.decode(checkpoint)? + 100,
                })
            })
            .with_commit(move || {
                counter.set(counter.get() + 1);
                Ok(())
            }),
        )
    }
    .unwrap();
    assert!(!host.is_accepting());
    assert!(poll(&mut restart).is_pending());
    assert_eq!(commits.get(), 0);
    assert_eq!(ready(&mut old_call).unwrap(), 1);
    let mut queued = client.handler(REQUEST).unwrap().call("next".into());
    assert!(poll(&mut driver).is_pending());
    assert!(poll(&mut queued).is_pending());
    assert_eq!(ready(&mut restart).unwrap(), 2);
    assert_eq!(commits.get(), 1);
    assert_eq!(
        host.worker_checkpoint(WorkerId(1), NEW).unwrap(),
        Some(Counter { total: 107 })
    );
    STARTS.with(|log| assert_eq!(&*log.borrow(), &[(1, 5)]));
    assert!(host.is_accepting());
    assert!(poll(&mut driver).is_pending());
    assert_eq!(ready(&mut queued).unwrap(), 2);
    assert!(host.poll_workers(&mut cx()).is_pending());
    STARTS.with(|log| assert_eq!(&*log.borrow(), &[(1, 5), (2, 107)]));
    let mut close = client.close();
    assert!(poll(&mut driver).is_ready());
    ready(&mut close).unwrap();
    cleanup(&host);
}

#[test]
fn migration_failure_preserves_old_state_and_retry_requires_explicit_resume() {
    let host = host();
    host.start_worker(WorkerId(1), WORK, 0, OLD, &10).unwrap();
    let _ = host.poll_workers(&mut cx());
    let mut failed = unsafe {
        host.restart_api_with(
            new_api(),
            NEW.migrate(|_, _| {
                Err(Error::Configuration(
                    "migration deliberately refused".into(),
                ))
            }),
        )
    }
    .unwrap();
    let error = ready(&mut failed).unwrap_err();
    assert!(error.to_string().contains("migration deliberately refused"));
    assert_eq!(host.generation(), 1);
    assert!(!host.is_accepting());
    assert_eq!(host.worker_checkpoint(WorkerId(1), OLD).unwrap(), Some(12));
    let mut retry = unsafe {
        host.restart_api_with(
            new_api(),
            NEW.migrate(|_, old| {
                Ok(Counter {
                    total: OLD.decode(old)?,
                })
            }),
        )
    }
    .unwrap();
    assert_eq!(ready(&mut retry).unwrap(), 2);
    assert!(!host.is_accepting());
    host.resume().unwrap();
    assert!(host.is_accepting());
    cleanup(&host);
}

#[test]
fn interrupted_restart_stays_paused_and_does_not_force_an_uncooperative_worker() {
    let host = host();
    host.start_worker(WorkerId(1), WORK, 2, OLD, &3).unwrap();
    let _ = host.poll_workers(&mut cx());
    let mut restart = unsafe { host.restart_api(handler_api()) }.unwrap();
    assert!(poll(&mut restart).is_pending());
    drop(restart);
    assert!(!host.is_accepting());
    assert_eq!(host.generation(), 1);
    assert_eq!(host.worker_status(WorkerId(1)), None);
    assert_eq!(host.worker_checkpoint(WorkerId(1), OLD).unwrap(), Some(4));
    cleanup(&host);
    host.resume().unwrap();
    let mut call = host.handler(REQUEST).unwrap().call("new".into());
    assert_eq!(ready(&mut call).unwrap(), 1);
}

#[test]
fn candidate_rejection_precedes_stop_and_worker_errors_block_restart() {
    let host = host();
    host.start_worker(WorkerId(1), WORK, 1, OLD, &0).unwrap();
    let error = unsafe { host.restart_api(new_api()) }.err().unwrap();
    assert!(matches!(error, Error::Contract { .. }));
    assert!(host.is_accepting());
    assert!(host.poll_workers(&mut cx()).is_ready());
    assert_eq!(
        host.worker_error(WorkerId(1)).as_deref(),
        Some("worker refused its configuration")
    );
    let mut restart = unsafe { host.restart_api(handler_api()) }.unwrap();
    assert!(matches!(
        ready(&mut restart),
        Err(Error::Restart {
            source: reknot::RouteError::WorkerFailed(WorkerId(1)),
            ..
        })
    ));
    assert_eq!(host.generation(), 1);
    assert!(!host.is_accepting());
    cleanup(&host);
}

#[test]
fn checkpoint_schema_rejects_wrong_structure_even_with_the_same_key_and_version() {
    let checkpoint = OLD.encode(&3).unwrap();
    let incompatible: WorkerState<Counter> = WorkerState::new("restart.counter", 1);
    assert!(matches!(
        incompatible.decode(&checkpoint),
        Err(Error::Configuration(_))
    ));
}

#[test]
fn restart_dropped_inside_a_worker_allows_explicit_recovery_after_the_poll() {
    let host = host();
    host.start_worker(WorkerId(1), WORK, 3, OLD, &0).unwrap();
    let restart = unsafe { host.restart_api(handler_api()) }.unwrap();
    DROP_RESTART.with(|slot| *slot.borrow_mut() = Some(restart));
    let _ = host.poll_workers(&mut cx());
    DROP_RESTART.with(|slot| assert!(slot.borrow().is_none()));
    assert!(!host.is_accepting());
    host.resume().unwrap();
    assert!(host.is_accepting());
    cleanup(&host);
}

#[test]
fn paused_bridge_is_woken_when_restart_restores_admission() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Signal(AtomicUsize);
    impl std::task::Wake for Signal {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let host = host();
    let (client, mut driver) = host.bridge(BridgeOptions::default()).unwrap();
    let mut restart = unsafe { host.restart_api(handler_api()) }.unwrap();
    let mut call = client.handler(REQUEST).unwrap().call("queued".into());
    let signal = Arc::new(Signal(AtomicUsize::new(0)));
    let waker = Waker::from(signal.clone());
    assert!(Pin::new(&mut driver)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    assert_eq!(signal.0.load(Ordering::SeqCst), 0);
    assert_eq!(ready(&mut restart).unwrap(), 2);
    assert!(signal.0.load(Ordering::SeqCst) > 0);
    let _ = poll(&mut driver);
    assert_eq!(ready(&mut call).unwrap(), 2);
    let mut close = client.close();
    assert!(poll(&mut driver).is_ready());
    ready(&mut close).unwrap();
}
