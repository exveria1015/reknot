#![cfg(feature = "typed-binary")]
use reknot::typed::{
    BridgeOptions, Error, Handler, Module, WireFormat, WorkerState, MAX_MESSAGE_BYTES,
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

#[reknot::schema]
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Frame {
    label: String,
    samples: Vec<f32>,
    note: Option<String>,
}
const FRAME: Handler<Frame, Frame> =
    Handler::new("binary.frame", 1).with_format(WireFormat::MessagePack);
const JSON: Handler<String, String> = Handler::new("binary.json", 1);
const BYTES: Handler<Vec<u8>, Vec<u8>> =
    Handler::new("binary.bytes", 1).with_format(WireFormat::MessagePack);
const WORK: Handler<u64, Result<(), String>> =
    Handler::new("binary.worker", 1).with_format(WireFormat::MessagePack);
const STATE: WorkerState<u64> = WorkerState::new("binary.state", 1);
#[reknot::handler(FRAME)]
fn frame(value: Frame) -> Frame {
    value
}
#[reknot::handler(JSON)]
fn text(value: String) -> String {
    value
}
#[reknot::handler(BYTES)]
fn bytes(value: Vec<u8>) -> Vec<u8> {
    value
}
#[reknot::handler(WORK)]
async fn worker(ctx: reknot::RequestContext, value: u64) -> Result<(), String> {
    STATE.save(&ctx, &value).unwrap();
    ctx.cancelled().await;
    Ok(())
}
fn module() -> Module {
    Module::new("binary", 1)
        .with(FRAME)
        .with(JSON)
        .with(BYTES)
        .with(WORK)
        .with_state(STATE)
}
reknot::export_handlers!(module(), [frame, text, bytes, worker]);
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
}
fn ready<F: Future + Unpin>(mut future: F) -> F::Output {
    for _ in 0..30 {
        if let Poll::Ready(result) = poll(&mut future) {
            return result;
        }
    }
    panic!("future did not finish")
}
fn payload() -> Frame {
    Frame {
        label: "frame".into(),
        samples: vec![0.125, -4.5, 123456.0],
        note: None,
    }
}
#[test]
fn mixed_formats_and_bridge_round_trip_owned_values() {
    let host = unsafe { module().builder().from_api(handler_api()) }.unwrap();
    assert_eq!(
        ready(host.handler(FRAME).unwrap().call(payload())).unwrap(),
        payload()
    );
    assert_eq!(
        ready(host.handler(JSON).unwrap().call("JSON".into())).unwrap(),
        "JSON"
    );
    let (client, mut driver) = host.bridge(BridgeOptions::default()).unwrap();
    let mut call = client.handler(FRAME).unwrap().call(payload());
    for _ in 0..10 {
        let _ = poll(&mut driver);
    }
    assert_eq!(ready(&mut call).unwrap(), payload());
    let handle = host.handler(BYTES).unwrap();
    let old = handle.call(vec![1, 128, 255]);
    unsafe { host.reload_api(handler_api()) }.unwrap();
    assert_eq!(old.generation(), Some(1));
    assert_eq!(ready(old).unwrap(), vec![1, 128, 255]);
    assert_eq!(ready(handle.call(vec![2])).unwrap(), vec![2]);
}
#[test]
fn format_mismatch_is_rejected_before_admission_changes() {
    let wrong = Module::new("binary", 1)
        .with(FRAME.with_format(WireFormat::Json))
        .with(JSON)
        .with(BYTES)
        .with(WORK)
        .with_state(STATE);
    let result = unsafe { wrong.builder().from_api(handler_api()) };
    let Error::Contract { report, .. } = result.err().unwrap() else {
        panic!("expected structured contract error")
    };
    assert!(report.to_string().contains("format"));
    let host = unsafe { module().builder().from_api(handler_api()) }.unwrap();
    assert!(host.handler(FRAME.with_format(WireFormat::Json)).is_err());
    assert!(host.is_accepting());
}
#[test]
#[cfg_attr(
    miri,
    ignore = "large allocation limit is covered natively; small codec bounds run in lib tests"
)]
fn binary_limit_applies_to_encoded_bytes() {
    let host = unsafe { module().builder().from_api(handler_api()) }.unwrap();
    let handle = host.handler(BYTES).unwrap();
    // Values >=128 take two MessagePack bytes; enforce the same output bound.
    assert!(matches!(
        ready(handle.call(vec![255; MAX_MESSAGE_BYTES / 2 + 1])),
        Err(Error::TooLarge { .. })
    ));
}
#[test]
fn binary_worker_restarts_with_json_checkpoint_and_binary_reply() {
    let host = unsafe { module().builder().from_api(handler_api()) }.unwrap();
    let id = reknot::WorkerId(1);
    host.start_worker(id, WORK, 7, STATE, &0).unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    let _ = host.poll_workers(&mut cx);
    assert_eq!(host.worker_checkpoint(id, STATE).unwrap(), Some(7));
    let mut restart = unsafe { host.restart_api(handler_api()) }.unwrap();
    for _ in 0..10 {
        let _ = host.poll_workers(&mut cx);
        if let Poll::Ready(result) = poll(&mut restart) {
            assert_eq!(result.unwrap(), 2);
            break;
        }
    }
    assert!(host.is_accepting());
    let _ = host.poll_workers(&mut cx);
    assert_eq!(host.worker_checkpoint(id, STATE).unwrap(), Some(7));
    host.remove_worker(id).unwrap();
    for _ in 0..10 {
        let _ = host.poll_cleanup(&mut cx);
    }
}
