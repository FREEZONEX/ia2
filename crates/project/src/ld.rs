//! Ladder Diagram (LD) JSON schema.
//!
//! An LD POU lives on disk as `pous/<slug>.ld.json` and follows the
//! shape defined here. Loaded by the store, transpiled to ST by the
//! bridge, then compiled by ironplc. The transpilation pass is in
//! `crates/ironplc-bridge/src/ld_transpile.rs`.
//!
//! Design notes (see also MEMORY/graphical-languages.md):
//!
//! - JSON is the *canonical* storage format. No PLCopen XML round-trip.
//! - Each rung carries a recursive `LdNode` boolean expression
//!   (contact / coil / and / or / not). Series = AND, parallel = OR.
//! - Coil kinds: standard (`:=`), set (latch-on), reset (latch-off).
//!   Pulse / edge variants come later.
//! - Variables declared up-front in `variables`. The transpiler emits
//!   VAR / VAR_INPUT / VAR_OUTPUT blocks from that list — keeping the
//!   schema flat (just a section enum per var) instead of grouping by
//!   section in the JSON, which would be more nesting for agents to
//!   produce.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Top-level LD POU. One file = one POU. Multi-declaration files
/// (the ST convention of stuffing multiple PROGRAMs in one `.st`) are
/// explicitly NOT supported for graphical languages — graphical POUs
/// are visually one diagram, splitting them would invite cognitive
/// load that doesn't pay off.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LdProgram {
    /// IEC identifier — what tasks.toml binds. Must match the file
    /// slug for now (`pous/motor_seal.ld.json` → `name: "motor_seal"`).
    pub name: String,
    /// PROGRAM or FUNCTION_BLOCK. Functions are less natural in LD
    /// (no I/O contacts), so we don't expose them in the UI yet, but
    /// the field accepts them for future cross-language reuse.
    #[serde(rename = "pou_type")]
    pub pou_type: LdPouType,
    /// Inline variable declarations. Flat list with explicit section;
    /// transpiler groups them back into VAR_INPUT / VAR_OUTPUT / VAR.
    pub variables: Vec<LdVariable>,
    /// Ordered list of rungs. Transpiled top-to-bottom so a later
    /// rung's contacts see the assignments of earlier rungs in the
    /// same scan (this matches classical LD evaluation semantics).
    pub rungs: Vec<LdRung>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum LdPouType {
    Program,
    FunctionBlock,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum LdVarSection {
    /// VAR — internal, persists across scans.
    Internal,
    /// VAR_INPUT — read by this POU, written by caller.
    Input,
    /// VAR_OUTPUT — written by this POU, read by caller.
    Output,
}

/// One variable. Type is a string so agents can declare any IEC type
/// (BOOL / INT / TIME / etc.) without us needing to enumerate.
/// Validation happens at transpile-time when ironplc parses the
/// generated ST.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LdVariable {
    pub name: String,
    /// IEC type name — e.g. "BOOL", "INT", "TIME", "REAL".
    #[serde(rename = "type")]
    pub type_name: String,
    pub section: LdVarSection,
    /// Optional initialiser literal (e.g. `"FALSE"`, `"T#100ms"`, `"42"`).
    /// Passed verbatim to the ST `:= <init>` clause. None → omit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LdRung {
    /// Stable identifier so the IDE / source-map can reference a rung
    /// across edits. Generated client-side; the transpiler doesn't
    /// look at it beyond echoing it into the source map.
    pub id: String,
    /// Optional human-readable label rendered above the rung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Boolean network feeding the coil. Free-form recursive AND/OR/NOT
    /// of contacts.
    pub logic: LdNode,
    /// Coil(s) driven by `logic`. Most rungs have one coil; an array
    /// lets a single network drive multiple outputs (a common
    /// real-world idiom — "if EmergencyOff then disable everything").
    pub coils: Vec<LdCoil>,
}

/// Boolean expression tree. The transpiler turns this into a parenthesised
/// ST expression: `and { args: [a, b] }` → `(a AND b)`.
///
/// Why a tree, not a 2-D grid of contacts: a tree matches the recursive
/// "series / parallel" structure operators actually compose with. The
/// renderer projects it to a 2-D layout deterministically (series →
/// horizontal, parallel → stacked vertically), so authors think in
/// expressions and the visualisation falls out.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LdNode {
    /// Normally-open contact (passes when `var` is TRUE).
    /// `negated: true` flips it to a normally-closed contact (passes
    /// when `var` is FALSE).
    Contact {
        var: String,
        #[serde(default)]
        negated: bool,
    },
    /// Series — all branches must conduct.
    And { args: Vec<LdNode> },
    /// Parallel — at least one branch must conduct.
    Or { args: Vec<LdNode> },
    /// Inverter wrapping any sub-expression.
    Not { arg: Box<LdNode> },
    /// Constant rail — `value: true` is always-passing, useful for
    /// unconditional outputs. `value: false` is rare but exposed for
    /// completeness.
    Const { value: bool },
    /// Comparison block — the IEC 61131-3 way to bridge a numeric
    /// variable into a boolean network. Renders as a small rectangle
    /// containing "left CMP right" (e.g. `temperature < 50.0`). The
    /// block conducts when the comparison evaluates TRUE.
    ///
    /// `left` and `right` are LD operands; the typical use is one
    /// variable + one literal, but two variables is allowed too. The
    /// transpiler emits `(left CMP right)` as a parenthesised ST
    /// boolean sub-expression that drops straight into the network.
    ///
    /// (Field is named `cmp` rather than `op` because serde's enum
    /// tag is already `op`.)
    Compare {
        left: LdOperand,
        cmp: LdComparator,
        right: LdOperand,
    },
    /// Standard function block call — TON / CTU / R_TRIG / etc.
    ///
    /// Renders as a rectangle with named pins. The `instance` is the
    /// FB instance variable (declared automatically in the internal
    /// VAR block as `<instance> : <fb_type>;`). The `inputs` bind
    /// pin names to operands (variables or literals). `output_pin`
    /// selects which output pin's value feeds the surrounding
    /// boolean network (typically `"Q"`, but CTUD has `QU` / `QD`).
    ///
    /// Transpile semantics: every FbCall in a rung produces
    /// `<instance>(<pin> := <operand>, ...);` as a statement *before*
    /// the rung's coil assignment. The node's position in the boolean
    /// expression is replaced with `<instance>.<output_pin>`. See
    /// `crates/ironplc-bridge/src/ld_transpile.rs`.
    ///
    /// The list of recognised `fb_type` strings (and their pin
    /// definitions) is fixed on the front-end side in
    /// `apps/web/src/lib/ld-fbs.ts`. The library itself is
    /// implemented in ironplc's VM as Rust intrinsics — see
    /// `MEMORY/graphical-languages.md` § "Standard function block
    /// library — owned by ironplc".
    FbCall {
        /// FB instance variable name. Declared once as
        /// `<instance> : <fb_type>;` in the internal VAR block by the
        /// transpiler — users do **not** add it to `variables` manually.
        ///
        /// The same instance may be placed in several rungs to read its
        /// output, but it runs once per scan, at its first placement, so
        /// every placement must bind the same `inputs` (the transpiler
        /// refuses anything else).
        instance: String,
        /// IEC type name of the FB — `"TON"`, `"CTU"`, `"R_TRIG"`,
        /// etc. The transpiler does not validate this string; it's
        /// passed verbatim to the VAR declaration, so any FB type
        /// ironplc knows about (intrinsic or user-defined) works.
        #[serde(rename = "fb_type")]
        fb_type: String,
        /// Pin bindings, in the order they appear in the rendered
        /// block. Ordered (not a map) so the JSON has a stable
        /// representation and the renderer respects authoring order.
        /// Missing pins fall back to ironplc's defaults
        /// (FALSE / 0 / T#0ms depending on type).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inputs: Vec<LdFbInput>,
        /// Which output pin's value to read into the surrounding
        /// boolean network. Most FBs only have one BOOL output
        /// (`Q`); CTUD has `QU` / `QD`. R_TRIG / F_TRIG / SR / RS
        /// use `Q1`. Default is `"Q"` if the field is omitted, but
        /// the editor sets it explicitly.
        #[serde(default = "default_output_pin")]
        output_pin: String,
    },
}

fn default_output_pin() -> String {
    "Q".to_string()
}

/// One pin binding in an `FbCall`. Either a variable reference or an
/// inline literal — same shape as `LdOperand` so the editor can reuse
/// its operand picker.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LdFbInput {
    /// Pin name on the FB — `"IN"`, `"PT"`, `"CU"`, `"R"`, etc. Case
    /// is preserved verbatim into the generated ST.
    pub pin: String,
    /// What's wired to the pin.
    pub value: LdOperand,
}

/// One operand of a comparison. Either a variable reference (string
/// name resolved at compile time) or an inline literal (passed
/// verbatim through to ST — `42`, `3.14`, `T#100ms`, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LdOperand {
    Var { name: String },
    Literal { value: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum LdComparator {
    /// `=` — equality.
    Eq,
    /// `<>` — inequality.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LdCoil {
    pub var: String,
    pub kind: LdCoilKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum LdCoilKind {
    /// `var := <logic>;` — assigned every scan.
    Standard,
    /// Set / latch — the network is evaluated into a rung temporary, then
    /// `IF <temporary> THEN var := TRUE; END_IF;`.
    Set,
    /// Reset / unlatch — as `Set`, assigning FALSE.
    Reset,
}

// =================================================================
//   Tiny smoke test that the schema round-trips through serde_json.
//   Real transpiler tests live in the bridge crate where they can
//   verify generated ST as well.
// =================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_minimal_program() {
        let prog = LdProgram {
            name: "motor".into(),
            pou_type: LdPouType::Program,
            variables: vec![LdVariable {
                name: "start_btn".into(),
                type_name: "BOOL".into(),
                section: LdVarSection::Input,
                init: None,
            }],
            rungs: vec![LdRung {
                id: "r0".into(),
                label: Some("first rung".into()),
                logic: LdNode::Contact {
                    var: "start_btn".into(),
                    negated: false,
                },
                coils: vec![LdCoil {
                    var: "motor_run".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        };
        let json = serde_json::to_string_pretty(&prog).unwrap();
        let back: LdProgram = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rungs.len(), 1);
        assert_eq!(back.rungs[0].coils[0].var, "motor_run");
    }

    #[test]
    fn nested_and_or_not_round_trips() {
        let n: LdNode = serde_json::from_str(
            r#"{"op":"and","args":[
                {"op":"contact","var":"a"},
                {"op":"or","args":[
                    {"op":"contact","var":"b","negated":true},
                    {"op":"not","arg":{"op":"contact","var":"c"}}
                ]}
            ]}"#,
        )
        .unwrap();
        match &n {
            LdNode::And { args } => assert_eq!(args.len(), 2),
            _ => panic!("expected And at root, got {:?}", n),
        }
    }
}
