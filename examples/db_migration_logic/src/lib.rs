use reknot::{RequestContext, ServiceError, ServiceKey, ServiceRequirement};
use std::task::Poll;
const SESSION: ServiceKey = ServiceKey::new(0x4442_5345, 1);
const STATE: ServiceKey = ServiceKey::new(0x4442_5354, 1);
const DB_VERSION: u32 = if cfg!(feature = "version_two") && !cfg!(feature = "compatible") {
    2
} else {
    1
};
const DB: ServiceKey = ServiceKey::new(0x4442_4d47, DB_VERSION);

#[reknot::reload_handler(SESSION, state = STATE, requires = &[ServiceRequirement::new(1, DB)])]
async fn logic(ctx: RequestContext) -> Result<(), ServiceError> {
    if ctx.input() == b"worker" {
        let state = ctx.checkpoint().ok_or(ServiceError::INVALID_ARGUMENT)?;
        ctx.cancelled().await;
        let count = *state.bytes.first().ok_or(ServiceError::INVALID_ARGUMENT)?;
        ctx.save_checkpoint(STATE, &[count + 1])?;
        return Ok(());
    }
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
    if ctx.stop_requested() {
        return Ok(());
    }
    let delta: i64 = if DB_VERSION == 2 { 100 } else { 1 };
    let mut out = [0; 8];
    ctx.call_service(1, 1, &delta.to_le_bytes(), &mut out)?;
    let value = i64::from_le_bytes(out);
    let version = if cfg!(feature = "version_two") { 2 } else { 1 };
    ctx.call(
        0,
        format!("v{version}:g{}:{value}", ctx.generation()).as_bytes(),
        &mut [],
    )?;
    Ok(())
}
