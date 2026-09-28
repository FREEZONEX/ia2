//! Sequential Function Chart (SFC) JSON schema.
//!
//! On disk as `pous/<slug>.sfc.json`. Loaded by the store, transpiled
//! to ST by `crates/ironplc-bridge/src/sfc_transpile.rs`, then compiled
//! by ironplc. ironplc does NOT have a native SFC codegen path; we
//! lower SFC to a plain ST `IF __sfc_step = '<name>' THEN …` dispatch
//! (not `CASE`: ironplc rejects a string selector, P4053).
//!
//! Design notes (see also MEMORY/graphical-languages.md § SFC):
//!
//! - State machine: **steps** are named states, **transitions** are
//!   `from → to` arrows with a boolean ST expression guarding them,
//!   **actions** are ST statements attached to steps with a
//!   "qualifier" controlling when they fire.
//! - Step names are string values stored in an internal `__sfc_step`
//!   variable — STRING, or WSTRING when a step name is not Latin-1.
//!   A string was chosen over an enum/DINT mainly for
//!   debuggability — operators see the actual step name in Monitor
//!   rather than `4`. If profiling ever shows string comparison as a
//!   hot path we'll change it.
//! - Qualifier MVP: `N` (while active), `S` (set on entry), `R`
//!   (reset on entry). P / P0 / P1 / time-qualified come later —
//!   they cover the last 10% of use cases and add a lot of edge-
//!   detection plumbing.
//! - We reuse `LdVariable` / `LdVarSection` for variable declarations
//!   (same reasoning as FBD — IEC VAR syntax is language-agnostic).
//! - **No positions in the schema**. SFC has a strict vertical
//!   step-over-transition-over-step layout; the renderer computes
//!   coordinates deterministically. Unlike FBD, there's nothing
//!   useful for the user to drag.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ld::{LdPouType, LdVariable};

/// Top-level SFC POU.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SfcProgram {
    pub name: String,
    #[serde(rename = "pou_type")]
    pub pou_type: LdPouType,
    pub variables: Vec<LdVariable>,
    /// Name of the step that's active when the program starts. Must
    /// be one of `steps[*].name`. Defaults to the first step at
    /// runtime if missing or unknown (transpiler emits a warning).
    pub initial_step: String,
    pub steps: Vec<SfcStep>,
    pub transitions: Vec<SfcTransition>,
}

/// One named state.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SfcStep {
    /// Unique within the POU. Used both as the on-the-wire identifier
    /// and as the source-map key. At runtime it is a string literal —
    /// STRING when every step name in the chart is Latin-1, WSTRING
    /// otherwise — so the transpiler refuses names containing `'` or `$`,
    /// names longer than 254 characters, and, in a WSTRING chart, names
    /// containing `"` or a character outside the Basic Multilingual Plane.
    pub name: String,
    /// Statements that fire while this step is active, on entry, etc.
    /// Order is preserved; the transpiler emits them in author order
    /// inside the step's `IF __sfc_step = 'name' THEN …` block
    /// (`"name"` when the chart's state is WSTRING).
    /// Always serialized; see `FbdProgram::outputs` for why we don't
    /// `skip_serializing_if = "Vec::is_empty"` here.
    #[serde(default)]
    pub actions: Vec<SfcAction>,
}

/// One action attached to a step.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SfcAction {
    pub qualifier: SfcQualifier,
    /// Inline ST statement(s). Passed through verbatim into the
    /// transpiler's wrapping `IF` block. Multi-statement bodies are
    /// fine (`a := 1; b := 2;`) — ironplc parses them as a sequence.
    pub body: String,
}

/// Action qualifier — when does the body fire?
///
/// MVP subset of IEC 61131-3 § 2.6.4.5. Time-modified qualifiers
/// (`L` / `D` / `SD` / `DS` / `SL`) and edge qualifiers
/// (`P` / `P1` / `P0`) come in a later phase.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "UPPERCASE")]
pub enum SfcQualifier {
    /// **N**on-stored: the body runs every scan the step is active. It is
    /// not run again when the step ends, so an assignment it made stays
    /// in place — `inlet_valve := TRUE` in `filling` leaves the valve open
    /// after `filling` ends unless a later step's action closes it (the
    /// SFC template does this with an `R` action in `idle`).
    N,
    /// **S**et: fires once when the step becomes active. Body
    /// typically latches an output (`drum_motor := TRUE`).
    S,
    /// **R**eset: fires once when the step becomes active. Same
    /// transpile semantics as `S`; the qualifier is kept as
    /// documentation — bodies for `R` typically deassert outputs
    /// (`drum_motor := FALSE`). Treat the distinction as authoring
    /// intent, not runtime behaviour.
    R,
}

/// One transition between two steps.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SfcTransition {
    /// Source step name.
    pub from: String,
    /// Target step name.
    pub to: String,
    /// Inline ST boolean expression. `start_btn`, `tank_full AND NOT
    /// estop`, `(temp_pv > setpoint)`. Wrapped in parentheses by the
    /// transpiler so AND precedence is unambiguous.
    pub condition: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ld::LdVarSection;

    #[test]
    fn round_trips_minimal_sfc() {
        let prog = SfcProgram {
            name: "batch".into(),
            pou_type: LdPouType::Program,
            variables: vec![LdVariable {
                name: "start".into(),
                type_name: "BOOL".into(),
                section: LdVarSection::Input,
                init: None,
            }],
            initial_step: "idle".into(),
            steps: vec![
                SfcStep {
                    name: "idle".into(),
                    actions: vec![],
                },
                SfcStep {
                    name: "running".into(),
                    actions: vec![SfcAction {
                        qualifier: SfcQualifier::N,
                        body: "motor := TRUE".into(),
                    }],
                },
            ],
            transitions: vec![
                SfcTransition {
                    from: "idle".into(),
                    to: "running".into(),
                    condition: "start".into(),
                },
                SfcTransition {
                    from: "running".into(),
                    to: "idle".into(),
                    condition: "NOT start".into(),
                },
            ],
        };
        let json = serde_json::to_string_pretty(&prog).unwrap();
        let back: SfcProgram = serde_json::from_str(&json).unwrap();
        assert_eq!(back.steps.len(), 2);
        assert_eq!(back.transitions.len(), 2);
        assert_eq!(back.steps[1].actions[0].qualifier, SfcQualifier::N);
    }

    #[test]
    fn qualifier_round_trips_as_uppercase() {
        let raw = r#"{"qualifier":"N","body":"x := 1"}"#;
        let a: SfcAction = serde_json::from_str(raw).unwrap();
        assert_eq!(a.qualifier, SfcQualifier::N);
        let back = serde_json::to_string(&a).unwrap();
        assert!(back.contains("\"N\""), "got: {back}");
    }
}
