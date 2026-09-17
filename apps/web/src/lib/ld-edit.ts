/**
 * Pure tree-transformation library for LD POU editing.
 *
 * The renderer (`LDEditor.tsx`) translates user gestures into calls
 * here; this module never reaches DOM, never touches React state.
 * Every function takes an `LdProgram` and returns a new `LdProgram`
 * (immutable). The corollary: every edit is unit-testable in isolation,
 * and undo/redo would be a stack of these snapshots.
 *
 * Path representation:
 *
 *   - `RungPath = number`                  index into `program.rungs`
 *   - `NodePath = readonly number[]`       sequence of child indices
 *     descending into a rung's `logic` tree. `[]` is the root;
 *     `[0]` is the 0th child; `[0, 1]` is the 1st child of the 0th.
 *     For `Not` nodes the only valid step is `0` (its single `arg`).
 *
 * Why a path and not an object reference: paths survive serialise →
 * mutate → deserialise round-trips (which we do every keystroke
 * because the canonical state is the JSON source). Object references
 * would dangle after each render.
 */

import type { LdCoilKind } from "@/types/generated/LdCoilKind"
import type { LdComparator } from "@/types/generated/LdComparator"
import type { LdFbInput } from "@/types/generated/LdFbInput"
import type { LdNode } from "@/types/generated/LdNode"
import type { LdOperand } from "@/types/generated/LdOperand"
import type { LdProgram } from "@/types/generated/LdProgram"

import { fbByType, fbInputs, fbBoolOutputs, suggestInstanceName } from "./ld-fbs"
import { parseProgramJson } from "./program-vars"

// `serializeProgram` and the variable mutators are byte-identical across
// the graphical editors — shared in `program-vars`, re-exported here so
// this module's public API (and its vitest suite) is unchanged.
export {
  addVariable,
  removeVariable,
  serializeProgram,
  updateVariable,
} from "./program-vars"

export type NodePath = readonly number[]

// =================================================================
//   Tree navigation
// =================================================================

/** Return the node at `path` within `root`. Throws on out-of-range
 *  steps — callers should always derive paths from a valid render
 *  pass, so a throw here is a real bug worth surfacing loudly. */
export function getNode(root: LdNode, path: NodePath): LdNode {
  let cur: LdNode = root
  for (let i = 0; i < path.length; i++) {
    const step = path[i]
    cur = childAt(cur, step)
  }
  return cur
}

function childAt(node: LdNode, step: number): LdNode {
  switch (node.op) {
    case "and":
    case "or":
      if (step < 0 || step >= node.args.length) {
        throw new Error(
          `LD path step ${step} out of range for ${node.op} (len=${node.args.length})`,
        )
      }
      return node.args[step]
    case "not":
      if (step !== 0) {
        throw new Error(
          `LD path step ${step} invalid for NOT — only child 0 exists`,
        )
      }
      return node.arg
    default:
      throw new Error(
        `LD path step ${step} attempted to descend into leaf ${node.op}`,
      )
  }
}

/** Replace the node at `path` with the result of `transform(old)`,
 *  returning a structurally-shared copy of `root`. */
function updateNode(
  root: LdNode,
  path: NodePath,
  transform: (n: LdNode) => LdNode,
): LdNode {
  if (path.length === 0) return transform(root)
  const [step, ...rest] = path
  switch (root.op) {
    case "and":
    case "or": {
      const args = root.args.slice()
      args[step] = updateNode(args[step], rest, transform)
      return { ...root, args }
    }
    case "not":
      if (step !== 0) throw new Error("NOT step must be 0")
      return { ...root, arg: updateNode(root.arg, rest, transform) }
    default:
      throw new Error(`cannot descend into leaf ${root.op}`)
  }
}

// =================================================================
//   Rung-level operations
// =================================================================

export function addRung(prog: LdProgram, atIndex?: number): LdProgram {
  const id = nextRungId(prog)
  // A fresh rung is a constant-TRUE network with no coil yet — the user
  // adds one next. Until then the transpiler refuses the rung ("no coils
  // — dead code"), which the editor shows as a diagnostic on it.
  const newRung = {
    id,
    label: null,
    logic: { op: "const", value: true } as LdNode,
    coils: [],
  }
  const idx = atIndex ?? prog.rungs.length
  const rungs = prog.rungs.slice()
  rungs.splice(idx, 0, newRung)
  return { ...prog, rungs }
}

export function deleteRung(prog: LdProgram, idx: number): LdProgram {
  if (idx < 0 || idx >= prog.rungs.length) return prog
  const rungs = prog.rungs.slice()
  rungs.splice(idx, 1)
  return { ...prog, rungs }
}

export function moveRung(
  prog: LdProgram,
  from: number,
  to: number,
): LdProgram {
  if (from === to) return prog
  if (from < 0 || from >= prog.rungs.length) return prog
  if (to < 0 || to >= prog.rungs.length) return prog
  const rungs = prog.rungs.slice()
  const [r] = rungs.splice(from, 1)
  rungs.splice(to, 0, r)
  return { ...prog, rungs }
}

export function setRungLabel(
  prog: LdProgram,
  rungIdx: number,
  label: string | null,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => ({ ...r, label }))
}

function updateRung(
  prog: LdProgram,
  rungIdx: number,
  transform: (r: LdProgram["rungs"][number]) => LdProgram["rungs"][number],
): LdProgram {
  if (rungIdx < 0 || rungIdx >= prog.rungs.length) return prog
  const rungs = prog.rungs.slice()
  rungs[rungIdx] = transform(rungs[rungIdx])
  return { ...prog, rungs }
}

function nextRungId(prog: LdProgram): string {
  // r0, r1, ... up to one past the highest existing numbered id. If
  // the user has custom ids, we still pick a non-colliding rN.
  const used = new Set(prog.rungs.map((r) => r.id))
  let n = prog.rungs.length
  while (used.has(`r${n}`)) n++
  return `r${n}`
}

// =================================================================
//   Logic-tree operations (operate within a single rung)
// =================================================================

/** Toggle the `negated` flag on a contact. No-op if the target isn't
 *  a contact. */
export function toggleNegated(
  prog: LdProgram,
  rungIdx: number,
  path: NodePath,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => ({
    ...r,
    logic: updateNode(r.logic, path, (n) =>
      n.op === "contact" ? { ...n, negated: !n.negated } : n,
    ),
  }))
}

/** Change the variable referenced by a contact. */
export function setContactVar(
  prog: LdProgram,
  rungIdx: number,
  path: NodePath,
  varName: string,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => ({
    ...r,
    logic: updateNode(r.logic, path, (n) =>
      n.op === "contact" ? { ...n, var: varName } : n,
    ),
  }))
}

/** Patch a Compare block — any subset of its three fields. No-op on
 *  non-compare nodes. */
export function updateCompare(
  prog: LdProgram,
  rungIdx: number,
  path: NodePath,
  patch: Partial<{ left: LdOperand; cmp: LdComparator; right: LdOperand }>,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => ({
    ...r,
    logic: updateNode(r.logic, path, (n) =>
      n.op === "compare" ? { ...n, ...patch } : n,
    ),
  }))
}

/** Patch an FbCall — used for the simple fields (instance, output_pin). */
export function updateFbCall(
  prog: LdProgram,
  rungIdx: number,
  path: NodePath,
  patch: Partial<{ instance: string; output_pin: string }>,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => ({
    ...r,
    logic: updateNode(r.logic, path, (n) =>
      n.op === "fb_call" ? { ...n, ...patch } : n,
    ),
  }))
}

/** Change an FbCall's FB type. Resets inputs to the new type's pin set
 *  (preserving existing operand values for pins of the same name) and
 *  resets `output_pin` to the new type's first BOOL output. This is the
 *  "swap TON → TOF" operation — they share pin names so it's lossless,
 *  but swap TON → CTU and the inputs reset since the pin set differs. */
export function setFbType(
  prog: LdProgram,
  rungIdx: number,
  path: NodePath,
  newType: string,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => ({
    ...r,
    logic: updateNode(r.logic, path, (n) => {
      if (n.op !== "fb_call") return n
      const newPins = fbInputs(newType)
      // Preserve operands for pins whose names exist in the new FB.
      const preserved = new Map(n.inputs.map((i) => [i.pin, i.value]))
      const inputs: LdFbInput[] = newPins.map((p) => ({
        pin: p.pin,
        value: preserved.get(p.pin) ?? defaultOperandFor(p.type),
      }))
      const outs = fbBoolOutputs(newType)
      const output_pin = outs.includes(n.output_pin) ? n.output_pin : outs[0] ?? "Q"
      return { ...n, fb_type: newType, inputs, output_pin }
    }),
  }))
}

/** Edit one of an FbCall's input pin operands. No-op if the pin name
 *  isn't in this FB's input list. */
export function setFbInputValue(
  prog: LdProgram,
  rungIdx: number,
  path: NodePath,
  pin: string,
  value: LdOperand,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => ({
    ...r,
    logic: updateNode(r.logic, path, (n) => {
      if (n.op !== "fb_call") return n
      // Replace the matching pin's value, leaving order intact.
      const inputs = n.inputs.map((i) => (i.pin === pin ? { ...i, value } : i))
      return { ...n, inputs }
    }),
  }))
}

/** Default operand for a pin of the given IEC type — sensible literal
 *  so a freshly-inserted FB doesn't have empty inputs (which would
 *  break the transpiler). */
function defaultOperandFor(iecType: string): LdOperand {
  const t = iecType.toUpperCase()
  if (t === "TIME") return { kind: "literal", value: "T#1s" }
  if (t === "BOOL") return { kind: "literal", value: "FALSE" }
  // INT / DINT / REAL etc.
  return { kind: "literal", value: "0" }
}

/**
 * Insert a new contact in series with the node at `path`. "Series"
 * means: extend the surrounding AND. If the target's parent is an AND,
 * append a sibling. Otherwise wrap the target in a fresh AND with the
 * new contact as the second sibling.
 *
 * `side`: "after" places the new contact to the right of the target,
 * "before" to the left.
 */
export function addInSeries(
  prog: LdProgram,
  rungIdx: number,
  path: NodePath,
  side: "before" | "after",
  newNode: LdNode,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => {
    const root = r.logic
    if (path.length === 0) {
      // Special case: root is already an AND — just extend it instead
      // of wrapping in another AND. Keeps the tree flat for common
      // "keep adding contacts to the same series" authoring flow.
      if (root.op === "and") {
        const args = side === "after" ? [...root.args, newNode] : [newNode, ...root.args]
        return { ...r, logic: { op: "and", args } }
      }
      // Adding in series with the root: wrap root in AND.
      const args = side === "after" ? [root, newNode] : [newNode, root]
      return { ...r, logic: { op: "and", args } }
    }
    const parentPath = path.slice(0, -1)
    const childIdx = path[path.length - 1]
    const parent = getNode(root, parentPath)
    if (parent.op === "and") {
      // Append a sibling without restructuring.
      const insertAt = side === "after" ? childIdx + 1 : childIdx
      const args = [...parent.args]
      args.splice(insertAt, 0, newNode)
      const newParent: LdNode = { op: "and", args }
      return {
        ...r,
        logic: updateNode(root, parentPath, () => newParent),
      }
    }
    // Wrap the target in a fresh AND.
    return {
      ...r,
      logic: updateNode(root, path, (target) => ({
        op: "and",
        args:
          side === "after" ? [target, newNode] : [newNode, target],
      })),
    }
  })
}

/**
 * Insert a new contact in parallel with the node at `path`. "Parallel"
 * extends the surrounding OR; if the parent isn't OR, wrap.
 *
 * `side`: "after" places the new branch below, "before" above.
 */
export function addInParallel(
  prog: LdProgram,
  rungIdx: number,
  path: NodePath,
  side: "before" | "after",
  newNode: LdNode,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => {
    const root = r.logic
    if (path.length === 0) {
      // Special case: root is already an OR — append rather than
      // nest another OR. Symmetric to addInSeries.
      if (root.op === "or") {
        const args = side === "after" ? [...root.args, newNode] : [newNode, ...root.args]
        return { ...r, logic: { op: "or", args } }
      }
      const args = side === "after" ? [root, newNode] : [newNode, root]
      return { ...r, logic: { op: "or", args } }
    }
    const parentPath = path.slice(0, -1)
    const childIdx = path[path.length - 1]
    const parent = getNode(root, parentPath)
    if (parent.op === "or") {
      const insertAt = side === "after" ? childIdx + 1 : childIdx
      const args = [...parent.args]
      args.splice(insertAt, 0, newNode)
      const newParent: LdNode = { op: "or", args }
      return {
        ...r,
        logic: updateNode(root, parentPath, () => newParent),
      }
    }
    return {
      ...r,
      logic: updateNode(root, path, (target) => ({
        op: "or",
        args:
          side === "after" ? [target, newNode] : [newNode, target],
      })),
    }
  })
}

/**
 * Delete the node at `path`. Removes it from its parent's child list;
 * if the parent then has exactly one remaining child, the parent
 * collapses into that child (avoids dangling 1-arg AND/OR nodes).
 *
 * Deleting the root replaces the rung's logic with `const true` —
 * we never leave a rung with no logic, because the transpiler can't
 * emit ST for an empty network.
 */
export function deleteNode(
  prog: LdProgram,
  rungIdx: number,
  path: NodePath,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => {
    if (path.length === 0) {
      return { ...r, logic: { op: "const", value: true } }
    }
    const parentPath = path.slice(0, -1)
    const childIdx = path[path.length - 1]
    const parent = getNode(r.logic, parentPath)
    if (parent.op === "and" || parent.op === "or") {
      const args = parent.args.filter((_, i) => i !== childIdx)
      const collapsed: LdNode =
        args.length === 1
          ? args[0]
          : args.length === 0
            ? // Empty AND/OR shouldn't happen in practice, but collapse
              // safely to identity.
              { op: "const", value: parent.op === "and" }
            : { ...parent, args }
      return {
        ...r,
        logic: updateNode(r.logic, parentPath, () => collapsed),
      }
    }
    if (parent.op === "not") {
      // Deleting the sole child of NOT collapses the NOT to identity.
      return {
        ...r,
        logic: updateNode(r.logic, parentPath, () => ({
          op: "const",
          value: true,
        })),
      }
    }
    return r
  })
}

// =================================================================
//   Coil operations
// =================================================================

export function addCoil(
  prog: LdProgram,
  rungIdx: number,
  coilVar: string,
  kind: LdCoilKind = "standard",
): LdProgram {
  return updateRung(prog, rungIdx, (r) => ({
    ...r,
    coils: [...r.coils, { var: coilVar, kind }],
  }))
}

export function deleteCoil(
  prog: LdProgram,
  rungIdx: number,
  coilIdx: number,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => ({
    ...r,
    coils: r.coils.filter((_, i) => i !== coilIdx),
  }))
}

export function setCoilVar(
  prog: LdProgram,
  rungIdx: number,
  coilIdx: number,
  varName: string,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => {
    const coils = r.coils.slice()
    if (coils[coilIdx]) coils[coilIdx] = { ...coils[coilIdx], var: varName }
    return { ...r, coils }
  })
}

export function setCoilKind(
  prog: LdProgram,
  rungIdx: number,
  coilIdx: number,
  kind: LdCoilKind,
): LdProgram {
  return updateRung(prog, rungIdx, (r) => {
    const coils = r.coils.slice()
    if (coils[coilIdx]) coils[coilIdx] = { ...coils[coilIdx], kind }
    return { ...r, coils }
  })
}

// Variable CRUD (addVariable / removeVariable / updateVariable) is
// shared — see the re-export from `program-vars` at the top of file.

// =================================================================
//   Construction helpers
// =================================================================

export function newContact(varName = "x", negated = false): LdNode {
  return { op: "contact", var: varName, negated }
}

/** Default new compare block — placeholder shape the user customises
 *  via the detail bar. `var < 0.0` is the most universally-applicable
 *  starter for a process-control feel. */
export function newCompare(): LdNode {
  return {
    op: "compare",
    left: { kind: "var", name: "x" },
    cmp: "lt",
    right: { kind: "literal", value: "0" },
  }
}

/** Construct a fresh FbCall node for the given FB type. Picks an
 *  unused instance name (`myT1`, `myT2`, ...), populates each input
 *  pin with a sensible default literal, and selects the FB's first
 *  BOOL output pin. Returns the node + the instance name so the
 *  caller can also auto-add a variable declaration if desired (we
 *  *don't* add a var entry — the transpiler synthesises the
 *  `inst : TYPE;` declaration). */
export function newFbCall(
  prog: LdProgram,
  fbType: string,
): { node: LdNode; instance: string } {
  const used = new Set<string>()
  // Collect every existing FbCall instance across the program.
  for (const r of prog.rungs) collectInstances(r.logic, used)
  const instance = suggestInstanceName(fbType, used)
  const def = fbByType(fbType)
  const inputs: LdFbInput[] = def
    ? def.pins
        .filter((p) => p.direction === "input")
        .map((p) => ({ pin: p.pin, value: defaultOperandFor(p.type) }))
    : []
  const output_pin = fbBoolOutputs(fbType)[0] ?? "Q"
  return {
    node: { op: "fb_call", instance, fb_type: fbType, inputs, output_pin },
    instance,
  }
}

function collectInstances(node: LdNode, out: Set<string>): void {
  switch (node.op) {
    case "fb_call":
      out.add(node.instance)
      return
    case "and":
    case "or":
      for (const a of node.args) collectInstances(a, out)
      return
    case "not":
      collectInstances(node.arg, out)
      return
    default:
      return
  }
}

// =================================================================
//   JSON round-trip
// =================================================================

/** Parse a source string into a typed program. Throws on invalid
 *  JSON; the caller (LDEditor) treats that as a parse error and
 *  falls back to the JSON-text view. (`serializeProgram` is shared —
 *  see the `program-vars` re-export at the top of file.) */
export function parseProgram(source: string): LdProgram {
  return parseProgramJson<LdProgram>(source)
}

// =================================================================
//   Online-mode evaluator
//
//   Given a snapshot of live BOOL values, compute whether each node in
//   the rung's logic tree is "conducting" — i.e. would carry power if
//   we projected its sub-expression onto a real ladder. The renderer
//   uses this to colour glyphs and wires in real time while the
//   program runs on the bridge.
//
//   This is a *pure* function over the tree; it doesn't know about
//   wires or rendering. The renderer recursively asks "is THIS node
//   conducting?" — same recursion shape as drawing the node.
// =================================================================

/** Live readings for the online evaluator. `null` means the value is not
 *  on the wire — the program is not running here, or the runtime does not
 *  publish it (FB outputs) — and is never the same thing as FALSE or 0. */
export type LiveReadings = {
  bool: (name: string) => boolean | null
  number: (name: string) => number | null
}

/** Readings over plain maps; a name absent from a map is unknown. */
export function readingsFrom(
  bools: Readonly<Record<string, boolean>>,
  numerics: Readonly<Record<string, number>> = {},
): LiveReadings {
  return {
    bool: (name) => (name in bools ? bools[name] === true : null),
    number: (name) => (name in numerics ? numerics[name] : null),
  }
}

/** Recursively evaluate an LD node against live values, in three-valued
 *  logic: TRUE / FALSE when the readings settle it, `null` when they don't.
 *  An AND with a FALSE branch is FALSE and an OR with a TRUE branch is TRUE
 *  whatever the unknowns; anything else touching an unknown is unknown.
 *
 *  A missing value used to read as FALSE. That drew an NC contact on an
 *  unpublished variable as closed, and every rung through an FB — whose
 *  outputs the runtime never publishes — as dead.
 *
 *  Important: `negated` on the wire format is `#[serde(default)]` in
 *  Rust, so a JSON file with `{op:"contact", var:"x"}` (no `negated`)
 *  is valid input. On the TS side `negated` then arrives as
 *  `undefined`, and `false !== undefined` evaluates to `true` — which
 *  would silently mark every non-negated FALSE contact as conducting.
 *  Coerce to a real boolean here.
 */
export function evaluateNode(node: LdNode, live: LiveReadings): boolean | null {
  switch (node.op) {
    case "contact": {
      const v = live.bool(node.var)
      return v === null ? null : v !== (node.negated === true)
    }
    case "const":
      return node.value === true
    case "not": {
      const v = evaluateNode(node.arg, live)
      return v === null ? null : !v
    }
    case "and": {
      let unknown = false
      for (const a of node.args) {
        const v = evaluateNode(a, live)
        if (v === false) return false
        if (v === null) unknown = true
      }
      return unknown ? null : true
    }
    case "or": {
      let unknown = false
      for (const a of node.args) {
        const v = evaluateNode(a, live)
        if (v === true) return true
        if (v === null) unknown = true
      }
      return unknown ? null : false
    }
    case "compare":
      return evaluateCompare(node.left, node.cmp, node.right, live)
    case "fb_call":
      // The FB body runs in the VM, and the runtime publishes the instance,
      // not its pins — so this is normally unknown. Read the dotted name in
      // case a runtime does publish it.
      return live.bool(`${node.instance}.${node.output_pin}`)
  }
}

/** Resolve a Compare operand to a number; `null` when a variable has no
 *  numeric live value or a literal has no number in it. */
export function readOperand(o: LdOperand, live: LiveReadings): number | null {
  if (o.kind === "var") return live.number(o.name)
  // Literal: parse the raw string. TIME literals like "T#100ms" parse
  // through `parseFloat` (gives 100, the ms — close enough for online
  // colouring; full TIME semantics are the bridge's job).
  const m = String(o.value).match(/-?\d+(?:\.\d+)?/)
  if (!m) return null
  const n = parseFloat(m[0])
  return Number.isFinite(n) ? n : null
}

function evaluateCompare(
  left: LdOperand,
  cmp: LdComparator,
  right: LdOperand,
  live: LiveReadings,
): boolean | null {
  const a = readOperand(left, live)
  const b = readOperand(right, live)
  if (a === null || b === null) return null
  switch (cmp) {
    case "eq":
      return a === b
    case "ne":
      return a !== b
    case "lt":
      return a < b
    case "le":
      return a <= b
    case "gt":
      return a > b
    case "ge":
      return a >= b
  }
}

/** Operator → ST/textual symbol, for renderer labels. */
export function comparatorSymbol(c: LdComparator): string {
  switch (c) {
    case "eq":
      return "="
    case "ne":
      return "≠"
    case "lt":
      return "<"
    case "le":
      return "≤"
    case "gt":
      return ">"
    case "ge":
      return "≥"
  }
}

/** Compact textual form for an operand — `var` shows the name, `literal`
 *  shows the value verbatim. Used inside the Compare block glyph. */
export function operandText(o: LdOperand): string {
  return o.kind === "var" ? o.name : o.value
}
