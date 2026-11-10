use reknot::typed::{Handler, Module};
use serde::{Deserialize, Serialize};

// Bump these versions when the wire format OR meaning changes, then restart
// the host. The handler's Rust function name can change without changing its ID.
pub const REPLY: Handler<Message, Result<String, String>> = Handler::new("bot.reply", 1);
pub const SUMMARY: Handler<Vec<String>, Result<String, String>> = Handler::new("bot.summary", 1);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub text: String,
    pub received: u64,
}

pub fn module() -> Module {
    Module::new("example.bot", 1).with(REPLY).with(SUMMARY)
}
