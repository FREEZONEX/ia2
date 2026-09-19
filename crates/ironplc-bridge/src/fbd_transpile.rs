//! Function Block Diagram → Structured Text transpiler.
//!
//! Same architecture as the LD transpiler (`ld_transpile.rs`):
//!
//!   pous/<name>.fbd.json   (canonical source)
//!     └── serde_json parse → project::FbdProgram   (typed AST)
//!         └── transpile_to_st(&FbdProgram) → String  (ST source)
//!             └── ironplc parser → DSL → codegen → bytecode
//!
//! Two passes over the program:
//!
//!  1. **Topological sort** of blocks by `Block → Block` input edges.
//!     Cycles (feedback loops) are forbidden in FBD — they require
//!     CFC semantics with explicit feedback markers, which is out of
//!     scope for the MVP. Cycle detection returns a `BridgeError::Parse`
//!     naming the offending blocks.
//!  2. **Emit** in topo order: each block becomes one `inst(PIN := ...)`
//!     statement, output bindings become trailing assignments
//!     `var := block.pin;`. The source map tracks which line came
//!     from which FBD element so diagnostics can locate back to the
//!     canvas.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use project::{
    FbdBlock, FbdInputSource, FbdOutputBinding, FbdProgram, LdPouType, LdVarSection, LdVariable,
};
use serde::Serialize;
use ts_rs::TS;

use crate::errors::BridgeError;

// =================================================================
//   Source map (parallel to ld_transpile::LdLocation)
// =================================================================

/// LD-equivalent for FBD: where in the diagram a given ST line came
/// from. Used by `check_pou_source` to annotate diagnostics so the
/// editor can highlight the offending block / variable.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FbdLocation {
    /// A variable declaration line.
    Variable { name: String },
    /// A block's `inst(PIN := …)` call statement.
    Block { block_id: String },
    /// An output-binding assignment line.
    Output { variable: String },
}

/// One-entry-per-ST-line. Index `i` corresponds to ST line `i+1`.
#[derive(Debug, Clone, Default)]
pub struct FbdSourceMap {
    pub lines: Vec<Option<FbdLocation>>,
}

impl FbdSourceMap {
    pub fn lookup(&self, line: usize) -> Option<&FbdLocation> {
        if line == 0 {
            return None;
        }
        self.lines.get(line - 1).and_then(|s| s.as_ref())
    }
}

/// Internal: emit one line of ST + push exactly one source-map entry.
struct StEmitter {
    out: String,
    map: Vec<Option<FbdLocation>>,
}

impl StEmitter {
    fn new() -> Self {
        Self {
            out: String::new(),
            map: Vec::new(),
        }
    }

    fn line(&mut self, span: Option<FbdLocation>, content: std::fmt::Arguments) {
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

/// Render an `FbdProgram` to a complete ST source. Discards the
/// source map — use `transpile_to_st_with_map` if you need diagnostic
/// mapping back to FBD elements.
pub fn transpile_to_st(prog: &FbdProgram) -> Result<String, BridgeError> {
    Ok(transpile_to_st_with_map(prog)?.0)
}

/// As above, but also returns the source map (one entry per ST line).
pub fn transpile_to_st_with_map(prog: &FbdProgram) -> Result<(String, FbdSourceMap), BridgeError> {
    if prog.name.is_empty() {
        return Err(BridgeError::Parse("FBD program name is empty".into()));
    }

    // ----- Validation pre-passes -----
    // 1. Block IDs unique.
    let mut block_ids = HashSet::new();
    for b in &prog.blocks {
        if b.id.is_empty() {
            return Err(BridgeError::Parse("FBD block has empty id".into()));
        }
        if !block_ids.insert(&b.id) {
            return Err(BridgeError::Parse(format!(
                "FBD block id '{}' is duplicated",
                b.id
            )));
        }
    }
    // 2. Instance names unique (two blocks can't share an FB instance).
    let mut instances: HashMap<&str, &str> = HashMap::new();
    for b in &prog.blocks {
        if b.instance.is_empty() {
            return Err(BridgeError::Parse(format!(
                "FBD block '{}' has empty instance name",
                b.id
            )));
        }
        if b.fb_type.is_empty() {
            return Err(BridgeError::Parse(format!(
                "FBD block '{}' has empty fb_type",
                b.id
            )));
        }
        if let Some(prev_id) = instances.insert(b.instance.as_str(), b.id.as_str()) {
            return Err(BridgeError::Parse(format!(
                "FB instance '{}' used by both block '{}' and block '{}'",
                b.instance, prev_id, b.id
            )));
        }
    }
    // 3. Wire endpoints reference existing blocks.
    let id_to_idx: HashMap<&str, usize> = prog
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.as_str(), i))
        .collect();
    for b in &prog.blocks {
        for input in &b.inputs {
            if let FbdInputSource::Block { block_id, pin } = &input.value {
                if !id_to_idx.contains_key(block_id.as_str()) {
                    return Err(BridgeError::Parse(format!(
                        "Block '{}' input pin '{}' wired from unknown block '{}'",
                        b.id, input.pin, block_id
                    )));
                }
                if pin.is_empty() {
                    return Err(BridgeError::Parse(format!(
                        "Block '{}' input pin '{}' has empty source pin",
                        b.id, input.pin
                    )));
                }
            }
        }
    }
    for out in &prog.outputs {
        if !id_to_idx.contains_key(out.from_block.as_str()) {
            return Err(BridgeError::Parse(format!(
                "Output binding '{}' wired from unknown block '{}'",
                out.variable, out.from_block
            )));
        }
    }
    // 4. Each VAR_OUTPUT is driven by at most one binding.
    let mut driven: HashMap<&str, &str> = HashMap::new();
    for out in &prog.outputs {
        if out.variable.is_empty() {
            return Err(BridgeError::Parse(
                "FBD output binding has empty variable".into(),
            ));
        }
        if let Some(prev) = driven.insert(out.variable.as_str(), out.from_block.as_str()) {
            return Err(BridgeError::Parse(format!(
                "Output variable '{}' driven by two blocks ({} and {})",
                out.variable, prev, out.from_block
            )));
        }
    }

    // ----- Topological sort -----
    let order = topo_sort(&prog.blocks, &id_to_idx)?;

    // ----- Emit -----
    let mut em = StEmitter::new();
    let (head, foot) = match prog.pou_type {
        LdPouType::Program => ("PROGRAM", "END_PROGRAM"),
        LdPouType::FunctionBlock => ("FUNCTION_BLOCK", "END_FUNCTION_BLOCK"),
    };

    // FB instances declared in the internal VAR block, in
    // deterministic order (BTree by instance name).
    let fb_instances: BTreeMap<String, String> = prog
        .blocks
        .iter()
        .map(|b| (b.instance.clone(), b.fb_type.clone()))
        .collect();

    em.line(None, format_args!("{} {}", head, prog.name));
    write_variable_blocks(&mut em, &prog.variables, &fb_instances);
    em.blank();

    // Block call statements in topo order.
    for &i in &order {
        let b = &prog.blocks[i];
        let args = render_inputs(&b.inputs, &id_to_idx, &prog.blocks)?;
        em.line(
            Some(FbdLocation::Block {
                block_id: b.id.clone(),
            }),
            format_args!("    {}({});", b.instance, args),
        );
    }

    // Output bindings, in author order.
    if !prog.outputs.is_empty() {
        em.blank();
        for o in &prog.outputs {
            emit_output_binding(&mut em, o, &id_to_idx, &prog.blocks)?;
        }
    }

    em.line(None, format_args!("{foot}"));
    Ok((em.out, FbdSourceMap { lines: em.map }))
}

// =================================================================
//   Helpers
// =================================================================

fn write_variable_blocks(
    em: &mut StEmitter,
    vars: &[LdVariable],
    fb_instances: &BTreeMap<String, String>,
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
                Some(FbdLocation::Variable {
                    name: v.name.clone(),
                }),
                format_args!("        {} : {}{};", v.name, v.type_name, init),
            );
        }
        // Internal VAR also carries the synthesised FB instance
        // declarations. These have no FBD origin (they're transpiler
        // bookkeeping); diagnostics on them are transpiler bugs, not
        // user authoring problems.
        if section == LdVarSection::Internal {
            for (inst, ty) in fb_instances {
                em.line(None, format_args!("        {inst} : {ty};"));
            }
        }
        em.line(None, format_args!("    END_VAR"));
    }
}

/// Render a block's input list as a comma-separated `PIN := value`
/// argument string ready to drop inside an `inst(...)` call.
fn render_inputs(
    inputs: &[project::FbdInputBinding],
    id_to_idx: &HashMap<&str, usize>,
    blocks: &[FbdBlock],
) -> Result<String, BridgeError> {
    let mut parts = Vec::with_capacity(inputs.len());
    for input in inputs {
        if input.pin.is_empty() {
            return Err(BridgeError::Parse("FBD input has empty pin name".into()));
        }
        let value = render_input_value(&input.value, id_to_idx, blocks)?;
        parts.push(format!("{} := {}", input.pin, value));
    }
    Ok(parts.join(", "))
}

/// Render one pin value source as ST text.
///   Var{name}      → `name`
///   Literal{value} → verbatim
///   Block{id, pin} → `<source_block.instance>.<pin>`
fn render_input_value(
    src: &FbdInputSource,
    id_to_idx: &HashMap<&str, usize>,
    blocks: &[FbdBlock],
) -> Result<String, BridgeError> {
    match src {
        FbdInputSource::Var { name } => {
            if name.is_empty() {
                Err(BridgeError::Parse("FBD var input has empty name".into()))
            } else {
                Ok(name.clone())
            }
        }
        FbdInputSource::Literal { value } => {
            if value.is_empty() {
                Err(BridgeError::Parse("FBD literal input is empty".into()))
            } else {
                Ok(value.clone())
            }
        }
        FbdInputSource::Block { block_id, pin } => {
            let idx = id_to_idx
                .get(block_id.as_str())
                .ok_or_else(|| BridgeError::Parse(format!("unknown block '{block_id}'")))?;
            Ok(format!("{}.{}", blocks[*idx].instance, pin))
        }
    }
}

fn emit_output_binding(
    em: &mut StEmitter,
    out: &FbdOutputBinding,
    id_to_idx: &HashMap<&str, usize>,
    blocks: &[FbdBlock],
) -> Result<(), BridgeError> {
    let idx = id_to_idx
        .get(out.from_block.as_str())
        .ok_or_else(|| BridgeError::Parse(format!("unknown block '{}'", out.from_block)))?;
    em.line(
        Some(FbdLocation::Output {
            variable: out.variable.clone(),
        }),
        format_args!(
            "    {} := {}.{};",
            out.variable, blocks[*idx].instance, out.from_pin
        ),
    );
    Ok(())
}

/// Kahn's algorithm: returns block indices in execution order.
/// Errors with a useful message when a cycle is detected.
fn topo_sort(
    blocks: &[FbdBlock],
    id_to_idx: &HashMap<&str, usize>,
) -> Result<Vec<usize>, BridgeError> {
    let n = blocks.len();
    let mut indegree = vec![0usize; n];
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, b) in blocks.iter().enumerate() {
        for input in &b.inputs {
            if let FbdInputSource::Block { block_id, .. } = &input.value {
                let u = *id_to_idx.get(block_id.as_str()).expect("validated above");
                if u == i {
                    return Err(BridgeError::Parse(format!(
                        "FBD block '{}' references itself — self-feedback isn't supported",
                        b.id
                    )));
                }
                adj[u].push(i);
                indegree[i] += 1;
            }
        }
    }
    let mut queue: VecDeque<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(u) = queue.pop_front() {
        order.push(u);
        for &v in &adj[u] {
            indegree[v] -= 1;
            if indegree[v] == 0 {
                queue.push_back(v);
            }
        }
    }
    if order.len() != n {
        // Name the blocks still in the cycle for a more useful error.
        let stuck: Vec<&str> = blocks
            .iter()
            .enumerate()
            .filter(|(i, _)| indegree[*i] > 0)
            .map(|(_, b)| b.id.as_str())
            .collect();
        return Err(BridgeError::Parse(format!(
            "FBD has a wire cycle through blocks: {}. Feedback loops require CFC, not FBD.",
            stuck.join(", ")
        )));
    }
    Ok(order)
}

// =================================================================
//   Tests
// =================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use project::{FbdInputBinding, LdVarSection, LdVariable};

    /// Helper: minimal valid program — single TON block driven by an
    /// input, output bound to a VAR_OUTPUT.
    fn ton_program() -> FbdProgram {
        FbdProgram {
            name: "delay".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "btn".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "done".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            blocks: vec![FbdBlock {
                id: "b0".into(),
                fb_type: "TON".into(),
                instance: "myT".into(),
                inputs: vec![
                    FbdInputBinding {
                        pin: "IN".into(),
                        value: FbdInputSource::Var { name: "btn".into() },
                    },
                    FbdInputBinding {
                        pin: "PT".into(),
                        value: FbdInputSource::Literal {
                            value: "T#3s".into(),
                        },
                    },
                ],
                position: None,
            }],
            outputs: vec![FbdOutputBinding {
                variable: "done".into(),
                from_block: "b0".into(),
                from_pin: "Q".into(),
            }],
        }
    }

    #[test]
    fn single_block_emits_decl_call_and_output_binding() {
        let st = transpile_to_st(&ton_program()).unwrap();
        assert!(st.contains("myT : TON;"), "got:\n{st}");
        assert!(st.contains("myT(IN := btn, PT := T#3s);"), "got:\n{st}");
        assert!(st.contains("done := myT.Q;"), "got:\n{st}");
        assert!(st.contains("END_PROGRAM"));
    }

    #[test]
    fn wire_between_blocks_uses_dot_access_on_source_instance() {
        // b0 (TON) → b1 (CTU on CU)
        let prog = FbdProgram {
            name: "chain".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "btn".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "done".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            blocks: vec![
                FbdBlock {
                    id: "b0".into(),
                    fb_type: "TON".into(),
                    instance: "myT".into(),
                    inputs: vec![
                        FbdInputBinding {
                            pin: "IN".into(),
                            value: FbdInputSource::Var { name: "btn".into() },
                        },
                        FbdInputBinding {
                            pin: "PT".into(),
                            value: FbdInputSource::Literal {
                                value: "T#1s".into(),
                            },
                        },
                    ],
                    position: None,
                },
                FbdBlock {
                    id: "b1".into(),
                    fb_type: "CTU".into(),
                    instance: "myCnt".into(),
                    inputs: vec![
                        FbdInputBinding {
                            pin: "CU".into(),
                            value: FbdInputSource::Block {
                                block_id: "b0".into(),
                                pin: "Q".into(),
                            },
                        },
                        FbdInputBinding {
                            pin: "PV".into(),
                            value: FbdInputSource::Literal { value: "5".into() },
                        },
                    ],
                    position: None,
                },
            ],
            outputs: vec![FbdOutputBinding {
                variable: "done".into(),
                from_block: "b1".into(),
                from_pin: "Q".into(),
            }],
        };
        let st = transpile_to_st(&prog).unwrap();
        assert!(st.contains("myCnt(CU := myT.Q, PV := 5);"), "got:\n{st}");
        // Topo order: b0 (TON) first, then b1 (CTU)
        let ton_pos = st.find("myT(IN").unwrap();
        let ctu_pos = st.find("myCnt(CU").unwrap();
        assert!(ton_pos < ctu_pos, "block order should be topological");
    }

    #[test]
    fn cycle_detection_errors_with_block_ids() {
        // b0 → b1 → b0  (self-loop variant: b0 reads b1.Q, b1 reads b0.Q)
        let prog = FbdProgram {
            name: "loop".into(),
            pou_type: LdPouType::Program,
            variables: vec![],
            blocks: vec![
                FbdBlock {
                    id: "a".into(),
                    fb_type: "TON".into(),
                    instance: "ta".into(),
                    inputs: vec![FbdInputBinding {
                        pin: "IN".into(),
                        value: FbdInputSource::Block {
                            block_id: "b".into(),
                            pin: "Q".into(),
                        },
                    }],
                    position: None,
                },
                FbdBlock {
                    id: "b".into(),
                    fb_type: "TON".into(),
                    instance: "tb".into(),
                    inputs: vec![FbdInputBinding {
                        pin: "IN".into(),
                        value: FbdInputSource::Block {
                            block_id: "a".into(),
                            pin: "Q".into(),
                        },
                    }],
                    position: None,
                },
            ],
            outputs: vec![],
        };
        let err = transpile_to_st(&prog).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("cycle"), "expected cycle error, got: {msg}");
        assert!(msg.contains("a") && msg.contains("b"), "{msg}");
    }

    #[test]
    fn duplicate_instance_across_blocks_errors() {
        let prog = FbdProgram {
            name: "dup".into(),
            pou_type: LdPouType::Program,
            variables: vec![],
            blocks: vec![
                FbdBlock {
                    id: "b0".into(),
                    fb_type: "TON".into(),
                    instance: "myT".into(),
                    inputs: vec![],
                    position: None,
                },
                FbdBlock {
                    id: "b1".into(),
                    fb_type: "TOF".into(),
                    instance: "myT".into(), // same as b0
                    inputs: vec![],
                    position: None,
                },
            ],
            outputs: vec![],
        };
        let err = transpile_to_st(&prog).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("instance") && msg.contains("myT"), "{msg}");
    }

    #[test]
    fn wire_to_unknown_block_errors_clearly() {
        let prog = FbdProgram {
            name: "bad".into(),
            pou_type: LdPouType::Program,
            variables: vec![],
            blocks: vec![FbdBlock {
                id: "b0".into(),
                fb_type: "TON".into(),
                instance: "myT".into(),
                inputs: vec![FbdInputBinding {
                    pin: "IN".into(),
                    value: FbdInputSource::Block {
                        block_id: "ghost".into(),
                        pin: "Q".into(),
                    },
                }],
                position: None,
            }],
            outputs: vec![],
        };
        let err = transpile_to_st(&prog).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("ghost"), "{msg}");
    }

    #[test]
    fn double_driven_output_errors() {
        let prog = FbdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![LdVariable {
                name: "out".into(),
                type_name: "BOOL".into(),
                section: LdVarSection::Output,
                init: None,
            }],
            blocks: vec![
                FbdBlock {
                    id: "b0".into(),
                    fb_type: "TON".into(),
                    instance: "t0".into(),
                    inputs: vec![],
                    position: None,
                },
                FbdBlock {
                    id: "b1".into(),
                    fb_type: "TOF".into(),
                    instance: "t1".into(),
                    inputs: vec![],
                    position: None,
                },
            ],
            outputs: vec![
                FbdOutputBinding {
                    variable: "out".into(),
                    from_block: "b0".into(),
                    from_pin: "Q".into(),
                },
                FbdOutputBinding {
                    variable: "out".into(), // same VAR driven twice
                    from_block: "b1".into(),
                    from_pin: "Q".into(),
                },
            ],
        };
        let err = transpile_to_st(&prog).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("driven by two blocks"), "{msg}");
    }

    #[test]
    fn source_map_locates_block_call_lines() {
        let (st, map) = transpile_to_st_with_map(&ton_program()).unwrap();
        let call_line = st
            .lines()
            .position(|l| l.contains("myT(IN := btn"))
            .map(|i| i + 1)
            .unwrap();
        match map.lookup(call_line) {
            Some(FbdLocation::Block { block_id }) => assert_eq!(block_id, "b0"),
            other => panic!("expected Block, got {other:?}\n{st}"),
        }
    }

    #[test]
    fn source_map_locates_output_binding_lines() {
        let (st, map) = transpile_to_st_with_map(&ton_program()).unwrap();
        let line = st
            .lines()
            .position(|l| l.contains("done := myT.Q"))
            .map(|i| i + 1)
            .unwrap();
        match map.lookup(line) {
            Some(FbdLocation::Output { variable }) => assert_eq!(variable, "done"),
            other => panic!("expected Output, got {other:?}\n{st}"),
        }
    }

    #[test]
    fn source_map_locates_variable_declarations() {
        let (st, map) = transpile_to_st_with_map(&ton_program()).unwrap();
        let line = st
            .lines()
            .position(|l| l.contains("btn : BOOL"))
            .map(|i| i + 1)
            .unwrap();
        match map.lookup(line) {
            Some(FbdLocation::Variable { name }) => assert_eq!(name, "btn"),
            other => panic!("expected Variable, got {other:?}\n{st}"),
        }
    }

    #[test]
    fn source_map_line_count_matches_output() {
        let (st, map) = transpile_to_st_with_map(&ton_program()).unwrap();
        assert_eq!(
            st.lines().count(),
            map.lines.len(),
            "one map entry per emitted line"
        );
    }

    #[test]
    fn end_to_end_ton_compiles_via_ironplc() {
        // The whole point: our generated ST must actually parse + analyse
        // cleanly in ironplc. Anything else means the transpiler is
        // emitting something IEC doesn't accept.
        let st = transpile_to_st(&ton_program()).unwrap();
        let diags = crate::check(&st);
        let errors: Vec<_> = diags.iter().filter(|d| d.severity == "error").collect();
        assert!(
            errors.is_empty(),
            "ironplc rejected our ST:\n{st}\nDIAG: {errors:#?}"
        );
        // `check` is the analyzer only; codegen rejects some shapes it accepts.
        if let Err(e) = crate::compile(&st) {
            panic!("ironplc cannot generate code for our ST:\n{st}\n{e}");
        }
    }

    #[test]
    fn end_to_end_chain_with_wire_compiles() {
        // Two blocks wired together (TON → CTU). Exercises the topo-sort
        // emit order AND the dot-access wire rendering.
        let prog = FbdProgram {
            name: "chain".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "tick".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "rst".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "done".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            blocks: vec![
                FbdBlock {
                    id: "edge".into(),
                    fb_type: "R_TRIG".into(),
                    instance: "rt".into(),
                    inputs: vec![FbdInputBinding {
                        pin: "CLK".into(),
                        value: FbdInputSource::Var {
                            name: "tick".into(),
                        },
                    }],
                    position: None,
                },
                FbdBlock {
                    id: "counter".into(),
                    fb_type: "CTU".into(),
                    instance: "cu".into(),
                    inputs: vec![
                        FbdInputBinding {
                            pin: "CU".into(),
                            value: FbdInputSource::Block {
                                block_id: "edge".into(),
                                pin: "Q".into(),
                            },
                        },
                        FbdInputBinding {
                            pin: "R".into(),
                            value: FbdInputSource::Var { name: "rst".into() },
                        },
                        FbdInputBinding {
                            pin: "PV".into(),
                            value: FbdInputSource::Literal { value: "3".into() },
                        },
                    ],
                    position: None,
                },
            ],
            outputs: vec![FbdOutputBinding {
                variable: "done".into(),
                from_block: "counter".into(),
                from_pin: "Q".into(),
            }],
        };
        let st = transpile_to_st(&prog).unwrap();
        let diags = crate::check(&st);
        let errors: Vec<_> = diags.iter().filter(|d| d.severity == "error").collect();
        assert!(
            errors.is_empty(),
            "ironplc rejected our ST:\n{st}\nDIAG: {errors:#?}"
        );
        // `check` is the analyzer only; codegen rejects some shapes it accepts.
        if let Err(e) = crate::compile(&st) {
            panic!("ironplc cannot generate code for our ST:\n{st}\n{e}");
        }
    }
}
