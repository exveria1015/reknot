use reknot::typed::{Handler, Module, WorkerState};
use reknot::{ServiceKey, ServiceRequirement};
use serde::{Deserialize, Serialize};

pub const INFO: Handler<(), RuleInfo> = Handler::new("quiz.info", 1);
pub const ANSWER: Handler<AnswerInput, Result<Verdict, String>> = Handler::new("quiz.answer", 1);
pub const CLOCK: Handler<(), Result<(), String>> = Handler::new("quiz.clock", 1);
pub const STATE: WorkerState<ClockState> = WorkerState::new("quiz.clock-state", 1);
pub const SERVICE: ServiceKey = ServiceKey::new(730, 1);
pub const REQUIRED: &[ServiceRequirement] = &[ServiceRequirement::new(1, SERVICE)];
pub const SLEEP: u32 = 0;
pub const PULSE: u32 = 1;

#[reknot::schema]
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleInfo {
    pub label: String,
    pub correct_points: u32,
    pub round_ms: u64,
    pub tick_ms: u64,
    pub generation: u64,
}

#[reknot::schema]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerInput {
    pub correct: u8,
    pub choice: u8,
    pub delay_ms: u64,
}

#[reknot::schema]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub correct: bool,
    pub points: u32,
    pub generation: u64,
    pub label: String,
}

#[reknot::schema]
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClockState {
    pub ticks: u64,
}

#[reknot::schema]
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClockPulse {
    pub ticks: u64,
    pub generation: u64,
    pub label: String,
}

pub fn module() -> Module {
    module_version(1)
}
pub fn module_version(version: u32) -> Module {
    Module::new("example.quiz", version)
        .with(INFO)
        .with(ANSWER)
        .with(CLOCK)
        .with_state(STATE)
}
