//! Static lint for `alarms.toml`: does each definition name a variable the
//! program actually has?
//!
//! The alarm engine matches a definition to a snapshot variable with `==` and
//! skips quietly when nothing matches (`monitor::alarms::note_snapshot`). A
//! definition naming a variable that does not exist therefore never evaluates,
//! and `GET /alarms` reports it as a calm, never-raised, already-acknowledged
//! entry — a green line claiming coverage that does not exist. Typos, an ST
//! rename that `alarms.toml` did not follow, and stale definitions all land
//! there, silently.
//!
//! This is the opposite failure from the one the iomap linter fixes. There, a
//! working binding looked broken: noisy, and someone investigates. Here a
//! broken alarm looks fine, which nobody investigates.

use std::collections::HashSet;

use crate::types::AlarmDef;

/// Reserved for runtime-generated device-health alarms, not project definitions.
pub const DEVICE_ALARM_PREFIX: &str = "__device/";

/// One finding, keyed to its position in `alarms.toml` (0-based) so a caller
/// can cite the offending entry. Every finding is an error: an alarm that
/// cannot fire is not a style question.
#[derive(Debug, Clone, PartialEq)]
pub struct AlarmIssue {
    pub alarm_index: usize,
    pub id: String,
    pub message: String,
}

/// Check every definition's `variable` against the names the program declares.
///
/// `declared` is every variable name in the project's ST sources, spelled as
/// declared — the compiler's debug map preserves that spelling, and so does
/// the snapshot, so a case-sensitive comparison is the same comparison the
/// engine will make. `instances` is the PROGRAM instance list from
/// `tasks.toml`.
///
/// A name is accepted in either form the snapshot can carry:
///   - bare — what a single-PROGRAM project always produces, and what a
///     multi-PROGRAM one produces for a name only one unit declares;
///   - `instance.name` — what a multi-PROGRAM project produces for a name
///     more than one unit declares.
///
/// **Known gap, deliberate.** It cannot tell that a *bare* name is one of the
/// shared ones, because that needs per-instance variable sets and static
/// extraction carries no POU attribution (`VariableInfo` is name/type/direction
/// only). So an alarm written bare against a shared variable still slips
/// through. Accepting both forms is what keeps this linter free of false
/// positives; flagging that case would need the compiler's per-unit debug maps,
/// which is a different (and much later) check.
pub fn validate_alarms(
    alarms: &[AlarmDef],
    declared: &HashSet<&str>,
    instances: &[String],
) -> Vec<AlarmIssue> {
    let mut issues = Vec::new();
    for (index, def) in alarms.iter().enumerate() {
        if def.id.starts_with(DEVICE_ALARM_PREFIX) {
            issues.push(AlarmIssue {
                alarm_index: index,
                id: def.id.clone(),
                message: format!(
                    "alarm id '{}' uses the reserved device-health prefix '{DEVICE_ALARM_PREFIX}'",
                    def.id
                ),
            });
        }
        if resolves(&def.variable, declared, instances) {
            continue;
        }
        issues.push(AlarmIssue {
            alarm_index: index,
            id: def.id.clone(),
            message: format!(
                "alarm '{id}' watches '{var}', which no POU in this project declares — \
                 the engine matches snapshot names exactly, so this alarm can never fire \
                 and will read as a calm, never-raised entry",
                id = def.id,
                var = def.variable,
            ),
        });
    }
    issues
}

fn resolves(variable: &str, declared: &HashSet<&str>, instances: &[String]) -> bool {
    if declared.contains(variable) {
        return true;
    }
    // `instance.name`: split at the FIRST dot, because that is how the runtime
    // composes it (`format!("{instance}.{name}")`) and IEC identifiers carry
    // no dots of their own.
    match variable.split_once('.') {
        Some((instance, name)) => {
            instances.iter().any(|i| i == instance) && declared.contains(name)
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AlarmCondition, AlarmSeverity};

    fn alarm(id: &str, variable: &str) -> AlarmDef {
        AlarmDef {
            id: id.into(),
            variable: variable.into(),
            condition: AlarmCondition::IsTrue,
            limit: None,
            deadband: 0.0,
            delay_ms: 0,
            severity: AlarmSeverity::default(),
            message: "m".into(),
        }
    }

    fn check(alarms: &[AlarmDef], declared: &[&str], instances: &[&str]) -> Vec<AlarmIssue> {
        let set: HashSet<&str> = declared.iter().copied().collect();
        let inst: Vec<String> = instances.iter().map(|s| s.to_string()).collect();
        validate_alarms(alarms, &set, &inst)
    }

    #[test]
    fn a_declared_bare_name_is_fine() {
        assert!(check(&[alarm("a", "level")], &["level", "tick"], &["main"]).is_empty());
        assert!(check(&[], &["level"], &["main"]).is_empty());
    }

    #[test]
    fn device_health_alarm_ids_are_reserved() {
        let issues = check(&[alarm("__device/bus0", "level")], &["level"], &["main"]);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("reserved"));
    }

    #[test]
    fn a_name_no_pou_declares_is_an_error_naming_the_alarm() {
        let issues = check(&[alarm("tank_hi", "levle")], &["level"], &["main"]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].alarm_index, 0);
        assert_eq!(issues[0].id, "tank_hi");
        assert!(issues[0].message.contains("'levle'"), "{:?}", issues[0]);
        assert!(
            issues[0].message.contains("can never fire"),
            "{:?}",
            issues[0]
        );
    }

    #[test]
    fn the_qualified_form_a_multi_program_snapshot_uses_is_accepted() {
        // Shared names arrive as `instance.name`; an alarm must say that, and
        // flagging it would be the false positive this linter must not make.
        assert!(check(
            &[alarm("a", "b_inst.level")],
            &["level"],
            &["a_inst", "b_inst"]
        )
        .is_empty());
    }

    #[test]
    fn a_qualified_name_is_still_checked_on_both_halves() {
        // Unknown instance — nothing will ever produce that prefix.
        assert_eq!(
            check(&[alarm("a", "ghost.level")], &["level"], &["main"]).len(),
            1
        );
        // Known instance, unknown variable.
        assert_eq!(
            check(&[alarm("a", "main.levle")], &["level"], &["main"]).len(),
            1
        );
    }

    #[test]
    fn case_matters_because_it_matters_to_the_engine() {
        // The compiler's debug map keeps the declared spelling and the engine
        // compares with `==`, so `Level` against a declared `level` really is
        // an alarm that never fires. Pinned end to end by
        // `runtime::tests::snapshot_names_match_the_declared_spelling`.
        assert_eq!(
            check(&[alarm("a", "Level")], &["level"], &["main"]).len(),
            1
        );
        assert!(check(&[alarm("a", "Level_SP")], &["Level_SP"], &["main"]).is_empty());
    }

    #[test]
    fn every_bad_definition_is_reported_not_just_the_first() {
        let issues = check(
            &[
                alarm("a", "nope"),
                alarm("b", "level"),
                alarm("c", "also_nope"),
            ],
            &["level"],
            &["main"],
        );
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert_eq!(issues[0].alarm_index, 0);
        assert_eq!(
            issues[1].alarm_index, 2,
            "index must point at the real entry"
        );
    }
}
