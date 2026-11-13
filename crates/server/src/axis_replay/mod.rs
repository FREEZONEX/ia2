//! Offline analysis of an already captured axis. The API accepts data, not paths to open.

mod compare;
mod metrics;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet};
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, FromRequest, Request};
use axum::routing::{post, MethodRouter};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::error::ApiError;
use compare::{Comparison, ComparisonResult};
use metrics::{Metrics, Profile};

const MAX_BODY_BYTES: usize = 24 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 2;
const MAX_RUNS: usize = 32;
const MAX_COMPARISONS: usize = 32;
const MAX_SAMPLES: usize = 100_000;
const MAX_PARAMETERS_PER_RUN: usize = 256;
const MAX_READBACK_BYTES: usize = 1024;
const MAX_ABS_COUNTS: u64 = 1 << 52;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplayRequest {
    schema_version: u32,
    sampling: Sampling,
    runs: Vec<Run>,
    #[serde(default)]
    comparisons: Vec<Comparison>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sampling {
    cycle_ns: NonZeroU32,
    counts_per_revolution: NonZeroU32,
    expected_wkc: NonZeroU16,
}

impl Sampling {
    fn width(self, window_ns: u64) -> usize {
        (window_ns / u64::from(self.cycle_ns.get())) as usize
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Source {
    path: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    id: String,
    source: Source,
    evidence: Evidence,
    cohort: Option<String>,
    profile: Option<Profile>,
    #[serde(default)]
    parameter_readback: BTreeMap<String, Vec<u8>>,
    samples: Vec<Sample>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    zero_capture_drops: bool,
    request_consumed: bool,
    plc_started: Option<bool>,
    normal_completion: bool,
    terminal_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sample {
    dc_ns: u64,
    position_counts: i64,
    target_counts: i64,
    following_error_counts: i32,
    torque_permille: i16,
    controlword: u16,
    statusword: u16,
    error_code: u16,
    wkc: u16,
}

impl Sample {
    fn enabled(&self) -> bool {
        self.statusword & 0x006f == 0x0027
    }

    fn enable_command(&self) -> bool {
        self.controlword & 0x008f == 0x000f
    }

    fn drive_fault(&self) -> bool {
        self.error_code != 0 || self.statusword & 0x0008 != 0
    }
}

#[derive(Serialize)]
pub(crate) struct ReplayResult {
    schema_version: u32,
    runs: Vec<RunResult>,
    comparisons: Vec<ComparisonResult>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Analyzed,
    DcDiscontinuity,
    PreEnableAbort,
    InvalidCapture,
    InvalidWkc,
    DriveFault,
    Incomplete,
    InvalidProfile,
}

#[derive(Serialize)]
struct RunResult {
    id: String,
    source: Source,
    status: Status,
    reason: Option<String>,
    terminal_reason: Option<String>,
    capture: CaptureQuality,
    metrics: Option<Metrics>,
}

#[derive(Default, Serialize)]
struct CaptureQuality {
    samples: usize,
    wkc_mismatches: usize,
    drive_fault_frames: usize,
    enabled_frames: usize,
    enable_command_frames: usize,
    dc_discontinuities: usize,
    dc_missing_slots: u64,
    first_dc_discontinuity: Option<DcDiscontinuity>,
}

#[derive(Serialize)]
struct DcDiscontinuity {
    sample_index: usize,
    previous_dc_ns: u64,
    dc_ns: u64,
    missing_slots: u64,
}

fn inspect_capture(samples: &[Sample], sampling: Sampling) -> CaptureQuality {
    let period = u64::from(sampling.cycle_ns.get());
    let mut quality = CaptureQuality {
        samples: samples.len(),
        ..CaptureQuality::default()
    };
    for sample in samples {
        quality.wkc_mismatches += usize::from(sample.wkc != sampling.expected_wkc.get());
        quality.drive_fault_frames += usize::from(sample.drive_fault());
        quality.enabled_frames += usize::from(sample.enabled());
        quality.enable_command_frames += usize::from(sample.enable_command());
    }
    let min_interval = (period / 2).max(1);
    let max_interval = period + period / 2;
    for (index, pair) in samples.windows(2).enumerate() {
        let previous = pair[0].dc_ns;
        let current = pair[1].dc_ns;
        let slot_step = (current / period).saturating_sub(previous / period);
        let interval = current.checked_sub(previous);
        if slot_step != 1 || !interval.is_some_and(|dt| (min_interval..=max_interval).contains(&dt))
        {
            let missing_slots = slot_step.saturating_sub(1);
            quality.dc_discontinuities += 1;
            quality.dc_missing_slots = quality.dc_missing_slots.saturating_add(missing_slots);
            quality
                .first_dc_discontinuity
                .get_or_insert(DcDiscontinuity {
                    sample_index: index + 1,
                    previous_dc_ns: previous,
                    dc_ns: current,
                    missing_slots,
                });
        }
    }
    quality
}

fn measure(
    run: &Run,
    sampling: Sampling,
    quality: &CaptureQuality,
) -> Result<Metrics, (Status, String)> {
    let fail = |status, reason: &str| (status, reason.to_owned());
    if !run.evidence.zero_capture_drops || quality.samples < 2 {
        return Err(fail(
            Status::InvalidCapture,
            "capture drops are unknown or present, or there are fewer than two samples",
        ));
    }
    if run.samples.iter().any(|sample| sample.dc_ns == 0) {
        return Err(fail(
            Status::InvalidCapture,
            "capture includes a zero reference-clock timestamp",
        ));
    }
    if quality.dc_discontinuities > 0 {
        return Err(fail(
            Status::DcDiscontinuity,
            "reference-clock slots are not consecutive",
        ));
    }
    if quality.wkc_mismatches > 0 {
        return Err(fail(
            Status::InvalidWkc,
            "working counter differs from the declared value",
        ));
    }
    if quality.drive_fault_frames > 0 {
        return Err(fail(
            Status::DriveFault,
            "drive error or fault feedback was recorded",
        ));
    }
    if quality.enabled_frames == 0
        && quality.enable_command_frames == 0
        && run.evidence.request_consumed
        && run.evidence.plc_started == Some(false)
        && !run.evidence.normal_completion
    {
        return Err(fail(
            Status::PreEnableAbort,
            "request consumed without a PLC start or axis enable",
        ));
    }
    if !run.evidence.request_consumed
        || run.evidence.plc_started != Some(true)
        || !run.evidence.normal_completion
    {
        return Err(fail(
            Status::Incomplete,
            "request consumption, PLC start, or normal completion is unconfirmed",
        ));
    }
    if run.samples.iter().any(|s| {
        s.position_counts.unsigned_abs() > MAX_ABS_COUNTS
            || s.target_counts.unsigned_abs() > MAX_ABS_COUNTS
    }) {
        return Err(fail(
            Status::InvalidProfile,
            "encoder counts exceed the supported range",
        ));
    }
    let profile = run
        .profile
        .as_ref()
        .ok_or_else(|| fail(Status::InvalidProfile, "motion profile is missing"))?;
    metrics::measure(&run.samples, sampling, profile)
        .map_err(|reason| fail(Status::InvalidProfile, reason))
}

fn analyze(request: ReplayRequest) -> Result<ReplayResult, String> {
    if request.schema_version != 1 {
        return Err("axis replay requires schema_version 1".into());
    }
    if !metrics::PEAK_NS.is_multiple_of(u64::from(request.sampling.cycle_ns.get())) {
        return Err("cycle_ns must divide the 20 ms speed window".into());
    }
    if !(1..=MAX_RUNS).contains(&request.runs.len()) {
        return Err(format!("provide between 1 and {MAX_RUNS} runs per request"));
    }
    if request.comparisons.len() > MAX_COMPARISONS {
        return Err(format!(
            "provide at most {MAX_COMPARISONS} comparisons per request"
        ));
    }
    if request.runs.iter().map(|r| r.samples.len()).sum::<usize>() > MAX_SAMPLES {
        return Err(format!("provide at most {MAX_SAMPLES} samples per request"));
    }
    let mut ids = HashSet::new();
    for run in &request.runs {
        if run.id.trim().is_empty() || !ids.insert(run.id.as_str()) {
            return Err(format!("empty or duplicate run ID {:?}", run.id));
        }
        if run.source.path.trim().is_empty()
            || run.source.sha256.len() != 64
            || !run.source.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(format!(
                "{} requires a source path label and SHA-256",
                run.id
            ));
        }
        if run.parameter_readback.len() > MAX_PARAMETERS_PER_RUN {
            return Err(format!(
                "{} exceeds {MAX_PARAMETERS_PER_RUN} parameter readbacks",
                run.id
            ));
        }
        if let Some((parameter, _)) = run
            .parameter_readback
            .iter()
            .find(|(_, value)| value.len() > MAX_READBACK_BYTES)
        {
            return Err(format!(
                "{} parameter {parameter:?} readback exceeds {MAX_READBACK_BYTES} bytes",
                run.id
            ));
        }
    }
    let runs: Vec<_> = request
        .runs
        .iter()
        .map(|run| {
            let capture = inspect_capture(&run.samples, request.sampling);
            let (status, reason, metrics) = match measure(run, request.sampling, &capture) {
                Ok(metrics) => (Status::Analyzed, None, Some(metrics)),
                Err((status, reason)) => (status, Some(reason), None),
            };
            RunResult {
                id: run.id.clone(),
                source: run.source.clone(),
                status,
                reason,
                terminal_reason: run.evidence.terminal_reason.clone(),
                capture,
                metrics,
            }
        })
        .collect();
    let by_id = request
        .runs
        .iter()
        .zip(&runs)
        .map(|(run, result)| (run.id.as_str(), (run, result)))
        .collect();
    let mut comparison_ids = HashSet::new();
    let mut comparisons = Vec::with_capacity(request.comparisons.len());
    for comparison in &request.comparisons {
        if comparison.id.trim().is_empty() || !comparison_ids.insert(comparison.id.as_str()) {
            return Err(format!(
                "empty or duplicate comparison ID {:?}",
                comparison.id
            ));
        }
        comparisons.push(compare::analyze(comparison, &by_id)?);
    }
    Ok(ReplayResult {
        schema_version: 1,
        runs,
        comparisons,
    })
}

pub(crate) fn route<S: Clone + Send + Sync + 'static>() -> MethodRouter<S> {
    post(replay)
        .route_layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .route_layer(Extension(Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS))))
}

async fn replay(
    Extension(slots): Extension<Arc<Semaphore>>,
    request: Request,
) -> Result<Json<ReplayResult>, ApiError> {
    let permit = slots
        .try_acquire_owned()
        .map_err(|_| ApiError::Unavailable("axis replay is busy, retry later".into()))?;
    let Json(request) = Json::<ReplayRequest>::from_request(request, &())
        .await
        .map_err(|err| {
            if err.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::PayloadTooLarge(err.body_text())
            } else {
                ApiError::BadRequest(err.body_text())
            }
        })?;
    tokio::task::spawn_blocking(move || {
        // A disconnected client cannot release the slot while its work is running.
        let _permit = permit;
        analyze(request)
    })
    .await
    .map_err(|err| ApiError::Internal(format!("axis replay task failed: {err}")))?
    .map(Json)
    .map_err(ApiError::BadRequest)
}
