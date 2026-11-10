#![cfg(feature = "typed")]
use reknot::typed::{Handler, Module};
use reknot::{
    HostResource, HostResources, RequestContext, ServiceError, ServiceKey, ServiceRequirement,
};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
const COUNT: Handler<(), u64> = Handler::new("counter.next", 1);
const SERVICE: ServiceKey = ServiceKey::new(420, 1);
const REQUIRED: &[ServiceRequirement] = &[ServiceRequirement::new(1, SERVICE)];
fn module() -> Module {
    Module::new("counter", 1).with(COUNT)
}
#[reknot::handler(COUNT)]
fn count(context: RequestContext, _: ()) -> u64 {
    let mut output = [0; 8];
    context.call_service(1, 0, &[], &mut output).unwrap();
    u64::from_le_bytes(output)
}
reknot::export_handlers!(module(), [count], requires = REQUIRED);
struct Counter(u64);
impl HostResource for Counter {
    fn call(&mut self, _: u32, _: &[u8], output: &mut [u8]) -> Result<usize, ServiceError> {
        if output.len() < 8 {
            return Err(ServiceError::BUFFER_TOO_SMALL);
        }
        self.0 += 1;
        output[..8].copy_from_slice(&self.0.to_le_bytes());
        Ok(8)
    }
}
#[test]
fn host_state_survives_reload_and_revocation_is_rechecked() {
    let resources = HostResources::new();
    let counter = resources.register(SERVICE, Counter(0));
    let host = unsafe {
        module()
            .builder()
            .resources(resources.clone())
            .bind(1, counter.id())
            .from_api(handler_api())
    }
    .unwrap();
    let handle = host.handler(COUNT).unwrap();
    for value in 1..=3 {
        let mut call = handle.call(());
        match Pin::new(&mut call).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(Ok(actual)) => assert_eq!(actual, value),
            result => panic!("unexpected result: {result:?}"),
        }
        unsafe { host.reload_api(handler_api()) }.unwrap();
    }
    assert_eq!(counter.try_borrow().unwrap().0, 3);
    resources.revoke(counter.id());
    assert!(matches!(
        Pin::new(&mut handle.call(())).poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(_))
    ));
}
