#![cfg(feature = "typed")]
use reknot::typed::{Error, Handler, Module, ModuleHost};
use reknot::{HandlerApi, RequestContext, ServiceError};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

const ECHO: Handler<String, String> = Handler::new("test.echo", 1);
const LENGTH: Handler<Vec<String>, usize> = Handler::new("test.length", 1);
const CHECK: Handler<i32, Result<i32, String>> = Handler::new("test.check", 1);
const PANIC: Handler<(), ()> = Handler::new("test.panic", 1);
const CHILD: Handler<bool, u64> = Handler::new("test.child", 1);

fn module() -> Module {
    Module::new("test.module", 1)
        .with(ECHO)
        .with(LENGTH)
        .with(CHECK)
        .with(PANIC)
        .with(CHILD)
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
#[reknot::handler(ECHO)]
async fn echo(input: String) -> String {
    yield_once().await;
    format!("v1:{input}")
}
mod nested {
    #[reknot::handler(super::LENGTH)]
    pub(super) fn length(input: Vec<String>) -> usize {
        input.len()
    }
}
#[reknot::handler(CHECK)]
fn check(input: i32) -> Result<i32, String> {
    if input < 0 {
        Err("positive number required".into())
    } else {
        Ok(input * 2)
    }
}
#[reknot::handler(PANIC)]
fn panic_handler(_: ()) {
    panic!("deliberate handler failure")
}
#[reknot::handler(CHILD)]
async fn child(context: RequestContext, forever: bool) -> u64 {
    context
        .spawn_child(|stop| async move {
            stop.cancelled().await;
            yield_once().await;
            Ok(())
        })
        .unwrap();
    if forever {
        std::future::pending::<()>().await;
    }
    context.generation()
}
reknot::export_handlers!(
    module(),
    [echo, nested::length, check, panic_handler, child]
);

fn host() -> ModuleHost {
    unsafe { module().builder().from_api(handler_api()) }.unwrap()
}
fn poll<T: Future + Unpin>(future: &mut T) -> Poll<T::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
}
fn ready<T: Future + Unpin>(mut future: T) -> T::Output {
    for _ in 0..10 {
        if let Poll::Ready(output) = poll(&mut future) {
            return output;
        }
    }
    panic!("future did not finish");
}
fn second_api() -> HandlerApi {
    unsafe extern "C" fn make(raw: *const reknot::HandlerRequest) -> reknot::TaskHandle {
        reknot::typed::__private::make(raw, |ctx| {
            Ok(Box::pin(reknot::typed::__private::invoke(
                ECHO,
                ctx,
                |_, input| async move { format!("v2:{input}") },
            )))
        })
    }
    HandlerApi {
        make_request: make,
        ..handler_api()
    }
}

#[test]
fn stable_handle_pins_admission_before_poll_and_survives_controller_drop() {
    let host = host();
    let handle = host.handler(ECHO).unwrap();
    let old = handle.call("old".into());
    assert_eq!(old.generation(), Some(1));
    assert_eq!(unsafe { host.reload_api(second_api()) }.unwrap(), 2);
    let new = handle.clone().call("new".into());
    assert_eq!(new.generation(), Some(2));
    drop(host);
    assert_eq!(ready(old).unwrap(), "v1:old");
    assert_eq!(ready(new).unwrap(), "v2:new");
}
#[test]
fn multiple_typed_handlers_and_application_errors_round_trip() {
    let host = host();
    assert_eq!(
        ready(
            host.handler(LENGTH)
                .unwrap()
                .call(vec!["a".into(), "b".into()])
        )
        .unwrap(),
        2
    );
    assert_eq!(
        ready(host.handler(CHECK).unwrap().call(-1)).unwrap(),
        Err("positive number required".into())
    );
    assert_eq!(ready(host.handler(CHECK).unwrap().call(3)).unwrap(), Ok(6));
    assert!(matches!(
        ready(host.handler(PANIC).unwrap().call(())),
        Err(Error::HandlerFailed { generation: 1, .. })
    ));
    assert_eq!(host.in_flight(), 0);
}
#[test]
fn rejected_candidate_and_failed_admissions_preserve_working_generation() {
    let host = host();
    let handle = host.handler(ECHO).unwrap();
    let mut api = handler_api();
    unsafe extern "C" fn reject(_: *const u8, _: usize) -> u32 {
        ServiceError::CONTRACT_MISMATCH.0
    }
    api.validate_init = Some(reject);
    let error = unsafe { host.reload_api(api) }.unwrap_err();
    assert!(matches!(error, Error::Contract { .. }));
    assert!(error.to_string().contains("test.echo"));
    assert_eq!(host.generation(), 1);
    assert_eq!(
        ready(handle.call("still works".into())).unwrap(),
        "v1:still works"
    );
    let wrong: Handler<u64, u64> = Handler::new("test.echo", 1);
    assert!(host.handler(wrong).is_err());
    assert!(module().with(ECHO).validate().is_err());
    let wrong_version = Module::new("test.module", 2).with(ECHO);
    assert!(matches!(
        unsafe { wrong_version.builder().from_api(handler_api()) },
        Err(Error::Contract { .. })
    ));
}
#[test]
fn in_flight_limit_applies_before_start_and_drop_releases_slot() {
    let host = unsafe { module().builder().max_in_flight(1).from_api(handler_api()) }.unwrap();
    let handle = host.handler(ECHO).unwrap();
    let call = handle.call("first".into());
    assert!(matches!(
        ready(handle.call("overflow".into())),
        Err(Error::Busy)
    ));
    drop(call);
    assert_eq!(host.in_flight(), 0);
    assert!(ready(handle.call("next".into())).is_ok());
}
// Exercise the real 1 MiB boundary natively; interpreting a large JSON string
// adds minutes without exercising additional ownership or FFI paths in Miri.
#[test]
#[cfg(not(miri))]
fn encoded_input_and_output_are_bounded_without_replaying_the_request() {
    let host = host();
    let handle = host.handler(ECHO).unwrap();
    assert!(matches!(
        ready(handle.call("x".repeat(reknot::typed::MAX_MESSAGE_BYTES))),
        Err(Error::TooLarge { .. })
    ));
    assert_eq!(host.in_flight(), 0);
    // JSON adds two quote bytes; the handler adds three more bytes for "v1:".
    let error = ready(handle.call("x".repeat(reknot::typed::MAX_MESSAGE_BYTES - 3))).unwrap_err();
    assert!(matches!(error, Error::Codec(_)));
    assert!(error.to_string().contains("exceeds"));
    assert_eq!(host.in_flight(), 0);
    assert_eq!(ready(handle.call("回復".into())).unwrap(), "v1:回復");
}
#[test]
fn completion_and_cancellation_wait_for_managed_children() {
    let host = host();
    let handle = host.handler(CHILD).unwrap();
    let mut completed = handle.call(false);
    assert!(poll(&mut completed).is_pending());
    assert_eq!(host.in_flight(), 1);
    assert_eq!(ready(completed).unwrap(), 1);
    let mut cancelled = handle.call(true);
    assert!(poll(&mut cancelled).is_pending());
    drop(cancelled);
    assert_eq!(host.in_flight(), 1);
    ready(std::future::poll_fn(|cx| host.poll_cleanup(cx)));
    assert_eq!(host.in_flight(), 0);
}
