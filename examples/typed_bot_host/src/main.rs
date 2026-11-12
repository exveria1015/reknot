use tokio::io::{AsyncBufReadExt, BufReader};
use typed_bot_protocol::{module, Message, REPLY, SUMMARY};

#[cfg(not(any(feature = "reload", feature = "static-logic")))]
compile_error!("enable reload or static-logic");

// ModuleHost/Call are owner-thread values. Use current_thread or a LocalSet;
// framework events from other threads need an owned-message bridge.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "reload")]
    let mut development = {
        use reknot::typed::{DevModule, DevOptions};
        let manifest =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../typed_bot_logic/Cargo.toml");
        eprintln!("Preparing typed_bot_logic (the first Cargo build can take a while)...");
        // SAFETY: this local project, future edits and dependencies are trusted
        // and obey the SDK native-code contracts. This does not auto-unload DLLs.
        unsafe {
            DevModule::start(
                module().builder(),
                DevOptions::new(manifest, "typed_bot_logic"),
            )
        }?
    };
    #[cfg(feature = "reload")]
    let host = development.host().clone();
    #[cfg(all(feature = "static-logic", not(feature = "reload")))]
    // SAFETY: generated callbacks are linked into this host and remain resident.
    let host = unsafe { module().builder().from_api(typed_bot_logic::handler_api()) }?;

    // Register these once with your framework. Calls use the current generation.
    let reply = host.handler(REPLY)?;
    let summary = host.handler(SUMMARY)?;
    println!(
        "Typed bot ready. Enter text, /summary or /quit. Edit typed_bot_logic/src/lib.rs and save."
    );
    let mut history = Vec::new();
    let mut received = 0u64;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(25));
    let mut pending: Option<reknot::typed::Call<Result<String, String>>> = None;
    let input_result = loop {
        tokio::select! {
            _ = tick.tick() => {
                #[cfg(feature = "reload")]
                if let Some(event) = development.update() { eprintln!("{event}"); }
                #[cfg(not(feature = "reload"))]
                let _ = host.poll_cleanup(&mut std::task::Context::from_waker(std::task::Waker::noop()));
            }
            result = async { pending.as_mut().unwrap().await }, if pending.is_some() => {
                pending = None;
                match result {
                    Ok(Ok(text)) => println!("{text}"),
                    Ok(Err(message)) => eprintln!("{message}"),
                    Err(error) => eprintln!("{error}"),
                }
            }
            line = lines.next_line() => {
                let text = match line {
                    Ok(Some(text)) => text,
                    Ok(None) => break Ok(()),
                    Err(error) => break Err(error),
                };
                if text == "/quit" { break Ok(()); }
                if pending.is_some() {
                    eprintln!("Request in progress; wait for its reply before sending more text.");
                    continue;
                }
                pending = Some(if text == "/summary" {
                    summary.call(history.clone())
                } else {
                    received += 1;
                    // State and connections live in the host, independently of logic.
                    // Keep this demo's history bounded too.
                    if history.len() == 100 { history.remove(0); }
                    history.push(text.clone());
                    reply.call(Message { text, received })
                });
            }
        }
    };
    // Dropping the root queues its registered cleanup before we await it.
    drop(pending);
    std::future::poll_fn(|cx| host.poll_cleanup(cx)).await;
    input_result?;
    Ok(())
}
