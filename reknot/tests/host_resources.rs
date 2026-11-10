use reknot::{HostResource, HostResources, ServiceError, ServiceKey};
use std::cell::Cell;
use std::rc::Rc;

const KEY: ServiceKey = ServiceKey::new(42, 1);

struct Counter {
    value: u8,
    drops: Rc<Cell<usize>>,
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}
impl HostResource for Counter {
    fn call(
        &mut self,
        operation: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, ServiceError> {
        match operation {
            0 => {
                if output.is_empty() {
                    return Err(ServiceError::BUFFER_TOO_SMALL);
                }
                self.value += input.first().copied().unwrap_or(1);
                output[0] = self.value;
                Ok(1)
            }
            1 => panic!("host implementation panic"),
            2 => Ok(output.len() + 1),
            _ => Err(ServiceError::UNSUPPORTED_OPERATION),
        }
    }
}

#[test]
fn service_clients_own_registry_but_revocation_invalidates_reused_slots() {
    let host = HostResources::new();
    let drops = Rc::new(Cell::new(0));
    let first = host.register(
        KEY,
        Counter {
            value: 0,
            drops: drops.clone(),
        },
    );
    let old_id = first.id();
    let services = host.services();
    let retained = services.clone();
    let mut output = [0];
    assert_eq!(services.call(old_id, KEY, 0, &[3], &mut output), Ok(1));
    assert_eq!(output, [3]);
    assert!(host.revoke(old_id));
    assert!(!host.revoke(old_id));
    let next = host.register(
        KEY,
        Counter {
            value: 10,
            drops: drops.clone(),
        },
    );
    assert_eq!(next.id().slot, old_id.slot);
    assert_ne!(next.id().generation, old_id.generation);
    assert_eq!(
        retained.call(old_id, KEY, 0, &[], &mut output),
        Err(ServiceError::REVOKED)
    );
    let id = next.id();
    drop(first);
    drop(next);
    assert_eq!(drops.get(), 1);
    drop(host);
    drop(services);
    assert_eq!(retained.call(id, KEY, 0, &[], &mut output), Ok(1));
    assert_eq!(output, [11]);
    assert_eq!(drops.get(), 1);
    drop(retained);
    assert_eq!(drops.get(), 2);
}

#[test]
fn wrong_registry_version_host_borrow_and_panic_are_explicit_errors() {
    let host = HostResources::new();
    let other = HostResources::new();
    let resource = host.register(
        KEY,
        Counter {
            value: 0,
            drops: Rc::new(Cell::new(0)),
        },
    );
    let services = host.services();
    let mut output = [0];
    assert_eq!(
        other
            .services()
            .call(resource.id(), KEY, 0, &[], &mut output),
        Err(ServiceError::REVOKED)
    );
    assert_eq!(
        services.call(resource.id(), ServiceKey::new(42, 2), 0, &[], &mut output),
        Err(ServiceError::CONTRACT_MISMATCH)
    );
    let borrowed = resource.try_borrow().unwrap();
    assert_eq!(
        services.call(resource.id(), KEY, 0, &[], &mut output),
        Err(ServiceError::BUSY)
    );
    drop(borrowed);
    assert_eq!(
        services.call(resource.id(), KEY, 0, &[], &mut []),
        Err(ServiceError::BUFFER_TOO_SMALL)
    );
    assert_eq!(
        resource.try_borrow().unwrap().value,
        0,
        "short output retried or applied side effect"
    );
    assert_eq!(
        services.call(resource.id(), KEY, 1, &[], &mut output),
        Err(ServiceError::PANICKED)
    );
    assert_eq!(
        services.call(resource.id(), KEY, 2, &[], &mut output),
        Err(ServiceError::INVALID_ARGUMENT)
    );
    assert_eq!(
        services.call(resource.id(), KEY, 0, &[], &mut output),
        Ok(1)
    );
}
