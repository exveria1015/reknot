use reknot::{RequestContext, ServiceError, ServiceKey};
use serde::{Deserialize, Serialize};

// Edit these constants, save, and watch the running window change.
const LABEL: &str = "Coral / v2";
const ACCENT: &str = "#c36743";
const STEP: u64 = 5;
const INTERVAL_MS: u64 = 600;

const VIEW: ServiceKey = ServiceKey::new(0x4755_4901, 1);
const COUNTER: ServiceKey = ServiceKey::new(0x434f_554e, 1);

#[derive(Deserialize)]
struct Message {
    text: String,
    slow: bool,
}
#[derive(Serialize)]
struct Frame {
    kind: &'static str,
    generation: u64,
    label: &'static str,
    accent: &'static str,
    counter: u64,
    step: u64,
    text: String,
    stopped: bool,
}

fn now(ctx: &RequestContext) -> Result<u64, ServiceError> {
    let mut out = [0; 8];
    ctx.call(0, &[], &mut out)?;
    Ok(u64::from_le_bytes(out))
}
async fn pause(ctx: &RequestContext, ms: u64) -> Result<(), ServiceError> {
    let until = now(ctx)?.saturating_add(ms);
    std::future::poll_fn(|_| {
        if ctx.stop_requested() {
            return std::task::Poll::Ready(Ok(()));
        }
        match now(ctx) {
            Ok(t) if t >= until => std::task::Poll::Ready(Ok(())),
            Ok(_) => std::task::Poll::Pending,
            Err(error) => std::task::Poll::Ready(Err(error)),
        }
    })
    .await
}
fn publish(
    ctx: &RequestContext,
    kind: &'static str,
    counter: u64,
    text: String,
) -> Result<(), ServiceError> {
    let bytes = serde_json::to_vec(&Frame {
        kind,
        generation: ctx.generation(),
        label: LABEL,
        accent: ACCENT,
        counter,
        step: STEP,
        text,
        stopped: ctx.stop_requested(),
    })
    .map_err(|_| ServiceError::FAILED)?;
    ctx.call(1, &bytes, &mut [])?;
    Ok(())
}

#[reknot::reload_handler(VIEW, state = COUNTER)]
async fn update(ctx: RequestContext) -> Result<(), ServiceError> {
    if ctx.input() == b"worker" {
        let saved = ctx.checkpoint().ok_or(ServiceError::INVALID_ARGUMENT)?;
        let mut count = u64::from_le_bytes(
            saved
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
        );
        while !ctx.stop_requested() {
            count = count.saturating_add(STEP);
            publish(&ctx, "worker", count, "checkpointed counter".into())?;
            ctx.save_checkpoint(COUNTER, &count.to_le_bytes())?;
            pause(&ctx, INTERVAL_MS).await?;
        }
        ctx.save_checkpoint(COUNTER, &count.to_le_bytes())?;
    } else {
        let message: Message =
            serde_json::from_slice(ctx.input()).map_err(|_| ServiceError::INVALID_ARGUMENT)?;
        pause(&ctx, if message.slow { 5000 } else { 150 }).await?;
        publish(&ctx, "reply", 0, format!("{LABEL} → {}", message.text))?;
    }
    Ok(())
}
