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

use crate::errors::{BridgeError, NameClash};

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
    // 2. Instance names unique (two blocks can't share an FB instance)
    //    and clear of the POU's variables, ignoring case.
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
    }
    if let Some(clash) = instance_name_clash(prog) {
        return Err(clash.into());
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
    // 4. Each VAR_OUTPUT is driven by at most one binding, in any
    //    spelling — `Out` and `out` are one variable.
    let mut driven: HashMap<String, &str> = HashMap::new();
    for out in &prog.outputs {
        if out.variable.is_empty() {
            return Err(BridgeError::Parse(
                "FBD output binding has empty variable".into(),
            ));
        }
        if let Some(prev) = driven.insert(out.variable.to_lowercase(), out.from_block.as_str()) {
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

/// The first block whose FB instance name clashes with another
/// declaration once IEC 61131-3's case-insensitive name rules apply, or
/// `None`: another block's instance in any spelling (two blocks cannot
/// share an instance), or a variable the POU declares (the diagram
/// declares its FB instances itself, so both would be declared). Located
/// at the later block so the editor can point at it. Empty names are
/// left to the transpiler's own checks.
pub fn instance_name_clash(prog: &FbdProgram) -> Option<NameClash<FbdLocation>> {
    let variables: HashMap<String, &str> = prog
        .variables
        .iter()
        .map(|v| (v.name.to_lowercase(), v.name.as_str()))
        .collect();
    let mut seen: HashMap<String, &FbdBlock> = HashMap::new();
    for b in prog.blocks.iter().filter(|b| !b.instance.is_empty()) {
        let key = b.instance.to_lowercase();
        let message = if let Some(var) = variables.get(&key) {
            format!(
                "FB instance '{}' of block '{}' has the same name as the variable '{var}' \
                 (IEC 61131-3 names are case-insensitive). The diagram declares its FB \
                 instances itself — rename the instance, or remove or rename the variable",
                b.instance, b.id
            )
        } else if let Some(prev) = seen.get(&key) {
            if prev.instance == b.instance {
                format!(
                    "FB instance '{}' used by both block '{}' and block '{}'",
                    b.instance, prev.id, b.id
                )
            } else {
                format!(
                    "FB instance '{}' of block '{}' differs only in case from instance '{}' of \
                     block '{}'. IEC 61131-3 names are case-insensitive, so both blocks would \
                     declare the same name — rename one of them",
                    b.instance, b.id, prev.instance, prev.id
                )
            }
        } else {
            seen.insert(key, b);
            continue;
        };
        return Some(NameClash {
            message,
            location: FbdLocation::Block {
                block_id: b.id.clone(),
            },
        });
    }
    None
}

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

    fn bool_var(name: &str, section: LdVarSection) -> LdVariable {
        LdVariable {
            name: name.into(),
            type_name: "BOOL".into(),
            section,
            init: None,
        }
    }

    fn ton_block(id: &str, instance: &str) -> FbdBlock {
        FbdBlock {
            id: id.into(),
            fb_type: "TON".into(),
            instance: instance.into(),
            inputs: vec![
                FbdInputBinding {
                    pin: "IN".into(),
                    value: FbdInputSource::Literal {
                        value: "TRUE".into(),
                    },
                },
                FbdInputBinding {
                    pin: "PT".into(),
                    value: FbdInputSource::Literal {
                        value: "T#200ms".into(),
                    },
                },
            ],
            position: None,
        }
    }

    fn output(variable: &str, from_block: &str) -> FbdOutputBinding {
        FbdOutputBinding {
            variable: variable.into(),
            from_block: from_block.into(),
            from_pin: "Q".into(),
        }
    }

    /// Two TON blocks `b1` / `b2` with the given instance names, each
    /// driving its own output.
    fn two_timers(first: &str, second: &str) -> FbdProgram {
        FbdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                bool_var("a", LdVarSection::Output),
                bool_var("b", LdVarSection::Output),
            ],
            blocks: vec![ton_block("b1", first), ton_block("b2", second)],
            outputs: vec![output("a", "b1"), output("b", "b2")],
        }
    }

    /// The single diagnostic the editor gets for `prog`, which must be a
    /// transpile error located on `block_id`.
    fn assert_located_at_block(prog: &FbdProgram, block_id: &str) {
        let source = serde_json::to_string(prog).unwrap();
        let diags = crate::check_pou_source(&source, project::PouLanguage::Fbd);
        assert_eq!(diags.len(), 1, "{diags:#?}");
        assert_eq!(diags[0].code, "FBD-TRANSPILE", "{diags:#?}");
        assert!(
            matches!(
                diags[0].fbd_location,
                Some(FbdLocation::Block { block_id: ref id }) if id == block_id
            ),
            "{diags:#?}"
        );
    }

    /// IEC names are case-insensitive: `T1` and `t1` would be one instance
    /// declared twice. The exact-duplicate check missed this, the program
    /// checked clean, and its VM never started.
    #[test]
    fn instances_differing_only_in_case_are_refused_at_the_later_block() {
        let prog = two_timers("T1", "t1");
        let err = transpile_to_st(&prog).unwrap_err().to_string();
        assert!(
            err.contains("'t1'")
                && err.contains("'T1'")
                && err.contains("'b1'")
                && err.contains("'b2'")
                && err.contains("case"),
            "{err}"
        );
        assert_located_at_block(&prog, "b2");
    }

    #[test]
    fn an_exact_duplicate_instance_is_located_at_the_later_block() {
        let prog = two_timers("T1", "T1");
        let err = transpile_to_st(&prog).unwrap_err().to_string();
        assert!(
            err.contains("FB instance 'T1' used by both block 'b1' and block 'b2'"),
            "{err}"
        );
        assert_located_at_block(&prog, "b2");
    }

    #[test]
    fn an_instance_named_like_a_variable_is_refused() {
        let mut prog = two_timers("T1", "timer1");
        prog.variables.push(LdVariable {
            name: "Timer1".into(),
            type_name: "TON".into(),
            section: LdVarSection::Internal,
            init: None,
        });
        let err = transpile_to_st(&prog).unwrap_err().to_string();
        assert!(
            err.contains("'timer1'") && err.contains("'Timer1'") && err.contains("variable"),
            "{err}"
        );
        assert_located_at_block(&prog, "b2");
    }

    /// `Out` and `out` are one variable, so binding both drives it twice
    /// and the later assignment silently wins.
    #[test]
    fn outputs_differing_only_in_case_are_double_driven() {
        let mut prog = two_timers("T1", "T2");
        prog.variables = vec![bool_var("Out", LdVarSection::Output)];
        prog.outputs = vec![output("Out", "b1"), output("out", "b2")];
        let err = transpile_to_st(&prog).unwrap_err().to_string();
        assert!(err.contains("driven by two blocks"), "{err}");
    }
}
