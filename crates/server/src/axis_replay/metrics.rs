use std::ops::Range;

use serde::{Deserialize, Serialize};

use super::{Sample, Sampling, MAX_ABS_COUNTS};

pub(super) const PEAK_NS: u64 = 20_000_000;
const SPEED_NS: u64 = 100_000_000;
const STARTUP_NS: u64 = 2_000_000_000;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Profile {
    Single {
        command_rpm: f64,
        steady_slots: u32,
        settle_ns: u64,
        encoder_span: Option<[i64; 2]>,
    },
    Reversal {
        command_rpm: f64,
        endpoints: [i64; 2],
        ramp_slots: u16,
        steady_slots: u16,
        decel_slots: u16,
        hold_slots: u16,
        legs: u16,
        steady_tail_slots: u16,
    },
}

#[derive(Debug, Serialize)]
pub(super) struct Metrics {
    pub(super) startup: Startup,
    pub(super) speed_peak_20ms_rpm: f64,
    pub(super) torque_peak_pct: f64,
    pub(super) legs: Vec<Leg>,
    #[serde(skip)]
    pub(super) command_interval: Range<usize>,
}

#[derive(Debug, Serialize)]
pub(super) struct Startup {
    pub(super) position_counts: i64,
    pub(super) speed_peak_20ms_rpm: f64,
    pub(super) following_error_peak_counts: u32,
}

#[derive(Debug, Serialize)]
pub(super) struct Leg {
    pub(super) direction: i8,
    pub(super) steady_samples: usize,
    pub(super) steady_speed_mean_rpm: f64,
    pub(super) steady_speed_rms_rpm: f64,
    pub(super) steady_following_error_mean_counts: f64,
    pub(super) steady_position_error_mean_counts: f64,
    pub(super) overshoot_counts: Option<i64>,
    pub(super) hold_end_error_counts: Option<i64>,
    pub(super) turnaround_torque_peak_pct: Option<f64>,
}

struct Trace<'a> {
    samples: &'a [Sample],
    sampling: Sampling,
    enabled: Range<usize>,
    command_off: usize,
}

impl<'a> Trace<'a> {
    fn new(samples: &'a [Sample], sampling: Sampling) -> Result<Self, &'static str> {
        let start = samples.iter().position(Sample::enabled);
        let last = samples.iter().rposition(Sample::enabled);
        let (Some(start), Some(last)) = (start, last) else {
            return Err("axis enable feedback is missing");
        };
        let enabled = start..last + 1;
        let command_start = samples.iter().position(Sample::enable_command);
        let command_last = samples.iter().rposition(Sample::enable_command);
        let (Some(command_start), Some(command_last)) = (command_start, command_last) else {
            return Err("axis enable command is missing");
        };
        let command_off = command_last + 1;
        if command_start == 0
            || command_start > start
            || start >= command_off
            || command_off > enabled.end
            || enabled.end == samples.len()
            || samples[command_start..command_off]
                .iter()
                .any(|sample| !sample.enable_command())
            || samples[enabled.clone()].iter().any(|s| !s.enabled())
        {
            return Err("a single disabled-to-disabled enable interval is required");
        }
        Ok(Self {
            samples,
            sampling,
            enabled,
            command_off,
        })
    }

    fn speed(&self, index: usize, width: usize) -> f64 {
        let a = &self.samples[index - width];
        let b = &self.samples[index];
        (b.position_counts - a.position_counts) as f64 * 60e9
            / (f64::from(self.sampling.counts_per_revolution.get()) * (b.dc_ns - a.dc_ns) as f64)
    }

    fn speed_peak(&self, range: Range<usize>) -> f64 {
        let width = self.sampling.width(PEAK_NS);
        range
            .map(|i| self.speed(i, width).abs())
            .fold(0.0, f64::max)
    }

    fn torque_peak(&self, range: Range<usize>) -> f64 {
        let peak = self.samples[range]
            .iter()
            .map(|s| s.torque_permille.unsigned_abs())
            .max();
        f64::from(peak.unwrap_or_default()) / 10.0
    }

    fn startup(&self) -> Result<Startup, &'static str> {
        let start = self.enabled.start;
        let elapsed = |i: usize| self.samples[i].dc_ns - self.samples[start].dc_ns;
        if elapsed(self.enabled.end - 1) < STARTUP_NS {
            return Err("the two-second startup window is incomplete");
        }
        let end = start
            + self.samples[self.enabled.clone()]
                .partition_point(|s| s.dc_ns - self.samples[start].dc_ns <= STARTUP_NS);
        Ok(Startup {
            position_counts: self.samples[start].position_counts,
            speed_peak_20ms_rpm: self.speed_peak(start + self.sampling.width(PEAK_NS)..end),
            following_error_peak_counts: self.samples[start..end]
                .iter()
                .map(|s| s.following_error_counts.unsigned_abs())
                .max()
                .unwrap_or_default(),
        })
    }

    fn steady_metrics(&self, steady: Range<usize>, direction: i8) -> Leg {
        let n = steady.len() as f64;
        let width = self.sampling.width(SPEED_NS);
        let mean = steady.clone().map(|i| self.speed(i, width)).sum::<f64>() / n;
        let variance = steady
            .clone()
            .map(|i| (self.speed(i, width) - mean).powi(2))
            .sum::<f64>()
            / n;
        let samples = &self.samples[steady];
        Leg {
            direction,
            steady_samples: samples.len(),
            steady_speed_mean_rpm: mean,
            steady_speed_rms_rpm: variance.sqrt(),
            steady_following_error_mean_counts: samples
                .iter()
                .map(|s| f64::from(s.following_error_counts))
                .sum::<f64>()
                / n,
            steady_position_error_mean_counts: samples
                .iter()
                .map(|s| (s.target_counts - s.position_counts) as f64)
                .sum::<f64>()
                / n,
            overshoot_counts: None,
            hold_end_error_counts: None,
            turnaround_torque_peak_pct: None,
        }
    }

    fn single(
        &self,
        rpm: f64,
        steady_slots: u32,
        settle_ns: u64,
        encoder_span: Option<[i64; 2]>,
    ) -> Result<Vec<Leg>, &'static str> {
        let width = self.sampling.width(SPEED_NS);
        if rpm == 0.0 || steady_slots as usize <= width {
            return Err("single move needs nonzero speed and a plateau longer than 100 ms");
        }
        let direction = if rpm > 0.0 { 1 } else { -1 };
        if self.samples[self.enabled.start + 1..self.command_off]
            .windows(2)
            .any(|pair| i64::from(direction) * (pair[1].target_counts - pair[0].target_counts) < 0)
        {
            return Err("commanded target moves opposite the single-move direction");
        }
        let counts = f64::from(self.sampling.counts_per_revolution.get());
        let rpm_per_count = 60e9 / (counts * SPEED_NS as f64);
        let tolerance = 0.05_f64.min(rpm.abs() * 0.05);
        // The setpoint advances on nominal PLC slots. Feedback speed instead
        // uses the measured reference-clock interval.
        let is_steady = |i: usize| {
            let delta = self.samples[i].target_counts - self.samples[i - width].target_counts;
            (delta as f64 * rpm_per_count - rpm).abs() <= tolerance
        };
        let search = (self.enabled.start + width..self.command_off).filter(|&i| is_steady(i));
        let (mut length, mut first, mut last) = (0_usize, 0, 0);
        for i in search {
            if length == 0 {
                first = i;
            }
            last = i;
            length += 1;
        }
        if length.abs_diff(steady_slots as usize) > width || last + 1 - first != length {
            return Err("the commanded steady plateau is incomplete or split");
        }
        let settled_start = (first..=last)
            .find(|&i| self.samples[i].dc_ns - self.samples[first].dc_ns >= settle_ns)
            .ok_or("the requested settling time leaves no steady samples")?;
        let settled = settled_start..last + 1;
        let window = if let Some([low, high]) = encoder_span {
            self.position_window(settled, direction, low, high)?
        } else {
            settled
        };
        Ok(vec![self.steady_metrics(window, direction)])
    }

    fn position_window(
        &self,
        settled: Range<usize>,
        direction: i8,
        low: i64,
        high: i64,
    ) -> Result<Range<usize>, &'static str> {
        let (start, end) = if direction > 0 {
            (low, high)
        } else {
            (high, low)
        };
        let reached = |i: usize, count: i64| {
            let position = self.samples[i].position_counts;
            if direction > 0 {
                position >= count
            } else {
                position <= count
            }
        };
        if low >= high
            || (reached(settled.start, start)
                && self.samples[settled.start].position_counts != start)
        {
            return Err("the settled plateau starts beyond the requested encoder span");
        }
        let first = settled.clone().find(|&i| reached(i, start));
        let last = settled.into_iter().find(|&i| reached(i, end));
        match (first, last) {
            (Some(a), Some(b)) if a < b => Ok(a..b),
            _ => Err("the settled plateau does not cover the encoder span"),
        }
    }

    fn reversal(&self, profile: &Profile) -> Result<Vec<Leg>, &'static str> {
        let Profile::Reversal {
            command_rpm,
            endpoints: [low, high],
            ramp_slots,
            steady_slots,
            decel_slots,
            hold_slots,
            legs,
            steady_tail_slots,
        } = *profile
        else {
            unreachable!("reversal profile")
        };
        let (ramp, steady, decel, hold, legs, tail) = (
            usize::from(ramp_slots),
            usize::from(steady_slots),
            usize::from(decel_slots),
            usize::from(hold_slots),
            usize::from(legs),
            usize::from(steady_tail_slots),
        );
        let width = self.sampling.width(SPEED_NS);
        if command_rpm <= 0.0
            || low >= high
            || [low, high]
                .iter()
                .any(|n| n.unsigned_abs() > MAX_ABS_COUNTS)
            || ramp == 0
            || decel == 0
            || hold == 0
            || hold > decel
            || tail == 0
            || steady < tail + width
            || legs < 2
            || !legs.is_multiple_of(2)
        {
            return Err("invalid reversal geometry, leg count, or steady tail");
        }
        let leg_slots = ramp + steady + decel + hold;
        let block_slots = leg_slots
            .checked_mul(legs)
            .ok_or("reversal duration overflow")?;
        let start = self
            .command_off
            .checked_sub(block_slots)
            .filter(|&i| i >= self.enabled.start)
            .ok_or("the enabled interval does not cover the reversal profile")?;
        let counts_per_slot = command_rpm
            * f64::from(self.sampling.counts_per_revolution.get())
            * f64::from(self.sampling.cycle_ns.get())
            / 60e9;
        let travel = high as f64 - low as f64;
        let expected_travel =
            counts_per_slot * (ramp as f64 / 2.0 + steady as f64 + decel as f64 / 2.0);
        if (travel - expected_travel).abs() > 2.0 {
            return Err("reversal endpoints differ from the declared trapezoid travel");
        }
        let mut result = Vec::with_capacity(legs);
        for leg in 0..legs {
            let first = start + leg * leg_slots;
            let end = first + leg_slots;
            let hold_start = end - hold;
            let (direction, origin, endpoint) = if leg % 2 == 0 {
                (1, low, high)
            } else {
                (-1, high, low)
            };
            for (offset, s) in self.samples[first..end].iter().enumerate() {
                let slot = offset + 1;
                let distance = if slot <= ramp {
                    counts_per_slot * (slot as f64).powi(2) / (2.0 * ramp as f64)
                } else if slot <= ramp + steady {
                    counts_per_slot * (slot as f64 - ramp as f64 / 2.0)
                } else {
                    let remaining = (ramp + steady + decel).saturating_sub(slot);
                    travel - counts_per_slot * (remaining as f64).powi(2) / (2.0 * decel as f64)
                };
                let sent = (s.target_counts as f64 - origin as f64) * f64::from(direction);
                if (sent - distance).abs() > 2.0 {
                    return Err("recorded target differs from the reversal profile");
                }
            }
            let plateau_end = first + ramp + steady;
            let mut row = self.steady_metrics(plateau_end - tail..plateau_end, direction);
            row.overshoot_counts = self.samples[hold_start..end]
                .iter()
                .map(|s| i64::from(direction) * (s.position_counts - endpoint))
                .max()
                .map(|peak| peak.max(0));
            row.hold_end_error_counts = Some(endpoint - self.samples[end - 1].position_counts);
            row.turnaround_torque_peak_pct =
                Some(self.torque_peak(hold_start - hold..(end + hold).min(self.command_off)));
            result.push(row);
        }
        Ok(result)
    }
}

pub(super) fn measure(
    samples: &[Sample],
    sampling: Sampling,
    profile: &Profile,
) -> Result<Metrics, &'static str> {
    let trace = Trace::new(samples, sampling)?;
    let startup = trace.startup()?;
    let legs = match *profile {
        Profile::Single {
            command_rpm,
            steady_slots,
            settle_ns,
            encoder_span,
        } => trace.single(command_rpm, steady_slots, settle_ns, encoder_span)?,
        Profile::Reversal { .. } => trace.reversal(profile)?,
    };
    let speed_start = trace.enabled.start.max(sampling.width(PEAK_NS));
    Ok(Metrics {
        startup,
        speed_peak_20ms_rpm: trace.speed_peak(speed_start..trace.enabled.end),
        torque_peak_pct: trace.torque_peak(trace.enabled.clone()),
        legs,
        command_interval: trace.enabled.start..trace.command_off,
    })
}
