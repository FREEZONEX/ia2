//! Sequential Function Chart → Structured Text transpiler.
//!
//! Same architecture as LD / FBD:
//!
//!   pous/<name>.sfc.json   (canonical source)
//!     └── serde → project::SfcProgram   (typed AST)
//!         └── transpile_to_st(&prog) → String   (ST source)
//!             └── ironplc parser → DSL → codegen → bytecode
//!
//! Lowering strategy
//! -----------------
//!
//! ironplc has no native SFC codegen, but SFC is just a state machine —
//! it lowers cleanly to plain ST. We emit:
//!
//!   1. Two internal variables: `__sfc_step` (current state) and
//!      `__sfc_prev` (previous-scan state, for entry-edge detection).
//!      Both are STRING values holding the step name.
//!   2. **Per-step action block**: one `IF __sfc_step = 'name' THEN …`
//!      for each step. Inside, action bodies render in author order
//!      according to their qualifier:
//!      - N → unconditional body
//!      - S → `IF __sfc_prev <> 'name' THEN body END_IF;` (entry edge)
//!      - R → same shape as S; the distinction is documentation
//!        (S typically asserts, R typically deasserts).
//!   3. `__sfc_prev := __sfc_step;` — snapshot **before** transitions,
//!      so the next scan's action block sees the correct prev value
//!      for entry detection.
//!   4. **Transition cascade**: one big `IF / ELSIF` chain. First
//!      satisfied condition wins. Authoring order = priority. Each
//!      branch:
//!      `IF __sfc_step = '<from>' AND (<condition>) THEN
//!           __sfc_step := '<to>';
//!       ELSIF …`
//!
//! Why STRING (not DINT enum)
//! --------------------------
//! Monitor / `cs check` round-trips show the actual step name, which
//! is enormously easier to debug than `4`. STRING comparison in
//! ironplc is fine for the scan-rate workload we target (≤ 100 ms
//! cycle); if profiling ever flags it we switch to DINT IDs with a
//! commented-out lookup table.
//!
//! What STRING costs, and the guards that pay for it
//! -------------------------------------------------
//! ironplc stores STRING as Latin-1 and encodes a literal by keeping each
//! character's low byte (`encode_string_literal`, ADR-0016). It does not
//! process `$` escapes. Three consequences, each enforced here rather than
//! left to surface at runtime:
//!
//!   * **Length.** A value longer than the declared STRING length is
//!     truncated, so a longer step name never compares equal to its own
//!     literal — the chart enters that step and silently stops: no action
//!     runs, no transition leaves. The state variables are sized to the
//!     longest name (at least 31). ironplc's comparison also misbehaves at
//!     a declared length of 255 or more, so names are capped at 254.
//!   * **Collisions.** Two names whose characters share low bytes (`等` is
//!     U+7B49, stored as `I`) are the same value: both steps are active at
//!     once. Such pairs are rejected.
//!   * **`$`** is the IEC escape character; a raw one is malformed ST that
//!     ironplc happens to accept. Rejected, like `'`.
//!
//! Limitations (deferred for later phases)
//! ---------------------------------------
//! - Qualifier set is N / S / R. P / P0 / P1 / L / D / SD / DS / SL
//!   come later.
//! - No simultaneous branches (parallel divergence/convergence).
//! - No nested SFC. The step body is `actions[]`, not a sub-chart.

use std::collections::HashSet;

use project::{LdPouType, LdVarSection, LdVariable, SfcProgram, SfcQualifier, SfcStep};
use serde::Serialize;
use ts_rs::TS;

use crate::errors::BridgeError;

// =================================================================
//   Source map
// =================================================================

/// Where in the SFC source an ST line came from. Used by
/// `check_pou_source` to surface ironplc diagnostics with a useful
/// hint for the editor (highlight this step / this transition).
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SfcLocation {
    /// A user-declared variable.
    Variable { name: String },
    /// A step's action block (`IF __sfc_step = 'name' THEN` … `END_IF;`).
    Step { name: String },
    /// One action body inside a step's IF block.
    Action { step: String, action_index: usize },
    /// One transition rule in the cascade. `index` matches the
    /// position in `SfcProgram::transitions`.
    Transition { index: usize },
}

#[derive(Debug, Clone, Default)]
pub struct SfcSourceMap {
    pub lines: Vec<Option<SfcLocation>>,
}

impl SfcSourceMap {
    pub fn lookup(&self, line: usize) -> Option<&SfcLocation> {
        if line == 0 {
            return None;
        }
        self.lines.get(line - 1).and_then(|s| s.as_ref())
    }
}

struct StEmitter {
    out: String,
    map: Vec<Option<SfcLocation>>,
}

impl StEmitter {
    fn new() -> Self {
        Self {
            out: String::new(),
            map: Vec::new(),
        }
    }
    fn line(&mut self, span: Option<SfcLocation>, content: std::fmt::Arguments) {
        use std::fmt::Write;
        let _ = writeln!(self.out, "{content}");
        self.map.push(span);
    }
    fn blank(&mut self) {
        self.out.push('\n');
        self.map.push(None);
    }
}

// =================================================================
//   Entry points
// =================================================================

/// Render an `SfcProgram` to a complete ST source. Discards the
/// source map — use `transpile_to_st_with_map` for diagnostic
/// mapping.
pub fn transpile_to_st(prog: &SfcProgram) -> Result<String, BridgeError> {
    Ok(transpile_to_st_with_map(prog)?.0)
}

/// As above, but also returns the line-resolution source map.
pub fn transpile_to_st_with_map(prog: &SfcProgram) -> Result<(String, SfcSourceMap), BridgeError> {
    // ----- Validate -----
    if prog.name.is_empty() {
        return Err(BridgeError::Parse("SFC program name is empty".into()));
    }
    if prog.steps.is_empty() {
        return Err(BridgeError::Parse(format!(
            "SFC program '{}' has no steps",
            prog.name
        )));
    }
    let mut step_names: HashSet<&str> = HashSet::new();
    for s in &prog.steps {
        if s.name.is_empty() {
            return Err(BridgeError::Parse("SFC step has empty name".into()));
        }
        if s.name.contains('\'') {
            return Err(BridgeError::Parse(format!(
                "SFC step name '{}' contains a single quote — that breaks the STRING literal we lower to",
                s.name
            )));
        }
        if s.name.contains('$') {
            return Err(BridgeError::Parse(format!(
                "SFC step name '{}' contains '$', the IEC string escape character — rename the step",
                s.name
            )));
        }
        let chars = s.name.chars().count();
        if chars > MAX_STEP_NAME_CHARS {
            return Err(BridgeError::Parse(format!(
                "SFC step name '{}…' is {chars} characters; the runtime compares at most {MAX_STEP_NAME_CHARS}",
                s.name.chars().take(24).collect::<String>()
            )));
        }
        if !step_names.insert(s.name.as_str()) {
            return Err(BridgeError::Parse(format!(
                "SFC step name '{}' duplicated",
                s.name
            )));
        }
    }
    let mut stored_as: std::collections::HashMap<Vec<u8>, &str> = std::collections::HashMap::new();
    for s in &prog.steps {
        if let Some(other) = stored_as.insert(runtime_bytes(&s.name), s.name.as_str()) {
            return Err(BridgeError::Parse(format!(
                "SFC steps '{other}' and '{}' are the same value to the runtime — it stores STRING as \
                 Latin-1 and keeps only the low byte of other characters — so both would be active at \
                 once; rename one",
                s.name
            )));
        }
    }
    if !step_names.contains(prog.initial_step.as_str()) {
        return Err(BridgeError::Parse(format!(
            "SFC initial_step '{}' isn't one of the declared steps",
            prog.initial_step
        )));
    }
    for (i, t) in prog.transitions.iter().enumerate() {
        if !step_names.contains(t.from.as_str()) {
            return Err(BridgeError::Parse(format!(
                "SFC transition #{i} from='{}' references unknown step",
                t.from
            )));
        }
        if !step_names.contains(t.to.as_str()) {
            return Err(BridgeError::Parse(format!(
                "SFC transition #{i} to='{}' references unknown step",
                t.to
            )));
        }
        if t.condition.trim().is_empty() {
            return Err(BridgeError::Parse(format!(
                "SFC transition #{i} ('{}' → '{}') has empty condition",
                t.from, t.to
            )));
        }
    }
    // Action bodies non-empty.
    for s in &prog.steps {
        for (ai, a) in s.actions.iter().enumerate() {
            if a.body.trim().is_empty() {
                return Err(BridgeError::Parse(format!(
                    "SFC step '{}' action #{ai} has empty body",
                    s.name
                )));
            }
        }
    }

    // ----- Emit -----
    let mut em = StEmitter::new();
    let (head, foot) = match prog.pou_type {
        LdPouType::Program => ("PROGRAM", "END_PROGRAM"),
        LdPouType::FunctionBlock => ("FUNCTION_BLOCK", "END_FUNCTION_BLOCK"),
    };

    let state_len = prog
        .steps
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(MIN_STATE_LEN);

    em.line(None, format_args!("{} {}", head, prog.name));
    write_variable_blocks(&mut em, &prog.variables, &prog.initial_step, state_len);
    em.blank();

    // --- Per-step action dispatch ---
    em.line(None, format_args!("    (* === SFC actions === *)"));
    for step in &prog.steps {
        emit_step_actions(&mut em, step);
    }
    em.blank();

    // --- Snapshot prev (must be BEFORE the transition cascade) ---
    em.line(
        None,
        format_args!("    (* snapshot for next scan's entry-edge detection *)"),
    );
    em.line(None, format_args!("    __sfc_prev := __sfc_step;"));
    em.blank();

    // --- Transition cascade ---
    em.line(None, format_args!("    (* === SFC transitions === *)"));
    if !prog.transitions.is_empty() {
        for (i, t) in prog.transitions.iter().enumerate() {
            let cond_clause = format!("__sfc_step = '{}' AND ({})", t.from, t.condition.trim());
            let span = Some(SfcLocation::Transition { index: i });
            if i == 0 {
                em.line(span.clone(), format_args!("    IF {cond_clause} THEN"));
            } else {
                em.line(span.clone(), format_args!("    ELSIF {cond_clause} THEN"));
            }
            em.line(span, format_args!("        __sfc_step := '{}';", t.to));
        }
        em.line(None, format_args!("    END_IF;"));
    }

    em.line(None, format_args!("{foot}"));
    Ok((em.out, SfcSourceMap { lines: em.map }))
}

// =================================================================
//   Helpers
// =================================================================

/// Longest step name the runtime can compare. ironplc's STRING equality
/// fails even for exact-fit contents once the declared length reaches 255.
const MAX_STEP_NAME_CHARS: usize = 254;

/// The state variables' declared length when every name is short — kept
/// at the historical 31 so existing charts produce unchanged ST.
const MIN_STATE_LEN: usize = 31;

/// The bytes ironplc stores for a STRING literal of `name`: one byte per
/// character, its low eight bits (`encode_string_literal`).
fn runtime_bytes(name: &str) -> Vec<u8> {
    name.chars().map(|c| (u32::from(c) & 0xFF) as u8).collect()
}

fn write_variable_blocks(
    em: &mut StEmitter,
    vars: &[LdVariable],
    initial_step: &str,
    state_len: usize,
) {
    for section in [
        LdVarSection::Input,
        LdVarSection::Output,
        LdVarSection::Internal,
    ] {
        let header = match section {
            LdVarSection::Input => "VAR_INPUT",
            LdVarSection::Output => "VAR_OUTPUT",
            LdVarSection::Internal => "VAR",
        };
        em.line(None, format_args!("    {header}"));
        for v in vars.iter().filter(|v| v.section == section) {
            let init = v
                .init
                .as_ref()
                .map(|s| format!(" := {s}"))
                .unwrap_or_default();
            em.line(
                Some(SfcLocation::Variable {
                    name: v.name.clone(),
                }),
                format_args!("        {} : {}{};", v.name, v.type_name, init),
            );
        }
        // The internal VAR block also carries the two SFC state vars.
        if section == LdVarSection::Internal {
            em.line(
                None,
                format_args!("        __sfc_step : STRING[{state_len}] := '{initial_step}';"),
            );
            em.line(
                None,
                format_args!("        __sfc_prev : STRING[{state_len}] := '';"),
            );
        }
        em.line(None, format_args!("    END_VAR"));
    }
}

fn emit_step_actions(em: &mut StEmitter, step: &SfcStep) {
    if step.actions.is_empty() {
        return;
    }
    let step_span = Some(SfcLocation::Step {
        name: step.name.clone(),
    });
    em.line(
        step_span.clone(),
        format_args!("    IF __sfc_step = '{}' THEN", step.name),
    );
    for (ai, action) in step.actions.iter().enumerate() {
        let action_span = Some(SfcLocation::Action {
            step: step.name.clone(),
            action_index: ai,
        });
        match action.qualifier {
            SfcQualifier::N => {
                emit_action_body(em, &action.body, action_span, /*indent*/ 8);
            }
            SfcQualifier::S | SfcQualifier::R => {
                // Entry-edge guard. We emit S and R identically; the
                // qualifier label is documentation, not runtime
                // behaviour. Action bodies decide whether to assert
                // (S) or deassert (R) the relevant outputs.
                em.line(
                    action_span.clone(),
                    format_args!(
                        "        IF __sfc_prev <> '{}' THEN  (* qualifier: {:?} *)",
                        step.name, action.qualifier
                    ),
                );
                emit_action_body(em, &action.body, action_span.clone(), /*indent*/ 12);
                em.line(action_span, format_args!("        END_IF;"));
            }
        }
    }
    em.line(step_span, format_args!("    END_IF;"));
}

/// Write an action body to the emitter, splitting on the user's own
/// newlines so each line gets its own source-map entry. The body is
/// trusted ST; the transpiler doesn't try to parse it.
fn emit_action_body(em: &mut StEmitter, body: &str, span: Option<SfcLocation>, indent: usize) {
    let pad = " ".repeat(indent);
    for line in body.trim_end_matches([' ', '\t']).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Preserve blank lines for readability but still record
            // a map slot so line counts stay aligned.
            em.line(span.clone(), format_args!(""));
            continue;
        }
        // Ensure the body ends with a semicolon — IEC ST requires
        // statement terminators. If the user already supplied one,
        // we won't double it up.
        let needs_semi =
            !trimmed.ends_with(';') && !trimmed.ends_with("END_IF") && !trimmed.ends_with("THEN");
        if needs_semi {
            em.line(span.clone(), format_args!("{pad}{trimmed};"));
        } else {
            em.line(span.clone(), format_args!("{pad}{trimmed}"));
        }
    }
}

// =================================================================
//   Tests
// =================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use project::{SfcAction, SfcStep, SfcTransition};

    fn batch_program() -> SfcProgram {
        SfcProgram {
            name: "batch".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "start_btn".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "tank_full".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "tank_empty".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "inlet_valve".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: Some("FALSE".into()),
                },
                LdVariable {
                    name: "drain_valve".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: Some("FALSE".into()),
                },
            ],
            initial_step: "idle".into(),
            steps: vec![
                SfcStep {
                    name: "idle".into(),
                    actions: vec![SfcAction {
                        qualifier: SfcQualifier::R,
                        body: "inlet_valve := FALSE;\ndrain_valve := FALSE".into(),
                    }],
                },
                SfcStep {
                    name: "filling".into(),
                    actions: vec![SfcAction {
                        qualifier: SfcQualifier::N,
                        body: "inlet_valve := TRUE".into(),
                    }],
                },
                SfcStep {
                    name: "draining".into(),
                    actions: vec![SfcAction {
                        qualifier: SfcQualifier::N,
                        body: "drain_valve := TRUE".into(),
                    }],
                },
            ],
            transitions: vec![
                SfcTransition {
                    from: "idle".into(),
                    to: "filling".into(),
                    condition: "start_btn".into(),
                },
                SfcTransition {
                    from: "filling".into(),
                    to: "draining".into(),
                    condition: "tank_full".into(),
                },
                SfcTransition {
                    from: "draining".into(),
                    to: "idle".into(),
                    condition: "tank_empty".into(),
                },
            ],
        }
    }

    #[test]
    fn three_step_machine_emits_expected_shape() {
        let st = transpile_to_st(&batch_program()).unwrap();
        // VAR has the two SFC bookkeeping variables.
        assert!(st.contains("__sfc_step : STRING[31] := 'idle';"));
        assert!(st.contains("__sfc_prev : STRING[31] := '';"));
        // Per-step action blocks
        assert!(st.contains("IF __sfc_step = 'filling' THEN"), "got:\n{st}");
        assert!(st.contains("inlet_valve := TRUE;"), "got:\n{st}");
        // Transition cascade
        assert!(
            st.contains("IF __sfc_step = 'idle' AND (start_btn) THEN"),
            "got:\n{st}"
        );
        assert!(
            st.contains("ELSIF __sfc_step = 'filling' AND (tank_full) THEN"),
            "got:\n{st}"
        );
        assert!(st.contains("__sfc_step := 'draining';"));
        // prev snapshot lives between actions and transitions
        let snap_pos = st.find("__sfc_prev := __sfc_step;").unwrap();
        let trans_pos = st.find("(* === SFC transitions === *)").unwrap();
        let actions_pos = st.find("(* === SFC actions === *)").unwrap();
        assert!(actions_pos < snap_pos && snap_pos < trans_pos, "got:\n{st}");
    }

    #[test]
    fn s_and_r_qualifiers_emit_entry_edge_guard() {
        let prog = SfcProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![LdVariable {
                name: "x".into(),
                type_name: "BOOL".into(),
                section: LdVarSection::Output,
                init: None,
            }],
            initial_step: "a".into(),
            steps: vec![
                SfcStep {
                    name: "a".into(),
                    actions: vec![SfcAction {
                        qualifier: SfcQualifier::S,
                        body: "x := TRUE".into(),
                    }],
                },
                SfcStep {
                    name: "b".into(),
                    actions: vec![],
                },
            ],
            transitions: vec![SfcTransition {
                from: "a".into(),
                to: "b".into(),
                condition: "TRUE".into(),
            }],
        };
        let st = transpile_to_st(&prog).unwrap();
        assert!(
            st.contains("IF __sfc_prev <> 'a' THEN"),
            "S qualifier should guard on entry edge; got:\n{st}"
        );
    }

    #[test]
    fn duplicate_step_names_error() {
        let prog = SfcProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![],
            initial_step: "a".into(),
            steps: vec![
                SfcStep {
                    name: "a".into(),
                    actions: vec![],
                },
                SfcStep {
                    name: "a".into(),
                    actions: vec![],
                },
            ],
            transitions: vec![],
        };
        let err = transpile_to_st(&prog).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("duplicated"), "{msg}");
    }

    #[test]
    fn unknown_initial_step_errors() {
        let prog = SfcProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![],
            initial_step: "ghost".into(),
            steps: vec![SfcStep {
                name: "a".into(),
                actions: vec![],
            }],
            transitions: vec![],
        };
        let err = transpile_to_st(&prog).unwrap_err();
        assert!(format!("{err:?}").contains("initial_step"), "{err:?}");
    }

    #[test]
    fn transition_to_unknown_step_errors() {
        let prog = SfcProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![],
            initial_step: "a".into(),
            steps: vec![SfcStep {
                name: "a".into(),
                actions: vec![],
            }],
            transitions: vec![SfcTransition {
                from: "a".into(),
                to: "ghost".into(),
                condition: "TRUE".into(),
            }],
        };
        let err = transpile_to_st(&prog).unwrap_err();
        assert!(format!("{err:?}").contains("ghost"), "{err:?}");
    }

    #[test]
    fn empty_transition_condition_errors() {
        let prog = SfcProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![],
            initial_step: "a".into(),
            steps: vec![
                SfcStep {
                    name: "a".into(),
                    actions: vec![],
                },
                SfcStep {
                    name: "b".into(),
                    actions: vec![],
                },
            ],
            transitions: vec![SfcTransition {
                from: "a".into(),
                to: "b".into(),
                condition: "  ".into(),
            }],
        };
        let err = transpile_to_st(&prog).unwrap_err();
        assert!(format!("{err:?}").contains("empty condition"), "{err:?}");
    }

    #[test]
    fn source_map_locates_step_lines() {
        let (st, map) = transpile_to_st_with_map(&batch_program()).unwrap();
        let n = st
            .lines()
            .position(|l| l.contains("IF __sfc_step = 'filling' THEN"))
            .map(|i| i + 1)
            .unwrap();
        match map.lookup(n) {
            Some(SfcLocation::Step { name }) => assert_eq!(name, "filling"),
            other => panic!("expected Step('filling'), got {other:?}\n{st}"),
        }
    }

    #[test]
    fn source_map_locates_transition_lines() {
        let (st, map) = transpile_to_st_with_map(&batch_program()).unwrap();
        let n = st
            .lines()
            .position(|l| l.contains("ELSIF __sfc_step = 'filling' AND (tank_full)"))
            .map(|i| i + 1)
            .unwrap();
        match map.lookup(n) {
            Some(SfcLocation::Transition { index }) => assert_eq!(*index, 1),
            other => panic!("expected Transition(1), got {other:?}\n{st}"),
        }
    }

    #[test]
    fn source_map_line_count_matches_output() {
        let (st, map) = transpile_to_st_with_map(&batch_program()).unwrap();
        assert_eq!(
            st.lines().count(),
            map.lines.len(),
            "one map entry per emitted line"
        );
    }

    // ---- step identity at runtime --------------------------------------

    fn var(name: &str, ty: &str, init: Option<&str>) -> LdVariable {
        LdVariable {
            name: name.into(),
            type_name: ty.into(),
            section: LdVarSection::Internal,
            init: init.map(Into::into),
        }
    }

    /// idle → `middle` → done, each transition always true. `middle` counts
    /// the scans it was active; `done` latches when reached.
    fn chain_through(middle: &str) -> SfcProgram {
        SfcProgram {
            name: "chain".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                var("go", "BOOL", Some("TRUE")),
                var("middle_scans", "INT", None),
                var("reached_done", "BOOL", None),
            ],
            initial_step: "idle".into(),
            steps: vec![
                SfcStep {
                    name: "idle".into(),
                    actions: vec![],
                },
                SfcStep {
                    name: middle.into(),
                    actions: vec![SfcAction {
                        qualifier: SfcQualifier::N,
                        body: "middle_scans := middle_scans + 1;".into(),
                    }],
                },
                SfcStep {
                    name: "done".into(),
                    actions: vec![SfcAction {
                        qualifier: SfcQualifier::N,
                        body: "reached_done := TRUE;".into(),
                    }],
                },
            ],
            transitions: vec![
                SfcTransition {
                    from: "idle".into(),
                    to: middle.into(),
                    condition: "go".into(),
                },
                SfcTransition {
                    from: middle.into(),
                    to: "done".into(),
                    condition: "TRUE".into(),
                },
            ],
        }
    }

    /// A step name longer than the state variable used to be truncated on
    /// assignment, so it never equalled its own literal: the chart entered
    /// the step and stopped there — no action, no way out — with a clean
    /// compile.
    #[test]
    fn a_long_step_name_still_runs_and_leaves() {
        for name in [
            "wait_for_operator_confirmation_x", // 32: one past the old limit
            &"s".repeat(254),
            "等待操作员确认后开始加料", // non-ASCII: counted in characters
        ] {
            let st = transpile_to_st(&chain_through(name)).unwrap();
            let v = crate::test_vm::run_st(&st, 10);
            assert_eq!(
                (v["middle_scans"], v["reached_done"]),
                (1, 1),
                "chart stalled in a {}-character step\n{st}",
                name.chars().count()
            );
        }
    }

    #[test]
    fn short_names_keep_the_historical_declaration() {
        let st = transpile_to_st(&batch_program()).unwrap();
        assert!(st.contains("__sfc_step : STRING[31] := 'idle';"), "{st}");
    }

    #[test]
    fn a_step_name_the_runtime_cannot_compare_is_refused() {
        let err = transpile_to_st(&chain_through(&"s".repeat(255))).unwrap_err();
        assert!(err.to_string().contains("at most 254"), "{err}");
    }

    /// `等` is U+7B49; ironplc stores its low byte, `I`. Both steps used to
    /// be active at once.
    #[test]
    fn names_the_runtime_stores_identically_are_refused() {
        let mut prog = chain_through("等");
        prog.steps.push(SfcStep {
            name: "I".into(),
            actions: vec![],
        });
        let err = transpile_to_st(&prog).unwrap_err().to_string();
        assert!(err.contains("'等' and 'I'"), "{err}");
        assert!(err.contains("same value"), "{err}");
    }

    #[test]
    fn distinct_chinese_names_still_run() {
        let mut prog = chain_through("加料");
        prog.steps.push(SfcStep {
            name: "排料".into(),
            actions: vec![],
        });
        let v = crate::test_vm::run_st(&transpile_to_st(&prog).unwrap(), 10);
        assert_eq!((v["middle_scans"], v["reached_done"]), (1, 1));
    }

    #[test]
    fn a_dollar_in_a_step_name_is_refused() {
        let err = transpile_to_st(&chain_through("cost$")).unwrap_err();
        assert!(err.to_string().contains("escape character"), "{err}");
    }

    #[test]
    fn end_to_end_batch_compiles_via_ironplc() {
        // The acid test: ironplc must accept the ST we synthesise.
        // If it doesn't, our STRING-based lowering is wrong and we'd
        // fall back to DINT IDs.
        let st = transpile_to_st(&batch_program()).unwrap();
        let diags = crate::check(&st);
        let errors: Vec<_> = diags.iter().filter(|d| d.severity == "error").collect();
        assert!(
            errors.is_empty(),
            "ironplc rejected our SFC-derived ST:\n{st}\nDIAG: {errors:#?}",
        );
        // `check` is the analyzer only; codegen rejects some shapes it accepts.
        if let Err(e) = crate::compile(&st) {
            panic!("ironplc cannot generate code for our SFC-derived ST:\n{st}\n{e}");
        }
    }
}
