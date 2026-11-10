use bevy_game_protocol::{Reply, Request, CLOCK, GAME, PUBLISH, TICKS};
use reknot::{RequestContext, ServiceError};
use std::task::Poll;

// Change this, save, and the running host rebuilds in the background.
const STEP: i64 = if cfg!(feature = "fast") { 3 } else { 1 };

fn frame(ctx: &RequestContext) -> Result<u64, ServiceError> {
    let mut bytes = [0; 8];
    ctx.call(CLOCK, &[], &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

async fn wait_until(
    ctx: &RequestContext,
    until: u64,
    cancellable: bool,
) -> Result<(), ServiceError> {
    std::future::poll_fn(|_| match frame(ctx) {
        Ok(now) if now >= until || (cancellable && ctx.stop_requested()) => Poll::Ready(Ok(())),
        Ok(_) => Poll::Pending,
        Err(error) => Poll::Ready(Err(error)),
    })
    .await
}

fn publish(ctx: &RequestContext, reply: Reply) -> Result<(), ServiceError> {
    let bytes = serde_json::to_vec(&reply).map_err(|_| ServiceError::FAILED)?;
    ctx.call(PUBLISH, &bytes, &mut [])?;
    Ok(())
}

#[reknot::reload_handler(GAME, state = TICKS)]
async fn game(ctx: RequestContext) -> Result<(), ServiceError> {
    let request: Request =
        serde_json::from_slice(ctx.input()).map_err(|_| ServiceError::INVALID_ARGUMENT)?;
    let generation = ctx.generation();
    match request {
        Request::Worker => {
            // A registered local child is part of this request's completion.
            // It cleans up over another frame poll after the stop notification.
            ctx.spawn_child(|stop| async move {
                stop.cancelled().await;
                let mut yielded = false;
                std::future::poll_fn(|cx| {
                    if std::mem::replace(&mut yielded, true) {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                Ok(())
            })?;
            let state = ctx.checkpoint().ok_or(ServiceError::INVALID_ARGUMENT)?;
            if state.schema != TICKS {
                return Err(ServiceError::CONTRACT_MISMATCH);
            }
            let mut ticks = u64::from_le_bytes(
                state
                    .bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
            );
            publish(&ctx, Reply::Started { generation, ticks })?;
            while !ctx.stop_requested() {
                ticks += 1;
                publish(
                    &ctx,
                    Reply::Step {
                        generation,
                        ticks,
                        delta: STEP,
                    },
                )?;
                ctx.save_checkpoint(TICKS, &ticks.to_le_bytes())?;
                wait_until(&ctx, frame(&ctx)? + 1, true).await?;
            }
            // Deliberately asynchronous cooperative cleanup: the game must keep ticking.
            // The child is explicitly registered; no independent spawn escapes its scope.
            wait_until(&ctx, frame(&ctx)? + 2, false).await?;
            ctx.save_checkpoint(TICKS, &ticks.to_le_bytes())?;
            publish(&ctx, Reply::Stopped { generation, ticks })?;
        }
        Request::Job {
            entity,
            revision,
            delay_frames,
        } => {
            wait_until(&ctx, frame(&ctx)? + delay_frames, true).await?;
            if !ctx.stop_requested() {
                publish(
                    &ctx,
                    Reply::Job {
                        generation,
                        entity,
                        revision,
                        delta: STEP * 10,
                    },
                )?;
            }
        }
    }
    Ok(())
}
