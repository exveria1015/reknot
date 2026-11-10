use reknot::typed::{Handler, Module};
const ECHO: Handler<String, String> = Handler::new("native.echo", 1);
const LENGTH: Handler<String, usize> = Handler::new("native.length", 1);
fn module() -> Module {
    Module::new("native.typed", 1).with(ECHO).with(LENGTH)
}

#[reknot::handler(ECHO)]
async fn echo(context: reknot::RequestContext, input: String) -> String {
    if input == "__wait_for_cancel" {
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
        std::future::pending::<()>().await;
    }
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
    format!("{}:{input}", typed_helper::label())
}
#[reknot::handler(LENGTH)]
fn length(input: String) -> usize {
    input.len()
}
reknot::export_handlers!(module(), [echo, length]);
