//! Compiler-matched Rust dylib fixture, not a stable public support ABI.
pub use reknot::*;
use std::cell::Cell;

// Native TLS is allowed here: the Windows loader initializes this PE DLL.
thread_local! { static PARSES: Cell<u64> = const { Cell::new(0) }; }

// Keep serde's non-generic work resident. Re-exporting serde alone would not
// keep logic-specific generic instantiations out of each COFF generation.
#[inline(never)]
pub fn parse_total(bytes: &[u8]) -> Result<u64, ServiceError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| ServiceError::INVALID_ARGUMENT)?;
    let total = value
        .get("total")
        .and_then(serde_json::Value::as_u64)
        .ok_or(ServiceError::INVALID_ARGUMENT)?;
    PARSES.with(|count| count.set(count.get() + 1));
    Ok(total)
}

#[inline(never)]
pub fn parse_count() -> u64 {
    PARSES.with(Cell::get)
}

// Test-only resident synchronization. Only joining the actual thread proves return.
static TAIL: std::sync::Mutex<(bool, bool)> = std::sync::Mutex::new((false, false));
static TAIL_WAKE: std::sync::Condvar = std::sync::Condvar::new();
pub fn tail_barrier() {
    let mut state = TAIL.lock().unwrap();
    state.0 = true;
    while !state.1 {
        state = TAIL_WAKE.wait(state).unwrap();
    }
}
#[no_mangle]
pub extern "C" fn layered_tail_entered() -> bool {
    TAIL.lock().unwrap().0
}
#[no_mangle]
pub extern "C" fn layered_release_tail() {
    TAIL.lock().unwrap().1 = true;
    TAIL_WAKE.notify_all();
}
