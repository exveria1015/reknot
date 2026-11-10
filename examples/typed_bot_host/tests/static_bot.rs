#![cfg(all(feature = "static-logic", not(feature = "reload")))]
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn console_bot_keeps_host_history_across_typed_calls() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_typed_bot_host"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hello\nworld\n/summary\n/quit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("logic-v1 / message 1: HELLO"), "{text}");
    assert!(text.contains("logic-v1 / message 2: WORLD"), "{text}");
    assert!(
        text.contains("2 messages kept by the host: hello, world"),
        "{text}"
    );
}
