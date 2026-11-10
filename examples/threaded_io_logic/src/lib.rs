use reknot::{RequestContext, ServiceError, ServiceKey};
const IO: ServiceKey = ServiceKey::new(0x5449_4f31, 1);

#[reknot::reload_handler(IO)]
async fn request(ctx: RequestContext) -> Result<(), ServiceError> {
    let bytes = ctx.call_async(1, ctx.input(), 64)?.await?;
    let version = if cfg!(feature = "version_two") { 2 } else { 1 };
    let mut reply = format!("v{version}:g{}:", ctx.generation()).into_bytes();
    reply.extend_from_slice(&bytes);
    ctx.call(2, &reply, &mut [])?;
    Ok(())
}
