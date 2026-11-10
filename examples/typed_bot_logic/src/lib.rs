use typed_bot_protocol::{module, Message, REPLY, SUMMARY};

#[reknot::handler(REPLY)]
async fn reply(message: Message) -> Result<String, String> {
    let text = message.text.trim();
    if text.is_empty() {
        return Err("Please enter a message.".into());
    }
    // Edit the label/helper and save. No watch, loading or byte-buffer code here.
    Ok(format!(
        "logic-v1 / message {}: {}",
        message.received,
        decorate(text)
    ))
}

fn decorate(text: &str) -> String {
    text.to_uppercase()
}

#[reknot::handler(SUMMARY)]
fn summary(messages: Vec<String>) -> Result<String, String> {
    if messages.is_empty() {
        return Err("No messages yet.".into());
    }
    Ok(format!(
        "{} messages kept by the host: {}",
        messages.len(),
        messages.join(", ")
    ))
}

// One export per crate. Both handlers and their helpers change together.
reknot::export_handlers!(module(), [reply, summary]);
