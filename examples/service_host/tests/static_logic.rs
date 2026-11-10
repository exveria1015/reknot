#![cfg(feature = "static-logic")]
use reknot::{HostResource, HostResources, RequestRouter, RunStatus, ServiceError, ServiceKey};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct Replies(Vec<Vec<u8>>);
impl HostResource for Replies {
    fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0.push(input.to_vec());
        Ok(0)
    }
}

#[test]
fn linked_logic_uses_the_same_host_service_contract_without_a_dll() {
    let host = HostResources::new();
    let replies = host.register(ServiceKey::new(0x4c49_4e45, 1), Replies::default());
    // SAFETY: generated handler code has static lifetime and owns request data.
    let router = unsafe { RequestRouter::from_api(service_logic::handler_api(), host) }.unwrap();
    let mut task = router.dispatch(replies.id(), b"hello\n").unwrap();
    assert_eq!(
        Pin::new(&mut task).poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(RunStatus::Done)
    );
    assert_eq!(
        replies.try_borrow().unwrap().0,
        [b"logic-v1 / generation 1: hello\n".to_vec()]
    );
    assert_eq!(router.in_flight(), 0);
}
