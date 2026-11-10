//! Copied wire messages only. No Bevy World, Entity reference or component crosses the DLL ABI.
use reknot::ServiceKey;
use serde::{Deserialize, Serialize};

pub const GAME: ServiceKey = ServiceKey::new(0x4741_4d45, 1);
pub const TICKS: ServiceKey = ServiceKey::new(0x5449_434b, 1);
pub const CLOCK: u32 = 0;
pub const PUBLISH: u32 = 1;
pub const MAX_MESSAGE: usize = 1024;
pub const MAX_QUEUE: usize = 64;

#[derive(Serialize, Deserialize)]
pub enum Request {
    Worker,
    Job {
        entity: u64,
        revision: u64,
        delay_frames: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    Started {
        generation: u64,
        ticks: u64,
    },
    Stopped {
        generation: u64,
        ticks: u64,
    },
    Step {
        generation: u64,
        ticks: u64,
        delta: i64,
    },
    Job {
        generation: u64,
        entity: u64,
        revision: u64,
        delta: i64,
    },
}
