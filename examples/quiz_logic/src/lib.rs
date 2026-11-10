use quiz_protocol::*;
use reknot::RequestContext;

// Save changes here while both browsers remain connected.
const LABEL: &str = "rules-v1";
const POINTS: u32 = 100;
const ROUND_MS: u64 = 30000;
const TICK_MS: u64 = 200;
// Change to 2 to observe rejection before the current generation is stopped.
const CONTRACT_VERSION: u32 = 1;

#[reknot::handler(INFO)]
fn info(context: RequestContext, _: ()) -> RuleInfo {
    RuleInfo {
        label: LABEL.into(),
        correct_points: POINTS,
        round_ms: ROUND_MS,
        tick_ms: TICK_MS,
        generation: context.generation(),
    }
}

async fn sleep(context: &RequestContext, milliseconds: u64) -> Result<(), String> {
    context
        .call_service_async(1, SLEEP, &milliseconds.to_le_bytes(), 0)
        .map_err(|e| e.to_string())?
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[reknot::handler(ANSWER)]
async fn answer(context: RequestContext, input: AnswerInput) -> Result<Verdict, String> {
    if input.delay_ms > 0 {
        sleep(&context, input.delay_ms).await?;
    }
    if context.stop_requested() {
        return Err("更新により回答を中止しました。再送はしていません。".into());
    }
    let correct = input.choice == input.correct;
    Ok(Verdict {
        correct,
        points: if correct { POINTS } else { 0 },
        generation: context.generation(),
        label: LABEL.into(),
    })
}

#[reknot::handler(CLOCK)]
async fn clock(context: RequestContext, _: ()) -> Result<(), String> {
    let mut state = STATE.restore(&context).map_err(|e| e.to_string())?;
    while !context.stop_requested() {
        if let Err(error) = sleep(&context, TICK_MS).await {
            if context.stop_requested() {
                break;
            }
            return Err(error);
        }
        if context.stop_requested() {
            break;
        }
        state.ticks += 1;
        STATE.save(&context, &state).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec(&ClockPulse {
            ticks: state.ticks,
            generation: context.generation(),
            label: LABEL.into(),
        })
        .map_err(|e| e.to_string())?;
        context
            .call_service(1, PULSE, &bytes, &mut [])
            .map_err(|e| e.to_string())?;
    }
    STATE.save(&context, &state).map_err(|e| e.to_string())
}

reknot::export_handlers!(
    module_version(CONTRACT_VERSION),
    [info, answer, clock],
    requires = REQUIRED
);
