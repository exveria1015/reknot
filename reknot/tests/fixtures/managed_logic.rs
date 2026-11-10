//! Real SDK-generated DLL fixture: registered locals and host-owned async notifications.
use reknot::{GenerationLocal, RequestContext, ResourceId, ServiceError, ServiceKey, Services};

const KEY: ServiceKey = ServiceKey::new(0x4d41_4e41, 1);
#[derive(Default)]
struct Local {
    target: Option<(Services, ResourceId, u64)>,
    fail: bool,
    count: u64,
}
impl Drop for Local {
    fn drop(&mut self) {
        if let Some((services, resource, generation)) = &self.target {
            services
                .call(*resource, KEY, 9, &generation.to_le_bytes(), &mut [])
                .unwrap();
        }
        assert!(!self.fail, "registered local destruction failed");
    }
}
static LOCAL: GenerationLocal<Local> = GenerationLocal::new(Local::default);
#[reknot::reload_handler(KEY)]
async fn handler(ctx: RequestContext) -> Result<(), ServiceError> {
    let count = LOCAL.with_mut(&ctx, |local| {
        local.target = Some((ctx.services().clone(), ctx.resource(), ctx.generation()));
        local.fail = ctx.input() == b"fail-drop";
        local.count += 1;
        local.count
    })?;
    ctx.call(0, &count.to_le_bytes(), &mut [])?;
    if ctx.input() != b"fail-drop" && ctx.input() != b"local" {
        let reply = ctx.call_async(0, ctx.input(), 32)?.await?;
        ctx.call(1, &reply, &mut [])?;
    }
    Ok(())
}
