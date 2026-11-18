//! Read-only exchange capture contract shared by the adapter and edge API.

use std::collections::BTreeMap;
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::axis_replay::{
    Evidence, Profile, Run, Sample, Sampling, Source, MAX_PARAMETERS_PER_RUN, MAX_READBACK_BYTES,
};

pub const MAX_DURATION_MS: u32 = 120_000;

pub trait AxisCapture: Send + Sync {
    fn status(&self) -> Status;
    fn start(&self, request: Start) -> Result<Progress, CaptureError>;
    fn stop(&self, id: &str) -> Result<Progress, CaptureError>;
    fn page(&self, id: &str, offset: usize) -> Result<SamplePage, CaptureError>;
    fn frozen(&self, id: &str) -> Result<Arc<Capture>, CaptureError>;
    /// Final access after the runtime has attempted its output safety and teardown.
    /// Return the retained archive with its original end reason if already frozen.
    fn freeze_for_shutdown(&self) -> Option<Arc<Capture>>;
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("no retained capture with that id")]
    NotFound,
    #[error("could not reserve the requested capture buffer")]
    Capacity,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub runtime_id: String,
    pub bus_generation: u64,
    pub slave: u16,
    pub counts_per_revolution: NonZeroU32,
    pub max_samples: usize,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub runtime_id: String,
    pub bus_generation: u64,
    pub device: String,
    pub slave: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlaveIdentity {
    pub index: u16,
    pub name: String,
    pub vendor_id: u32,
    pub product_id: u32,
    pub revision: Option<u32>,
    pub serial: Option<u32>,
    pub input_bytes: u16,
    pub output_bytes: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bus {
    pub generation: u64,
    pub simulated: bool,
    pub cycle_ns: NonZeroU32,
    pub expected_wkc: NonZeroU16,
    pub capturable_slaves: Vec<u16>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub bus: Option<Bus>,
    pub capture: Option<Progress>,
}

#[derive(Debug, Serialize)]
pub struct Progress {
    pub capture_id: String,
    pub scope: Scope,
    pub samples: usize,
    pub dropped_samples: u32,
    pub end_reason: Option<EndReason>,
}

#[derive(Debug, Serialize)]
pub struct SamplePage {
    pub progress: Progress,
    pub offset: usize,
    pub samples: Vec<Exchange>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    OperatorStop,
    DurationLimit,
    SampleLimit,
    BusSessionEnded,
    WorkerStopped,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PdoField {
    pub role: &'static str,
    pub direction: &'static str,
    pub object: u16,
    pub sub_index: u8,
    pub offset_bytes: u16,
    pub width_bits: u8,
    pub signed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct Command {
    pub controlword: u16,
    pub target_raw: i32,
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct Feedback {
    pub statusword: u16,
    pub position_raw: i32,
    pub following_error_counts: i32,
    pub torque_permille: i16,
    pub error_code: u16,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputQuality {
    Fresh,
    UnexpectedWkc,
    NotOperational,
    Timeout,
    ExchangeFailed,
    #[default]
    MissingPdi,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Exchange {
    pub attempt: u64,
    pub tx_monotonic_ns: u64,
    pub rx_monotonic_ns: u64,
    pub output_ticket: Option<u64>,
    pub dc_ns: Option<u64>,
    pub wkc: Option<u16>,
    pub all_op: Option<bool>,
    pub input_quality: InputQuality,
    /// Number of missed bus slots relative to the preceding recorded
    /// attempt. Some(0) records duplicate, backwards or irregular timing.
    pub dc_gap: Option<u64>,
    pub command: Option<Command>,
    pub feedback: Option<Feedback>,
}

#[derive(Debug, Serialize)]
pub struct Capture {
    pub capture_schema_version: u32,
    pub capture_id: String,
    pub scope: Scope,
    pub slave: SlaveIdentity,
    pub mapping: [PdoField; 7],
    pub sampling: Sampling,
    pub simulated: bool,
    pub max_samples: usize,
    pub duration_ms: u32,
    pub dropped_samples: u32,
    pub end_reason: Option<EndReason>,
    pub samples: Vec<Exchange>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayImport {
    pub run_id: String,
    pub plc: PlcEvidence,
    pub profile: Option<Profile>,
    pub cohort: Option<String>,
    pub parameter_readback: Option<ParameterEvidence>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlcEvidence {
    pub capture_id: String,
    pub scope: Scope,
    pub request_consumed: bool,
    pub plc_started: Option<bool>,
    pub normal_completion: bool,
    pub terminal_reason: Option<String>,
    pub scan_count_before: Option<u64>,
    pub scan_count_after: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParameterEvidence {
    pub scope: Scope,
    pub values: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, Serialize)]
pub struct ReplayExport {
    pub schema_version: u32,
    pub sampling: Sampling,
    pub runs: [Run; 1],
}

/// A signed 32-bit encoder wraps in both directions. Advance with the
/// shortest modular delta between consecutive values. Bounded capture
/// sizes keep the accumulated values inside i64.
#[derive(Default)]
struct Encoder(Option<(i32, i64)>);

impl Encoder {
    fn lift(&mut self, raw: i32, initial: i64) -> i64 {
        let count = self.0.map_or(initial, |(previous, count)| {
            count + i64::from(raw.wrapping_sub(previous))
        });
        self.0 = Some((raw, count));
        count
    }
}

impl Capture {
    pub fn replay(
        &self,
        import: ReplayImport,
        source: Source,
    ) -> Result<ReplayExport, CaptureError> {
        if self.end_reason.is_none() {
            return Err(CaptureError::Conflict("capture is still recording"));
        }
        let invalid = |message: &str| CaptureError::Invalid(message.into());
        let plc = import.plc;
        if plc.scope != self.scope || plc.capture_id != self.capture_id {
            return Err(invalid(
                "PLC evidence belongs to a different capture or bus session",
            ));
        }
        if import.run_id.trim().is_empty() {
            return Err(invalid("run_id is required"));
        }
        let scans = plc.scan_count_before.zip(plc.scan_count_after);
        if plc.scan_count_before.is_some() != plc.scan_count_after.is_some()
            || scans.is_some_and(|(before, after)| after < before)
        {
            return Err(invalid("provide an ordered pair of PLC scan counts"));
        }
        let parameter_readback = if let Some(parameters) = import.parameter_readback {
            if parameters.scope != self.scope {
                return Err(invalid(
                    "parameter evidence belongs to a different axis or bus session",
                ));
            }
            if parameters.values.len() > MAX_PARAMETERS_PER_RUN
                || parameters.values.iter().any(|(id, value)| {
                    id.trim().is_empty() || value.is_empty() || value.len() > MAX_READBACK_BYTES
                })
            {
                return Err(invalid("invalid parameter IDs, count or byte lengths"));
            }
            parameters.values
        } else {
            BTreeMap::new()
        };
        if plc.normal_completion
            && (!plc.request_consumed
                || plc.plc_started != Some(true)
                || scans.is_none_or(|(before, after)| after <= before)
                || import.profile.is_none()
                || parameter_readback.is_empty())
        {
            return Err(invalid(
                "normal completion requires PLC execution and scan counts, a motion profile and parameter readback",
            ));
        }

        let mut replayable = self.dropped_samples == 0
            && !matches!(
                self.end_reason,
                Some(EndReason::BusSessionEnded | EndReason::WorkerStopped)
            );
        let mut target = Encoder::default();
        let mut position = Encoder::default();
        let mut previous_attempt = None;
        let samples = self
            .samples
            .iter()
            .map(|row| {
                if previous_attempt
                    .is_some_and(|previous| row.attempt.checked_sub(previous) != Some(1))
                {
                    replayable = false;
                }
                previous_attempt = Some(row.attempt);
                // V1 has quality lanes for clock/WKC. For other missing
                // PDO evidence, force a V1 invalid_capture exclusion.
                replayable &= row.command.is_some()
                    && match row.input_quality {
                        InputQuality::Fresh => row.feedback.is_some(),
                        InputQuality::UnexpectedWkc => row
                            .wkc
                            .is_some_and(|wkc| wkc != self.sampling.expected_wkc.get()),
                        _ => false,
                    };
                let command = row.command.unwrap_or_default();
                let feedback = row.feedback.unwrap_or_default();
                let target_counts = row.command.map_or(0, |command| {
                    target.lift(command.target_raw, i64::from(command.target_raw))
                });
                let position_counts = row.feedback.map_or(0, |feedback| {
                    // Put the two encoders in the same coordinate frame
                    // if the first readable pair straddles a signed wrap.
                    let initial = if row.command.is_some() {
                        target_counts
                            + i64::from(feedback.position_raw.wrapping_sub(command.target_raw))
                    } else {
                        i64::from(feedback.position_raw)
                    };
                    position.lift(feedback.position_raw, initial)
                });
                Sample {
                    dc_ns: row.dc_ns.unwrap_or(0),
                    position_counts,
                    target_counts,
                    following_error_counts: feedback.following_error_counts,
                    torque_permille: feedback.torque_permille,
                    controlword: command.controlword,
                    statusword: feedback.statusword,
                    error_code: feedback.error_code,
                    wkc: row.wkc.unwrap_or(0),
                }
            })
            .collect();

        Ok(ReplayExport {
            schema_version: 1,
            sampling: self.sampling,
            runs: [Run {
                id: import.run_id,
                source,
                evidence: Evidence {
                    zero_capture_drops: replayable,
                    request_consumed: plc.request_consumed,
                    plc_started: plc.plc_started,
                    normal_completion: plc.normal_completion,
                    terminal_reason: plc.terminal_reason,
                },
                cohort: import.cohort,
                profile: import.profile,
                parameter_readback,
                samples,
            }],
        })
    }
}
