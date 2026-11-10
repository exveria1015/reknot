use contracts_protocol::*;
use reknot::{RequestContext, ServiceError};
use std::task::Poll;

#[reknot::reload_handler(PRIMARY, requires = REQUIREMENTS, init = Config)]
async fn handler(ctx: RequestContext) -> Result<(), ServiceError> {
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
    let config = ctx.initial::<Config>()?;
    let mut count = [0; 4];
    ctx.call_service(1, 0, &config.step.to_le_bytes(), &mut count)?;
    let bytes = ctx.call_service_async(2, 0, &count, 64)?.await?;
    let version = if cfg!(feature = "version_two") { 2 } else { 1 };
    let mut reply = format!("v{version}:g{}:", ctx.generation()).into_bytes();
    reply.extend_from_slice(&bytes);
    ctx.call(0, &reply, &mut [])?;
    Ok(())
}
