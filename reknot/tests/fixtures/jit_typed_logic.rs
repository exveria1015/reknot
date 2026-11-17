use reknot::typed::{Handler, Module, WorkerState};
const ECHO: Handler<String, String> = Handler::new("jit.typed.echo", 1);
const WORK: Handler<(), Result<(), String>> = Handler::new("jit.typed.worker", 1);
const SAMPLES: Handler<Vec<f32>, Vec<f32>> = Handler::new("jit.typed.samples", 1);
const STATE: WorkerState<u64> = WorkerState::new("jit.typed.state", 1);
const VERSION: &str = "v1";

async fn yield_once() {
    let mut first = true;
    std::future::poll_fn(|cx| {
        if std::mem::take(&mut first) {
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
}
#[reknot::handler(ECHO)]
async fn echo(context: reknot::RequestContext, input: String) -> String {
    if input == "cancel" {
        context
            .spawn_child(|stop| async move {
                stop.cancelled().await;
                yield_once().await;
                Ok(())
            })
            .unwrap();
        context.cancelled().await;
    }
    yield_once().await;
    format!("{VERSION}:{input}")
}
#[reknot::handler(WORK)]
async fn worker(context: reknot::RequestContext, _: ()) -> Result<(), String> {
    let value = STATE.restore(&context).map_err(|e| e.to_string())?;
    STATE
        .save(&context, &(value + 1))
        .map_err(|e| e.to_string())?;
    context.cancelled().await;
    STATE
        .save(&context, &(value + 2))
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn module() -> Module {
    Module::new("jit.typed", 1)
        .with(ECHO)
        .with(WORK)
        .with(SAMPLES)
        .with_state(STATE)
}
#[reknot::handler(SAMPLES)]
fn samples(input: Vec<f32>) -> Vec<f32> {
    input
}
reknot::export_handlers!(module(), [echo, worker, samples]);
