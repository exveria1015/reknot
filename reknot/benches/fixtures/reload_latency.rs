use reknot::typed::{Handler, Module, WorkerState};
use std::time::{Duration, Instant};

const REQUEST: Handler<(u64, u64, u64), (String, u64)> = Handler::new("latency.request", 1);
const WORK: Handler<u64, Result<(), String>> = Handler::new("latency.worker", 1);
const STATE: WorkerState<Vec<u8>> = WorkerState::new("latency.state", 1);
const VERSION: &str = "initial";

// The benchmark executor polls every millisecond. This models cooperative I/O
// without a timer runtime, threads, or callbacks escaping a managed generation.
async fn delay(ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    std::future::poll_fn(|_| {
        if Instant::now() >= until {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
}

#[reknot::handler(REQUEST)]
async fn request((sequence, cpu_us, wait_ms): (u64, u64, u64)) -> (String, u64) {
    let until = Instant::now() + Duration::from_micros(cpu_us);
    while Instant::now() < until {
        std::hint::spin_loop();
    }
    delay(wait_ms).await;
    (VERSION.into(), sequence)
}

#[reknot::handler(WORK)]
async fn worker(context: reknot::RequestContext, cleanup_ms: u64) -> Result<(), String> {
    let mut state = STATE.restore(&context).map_err(|e| e.to_string())?;
    state[0] = state[0].wrapping_add(1);
    STATE.save(&context, &state).map_err(|e| e.to_string())?;
    context
        .spawn_child(move |stop| async move {
            stop.cancelled().await;
            delay(cleanup_ms).await;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    context.cancelled().await;
    STATE.save(&context, &state).map_err(|e| e.to_string())?;
    Ok(())
}

fn module() -> Module {
    Module::new("latency", 1)
        .with(REQUEST)
        .with(WORK)
        .with_state(STATE)
}
reknot::export_handlers!(module(), [request, worker]);
