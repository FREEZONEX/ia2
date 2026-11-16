//! Wire types for the V1 offline axis replay request.

use std::collections::BTreeMap;
use std::num::{NonZeroU16, NonZeroU32};

use serde::{Deserialize, Serialize};

pub const MAX_SAMPLES: usize = 100_000;
pub const MAX_PARAMETERS_PER_RUN: usize = 256;
pub const MAX_READBACK_BYTES: usize = 1024;
pub const PEAK_NS: u64 = 20_000_000;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Sampling {
    pub cycle_ns: NonZeroU32,
    pub counts_per_revolution: NonZeroU32,
    pub expected_wkc: NonZeroU16,
}

impl Sampling {
    pub fn width(self, window_ns: u64) -> usize {
        (window_ns / u64::from(self.cycle_ns.get())) as usize
    }

    pub fn supports_cycle(cycle_ns: u32) -> bool {
        cycle_ns != 0 && PEAK_NS.is_multiple_of(u64::from(cycle_ns))
    }

    /// None means consecutive. Some(0) still reports a discontinuity,
    /// e.g. a repeated or backwards timestamp.
    pub fn dc_gap(self, previous: u64, current: u64) -> Option<u64> {
        let period = u64::from(self.cycle_ns.get());
        let step = (current / period).saturating_sub(previous / period);
        let interval = current.checked_sub(previous);
        let allowed = (period / 2).max(1)..=period + period / 2;
        (step != 1 || !interval.is_some_and(|dt| allowed.contains(&dt)))
            .then(|| step.saturating_sub(1))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub id: String,
    pub source: Source,
    pub evidence: Evidence,
    pub cohort: Option<String>,
    pub profile: Option<Profile>,
    #[serde(default)]
    pub parameter_readback: BTreeMap<String, Vec<u8>>,
    pub samples: Vec<Sample>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub zero_capture_drops: bool,
    pub request_consumed: bool,
    pub plc_started: Option<bool>,
    pub normal_completion: bool,
    pub terminal_reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub dc_ns: u64,
    pub position_counts: i64,
    pub target_counts: i64,
    pub following_error_counts: i32,
    pub torque_permille: i16,
    pub controlword: u16,
    pub statusword: u16,
    pub error_code: u16,
    pub wkc: u16,
}

impl Sample {
    pub fn enabled(&self) -> bool {
        self.statusword & 0x006f == 0x0027
    }

    pub fn enable_command(&self) -> bool {
        self.controlword & 0x008f == 0x000f
    }

    pub fn drive_fault(&self) -> bool {
        self.error_code != 0 || self.statusword & 0x0008 != 0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Profile {
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
