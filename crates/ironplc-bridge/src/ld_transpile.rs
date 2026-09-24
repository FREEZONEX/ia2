//! Ladder Diagram → Structured Text transpiler.
//!
//! The pipeline for an LD POU is:
//!
//!   pous/<name>.ld.json   (canonical source)
//!     └── serde_json parse → project::LdProgram   (typed AST)
//!         └── transpile_to_st(&LdProgram) → String  (ST source)
//!             └── ironplc parser → DSL → codegen → bytecode
//!
//! Why ST as the intermediate: it's human-readable, agent-debuggable,
//! and reuses ironplc's existing pipeline end-to-end. See
//! MEMORY/graphical-languages.md for the full rationale and trade-off
//! against PLCopen XML / direct-DSL alternatives.
//!
//! Source-map: not yet emitted. The current implementation produces
//! ST whose lines map line-for-line to elements in the LD JSON
//! (one rung → roughly one statement / IF block), so diagnostics
//! coming back from ironplc with line numbers can be best-effort
//! mapped to rung IDs by counting transpiler emit order. Proper
//! source-map plumbing lands in a follow-up; the rung `id` field on
//! `LdRung` is the anchor.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;

use project::{
    LdCoilKind, LdComparator, LdFbInput, LdNode, LdOperand, LdPouType, LdProgram, LdRung,
    LdVarSection, LdVariable,
};
use serde::Serialize;
use ts_rs::TS;

use crate::errors::{BridgeError, NameClash};

/// `(instance_name, fb_type)`. Stable across iterations because we
/// build it from a deterministic walk of the program.
type FbInstanceTable = BTreeMap<String, String>;

// =================================================================
//   Source map
//
//   ironplc reports diagnostics keyed by (line, column) in the ST
//   source it received. Our LD pipeline generates that ST — the user
//   never sees those lines. The source map turns "ST line 47" back
//   into "the FB call to myT1 in rung r3", so the IDE can highlight
//   the right element on the canvas.
//
//   The map is line-resolution (one slot per generated ST line),
//   1-indexed. Slots that don't correspond to a specific LD element
//   are `None` (POU headers, VAR-block headers, END_PROGRAM, etc.).
// =================================================================

/// Location of an LD element addressable from a diagnostic.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LdLocation {
    /// A specific declared variable.
    Variable { name: String },
    /// Anywhere within a rung — most common case (the rung's main
    /// assignment line). `rung_id` matches `LdRung::id` in the JSON.
    Rung { rung_id: String },
    /// A specific coil in a multi-coil rung. `coil_index` is the
    /// 0-based position in `LdRung::coils`.
    Coil { rung_id: String, coil_index: usize },
    /// The `inst(PIN := ..., ...);` statement for an FB call.
    FbCall { rung_id: String, instance: String },
}

/// One-line-per-entry mapping. Index `i` describes ST line `i+1`
/// (lines are 1-indexed in ironplc diagnostics).
#[derive(Debug, Clone, Default)]
pub struct LdSourceMap {
    pub lines: Vec<Option<LdLocation>>,
}

impl LdSourceMap {
    /// Look up the LD origin of an ST line (1-indexed). Returns
    /// `None` for boilerplate lines and out-of-range queries.
    pub fn lookup(&self, line: usize) -> Option<&LdLocation> {
        if line == 0 {
            return None;
        }
        self.lines.get(line - 1).and_then(|s| s.as_ref())
    }
}

/// Internal writer that batches `out: String` and the source map.
/// Every line emitted via `writeln` records exactly one span entry —
/// so `out.lines().count() == map.lines.len()` at all times.
struct StEmitter {
    out: String,
    map: Vec<Option<LdLocation>>,
}

impl StEmitter {
    fn new() -> Self {
        Self {
            out: String::new(),
            map: Vec::new(),
        }
    }

    /// Emit one line of ST plus a source-map entry.
    fn line(&mut self, span: Option<LdLocation>, content: std::fmt::Arguments) {
        let _ = writeln!(self.out, "{content}");
        self.map.push(span);
    }

    /// Emit a single newline (no LD origin).
    fn blank(&mut self) {
        self.out.push('\n');
        self.map.push(None);
    }
}

/// Top-level entry: render an `LdProgram` to a complete ST source
/// string ready to feed `ironplc_bridge::compile` (or
/// `compile_isolated_source`). Discards the source map — use
/// `transpile_to_st_with_map` if you need diagnostic mapping.
pub fn transpile_to_st(prog: &LdProgram) -> Result<String, BridgeError> {
    Ok(transpile_to_st_with_map(prog)?.0)
}

/// Like `transpile_to_st` but also returns the source map so callers
/// can translate diagnostic line numbers back to LD elements.
pub fn transpile_to_st_with_map(prog: &LdProgram) -> Result<(String, LdSourceMap), BridgeError> {
    if prog.name.is_empty() {
        return Err(BridgeError::Parse("LD program name is empty".into()));
    }
    if prog.rungs.is_empty() {
        return Err(BridgeError::Parse(format!(
            "LD program '{}' has no rungs",
            prog.name
        )));
    }

    let mut em = StEmitter::new();
    let (head, foot) = match prog.pou_type {
        LdPouType::Program => ("PROGRAM", "END_PROGRAM"),
        LdPouType::FunctionBlock => ("FUNCTION_BLOCK", "END_FUNCTION_BLOCK"),
    };

    // Pre-scan: collect every FB instance referenced from any rung's
    // logic tree. Each instance becomes a `name : fb_type;` line in
    // the internal VAR block. Conflicts error early — a name clash with
    // another declaration, or one instance with two FB types — since
    // silently picking one would lead to a very confusing ironplc
    // diagnostic later, or none and a VM that never starts.
    if let Some(clash) = instance_name_clash(prog) {
        return Err(clash.into());
    }
    let fb_instances = collect_fb_instances(&prog.rungs)?;

    // Pre-scan: which rungs evaluate their network into a BOOL temporary.
    // ironplc requires every identifier to be declared in a VAR block
    // before use, so the names are fixed up-front and emitted as internal
    // variables. See `needs_temp` for which rungs, and why.
    let reserved = prog
        .variables
        .iter()
        .map(|v| v.name.as_str())
        .chain(fb_instances.keys().map(String::as_str));
    let rung_temps = rung_temporaries(&prog.rungs, reserved);

    em.line(None, format_args!("{} {}", head, prog.name));
    let declared_temps: Vec<String> = rung_temps.iter().flatten().cloned().collect();
    write_variable_blocks(&mut em, &prog.variables, &declared_temps, &fb_instances);
    em.blank();

    // FB instances already called this scan, with the inputs they were
    // called with — see `emit_fb_calls_for_rung`.
    let mut called: BTreeMap<String, String> = BTreeMap::new();
    for (idx, rung) in prog.rungs.iter().enumerate() {
        emit_rung(&mut em, rung, idx, rung_temps[idx].as_deref(), &mut called)?;
    }

    em.line(None, format_args!("{foot}"));
    Ok((em.out, LdSourceMap { lines: em.map }))
}

/// Whether a rung evaluates its network into a temporary before driving
/// its coils:
///
///   * **several coils** — evaluate once, so every coil sees the same value;
///   * **any set / reset coil** — `IF <network> THEN …` is the shape ironplc's
///     codegen cannot compile when the condition *begins* with an FB output
///     (`IF t.Q THEN`, `IF NOT e.Q THEN`, `IF (e.Q OR a) THEN` all fail with
///     P9999 "not implemented"), while the analyzer — which is all
///     `cs check` and the editor run — accepts it. A "timer done → set lamp"
///     rung therefore checked clean and could not be run. Assigning to a
///     temporary compiles for any network, and `IF <temporary> THEN` does too.
fn needs_temp(rung: &LdRung) -> bool {
    rung.coils.len() > 1 || rung.coils.iter().any(|c| c.kind != LdCoilKind::Standard)
}

/// One slot per rung: the temporary's name, or `None`. Names derive from
/// the rung id and must be unique the way IEC compares names — ignoring
/// case — among themselves and against `reserved` (the POU's variables
/// and FB instances). They are the transpiler's own names, so a clash is
/// renamed, not refused: two ids that sanitise alike, or differ only in
/// case (`Fill` / `fill`), get the rung index appended, then a counter.
fn rung_temporaries<'a>(
    rungs: &[LdRung],
    reserved: impl IntoIterator<Item = &'a str>,
) -> Vec<Option<String>> {
    let mut taken: HashSet<String> = reserved.into_iter().map(str::to_lowercase).collect();
    rungs
        .iter()
        .enumerate()
        .map(|(idx, rung)| {
            if !needs_temp(rung) {
                return None;
            }
            let base = format!("__rung_{}", sanitise_ident(&rung.id));
            let mut name = base.clone();
            let mut attempt = 0;
            while !taken.insert(name.to_lowercase()) {
                attempt += 1;
                name = match attempt {
                    1 => format!("{base}_{idx}"),
                    n => format!("{base}_{idx}_{n}"),
                };
            }
            Some(name)
        })
        .collect()
}

/// The first FB placement whose instance name clashes with another
/// declaration once IEC 61131-3's case-insensitive name rules apply, or
/// `None`. Both clashes are refused rather than guessed at:
///
///   * a placement whose name differs only in case from an earlier one
///     (`T1` / `t1`). An exact repeat shares one instance across rungs;
///     a case-only difference is ambiguous — one instance or two?
///   * a placement named like a variable the POU declares. The diagram
///     declares its FB instances itself, so both would be declared.
///
/// Located at the clashing placement so the editor can point at its
/// rung. Empty names are left to `collect_fb_instances`.
pub fn instance_name_clash(prog: &LdProgram) -> Option<NameClash<LdLocation>> {
    let variables: HashMap<String, &str> = prog
        .variables
        .iter()
        .map(|v| (v.name.to_lowercase(), v.name.as_str()))
        .collect();
    // lower-cased name → (first spelling, its rung id)
    let mut seen: HashMap<String, (String, &str)> = HashMap::new();
    for rung in &prog.rungs {
        let mut placed = Vec::new();
        let _ = walk_in_order(&rung.logic, &mut |node| {
            if let LdNode::FbCall { instance, .. } = node {
                placed.push(instance.clone());
            }
            Ok(())
        });
        for instance in placed.into_iter().filter(|i| !i.is_empty()) {
            let key = instance.to_lowercase();
            let message = if let Some(var) = variables.get(&key) {
                format!(
                    "FB instance '{instance}' in rung '{}' has the same name as the variable \
                     '{var}' (IEC 61131-3 names are case-insensitive). The diagram declares its \
                     FB instances itself — rename the instance, or remove or rename the variable",
                    rung.id
                )
            } else if let Some((first, first_rung)) = seen.get(&key) {
                if *first == instance {
                    continue;
                }
                format!(
                    "FB instance '{instance}' in rung '{}' differs only in case from instance \
                     '{first}' in rung '{first_rung}'. IEC 61131-3 names are case-insensitive, so \
                     both would declare the same name — use the same spelling to share one \
                     instance, or a different name for a separate one",
                    rung.id
                )
            } else {
                seen.insert(key, (instance, rung.id.as_str()));
                continue;
            };
            return Some(NameClash {
                message,
                location: LdLocation::FbCall {
                    rung_id: rung.id.clone(),
                    instance,
                },
            });
        }
    }
    None
}

/// Walk all rung logic trees, gathering every `FbCall` instance. Errors
/// if the same instance name appears with two different FB types.
fn collect_fb_instances(rungs: &[LdRung]) -> Result<FbInstanceTable, BridgeError> {
    let mut table = FbInstanceTable::new();
    for rung in rungs {
        walk_for_fb_calls(&rung.logic, &mut |instance, fb_type| {
            if let Some(prev) = table.get(instance) {
                if prev != fb_type {
                    return Err(BridgeError::Parse(format!(
                        "FB instance '{instance}' declared with conflicting types '{prev}' and '{fb_type}'"
                    )));
                }
            } else {
                table.insert(instance.to_string(), fb_type.to_string());
            }
            Ok(())
        })?;
    }
    Ok(table)
}

/// Tree walker that calls `visit(instance, fb_type)` for every
/// `FbCall` node encountered.
fn walk_for_fb_calls(
    node: &LdNode,
    visit: &mut dyn FnMut(&str, &str) -> Result<(), BridgeError>,
) -> Result<(), BridgeError> {
    match node {
        LdNode::Contact { .. } | LdNode::Const { .. } | LdNode::Compare { .. } => Ok(()),
        LdNode::And { args } | LdNode::Or { args } => {
            for a in args {
                walk_for_fb_calls(a, visit)?;
            }
            Ok(())
        }
        LdNode::Not { arg } => walk_for_fb_calls(arg, visit),
        LdNode::FbCall {
            instance, fb_type, ..
        } => {
            if instance.is_empty() {
                return Err(BridgeError::Parse("FB call has empty instance name".into()));
            }
            if fb_type.is_empty() {
                return Err(BridgeError::Parse(format!(
                    "FB call '{instance}' has empty fb_type"
                )));
            }
            visit(instance, fb_type)
        }
    }
}

fn write_variable_blocks(
    em: &mut StEmitter,
    vars: &[LdVariable],
    rung_temps: &[String],
    fb_instances: &FbInstanceTable,
) {
    // IEC requires VAR blocks in a specific order with no duplicates,
    // and ironplc's `allow_empty_var_blocks` already lets us emit
    // empty sections — so we always emit all three for shape stability.
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
                Some(LdLocation::Variable {
                    name: v.name.clone(),
                }),
                format_args!("        {} : {}{};", v.name, v.type_name, init),
            );
        }
        // The VAR block also carries (a) multi-coil rung temporaries
        // (`__rung_<id> : BOOL`) and (b) FB instances synthesised
        // from FbCall nodes (`myT1 : TON;`). They're transpiler
        // bookkeeping; we keep them in the same block as the user's
        // internal vars because ironplc parses one VAR section per
        // kind. Synthetic lines have no LD origin — diagnostics on
        // them would be transpiler bugs, not user authoring problems.
        if section == LdVarSection::Internal {
            for tmp in rung_temps {
                em.line(None, format_args!("        {tmp} : BOOL;"));
            }
            for (instance, fb_type) in fb_instances {
                em.line(None, format_args!("        {instance} : {fb_type};"));
            }
        }
        em.line(None, format_args!("    END_VAR"));
    }
}

fn emit_rung(
    em: &mut StEmitter,
    rung: &project::LdRung,
    idx: usize,
    temp: Option<&str>,
    called: &mut BTreeMap<String, String>,
) -> Result<(), BridgeError> {
    if rung.coils.is_empty() {
        return Err(BridgeError::Parse(format!(
            "LD rung {} ({}) has no coils — a rung with no output is dead code",
            idx, rung.id
        )));
    }
    let logic = render_node(&rung.logic)?;
    let rung_span = LdLocation::Rung {
        rung_id: rung.id.clone(),
    };
    if let Some(label) = &rung.label {
        em.line(
            Some(rung_span.clone()),
            format_args!("    (* rung {}: {} *)", rung.id, label),
        );
    } else {
        em.line(
            Some(rung_span.clone()),
            format_args!("    (* rung {} *)", rung.id),
        );
    }

    // Emit FB call statements for any FbCall nodes in this rung's
    // logic tree, BEFORE the coil assignment. ironplc's call-by-name
    // syntax is `inst(PIN := value, PIN := value);`. Each instance is
    // called once per scan, at the first rung that places it; later
    // references read its output pin via dot syntax.
    emit_fb_calls_for_rung(em, &rung.id, &rung.logic, called)?;

    match temp {
        None => emit_coil(em, &rung.coils[0], 0, &rung.id, &logic),
        Some(tmp) => {
            em.line(
                Some(rung_span.clone()),
                format_args!("    {tmp} := {logic};"),
            );
            for (i, coil) in rung.coils.iter().enumerate() {
                emit_coil(em, coil, i, &rung.id, tmp);
            }
        }
    }
    em.blank();
    Ok(())
}

/// Walk the rung's logic tree in source order and emit one
/// `instance(PIN := value, ...);` line for each FbCall instance that has
/// not been called yet this scan.
///
/// An instance runs **once per scan**, at the first rung that places it.
/// It used to run once per rung, on the theory that a second call "sees
/// CLK unchanged, so it's a no-op" — true of an edge detector's memory,
/// false of its output: R_TRIG's second call computes `Q := CLK AND NOT M`
/// with `M` already TRUE, clearing the edge before any later rung could
/// read it. A later placement may only read the output, so it must bind
/// exactly the same inputs; different ones are an error rather than a
/// second, silently competing call.
fn emit_fb_calls_for_rung(
    em: &mut StEmitter,
    rung_id: &str,
    logic: &LdNode,
    called: &mut BTreeMap<String, String>,
) -> Result<(), BridgeError> {
    // Collect the FB calls first (order + uniqueness) before emitting,
    // so we don't have to thread the emitter through the visitor's
    // FnMut closure (which conflicts with rustc's borrow checker when
    // both em and the closure capture mutate state).
    let mut calls: Vec<(String, String)> = Vec::new();
    let mut collect = |node: &LdNode| -> Result<(), BridgeError> {
        if let LdNode::FbCall {
            instance, inputs, ..
        } = node
        {
            let args = render_fb_inputs(inputs)?;
            let earlier = called
                .get(instance)
                .or_else(|| calls.iter().find(|(i, _)| i == instance).map(|(_, a)| a));
            if let Some(first) = earlier {
                if *first != args {
                    return Err(BridgeError::Parse(format!(
                        "FB instance '{instance}' is placed again in rung '{rung_id}' with inputs \
                         ({args}) that differ from its first placement ({first}); an instance runs \
                         once per scan, so a later placement can only read its outputs — bind the \
                         same inputs, or use a separate instance"
                    )));
                }
            } else {
                calls.push((instance.clone(), args));
            }
        }
        Ok(())
    };
    walk_in_order(logic, &mut collect)?;
    for (instance, args) in calls {
        called.insert(instance.clone(), args.clone());
        em.line(
            Some(LdLocation::FbCall {
                rung_id: rung_id.to_string(),
                instance: instance.clone(),
            }),
            format_args!("    {instance}({args});"),
        );
    }
    Ok(())
}

/// In-order traversal that calls `visit` on every node. Used by
/// `emit_fb_calls_for_rung` so the call order matches authoring order.
fn walk_in_order(
    node: &LdNode,
    visit: &mut dyn FnMut(&LdNode) -> Result<(), BridgeError>,
) -> Result<(), BridgeError> {
    visit(node)?;
    match node {
        LdNode::Contact { .. }
        | LdNode::Const { .. }
        | LdNode::Compare { .. }
        | LdNode::FbCall { .. } => Ok(()),
        LdNode::And { args } | LdNode::Or { args } => {
            for a in args {
                walk_in_order(a, visit)?;
            }
            Ok(())
        }
        LdNode::Not { arg } => walk_in_order(arg, visit),
    }
}

/// Format an FB's input pins as a comma-separated `PIN := value` list
/// ready to drop inside an `inst(...)` call.
fn render_fb_inputs(inputs: &[LdFbInput]) -> Result<String, BridgeError> {
    let mut parts = Vec::with_capacity(inputs.len());
    for inp in inputs {
        if inp.pin.is_empty() {
            return Err(BridgeError::Parse("FB call has empty pin name".into()));
        }
        let value = render_operand(&inp.value)?;
        parts.push(format!("{} := {}", inp.pin, value));
    }
    Ok(parts.join(", "))
}

fn emit_coil(
    em: &mut StEmitter,
    coil: &project::LdCoil,
    coil_index: usize,
    rung_id: &str,
    expr: &str,
) {
    let span = Some(LdLocation::Coil {
        rung_id: rung_id.to_string(),
        coil_index,
    });
    match coil.kind {
        LdCoilKind::Standard => {
            em.line(span, format_args!("    {} := {};", coil.var, expr));
        }
        LdCoilKind::Set => {
            em.line(
                span,
                format_args!("    IF {expr} THEN {} := TRUE; END_IF;", coil.var),
            );
        }
        LdCoilKind::Reset => {
            em.line(
                span,
                format_args!("    IF {expr} THEN {} := FALSE; END_IF;", coil.var),
            );
        }
    }
}

/// Render a boolean network into a parenthesised ST expression.
/// The recursion produces the obvious mapping:
///   contact{var}                → `var`            (or `NOT var` if negated)
///   and{args:[a,b,c]}           → `(a AND b AND c)`
///   or{args:[a,b]}              → `(a OR b)`
///   not{arg:a}                  → `(NOT a)`
///   const{value:true}           → `TRUE`
///
/// Empty AND/OR collapse to identity literals (`TRUE` / `FALSE`)
/// rather than erroring — saves UI clients from having to special-
/// case in-progress edits where a parallel branch is currently empty.
fn render_node(node: &LdNode) -> Result<String, BridgeError> {
    Ok(match node {
        LdNode::Contact { var, negated } => {
            if var.is_empty() {
                return Err(BridgeError::Parse(
                    "LD contact has empty variable name".into(),
                ));
            }
            if *negated {
                format!("NOT {var}")
            } else {
                var.clone()
            }
        }
        LdNode::And { args } => combine(args, "AND", "TRUE")?,
        LdNode::Or { args } => combine(args, "OR", "FALSE")?,
        LdNode::Not { arg } => format!("(NOT {})", render_node(arg)?),
        LdNode::Const { value } => if *value { "TRUE" } else { "FALSE" }.to_string(),
        LdNode::Compare { left, cmp, right } => {
            // Compare block — bridges a numeric var into the boolean
            // network by emitting `(left CMP right)` as a parenthesised
            // ST sub-expression. ironplc parses these as normal
            // comparison operators against any numeric type, so the
            // user can mix INT, REAL, TIME literals etc. on either side.
            let lhs = render_operand(left)?;
            let rhs = render_operand(right)?;
            let op_str = match cmp {
                LdComparator::Eq => "=",
                LdComparator::Ne => "<>",
                LdComparator::Lt => "<",
                LdComparator::Le => "<=",
                LdComparator::Gt => ">",
                LdComparator::Ge => ">=",
            };
            format!("({lhs} {op_str} {rhs})")
        }
        LdNode::FbCall {
            instance,
            output_pin,
            ..
        } => {
            // The actual call statement (`instance(IN := ..., ...);`)
            // was already emitted by `emit_fb_calls_for_rung` before
            // we got here. In the boolean expression, an FbCall node
            // contributes its chosen output pin's value: `instance.Q`,
            // `instance.QU`, etc. The dot-access syntax is standard
            // IEC 61131-3 — ironplc parses it as a member reference
            // on the FB instance.
            if instance.is_empty() {
                return Err(BridgeError::Parse("FB call has empty instance name".into()));
            }
            if output_pin.is_empty() {
                return Err(BridgeError::Parse(format!(
                    "FB call '{instance}' has empty output_pin"
                )));
            }
            format!("{instance}.{output_pin}")
        }
    })
}

fn render_operand(o: &LdOperand) -> Result<String, BridgeError> {
    Ok(match o {
        LdOperand::Var { name } => {
            if name.is_empty() {
                return Err(BridgeError::Parse(
                    "Compare operand has empty variable name".into(),
                ));
            }
            name.clone()
        }
        LdOperand::Literal { value } => {
            if value.is_empty() {
                return Err(BridgeError::Parse(
                    "Compare operand has empty literal".into(),
                ));
            }
            value.clone()
        }
    })
}

fn combine(args: &[LdNode], op: &str, identity: &str) -> Result<String, BridgeError> {
    match args.len() {
        0 => Ok(identity.to_string()),
        1 => render_node(&args[0]),
        _ => {
            let mut parts = Vec::with_capacity(args.len());
            for a in args {
                parts.push(render_node(a)?);
            }
            Ok(format!("({})", parts.join(&format!(" {op} "))))
        }
    }
}

/// Borrowed from the synthesise-configuration code path in lib.rs —
/// turn an arbitrary rung id into something IEC allows as an
/// identifier (alphanumeric + underscore, leading char a letter).
/// Kept local so this module stays drop-in.
fn sanitise_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 1);
    let mut chars = s.chars();
    if let Some(c) = chars.next() {
        if c.is_ascii_alphabetic() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    for c in chars {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

// =================================================================
//   Tests
// =================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use project::{LdCoil, LdRung, LdVariable};

    fn motor_seal_program() -> LdProgram {
        // Classic seal-in: pressing Start latches motor_run, which
        // remains latched until Stop is pressed.
        //   motor_run := Start OR (motor_run AND NOT Stop)
        LdProgram {
            name: "motor".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "start_btn".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "stop_btn".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "motor_run".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: Some("FALSE".into()),
                },
            ],
            rungs: vec![LdRung {
                id: "r0".into(),
                label: Some("motor seal-in".into()),
                logic: LdNode::Or {
                    args: vec![
                        LdNode::Contact {
                            var: "start_btn".into(),
                            negated: false,
                        },
                        LdNode::And {
                            args: vec![
                                LdNode::Contact {
                                    var: "motor_run".into(),
                                    negated: false,
                                },
                                LdNode::Contact {
                                    var: "stop_btn".into(),
                                    negated: true,
                                },
                            ],
                        },
                    ],
                },
                coils: vec![LdCoil {
                    var: "motor_run".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        }
    }

    #[test]
    fn seal_in_transpiles_to_expected_st() {
        let st = transpile_to_st(&motor_seal_program()).unwrap();
        assert!(st.contains("PROGRAM motor"));
        assert!(st.contains("VAR_INPUT"));
        assert!(st.contains("start_btn : BOOL"));
        assert!(st.contains("motor_run : BOOL := FALSE"));
        // The full network: (start_btn OR (motor_run AND NOT stop_btn))
        assert!(
            st.contains("motor_run := (start_btn OR (motor_run AND NOT stop_btn));"),
            "unexpected ST:\n{st}"
        );
        assert!(st.contains("END_PROGRAM"));
    }

    #[test]
    fn negated_contact_becomes_not() {
        let st = transpile_to_st(&LdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![LdVariable {
                name: "x".into(),
                type_name: "BOOL".into(),
                section: LdVarSection::Internal,
                init: None,
            }],
            rungs: vec![LdRung {
                id: "r".into(),
                label: None,
                logic: LdNode::Contact {
                    var: "x".into(),
                    negated: true,
                },
                coils: vec![LdCoil {
                    var: "x".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        })
        .unwrap();
        assert!(st.contains("x := NOT x;"));
    }

    #[test]
    fn set_and_reset_coils_emit_if_blocks() {
        let prog = LdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "trig".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "latched".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![
                LdRung {
                    id: "set".into(),
                    label: None,
                    logic: LdNode::Contact {
                        var: "trig".into(),
                        negated: false,
                    },
                    coils: vec![LdCoil {
                        var: "latched".into(),
                        kind: LdCoilKind::Set,
                    }],
                },
                LdRung {
                    id: "rst".into(),
                    label: None,
                    logic: LdNode::Contact {
                        var: "trig".into(),
                        negated: true,
                    },
                    coils: vec![LdCoil {
                        var: "latched".into(),
                        kind: LdCoilKind::Reset,
                    }],
                },
            ],
        };
        let st = transpile_to_st(&prog).unwrap();
        assert!(st.contains("__rung_set := trig;"), "{st}");
        assert!(
            st.contains("IF __rung_set THEN latched := TRUE; END_IF;"),
            "{st}"
        );
        assert!(st.contains("__rung_rst := NOT trig;"), "{st}");
        assert!(
            st.contains("IF __rung_rst THEN latched := FALSE; END_IF;"),
            "{st}"
        );
        assert!(
            st.contains("__rung_set : BOOL;") && st.contains("__rung_rst : BOOL;"),
            "{st}"
        );
    }

    #[test]
    fn empty_and_or_collapse_to_identity() {
        assert_eq!(render_node(&LdNode::And { args: vec![] }).unwrap(), "TRUE");
        assert_eq!(render_node(&LdNode::Or { args: vec![] }).unwrap(), "FALSE");
    }

    #[test]
    fn compare_block_emits_parenthesised_comparison() {
        // `temperature < 50.0`: a var on the left, a literal on the right.
        let node = LdNode::Compare {
            left: LdOperand::Var {
                name: "temperature".into(),
            },
            cmp: LdComparator::Lt,
            right: LdOperand::Literal {
                value: "50.0".into(),
            },
        };
        assert_eq!(render_node(&node).unwrap(), "(temperature < 50.0)");

        // Two-variable compare:
        let node2 = LdNode::Compare {
            left: LdOperand::Var { name: "pv".into() },
            cmp: LdComparator::Ge,
            right: LdOperand::Var { name: "sp".into() },
        };
        assert_eq!(render_node(&node2).unwrap(), "(pv >= sp)");

        // Inequality keyword maps to ST's `<>`:
        let node3 = LdNode::Compare {
            left: LdOperand::Var { name: "x".into() },
            cmp: LdComparator::Ne,
            right: LdOperand::Literal { value: "0".into() },
        };
        assert_eq!(render_node(&node3).unwrap(), "(x <> 0)");
    }

    #[test]
    fn compare_block_works_inside_and_chain() {
        // Real usage: a series AND with a compare block in the middle.
        //   start_btn AND (temperature < 50.0) AND NOT stop_btn
        let prog = LdProgram {
            name: "guard".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "start_btn".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "stop_btn".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "temperature".into(),
                    type_name: "REAL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "heater".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![LdRung {
                id: "guarded_heater".into(),
                label: None,
                logic: LdNode::And {
                    args: vec![
                        LdNode::Contact {
                            var: "start_btn".into(),
                            negated: false,
                        },
                        LdNode::Compare {
                            left: LdOperand::Var {
                                name: "temperature".into(),
                            },
                            cmp: LdComparator::Lt,
                            right: LdOperand::Literal {
                                value: "50.0".into(),
                            },
                        },
                        LdNode::Contact {
                            var: "stop_btn".into(),
                            negated: true,
                        },
                    ],
                },
                coils: vec![LdCoil {
                    var: "heater".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        };
        let st = transpile_to_st(&prog).unwrap();
        assert!(
            st.contains("heater := (start_btn AND (temperature < 50.0) AND NOT stop_btn);"),
            "unexpected ST:\n{st}"
        );
    }

    #[test]
    fn multiple_coil_rung_declares_temporary_in_var_block() {
        // Regression: ironplc refuses ST that references an undeclared
        // identifier, so the `__rung_X` temp the transpiler synthesises
        // for multi-coil rungs must show up in the VAR section.
        let prog = LdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "a".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "x".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
                LdVariable {
                    name: "y".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![LdRung {
                id: "multi".into(),
                label: None,
                logic: LdNode::Contact {
                    var: "a".into(),
                    negated: false,
                },
                coils: vec![
                    LdCoil {
                        var: "x".into(),
                        kind: LdCoilKind::Standard,
                    },
                    LdCoil {
                        var: "y".into(),
                        kind: LdCoilKind::Standard,
                    },
                ],
            }],
        };
        let st = transpile_to_st(&prog).unwrap();
        // The temp must be declared in VAR before the rung uses it.
        let temp_decl_pos = st
            .find("__rung_multi : BOOL")
            .expect("temp must be declared");
        let temp_use_pos = st.find("__rung_multi := a").expect("temp must be used");
        assert!(
            temp_decl_pos < temp_use_pos,
            "temp declaration must precede its use:\n{st}"
        );
        // And it must sit inside the internal VAR block, not VAR_INPUT/OUTPUT.
        let internal_var_pos = st.rfind("    VAR\n").expect("internal VAR block present");
        assert!(
            temp_decl_pos > internal_var_pos,
            "temp must be inside the internal VAR block, not the input/output ones"
        );
    }

    #[test]
    fn fb_call_emits_instance_decl_call_stmt_and_dot_access() {
        // The smallest possible TON example: button → 3-second delay → motor.
        // Verifies all three pieces of the FbCall transpile contract:
        //   1. `myT : TON;` lands in the internal VAR block
        //   2. `myT(IN := btn, PT := T#3s);` is emitted before the coil
        //   3. The boolean expression reads `myT.Q`
        let prog = LdProgram {
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
                    name: "motor".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![LdRung {
                id: "delayed_start".into(),
                label: None,
                logic: LdNode::FbCall {
                    instance: "myT".into(),
                    fb_type: "TON".into(),
                    inputs: vec![
                        LdFbInput {
                            pin: "IN".into(),
                            value: LdOperand::Var { name: "btn".into() },
                        },
                        LdFbInput {
                            pin: "PT".into(),
                            value: LdOperand::Literal {
                                value: "T#3s".into(),
                            },
                        },
                    ],
                    output_pin: "Q".into(),
                },
                coils: vec![LdCoil {
                    var: "motor".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        };
        let st = transpile_to_st(&prog).unwrap();

        // VAR declaration
        let decl_pos = st.find("myT : TON").expect("FB instance must be declared");
        // Call statement
        let call_pos = st
            .find("myT(IN := btn, PT := T#3s);")
            .expect("FB call statement must be emitted");
        // Boolean expression reads the output pin
        let use_pos = st
            .find("motor := myT.Q;")
            .expect("coil assignment must read myT.Q");

        // Declaration before use is mandatory for ironplc
        assert!(decl_pos < call_pos, "VAR decl must come before call:\n{st}");
        // Call must precede the read for IEC scan semantics
        assert!(call_pos < use_pos, "FB call must precede .Q read:\n{st}");
    }

    #[test]
    fn fb_call_inside_and_chain_renders_dot_access_in_expression() {
        // Guard pattern: only run motor while button held AND 3s elapsed.
        //   motor := btn AND myT.Q
        // The FbCall lives mid-expression and its position becomes
        // `myT.Q`; the call statement is hoisted before the assignment.
        let prog = LdProgram {
            name: "guarded".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "btn".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "motor".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![LdRung {
                id: "r".into(),
                label: None,
                logic: LdNode::And {
                    args: vec![
                        LdNode::Contact {
                            var: "btn".into(),
                            negated: false,
                        },
                        LdNode::FbCall {
                            instance: "myT".into(),
                            fb_type: "TON".into(),
                            inputs: vec![
                                LdFbInput {
                                    pin: "IN".into(),
                                    value: LdOperand::Var { name: "btn".into() },
                                },
                                LdFbInput {
                                    pin: "PT".into(),
                                    value: LdOperand::Literal {
                                        value: "T#3s".into(),
                                    },
                                },
                            ],
                            output_pin: "Q".into(),
                        },
                    ],
                },
                coils: vec![LdCoil {
                    var: "motor".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        };
        let st = transpile_to_st(&prog).unwrap();
        assert!(
            st.contains("motor := (btn AND myT.Q);"),
            "expression should fold FbCall to dot-access:\n{st}"
        );
        assert!(st.contains("myT(IN := btn, PT := T#3s);"));
    }

    #[test]
    fn fb_call_dedupes_same_instance_within_rung() {
        // If a rung references the same FB instance twice (e.g. via
        // OR), only one call statement should be emitted — calling
        // edge detectors twice would mask the rising edge on the
        // second invocation.
        let prog = LdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "clk".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "out".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![LdRung {
                id: "r".into(),
                label: None,
                logic: LdNode::Or {
                    args: vec![
                        LdNode::FbCall {
                            instance: "edge".into(),
                            fb_type: "R_TRIG".into(),
                            inputs: vec![LdFbInput {
                                pin: "CLK".into(),
                                value: LdOperand::Var { name: "clk".into() },
                            }],
                            output_pin: "Q".into(),
                        },
                        LdNode::FbCall {
                            instance: "edge".into(),
                            fb_type: "R_TRIG".into(),
                            inputs: vec![LdFbInput {
                                pin: "CLK".into(),
                                value: LdOperand::Var { name: "clk".into() },
                            }],
                            output_pin: "Q".into(),
                        },
                    ],
                },
                coils: vec![LdCoil {
                    var: "out".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        };
        let st = transpile_to_st(&prog).unwrap();
        let n = st.matches("edge(").count();
        assert_eq!(n, 1, "FB call should be emitted exactly once:\n{st}");
    }

    #[test]
    fn fb_call_conflicting_types_for_same_instance_errors() {
        // Defensive: if a project somehow ends up with the same
        // instance name bound to two different FB types, we must
        // surface a clear error — ironplc would otherwise emit a
        // confusing duplicate-declaration error.
        let prog = LdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "a".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "o".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![
                LdRung {
                    id: "r1".into(),
                    label: None,
                    logic: LdNode::FbCall {
                        instance: "t".into(),
                        fb_type: "TON".into(),
                        inputs: vec![],
                        output_pin: "Q".into(),
                    },
                    coils: vec![LdCoil {
                        var: "o".into(),
                        kind: LdCoilKind::Standard,
                    }],
                },
                LdRung {
                    id: "r2".into(),
                    label: None,
                    logic: LdNode::FbCall {
                        instance: "t".into(),
                        fb_type: "TOF".into(),
                        inputs: vec![],
                        output_pin: "Q".into(),
                    },
                    coils: vec![LdCoil {
                        var: "o".into(),
                        kind: LdCoilKind::Standard,
                    }],
                },
            ],
        };
        let err = transpile_to_st(&prog).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("conflicting types"), "got: {msg}");
    }

    #[test]
    fn fb_call_with_alternate_output_pin_uses_it_in_expression() {
        // CTUD has QU and QD; verify the renderer respects the
        // selected pin instead of always defaulting to .Q.
        let prog = LdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "cu".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "underflow".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![LdRung {
                id: "r".into(),
                label: None,
                logic: LdNode::FbCall {
                    instance: "c".into(),
                    fb_type: "CTUD".into(),
                    inputs: vec![LdFbInput {
                        pin: "CU".into(),
                        value: LdOperand::Var { name: "cu".into() },
                    }],
                    output_pin: "QD".into(),
                },
                coils: vec![LdCoil {
                    var: "underflow".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        };
        let st = transpile_to_st(&prog).unwrap();
        assert!(st.contains("c : CTUD;"));
        assert!(st.contains("underflow := c.QD;"), "got:\n{st}");
    }

    #[test]
    fn multiple_coils_use_temporary() {
        let prog = LdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "a".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "x".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
                LdVariable {
                    name: "y".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![LdRung {
                id: "multi".into(),
                label: None,
                logic: LdNode::Contact {
                    var: "a".into(),
                    negated: false,
                },
                coils: vec![
                    LdCoil {
                        var: "x".into(),
                        kind: LdCoilKind::Standard,
                    },
                    LdCoil {
                        var: "y".into(),
                        kind: LdCoilKind::Standard,
                    },
                ],
            }],
        };
        let st = transpile_to_st(&prog).unwrap();
        assert!(st.contains("__rung_multi := a;"));
        assert!(st.contains("x := __rung_multi;"));
        assert!(st.contains("y := __rung_multi;"));
    }

    // =================================================================
    //   End-to-end: our generated ST must actually compile in ironplc.
    //   These cover every standard FB the editor will expose, so that
    //   a UI regression that emits a bad pin name fails here too.
    // =================================================================

    /// Build a one-rung program with a single FbCall driving one coil.
    /// `inputs` is `(pin, var_name_or_literal)`; literals are detected
    /// by leading `T#` or digit.
    fn single_fb_rung_program(
        fb_type: &str,
        instance: &str,
        out_pin: &str,
        inputs: &[(&str, &str)],
        extra_vars: &[(&str, &str, LdVarSection)],
    ) -> LdProgram {
        let mut variables = vec![LdVariable {
            name: "out".into(),
            type_name: "BOOL".into(),
            section: LdVarSection::Output,
            init: None,
        }];
        for (name, ty, sec) in extra_vars {
            variables.push(LdVariable {
                name: (*name).into(),
                type_name: (*ty).into(),
                section: *sec,
                init: None,
            });
        }
        let fb_inputs = inputs
            .iter()
            .map(|(pin, val)| {
                let is_literal = val.starts_with("T#")
                    || val.starts_with(|c: char| c.is_ascii_digit())
                    || *val == "TRUE"
                    || *val == "FALSE";
                LdFbInput {
                    pin: (*pin).into(),
                    value: if is_literal {
                        LdOperand::Literal {
                            value: (*val).into(),
                        }
                    } else {
                        LdOperand::Var {
                            name: (*val).into(),
                        }
                    },
                }
            })
            .collect();
        LdProgram {
            name: format!("smoke_{}", instance),
            pou_type: LdPouType::Program,
            variables,
            rungs: vec![LdRung {
                id: "r".into(),
                label: None,
                logic: LdNode::FbCall {
                    instance: instance.into(),
                    fb_type: fb_type.into(),
                    inputs: fb_inputs,
                    output_pin: out_pin.into(),
                },
                coils: vec![LdCoil {
                    var: "out".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        }
    }

    /// Asserts that transpiling `prog` yields ST that ironplc accepts
    /// without any error diagnostics. Warnings/info are tolerated.
    fn assert_compiles_clean(prog: &LdProgram) {
        let st = transpile_to_st(prog).unwrap();
        // The analyzer (what `check` runs) accepts shapes ironplc's codegen
        // rejects, so a clean check alone never proved a program would run.
        if let Err(e) = crate::compile(&st) {
            panic!(
                "ironplc cannot generate code for our ST for {}:\n{st}\n{e}",
                prog.name
            );
        }
        let diags = crate::check(&st);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| matches!(d.severity.as_str(), "error" | "Error"))
            .collect();
        assert!(
            errors.is_empty(),
            "ironplc rejected our ST for {}:\n--- ST ---\n{}\n--- DIAG ---\n{:#?}",
            prog.name,
            st,
            errors
        );
    }

    // ---- behaviour, run in the VM ---------------------------------------

    fn bool_var(name: &str, init: Option<&str>) -> LdVariable {
        LdVariable {
            name: name.into(),
            type_name: "BOOL".into(),
            section: LdVarSection::Internal,
            init: init.map(Into::into),
        }
    }

    fn contact(var: &str) -> LdNode {
        LdNode::Contact {
            var: var.into(),
            negated: false,
        }
    }

    fn fb(instance: &str, fb_type: &str, pins: &[(&str, LdOperand)], out: &str) -> LdNode {
        LdNode::FbCall {
            instance: instance.into(),
            fb_type: fb_type.into(),
            inputs: pins
                .iter()
                .map(|(pin, value)| LdFbInput {
                    pin: (*pin).into(),
                    value: value.clone(),
                })
                .collect(),
            output_pin: out.into(),
        }
    }

    fn var_op(name: &str) -> LdOperand {
        LdOperand::Var { name: name.into() }
    }

    fn lit(value: &str) -> LdOperand {
        LdOperand::Literal {
            value: value.into(),
        }
    }

    fn rung(id: &str, logic: LdNode, coil: &str, kind: LdCoilKind) -> LdRung {
        LdRung {
            id: id.into(),
            label: None,
            logic,
            coils: vec![LdCoil {
                var: coil.into(),
                kind,
            }],
        }
    }

    fn program(variables: Vec<LdVariable>, rungs: Vec<LdRung>) -> LdProgram {
        LdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables,
            rungs,
        }
    }

    /// "Timer done → set lamp" and "edge → reset": a set/reset coil whose
    /// network begins with an FB output. `check` passed these while the
    /// runtime could not compile them (P9999 "not implemented").
    #[test]
    fn set_and_reset_coils_driven_by_an_fb_compile_and_latch() {
        let edge = || fb("stop_edge", "R_TRIG", &[("CLK", var_op("stop"))], "Q");
        let prog = program(
            vec![
                bool_var("start", Some("TRUE")),
                bool_var("stop", Some("TRUE")),
                bool_var("lamp", None),
                bool_var("cleared", Some("TRUE")),
            ],
            vec![
                rung(
                    "done",
                    fb(
                        "dwell",
                        "TON",
                        &[("IN", var_op("start")), ("PT", lit("T#100ms"))],
                        "Q",
                    ),
                    "lamp",
                    LdCoilKind::Set,
                ),
                rung(
                    "clear",
                    LdNode::Not {
                        arg: Box::new(LdNode::Not {
                            arg: Box::new(edge()),
                        }),
                    },
                    "cleared",
                    LdCoilKind::Reset,
                ),
            ],
        );
        let st = transpile_to_st(&prog).unwrap();
        let v = crate::test_vm::run_st(&st, 5);
        assert_eq!(v["lamp"], 1, "the set coil never latched:\n{st}");
        assert_eq!(v["cleared"], 0, "the reset coil never unlatched:\n{st}");
    }

    /// One R_TRIG placed in two rungs. The second rung used to call it
    /// again, which clears Q in the same scan — so it never saw the edge.
    #[test]
    fn every_rung_placing_an_edge_detector_sees_the_edge() {
        let edge = || fb("start_edge", "R_TRIG", &[("CLK", var_op("btn"))], "Q");
        let seal = |id: &str, var: &str| {
            rung(
                id,
                LdNode::Or {
                    args: vec![contact(var), edge()],
                },
                var,
                LdCoilKind::Standard,
            )
        };
        let prog = program(
            vec![
                bool_var("btn", Some("TRUE")),
                bool_var("seen_first", None),
                bool_var("seen_second", None),
            ],
            vec![seal("r1", "seen_first"), seal("r2", "seen_second")],
        );
        let st = transpile_to_st(&prog).unwrap();
        assert_eq!(st.matches("start_edge(").count(), 1, "{st}");
        let v = crate::test_vm::run_st(&st, 5);
        assert_eq!(
            (v["seen_first"], v["seen_second"]),
            (1, 1),
            "a later rung missed the rising edge:\n{st}"
        );
    }

    #[test]
    fn placing_an_instance_again_with_different_inputs_is_refused() {
        let prog = program(
            vec![
                bool_var("a", None),
                bool_var("b", None),
                bool_var("x", None),
            ],
            vec![
                rung(
                    "r1",
                    fb("e", "R_TRIG", &[("CLK", var_op("a"))], "Q"),
                    "x",
                    LdCoilKind::Standard,
                ),
                rung(
                    "r2",
                    fb("e", "R_TRIG", &[("CLK", var_op("b"))], "Q"),
                    "x",
                    LdCoilKind::Standard,
                ),
            ],
        );
        let err = transpile_to_st(&prog).unwrap_err().to_string();
        assert!(
            err.contains("'e'") && err.contains("r2") && err.contains("once per scan"),
            "{err}"
        );

        // The same mismatch inside one rung is refused too.
        let prog = program(
            vec![
                bool_var("a", None),
                bool_var("b", None),
                bool_var("x", None),
            ],
            vec![rung(
                "r1",
                LdNode::Or {
                    args: vec![
                        fb("e", "R_TRIG", &[("CLK", var_op("a"))], "Q"),
                        fb("e", "R_TRIG", &[("CLK", var_op("b"))], "Q"),
                    ],
                },
                "x",
                LdCoilKind::Standard,
            )],
        );
        assert!(transpile_to_st(&prog).is_err());
    }

    #[test]
    fn rung_ids_that_sanitise_alike_get_distinct_temporaries() {
        let prog = program(
            vec![
                bool_var("a", None),
                bool_var("x", None),
                bool_var("y", None),
            ],
            vec![
                rung("r-1", contact("a"), "x", LdCoilKind::Set),
                rung("r_1", contact("a"), "y", LdCoilKind::Set),
            ],
        );
        let st = transpile_to_st(&prog).unwrap();
        assert!(
            st.contains("__rung_r_1 : BOOL;") && st.contains("__rung_r_1_1 : BOOL;"),
            "{st}"
        );
        let v = crate::test_vm::run_st(&st, 1);
        assert_eq!((v["x"], v["y"]), (0, 0));
    }

    #[test]
    fn end_to_end_ton_compiles() {
        let prog = single_fb_rung_program(
            "TON",
            "myT",
            "Q",
            &[("IN", "btn"), ("PT", "T#3s")],
            &[("btn", "BOOL", LdVarSection::Input)],
        );
        assert_compiles_clean(&prog);
    }

    #[test]
    fn end_to_end_tof_compiles() {
        let prog = single_fb_rung_program(
            "TOF",
            "myTof",
            "Q",
            &[("IN", "trig"), ("PT", "T#500ms")],
            &[("trig", "BOOL", LdVarSection::Input)],
        );
        assert_compiles_clean(&prog);
    }

    #[test]
    fn end_to_end_tp_compiles() {
        let prog = single_fb_rung_program(
            "TP",
            "myPulse",
            "Q",
            &[("IN", "trig"), ("PT", "T#100ms")],
            &[("trig", "BOOL", LdVarSection::Input)],
        );
        assert_compiles_clean(&prog);
    }

    #[test]
    fn end_to_end_ctu_compiles() {
        let prog = single_fb_rung_program(
            "CTU",
            "myCnt",
            "Q",
            &[("CU", "click"), ("R", "rst"), ("PV", "5")],
            &[
                ("click", "BOOL", LdVarSection::Input),
                ("rst", "BOOL", LdVarSection::Input),
            ],
        );
        assert_compiles_clean(&prog);
    }

    #[test]
    fn end_to_end_ctd_compiles() {
        let prog = single_fb_rung_program(
            "CTD",
            "myDown",
            "Q",
            &[("CD", "click"), ("LD", "load"), ("PV", "5")],
            &[
                ("click", "BOOL", LdVarSection::Input),
                ("load", "BOOL", LdVarSection::Input),
            ],
        );
        assert_compiles_clean(&prog);
    }

    #[test]
    fn end_to_end_ctud_qu_compiles() {
        let prog = single_fb_rung_program(
            "CTUD",
            "myUd",
            "QU",
            &[
                ("CU", "up"),
                ("CD", "dn"),
                ("R", "rst"),
                ("LD", "load"),
                ("PV", "5"),
            ],
            &[
                ("up", "BOOL", LdVarSection::Input),
                ("dn", "BOOL", LdVarSection::Input),
                ("rst", "BOOL", LdVarSection::Input),
                ("load", "BOOL", LdVarSection::Input),
            ],
        );
        assert_compiles_clean(&prog);
    }

    #[test]
    fn end_to_end_r_trig_compiles() {
        let prog = single_fb_rung_program(
            "R_TRIG",
            "myEdge",
            "Q",
            &[("CLK", "btn")],
            &[("btn", "BOOL", LdVarSection::Input)],
        );
        assert_compiles_clean(&prog);
    }

    #[test]
    fn end_to_end_f_trig_compiles() {
        let prog = single_fb_rung_program(
            "F_TRIG",
            "myFall",
            "Q",
            &[("CLK", "btn")],
            &[("btn", "BOOL", LdVarSection::Input)],
        );
        assert_compiles_clean(&prog);
    }

    #[test]
    fn end_to_end_sr_compiles() {
        let prog = single_fb_rung_program(
            "SR",
            "mySr",
            "Q1",
            &[("S1", "set_btn"), ("R", "rst_btn")],
            &[
                ("set_btn", "BOOL", LdVarSection::Input),
                ("rst_btn", "BOOL", LdVarSection::Input),
            ],
        );
        assert_compiles_clean(&prog);
    }

    // =================================================================
    //   Source map tests
    // =================================================================

    /// Find the 1-indexed line number where `needle` first appears in
    /// `s`. Panics if not found — test assertions need a hit, not a
    /// silent None.
    fn line_of(s: &str, needle: &str) -> usize {
        for (i, line) in s.lines().enumerate() {
            if line.contains(needle) {
                return i + 1;
            }
        }
        panic!("`{needle}` not found in:\n{s}");
    }

    #[test]
    fn source_map_locates_variable_declaration_lines() {
        let prog = motor_seal_program();
        let (st, map) = transpile_to_st_with_map(&prog).unwrap();
        // The line declaring `start_btn` must map to that variable.
        let n = line_of(&st, "start_btn : BOOL");
        match map.lookup(n) {
            Some(LdLocation::Variable { name }) => assert_eq!(name, "start_btn"),
            other => panic!("expected Variable, got {other:?}\n{st}"),
        }
    }

    #[test]
    fn source_map_locates_coil_assignment_lines() {
        let prog = motor_seal_program();
        let (st, map) = transpile_to_st_with_map(&prog).unwrap();
        // The single coil assignment is on the line that contains
        // `motor_run :=`.
        let n = line_of(&st, "motor_run := (");
        match map.lookup(n) {
            Some(LdLocation::Coil {
                rung_id,
                coil_index,
            }) => {
                assert_eq!(rung_id, "r0");
                assert_eq!(*coil_index, 0);
            }
            other => panic!("expected Coil, got {other:?}\n{st}"),
        }
    }

    #[test]
    fn source_map_locates_multi_coil_rung_lines() {
        // Multi-coil rung: temp assignment line maps to Rung; each
        // following coil assignment maps to its Coil with the right
        // index.
        let prog = LdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "a".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "x".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
                LdVariable {
                    name: "y".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![LdRung {
                id: "multi".into(),
                label: None,
                logic: LdNode::Contact {
                    var: "a".into(),
                    negated: false,
                },
                coils: vec![
                    LdCoil {
                        var: "x".into(),
                        kind: LdCoilKind::Standard,
                    },
                    LdCoil {
                        var: "y".into(),
                        kind: LdCoilKind::Standard,
                    },
                ],
            }],
        };
        let (st, map) = transpile_to_st_with_map(&prog).unwrap();
        // tmp assignment line is `__rung_multi := a;`
        let tmp_line = line_of(&st, "__rung_multi := a");
        match map.lookup(tmp_line) {
            Some(LdLocation::Rung { rung_id }) => assert_eq!(rung_id, "multi"),
            other => panic!("expected Rung, got {other:?}\n{st}"),
        }
        // Two coil lines
        let x_line = line_of(&st, "x := __rung_multi");
        match map.lookup(x_line) {
            Some(LdLocation::Coil {
                rung_id,
                coil_index,
            }) => {
                assert_eq!(rung_id, "multi");
                assert_eq!(*coil_index, 0);
            }
            other => panic!("expected Coil[0], got {other:?}\n{st}"),
        }
        let y_line = line_of(&st, "y := __rung_multi");
        match map.lookup(y_line) {
            Some(LdLocation::Coil {
                rung_id,
                coil_index,
            }) => {
                assert_eq!(rung_id, "multi");
                assert_eq!(*coil_index, 1);
            }
            other => panic!("expected Coil[1], got {other:?}\n{st}"),
        }
    }

    #[test]
    fn source_map_locates_fb_call_lines() {
        // FB call statement gets its own LdLocation::FbCall entry,
        // distinct from the rung's coil assignment line.
        let prog = LdProgram {
            name: "p".into(),
            pou_type: LdPouType::Program,
            variables: vec![
                LdVariable {
                    name: "btn".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Input,
                    init: None,
                },
                LdVariable {
                    name: "out".into(),
                    type_name: "BOOL".into(),
                    section: LdVarSection::Output,
                    init: None,
                },
            ],
            rungs: vec![LdRung {
                id: "rdelay".into(),
                label: None,
                logic: LdNode::FbCall {
                    instance: "myT".into(),
                    fb_type: "TON".into(),
                    inputs: vec![
                        LdFbInput {
                            pin: "IN".into(),
                            value: LdOperand::Var { name: "btn".into() },
                        },
                        LdFbInput {
                            pin: "PT".into(),
                            value: LdOperand::Literal {
                                value: "T#1s".into(),
                            },
                        },
                    ],
                    output_pin: "Q".into(),
                },
                coils: vec![LdCoil {
                    var: "out".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        };
        let (st, map) = transpile_to_st_with_map(&prog).unwrap();
        let call_line = line_of(&st, "myT(IN := btn");
        match map.lookup(call_line) {
            Some(LdLocation::FbCall { rung_id, instance }) => {
                assert_eq!(rung_id, "rdelay");
                assert_eq!(instance, "myT");
            }
            other => panic!("expected FbCall, got {other:?}\n{st}"),
        }
        // The coil assignment one line below should still be a Coil.
        let coil_line = line_of(&st, "out := myT.Q");
        match map.lookup(coil_line) {
            Some(LdLocation::Coil {
                rung_id,
                coil_index,
            }) => {
                assert_eq!(rung_id, "rdelay");
                assert_eq!(*coil_index, 0);
            }
            other => panic!("expected Coil, got {other:?}\n{st}"),
        }
    }

    #[test]
    fn source_map_returns_none_for_boilerplate_lines() {
        let prog = motor_seal_program();
        let (st, map) = transpile_to_st_with_map(&prog).unwrap();
        // PROGRAM header has no LD origin
        assert!(map.lookup(line_of(&st, "PROGRAM motor")).is_none());
        // VAR header line
        assert!(map.lookup(line_of(&st, "VAR_INPUT")).is_none());
        // END_PROGRAM line
        assert!(map.lookup(line_of(&st, "END_PROGRAM")).is_none());
    }

    #[test]
    fn source_map_line_count_matches_output() {
        // Integrity invariant — every emitted line gets exactly one
        // map slot. If a future emitter forgets to push, the
        // diagnostics on later lines drift by one.
        let prog = motor_seal_program();
        let (st, map) = transpile_to_st_with_map(&prog).unwrap();
        let line_count = st.lines().count();
        assert_eq!(
            line_count,
            map.lines.len(),
            "expected one map entry per emitted line; got {} lines vs {} entries\n{}",
            line_count,
            map.lines.len(),
            st,
        );
    }

    #[test]
    fn end_to_end_ld_diagnostic_carries_ld_location() {
        // A coil that references a variable not declared in `variables`
        // should produce an ironplc diagnostic. We want it to come back
        // tagged with `LdLocation::Coil` pointing at the offending rung,
        // so the editor can put a squiggle on the right glyph.
        let prog = LdProgram {
            name: "bad".into(),
            pou_type: LdPouType::Program,
            variables: vec![LdVariable {
                name: "btn".into(),
                type_name: "BOOL".into(),
                section: LdVarSection::Input,
                init: None,
            }],
            rungs: vec![LdRung {
                id: "loose".into(),
                label: None,
                logic: LdNode::Contact {
                    var: "btn".into(),
                    negated: false,
                },
                // `nope` is not declared anywhere — ironplc should
                // complain about an undefined variable.
                coils: vec![LdCoil {
                    var: "nope".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        };
        let source = serde_json::to_string(&prog).unwrap();
        let diags = crate::check_pou_source(&source, project::PouLanguage::Ld);
        assert!(!diags.is_empty(), "expected at least one diagnostic");
        // At least one diagnostic should be tagged as belonging to the
        // offending coil — we don't pin the exact code because that's
        // an ironplc implementation detail.
        let tagged = diags.iter().find(|d| {
            matches!(
                d.ld_location,
                Some(crate::ld_transpile::LdLocation::Coil { ref rung_id, coil_index })
                    if rung_id == "loose" && coil_index == 0
            )
        });
        assert!(
            tagged.is_some(),
            "expected a diagnostic tagged Coil(loose, 0); got: {diags:#?}",
        );
    }

    #[test]
    fn end_to_end_diagnostic_carries_problem_doc_explanation() {
        // ironplc's P4007 ("Variable not defined") has a doc page in
        // vendor/ironplc/docs/reference/compiler/problems/P4007.rst.
        // Confirm the bridge embeds it into the diagnostic so agents
        // and humans both see the explanation alongside the message.
        let prog = LdProgram {
            name: "explained".into(),
            pou_type: LdPouType::Program,
            variables: vec![LdVariable {
                name: "btn".into(),
                type_name: "BOOL".into(),
                section: LdVarSection::Input,
                init: None,
            }],
            rungs: vec![LdRung {
                id: "r".into(),
                label: None,
                logic: LdNode::Contact {
                    var: "btn".into(),
                    negated: false,
                },
                coils: vec![LdCoil {
                    var: "nope".into(),
                    kind: LdCoilKind::Standard,
                }],
            }],
        };
        let src = serde_json::to_string(&prog).unwrap();
        let diags = crate::check_pou_source(&src, project::PouLanguage::Ld);
        let p4007 = diags
            .iter()
            .find(|d| d.code == "P4007")
            .expect("expected P4007 in diagnostics");
        let explanation = p4007
            .explanation
            .as_ref()
            .expect("P4007 must carry the embedded RST explanation");
        // The RST body talks about "variable" + provides an example.
        assert!(
            explanation.to_lowercase().contains("variable"),
            "explanation should mention 'variable'; got: {explanation}",
        );
        // Context should include the offending variable name as
        // ironplc's `described` entry.
        assert!(
            p4007.context.iter().any(|c| c.contains("nope")),
            "expected `variable=nope` in context, got: {:?}",
            p4007.context,
        );
    }

    #[test]
    fn end_to_end_ld_malformed_json_returns_parse_diagnostic() {
        // A user mid-edit can save broken JSON. Rather than the LSP
        // endpoint going silent, surface a synthetic LD-PARSE diagnostic
        // so the editor at least shows "this file is broken".
        let diags = crate::check_pou_source("{ this isn't json", project::PouLanguage::Ld);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "LD-PARSE");
        assert!(diags[0].ld_location.is_none());
    }

    #[test]
    fn end_to_end_rs_compiles() {
        let prog = single_fb_rung_program(
            "RS",
            "myRs",
            "Q1",
            &[("S", "set_btn"), ("R1", "rst_btn")],
            &[
                ("set_btn", "BOOL", LdVarSection::Input),
                ("rst_btn", "BOOL", LdVarSection::Input),
            ],
        );
        assert_compiles_clean(&prog);
    }

    fn ton(instance: &str, input: &str) -> LdNode {
        fb(
            instance,
            "TON",
            &[("IN", lit(input)), ("PT", lit("T#200ms"))],
            "Q",
        )
    }

    /// The single diagnostic the editor gets for `prog`, which must be a
    /// located transpile error.
    fn transpile_diagnostic(prog: &LdProgram) -> crate::CheckDiagnostic {
        let source = serde_json::to_string(prog).unwrap();
        let diags = crate::check_pou_source(&source, project::PouLanguage::Ld);
        assert_eq!(diags.len(), 1, "{diags:#?}");
        assert_eq!(diags[0].code, "LD-TRANSPILE", "{diags:#?}");
        diags[0].clone()
    }

    /// IEC names are case-insensitive, so `T1` and `t1` are one name. The
    /// transpiler used to key instances by exact string and declare both;
    /// the program checked clean and its VM never started. Refused now,
    /// pointing at the later placement and naming both.
    #[test]
    fn instances_differing_only_in_case_are_refused_at_the_later_rung() {
        let prog = program(
            vec![bool_var("a", None), bool_var("b", None)],
            vec![
                rung("r0", ton("T1", "TRUE"), "a", LdCoilKind::Standard),
                rung("r1", ton("t1", "FALSE"), "b", LdCoilKind::Standard),
            ],
        );
        let err = transpile_to_st(&prog).unwrap_err().to_string();
        assert!(
            err.contains("'t1'")
                && err.contains("'T1'")
                && err.contains("'r1'")
                && err.contains("'r0'")
                && err.contains("case"),
            "{err}"
        );

        let d = transpile_diagnostic(&prog);
        assert!(
            matches!(
                d.ld_location,
                Some(LdLocation::FbCall { ref rung_id, ref instance })
                    if rung_id == "r1" && instance == "t1"
            ),
            "{d:#?}"
        );
    }

    /// An FB instance the diagram declares must not also be a variable
    /// the POU declares — in any spelling.
    #[test]
    fn an_instance_named_like_a_variable_is_refused() {
        let prog = program(
            vec![
                bool_var("a", None),
                LdVariable {
                    name: "Timer1".into(),
                    type_name: "TON".into(),
                    section: LdVarSection::Internal,
                    init: None,
                },
            ],
            vec![rung("r0", ton("timer1", "TRUE"), "a", LdCoilKind::Standard)],
        );
        let err = transpile_to_st(&prog).unwrap_err().to_string();
        assert!(
            err.contains("'timer1'") && err.contains("'Timer1'") && err.contains("variable"),
            "{err}"
        );

        let d = transpile_diagnostic(&prog);
        assert!(
            matches!(
                d.ld_location,
                Some(LdLocation::FbCall { ref rung_id, ref instance })
                    if rung_id == "r0" && instance == "timer1"
            ),
            "{d:#?}"
        );
    }

    /// Rung temporaries are the transpiler's own names, so a clash is
    /// renamed rather than refused — case-insensitively, and away from
    /// the POU's variables. `Fill`/`fill` used to declare `__rung_Fill`
    /// and `__rung_fill`, one name twice, and the VM never started.
    #[test]
    fn rung_temporaries_are_distinct_ignoring_case_and_user_variables() {
        let prog = program(
            vec![
                bool_var("a", Some("TRUE")),
                bool_var("p", None),
                bool_var("q", None),
                bool_var("r", None),
                bool_var("__rung_x", None),
            ],
            vec![
                rung("Fill", contact("a"), "p", LdCoilKind::Set),
                rung(
                    "fill",
                    LdNode::Not {
                        arg: Box::new(contact("a")),
                    },
                    "q",
                    LdCoilKind::Set,
                ),
                rung("x", contact("a"), "r", LdCoilKind::Set),
            ],
        );
        let st = transpile_to_st(&prog).unwrap();
        let v = crate::test_vm::run_st(&st, 2);
        assert_eq!((v["p"], v["q"], v["r"]), (1, 0, 1), "{st}");
    }
}
