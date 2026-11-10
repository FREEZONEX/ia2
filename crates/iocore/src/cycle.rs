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
    /// Linux CLOCK_MONOTONIC timestamp corresponding to host_us = 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub host_mono_origin_ns: Option<u64>,
    /// Width of the origin calibration bracket, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub host_mono_uncertainty_ns: Option<u64>,
    /// Extrema over every accepted commit since the gate was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub handoff_timing: Option<CycleTiming>,
    /// Recent events only; monotonic host microseconds from this gate's birth.
    pub events: Vec<CycleEvent>,
    pub events_dropped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CycleTiming {
    #[ts(type = "number")]
    pub max_issue_to_begin_us: u64,
    #[ts(type = "number")]
    pub max_begin_to_commit_us: u64,
    #[ts(type = "number")]
    pub min_begin_margin_us: u64,
    #[ts(type = "number")]
    pub min_commit_margin_us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CycleEvent {
    pub sequence: u64,
    pub kind: String,
    pub host_us: u64,
    pub deadline_us: Option<u64>,
}
