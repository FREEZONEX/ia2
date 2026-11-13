use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::metrics::{Leg, Metrics};
use super::{Run, RunResult};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Comparison {
    pub(super) id: String,
    parameter: String,
    max_start_span_counts: u64,
    max_relative_target_delta_counts: u64,
    design: Design,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Design {
    Pair {
        reference: String,
        candidate: String,
    },
    Aba {
        before: String,
        candidate: String,
        after: String,
    },
}

#[derive(Serialize)]
pub(super) struct ComparisonResult {
    id: String,
    parameter: String,
    reference_runs: Vec<String>,
    candidate_run: String,
    exclusion_reason: Option<String>,
    result: Option<Comparable>,
}

#[derive(Serialize)]
struct Comparable {
    compared_parameters: usize,
    reference_raw: Vec<u8>,
    candidate_raw: Vec<u8>,
    start_span_counts: u64,
    relative_target_delta_counts: u64,
    directions: Vec<DirectionResult>,
}

#[derive(Serialize)]
struct DirectionResult {
    direction: i8,
    legs_per_group: usize,
    steady_speed_rms_rpm: Effect,
    following_bias_counts: Effect,
    position_bias_counts: Effect,
    overshoot_counts: Option<Effect>,
    turnaround_torque_pct: Option<Effect>,
    startup_following_peak_counts: Option<Effect>,
}

#[derive(Serialize)]
struct Effect {
    references: Vec<f64>,
    candidate: f64,
    reference_span: f64,
    reduction: f64,
    reduction_pct: Option<f64>,
    assessment: Assessment,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Assessment {
    LowerThanReferences,
    HigherThanReferences,
    WithinReferenceRange,
    Equal,
}

impl Effect {
    fn new(references: Vec<f64>, candidate: f64) -> Self {
        let min = references.iter().copied().fold(f64::INFINITY, f64::min);
        let max = references.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = references.iter().sum::<f64>() / references.len() as f64;
        let assessment = if candidate < min {
            Assessment::LowerThanReferences
        } else if candidate > max {
            Assessment::HigherThanReferences
        } else if min == max {
            Assessment::Equal
        } else {
            Assessment::WithinReferenceRange
        };
        Self {
            references,
            candidate,
            reference_span: max - min,
            reduction: mean - candidate,
            reduction_pct: (mean > 0.0).then(|| (1.0 - candidate / mean) * 100.0),
            assessment,
        }
    }
}

type EvaluatedRun<'a> = (&'a Run, &'a RunResult);

pub(super) fn analyze(
    comparison: &Comparison,
    by_id: &HashMap<&str, EvaluatedRun<'_>>,
) -> Result<ComparisonResult, String> {
    let (ids, candidate) = match &comparison.design {
        Design::Pair {
            reference,
            candidate,
        } => (vec![reference], candidate),
        Design::Aba {
            before,
            candidate,
            after,
        } => (vec![before, after], candidate),
    };
    let references = ids
        .iter()
        .map(|id| {
            by_id
                .get(id.as_str())
                .copied()
                .ok_or_else(|| format!("{} refers to an unknown run {id}", comparison.id))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let candidate_run = by_id
        .get(candidate.as_str())
        .copied()
        .ok_or_else(|| format!("{} refers to an unknown run {candidate}", comparison.id))?;
    let (result, exclusion_reason) = match comparable(comparison, &references, candidate_run) {
        Ok(result) => (Some(result), None),
        Err(reason) => (None, Some(reason)),
    };
    Ok(ComparisonResult {
        id: comparison.id.clone(),
        parameter: comparison.parameter.clone(),
        reference_runs: ids.into_iter().cloned().collect(),
        candidate_run: candidate.clone(),
        exclusion_reason,
        result,
    })
}

fn comparable(
    comparison: &Comparison,
    references: &[EvaluatedRun<'_>],
    candidate: EvaluatedRun<'_>,
) -> Result<Comparable, String> {
    let candidate_run = candidate.0;
    let reference_run = references[0].0;
    let all = std::iter::once(candidate).chain(references.iter().copied());
    let mut sources = HashSet::new();
    let mut evaluated_metrics = Vec::with_capacity(references.len() + 1);
    for (run, result) in all {
        let metrics = result
            .metrics
            .as_ref()
            .ok_or_else(|| format!("run {} has status {:?}", run.id, result.status))?;
        if !sources.insert(run.source.sha256.to_ascii_lowercase()) {
            return Err("comparison reuses a capture".into());
        }
        if candidate_run
            .cohort
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
            || run.cohort != candidate_run.cohort
            || run.profile != candidate_run.profile
        {
            return Err("comparison needs the same cohort and motion profile".into());
        }
        evaluated_metrics.push(metrics);
    }
    let (metrics, reference_metrics) = evaluated_metrics.split_first().expect("candidate included");
    let position = metrics.startup.position_counts;
    let (low, high) =
        reference_metrics
            .iter()
            .fold((position, position), |(low, high), metrics| {
                let position = metrics.startup.position_counts;
                (low.min(position), high.max(position))
            });
    let span = high.abs_diff(low);
    if span > comparison.max_start_span_counts {
        return Err(format!(
            "enable positions span {span} counts, beyond the comparison tolerance"
        ));
    }
    let target = &candidate_run.samples[metrics.command_interval.clone()];
    let origin = target[0].target_counts;
    let mut relative_target_delta_counts = 0;
    for ((run, _), metrics) in references.iter().zip(reference_metrics) {
        let other = &run.samples[metrics.command_interval.clone()];
        if other.len() != target.len() {
            return Err("recorded target intervals have different slot counts".into());
        }
        let other_origin = other[0].target_counts;
        let delta = other
            .iter()
            .zip(target)
            .map(|(a, b)| (a.target_counts - other_origin).abs_diff(b.target_counts - origin))
            .max()
            .unwrap_or(0);
        relative_target_delta_counts = relative_target_delta_counts.max(delta);
    }
    if relative_target_delta_counts > comparison.max_relative_target_delta_counts {
        return Err(format!("relative targets differ by {relative_target_delta_counts} counts, beyond the comparison tolerance"));
    }
    let baseline = &reference_run.parameter_readback;
    let variant = &candidate_run.parameter_readback;
    let key = &comparison.parameter;
    let reference_raw = baseline.get(key).filter(|v| !v.is_empty());
    let candidate_raw = variant.get(key).filter(|v| !v.is_empty());
    let (Some(reference_raw), Some(candidate_raw)) = (reference_raw, candidate_raw) else {
        return Err("the selected parameter lacks independent readback".into());
    };
    if reference_raw.len() != candidate_raw.len()
        || reference_raw == candidate_raw
        || references
            .iter()
            .any(|(run, _)| &run.parameter_readback != baseline)
        || baseline.len() != variant.len()
        || baseline
            .iter()
            .any(|(k, value)| value.is_empty() || (k != key && variant.get(k) != Some(value)))
    {
        return Err("parameter readbacks must differ only on the selected parameter".into());
    }
    let directions = [1, -1]
        .into_iter()
        .filter_map(|direction| summarize_direction(direction, reference_metrics, metrics))
        .collect();
    Ok(Comparable {
        compared_parameters: baseline.len(),
        reference_raw: reference_raw.clone(),
        candidate_raw: candidate_raw.clone(),
        start_span_counts: span,
        relative_target_delta_counts,
        directions,
    })
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut sorted: Vec<_> = values.collect();
    sorted.sort_by(f64::total_cmp);
    (sorted[(sorted.len() - 1) / 2] + sorted[sorted.len() / 2]) / 2.0
}

fn summarize_direction(
    direction: i8,
    references: &[&Metrics],
    candidate: &Metrics,
) -> Option<DirectionResult> {
    let group = |metrics: &Metrics, value: fn(&Leg) -> f64| {
        median(
            metrics
                .legs
                .iter()
                .filter(|leg| leg.direction == direction)
                .map(value),
        )
    };
    let legs = candidate
        .legs
        .iter()
        .filter(|leg| leg.direction == direction)
        .count();
    let first = candidate
        .legs
        .iter()
        .find(|leg| leg.direction == direction)?;
    let effect = |value| {
        Effect::new(
            references.iter().map(|m| group(m, value)).collect(),
            group(candidate, value),
        )
    };
    Some(DirectionResult {
        direction,
        legs_per_group: legs,
        steady_speed_rms_rpm: effect(|l| l.steady_speed_rms_rpm),
        following_bias_counts: effect(|l| l.steady_following_error_mean_counts.abs()),
        position_bias_counts: effect(|l| l.steady_position_error_mean_counts.abs()),
        overshoot_counts: first
            .overshoot_counts
            .map(|_| effect(|l| l.overshoot_counts.unwrap() as f64)),
        turnaround_torque_pct: first
            .turnaround_torque_peak_pct
            .map(|_| effect(|l| l.turnaround_torque_peak_pct.unwrap())),
        startup_following_peak_counts: (candidate.legs.len() == 1).then(|| {
            Effect::new(
                references
                    .iter()
                    .map(|m| f64::from(m.startup.following_error_peak_counts))
                    .collect(),
                f64::from(candidate.startup.following_error_peak_counts),
            )
        }),
    })
}
