//! cargo run -p reknot --example restart
use reknot::*;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Waker};

const SESSION: ServiceKey = ServiceKey::new(1, 1);
const V1: ServiceKey = ServiceKey::new(2, 1);
const V2: ServiceKey = ServiceKey::new(2, 2);

struct Console;
impl HostResource for Console {
    fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        println!("{}", String::from_utf8_lossy(input));
        Ok(0)
    }
}

#[reload_handler(SESSION, state = V1)]
async fn worker(request: RequestContext) -> Result<(), ServiceError> {
    let state = request.checkpoint().ok_or(ServiceError::INVALID_ARGUMENT)?;
    let value = if state.schema == V1 {
        *state.bytes.first().ok_or(ServiceError::INVALID_ARGUMENT)? as u16
    } else {
        u16::from_le_bytes(
            state
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
        )
    };
    request.call(
        0,
        format!("generation {}: restored {value}", request.generation()).as_bytes(),
        &mut [],
    )?;
    request.cancelled().await;
    let next = if state.schema == V1 {
        vec![(value + 1) as u8]
    } else {
        (value + 1).to_le_bytes().to_vec()
    };
    request.save_checkpoint(state.schema, &next)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = HostResources::new();
    let console = host.register(SESSION, Console);
    let api = unsafe { *hot_handler_api() };
    // Statically linked SDK callbacks remain valid for the process lifetime.
    let mut router = unsafe { RequestRouter::from_api(api, host) }?;
    router.start_worker(
        WorkerId(1),
        console.id(),
        b"worker",
        Checkpoint::new(V1, vec![7]),
    )?;
    let mut cx = Context::from_waker(Waker::noop());
    let _ = router.poll_workers(&mut cx);
    let candidate = unsafe { router.prepare_api(api.with_state(V2)) }?;
    let mut restart = router.restart(candidate, |_, old, schema| {
        let value = old.bytes[0] as u16 * 10;
        Ok(Checkpoint::new(schema, value.to_le_bytes().to_vec()))
    })?;
    let generation = match Pin::new(&mut restart).poll(&mut cx) {
        std::task::Poll::Ready(result) => result?,
        std::task::Poll::Pending => return Err("unexpected pending shutdown".into()),
    };
    drop(restart);
    let _ = router.poll_workers(&mut cx);
    println!(
        "cutover: generation {generation}, budget {}/{}",
        router.options().generations.used(),
        router.options().generations.limit()
    );
    Ok(())
}
