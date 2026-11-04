//! Read-only diagnostics for the bounded PLC / bus-cycle handoff.
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CycleStatus {
    pub period_us: u64,
    pub issued: u64,
    pub committed: u64,
    pub consumed: u64,
    pub fault: Option<String>,
    /// Recent events only; monotonic host microseconds from this gate's birth.
    pub events: Vec<CycleEvent>,
    pub events_dropped: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CycleEvent {
    pub sequence: u64,
    pub kind: String,
    pub host_us: u64,
    pub deadline_us: Option<u64>,
}
