use reknot::{RequestContext, ServiceError, ServiceKey};

const LINE_SESSION: ServiceKey = ServiceKey::new(0x4c49_4e45, 1);

#[reknot::reload_handler(LINE_SESSION)]
async fn handle(request: RequestContext) -> Result<(), ServiceError> {
    // Send "slow\n" to allow a rebuild while this old request is in progress.
    if request.input() == b"slow\n" {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        std::future::poll_fn(|cx| {
            if std::time::Instant::now() >= deadline {
                std::task::Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })
        .await;
    }
    // Edit the label, save, and send another line on the SAME connection.
    let mut reply = format!("logic-v1 / generation {}: ", request.generation()).into_bytes();
    reply.extend_from_slice(request.input());
    request.call(0, &reply, &mut [])?;
    Ok(())
}
