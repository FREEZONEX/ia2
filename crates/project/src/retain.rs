//! Project-owned RETAIN contract. Stable identities are explicit, not release hashes.
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ColdStartCondition {
    Missing,
    Corrupt,
    UnsupportedVersion,
    LegacyUnverified,
    IdentityMismatch,
    LayoutMismatch,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum LossAction {
    #[default]
    Warn,
    Inhibit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default, deny_unknown_fields)]
pub struct RetainPolicy {
    pub project_id: Option<String>,
    pub instance_id: String,
    /// Fully qualified PROGRAM-instance.variable keys; no ambiguous bare names.
    pub required: Vec<String>,
    pub allow_cold_start: Vec<ColdStartCondition>,
    #[ts(type = "number")]
    pub sample_interval_ms: u64,
    #[ts(type = "number | null")]
    pub max_loss_ms: Option<u64>,
    pub on_loss: LossAction,
}
impl Default for RetainPolicy {
    fn default() -> Self {
        Self {
            project_id: None,
            instance_id: "default".into(),
            required: vec![],
            allow_cold_start: vec![ColdStartCondition::Missing],
            sample_interval_ms: 5000,
            max_loss_ms: None,
            on_loss: LossAction::Warn,
        }
    }
}
impl RetainPolicy {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
    pub fn validate(&self) -> Result<(), String> {
        if self
            .project_id
            .as_ref()
            .is_some_and(|id| id.trim().is_empty())
            || self.instance_id.trim().is_empty()
        {
            return Err("RETAIN identities must be nonempty".into());
        }
        if !(10..=3_600_000).contains(&self.sample_interval_ms) {
            return Err("RETAIN sample_interval_ms must be 10..3600000".into());
        }
        if self.max_loss_ms == Some(0) {
            return Err("RETAIN max_loss_ms must be positive".into());
        }
        if self.on_loss == LossAction::Inhibit && self.max_loss_ms.is_none() {
            return Err("RETAIN inhibit requires max_loss_ms".into());
        }
        let mut keys = std::collections::HashSet::new();
        for key in &self.required {
            if !key.contains('.')
                || key.trim() != key
                || key.is_empty()
                || !keys.insert(key.to_lowercase())
            {
                return Err("RETAIN required must contain unique instance.variable keys".into());
            }
        }
        if !self.required.is_empty() && self.project_id.is_none() {
            return Err("RETAIN required data needs project_id".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_policies_are_rejected_and_defaults_are_explicit() {
        assert!(RetainPolicy::default().validate().is_ok());
        assert!(toml::from_str::<RetainPolicy>("max_lost_ms = 10").is_err());
        for policy in [
            RetainPolicy {
                project_id: Some(" ".into()),
                ..Default::default()
            },
            RetainPolicy {
                required: vec!["value".into()],
                ..Default::default()
            },
            RetainPolicy {
                sample_interval_ms: 0,
                ..Default::default()
            },
            RetainPolicy {
                on_loss: LossAction::Inhibit,
                ..Default::default()
            },
            RetainPolicy {
                project_id: Some("p".into()),
                required: vec!["a.value".into(), "A.VALUE".into()],
                ..Default::default()
            },
        ] {
            assert!(policy.validate().is_err());
        }
    }
}
