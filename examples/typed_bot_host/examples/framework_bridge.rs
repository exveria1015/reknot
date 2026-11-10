//! A framework owns its own thread/runtime; only copied messages reach the DLL owner.
use reknot::typed::{BridgeOptions, Error};
use typed_bot_protocol::{module, Message, REPLY};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: generated callbacks are statically linked and stay resident.
    let host = unsafe { module().builder().from_api(typed_bot_logic::handler_api()) }?;
    let (client, driver) = host.bridge(BridgeOptions::default())?;
    let reply = client.handler(REPLY)?;
    let framework = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            // The framework can clone reply into its Send event-handler futures.
            let text = reply
                .call(Message {
                    text: "hello from the framework".into(),
                    received: 1,
                })
                .await?;
            println!("{}", text.expect("valid input"));
            client.close().await?;
            Ok::<(), Error>(())
        })
    });
    // Polling, native task destruction and child cleanup stay on this owner.
    driver.await;
    framework.join().expect("framework thread panicked")?;
    Ok(())
}
