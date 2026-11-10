use reknot::typed::{Handler, Module, WorkerState};
use reknot::RequestContext;
use serde::{Deserialize, Serialize};
const ECHO: Handler<String, String> = Handler::new("evolution.echo", 1);
const PACKET: Handler<Packet, String> = Handler::new("evolution.packet", 1);
const WORK: Handler<(), Result<(), String>> = Handler::new("evolution.worker", 1);
const STATE: WorkerState<u64> = WorkerState::new("evolution.state", 1);
#[reknot::schema]
#[derive(Serialize, Deserialize)]
struct Packet {
    value: u32,
}
fn module() -> Module {
    Module::new("evolution", 1)
        .with(ECHO)
        .with(PACKET)
        .with(WORK)
        .with_state(STATE)
}
#[reknot::handler(ECHO)]
fn echo(input: String) -> String {
    format!("logic-v1:{input}")
}
#[reknot::handler(PACKET)]
fn packet(input: Packet) -> String {
    format!("{:?}", input.value)
}
#[reknot::handler(WORK)]
async fn worker(context: RequestContext, _: ()) -> Result<(), String> {
    let state = STATE.restore(&context).map_err(|e| e.to_string())?;
    STATE
        .save(&context, &(state + 1))
        .map_err(|e| e.to_string())?;
    context
        .spawn_child(|stop| async move {
            stop.cancelled().await;
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
            Ok(())
        })
        .unwrap();
    context.cancelled().await;
    STATE
        .save(&context, &(state + 2))
        .map_err(|e| e.to_string())?;
    Ok(())
}
reknot::export_handlers!(module(), [echo, packet, worker]);
