//! Shared gear channel contract for static I/O validation and adapter routing.
//!
//! These channels address the engine's parameter mailbox, not PDO bytes.
//!
//! This crate OWNS the contract; `iomap-ethercat` consumes it. Both the linter
//! and the real/simulated adapters resolve gear routes from here, so a second
//! copy on the adapter side would be the drift this module exists to prevent.
use std::collections::HashSet;

use crate::EthercatGear;

/// Output bytes the in-cycle gear engine writes every scan, starting at the
/// gear's `target_pos_offset` on its follower slave.
///
/// Mirrors `iomap_ethercat::gear::write_i32`, which copies exactly four
/// little-endian bytes of the i32 target into the output PDI after the output
/// phase has run. A PLC Output mapping onto those bytes is therefore
/// overwritten every cycle — it validates, deploys, and does nothing.
pub const GEAR_TARGET_BYTES: u16 = 4;

/// Writable engine parameters. Not every engine operation is exposed by
/// the device facade; see [`EthercatGear::routed_channels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GearParam {
    Engage,
    /// Reserved: in the schema and the engine, but NOT routed by the device
    /// facade. See [`EthercatGear::routed_channels`] — do not "complete" the
    /// catalog by adding it.
    RatioApply,
    RatioNum,
    RatioDen,
    RatioStep,
    PhaseOfs,
    MasterVel,
    MaxTravel,
}

/// Parameter echoes and read-only engine feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GearReadback {
    Engage,
    /// Reserved, not routed — see [`GearParam::RatioApply`].
    RatioApply,
    /// Reserved, not routed — see [`GearParam::RatioApply`].
    RatioAck,
    RatioNum,
    RatioDen,
    RatioStep,
    PhaseOfs,
    MasterVel,
    MaxTravel,
    Engaged,
    Trip,
}

impl GearReadback {
    /// Parameters are readable echoes as well as writable; feedback is not.
    pub fn parameter(self) -> Option<GearParam> {
        match self {
            Self::Engage => Some(GearParam::Engage),
            Self::RatioApply => Some(GearParam::RatioApply),
            Self::RatioNum => Some(GearParam::RatioNum),
            Self::RatioDen => Some(GearParam::RatioDen),
            Self::RatioStep => Some(GearParam::RatioStep),
            Self::PhaseOfs => Some(GearParam::PhaseOfs),
            Self::MasterVel => Some(GearParam::MasterVel),
            Self::MaxTravel => Some(GearParam::MaxTravel),
            Self::RatioAck | Self::Engaged | Self::Trip => None,
        }
    }

    pub fn is_bool(self) -> bool {
        matches!(
            self,
            Self::Engage | Self::RatioApply | Self::Engaged | Self::Trip
        )
    }
}

impl EthercatGear {
    /// The nine routes currently exposed by both real and simulated devices.
    /// Names come from this configuration, including any user overrides.
    ///
    /// Ratio-apply/ack exist in the schema and engine but are not wired into
    /// the device facade. Do not advertise them merely because they parse:
    /// exposing those motion operations requires its own behavioral change.
    ///
    /// Duplicate names make the catalog ambiguous and which entry a routing
    /// table would keep is unspecified — callers building routes or resolving
    /// a channel must clear [`validate_gear_channel_names`] first.
    pub fn routed_channels(&self) -> [(&str, GearReadback); 9] {
        [
            (&self.engage_channel, GearReadback::Engage),
            (&self.ratio_num_channel, GearReadback::RatioNum),
            (&self.ratio_den_channel, GearReadback::RatioDen),
            (&self.ratio_step_channel, GearReadback::RatioStep),
            (&self.phase_channel, GearReadback::PhaseOfs),
            (&self.master_vel_channel, GearReadback::MasterVel),
            (&self.max_travel_channel, GearReadback::MaxTravel),
            (&self.engaged_channel, GearReadback::Engaged),
            (&self.trip_channel, GearReadback::Trip),
        ]
    }
}

impl EthercatGear {
    /// Does this gear's engine own `[start, start + len)` of `slave`'s OUTPUT
    /// PDI? The engine writes its target there after the output phase, so a
    /// PLC Output mapping onto those bytes never reaches the wire.
    ///
    /// Only the target window is owned. `actual_pos_offset` and
    /// `status_word_offset` are INPUT bytes the engine reads; mapping those as
    /// Input is exactly right and must not be flagged.
    pub fn owns_output_bytes(&self, slave: u16, start: u16, len: u16) -> bool {
        if slave != self.slave_index {
            return false;
        }
        let end = start.saturating_add(len.max(1));
        let gear_end = self.target_pos_offset.saturating_add(GEAR_TARGET_BYTES);
        start < gear_end && self.target_pos_offset < end
    }
}

/// Reject ambiguous names before either validation or runtime routing can
/// choose a winner. Keep the existing reservation of ratio-apply/ack names,
/// even though those two schema fields do not currently create routes.
pub fn validate_gear_channel_names(
    gears: &[EthercatGear],
    pdo_names: &HashSet<&str>,
) -> Result<(), String> {
    let mut seen = HashSet::new();
    // Report every collision, not just the first: fixing a config one error per
    // re-run is the linter experience this crate already rejects elsewhere
    // (see `validate_reports_all_errors_at_once`).
    let mut problems = Vec::new();
    for gear in gears {
        let names = gear.routed_channels().map(|(name, _)| name);
        for name in names.into_iter().chain([
            gear.ratio_apply_channel.as_str(),
            gear.ratio_ack_channel.as_str(),
        ]) {
            if pdo_names.contains(name) {
                problems.push(format!(
                    "gear channel '{name}' collides with a PDO channel name"
                ));
            } else if !seen.insert(name) {
                problems.push(format!(
                    "gear channel '{name}' is used by more than one gear axis or parameter"
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}
