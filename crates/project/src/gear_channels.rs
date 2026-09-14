//! Shared gear channel contract for static I/O validation and adapter routing.
//!
//! These channels address the engine's parameter mailbox, not PDO bytes.
use std::collections::HashSet;

use crate::EthercatGear;

/// Writable engine parameters. Not every engine operation is exposed by
/// the device facade; see [`EthercatGear::routed_channels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GearParam {
    Engage,
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
    RatioApply,
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

/// Reject ambiguous names before either validation or runtime routing can
/// choose a winner. Keep the existing reservation of ratio-apply/ack names,
/// even though those two schema fields do not currently create routes.
pub fn validate_gear_channel_names(
    gears: &[EthercatGear],
    pdo_names: &HashSet<&str>,
) -> Result<(), String> {
    let mut seen = HashSet::new();
    for gear in gears {
        let names = gear.routed_channels().map(|(name, _)| name);
        for name in names.into_iter().chain([
            gear.ratio_apply_channel.as_str(),
            gear.ratio_ack_channel.as_str(),
        ]) {
            if pdo_names.contains(name) {
                return Err(format!(
                    "gear channel '{name}' collides with a PDO channel name"
                ));
            }
            if !seen.insert(name) {
                return Err(format!(
                    "gear channel '{name}' is used by more than one gear axis or parameter"
                ));
            }
        }
    }
    Ok(())
}
